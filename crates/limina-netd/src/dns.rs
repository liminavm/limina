//! DNS at the gateway, answered through the host's resolver (mDNSResponder's
//! `DNSServiceQueryRecord`), so a VPN's split DNS and scoped resolvers apply to the guest as to
//! any app. netd parses the guest's question, asks for that name, type and class, and builds the
//! response from the records the callbacks hand back (`spikes/netd-dns-probe/RESULTS.md`):
//!
//! - with `ReturnIntermediates | Timeout`, a query's answer is one batch of callbacks, ending at the
//!   first without `MoreComing`, CNAMEs first and each record under its own owner name;
//! - a missing name and an empty answer both arrive as `NoSuchRecord`, so both become NOERROR
//!   with no records.
//!
//! Each query has its own `DNSServiceRef`, whose socket joins netd's kqueue.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use crate::poller::Poller;

/// How long a question may wait for the host's resolver before netd gives up on it (SERVFAIL).
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// The most questions in flight; beyond it new ones fail at once.
pub const MAX_PENDING: usize = 256;
/// The UDP payload size netd advertises in EDNS, and the most it answers with.
const EDNS_SIZE: u16 = 1232;

pub const RCODE_NOERROR: u8 = 0;
pub const RCODE_FORMERR: u8 = 1;
pub const RCODE_SERVFAIL: u8 = 2;
pub const RCODE_NOTIMP: u8 = 4;

const TYPE_OPT: u16 = 41;

/// A question from the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    id: u16,
    /// The guest's RD bit, echoed back.
    recursion_desired: bool,
    /// The question section as the guest sent it, name and all.
    question: Vec<u8>,
    /// The name in the presentation form mDNSResponder takes, escapes and trailing dot included.
    name: String,
    qtype: u16,
    qclass: u16,
    /// The UDP payload size the guest advertised in EDNS, if it sent an OPT record.
    edns: Option<u16>,
}

/// Why a datagram to port 53 gets no question asked.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not something to answer at all (a response, or too short to have an ID).
    Drop,
    /// Answered with this response code and no records.
    Answer { id: u16, rcode: u8 },
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

/// Parses a name in the guest's question into presentation form, returning it and where the name
/// ended. Compression pointers are refused: a question has nothing earlier to point at.
fn parse_name(msg: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut wire_len = 0;
    loop {
        let len = *msg.get(at)? as usize;
        at += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return None;
        }
        wire_len += len + 1;
        if wire_len > 255 {
            return None;
        }
        for &b in msg.get(at..at + len)? {
            match b {
                b'.' | b'\\' => {
                    name.push('\\');
                    name.push(b as char);
                }
                0x21..=0x7e => name.push(b as char),
                _ => name.push_str(&format!("\\{b:03}")),
            }
        }
        name.push('.');
        at += len;
    }
    if name.is_empty() {
        name.push('.');
    }
    Some((name, at))
}

/// Parses the guest's DNS message.
pub fn parse_query(msg: &[u8]) -> Result<Query, Refusal> {
    let id = u16_at(msg, 0).ok_or(Refusal::Drop)?;
    let flags = u16_at(msg, 2).ok_or(Refusal::Drop)?;
    if flags & 0x8000 != 0 {
        return Err(Refusal::Drop);
    }
    let formerr = Refusal::Answer {
        id,
        rcode: RCODE_FORMERR,
    };
    let opcode = (flags >> 11) & 0xf;
    if opcode != 0 {
        return Err(Refusal::Answer {
            id,
            rcode: RCODE_NOTIMP,
        });
    }
    let (qd, an, ns, ar) = (
        u16_at(msg, 4),
        u16_at(msg, 6),
        u16_at(msg, 8),
        u16_at(msg, 10),
    );
    if qd != Some(1) || an != Some(0) || ns != Some(0) {
        return Err(formerr);
    }
    let (name, end) = parse_name(msg, 12).ok_or(Refusal::Answer {
        id,
        rcode: RCODE_FORMERR,
    })?;
    let qtype = u16_at(msg, end).ok_or(Refusal::Answer {
        id,
        rcode: RCODE_FORMERR,
    })?;
    let qclass = u16_at(msg, end + 2).ok_or(Refusal::Answer {
        id,
        rcode: RCODE_FORMERR,
    })?;
    let question = msg[12..end + 4].to_vec();
    // The one additional record a stub sends is EDNS's OPT: root name, type 41, class = size.
    let mut edns = None;
    if ar == Some(1) {
        let at = end + 4;
        if msg.get(at) == Some(&0) && u16_at(msg, at + 1) == Some(TYPE_OPT) {
            edns = u16_at(msg, at + 3);
        }
    }
    Ok(Query {
        id,
        recursion_desired: flags & 0x0100 != 0,
        question,
        name,
        qtype,
        qclass,
        edns,
    })
}

/// A record mDNSResponder handed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The owner name, in presentation form.
    pub name: String,
    pub rtype: u16,
    pub rclass: u16,
    pub ttl: u32,
    /// In wire form, names inside it uncompressed.
    pub rdata: Vec<u8>,
}

/// Encodes a presentation-form name (`\.`, `\\` and `\ddd` escapes) in wire form.
fn encode_name(name: &str, out: &mut Vec<u8>) -> Option<()> {
    let bytes = name.as_bytes();
    let mut label = Vec::new();
    let mut i = 0;
    let start = out.len();
    let flush = |label: &mut Vec<u8>, out: &mut Vec<u8>| -> Option<()> {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label);
        label.clear();
        Some(())
    };
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let rest = bytes.get(i + 1..)?;
                if rest.len() >= 3 && rest[..3].iter().all(u8::is_ascii_digit) {
                    let v: u16 = std::str::from_utf8(&rest[..3]).ok()?.parse().ok()?;
                    label.push(u8::try_from(v).ok()?);
                    i += 4;
                } else {
                    label.push(*rest.first()?);
                    i += 2;
                }
            }
            b'.' => {
                flush(&mut label, out)?;
                i += 1;
            }
            b => {
                label.push(b);
                i += 1;
            }
        }
    }
    if !label.is_empty() {
        flush(&mut label, out)?;
    }
    out.push(0);
    (out.len() - start <= 255).then_some(())
}

/// The response to `q`: `rcode` and `answers`, cut to what the guest can take over UDP (with TC
/// set and no answers when they do not fit).
pub fn response(q: &Query, rcode: u8, answers: &[Record]) -> Vec<u8> {
    let limit = q.edns.map_or(512, |s| s.clamp(512, EDNS_SIZE)) as usize;
    let header = |out: &mut Vec<u8>, ancount: u16, truncated: bool| {
        out.extend_from_slice(&q.id.to_be_bytes());
        let mut flags: u16 = 0x8000 | 0x0080; // QR, RA
        if q.recursion_desired {
            flags |= 0x0100;
        }
        if truncated {
            flags |= 0x0200;
        }
        flags |= rcode as u16;
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&ancount.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&(q.edns.is_some() as u16).to_be_bytes());
        out.extend_from_slice(&q.question);
    };
    let opt = |out: &mut Vec<u8>| {
        if q.edns.is_some() {
            out.push(0);
            out.extend_from_slice(&TYPE_OPT.to_be_bytes());
            out.extend_from_slice(&EDNS_SIZE.to_be_bytes());
            out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        }
    };
    let mut body = Vec::new();
    let mut count = 0u16;
    for r in answers {
        let mark = body.len();
        if encode_name(&r.name, &mut body).is_none() || r.rdata.len() > u16::MAX as usize {
            body.truncate(mark);
            continue;
        }
        body.extend_from_slice(&r.rtype.to_be_bytes());
        body.extend_from_slice(&r.rclass.to_be_bytes());
        body.extend_from_slice(&r.ttl.to_be_bytes());
        body.extend_from_slice(&(r.rdata.len() as u16).to_be_bytes());
        body.extend_from_slice(&r.rdata);
        count += 1;
    }
    let mut out = Vec::new();
    header(&mut out, count, false);
    out.extend_from_slice(&body);
    opt(&mut out);
    if out.len() > limit {
        out.clear();
        header(&mut out, 0, true);
        opt(&mut out);
    }
    out
}

/// The response for a question netd will not ask (`Refusal::Answer`), echoing what it can of the
/// guest's header.
pub fn refusal(msg: &[u8], id: u16, rcode: u8) -> Vec<u8> {
    let rd = u16_at(msg, 2).unwrap_or(0) & 0x0100;
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&(0x8000 | 0x0080 | rd | rcode as u16).to_be_bytes());
    out.extend_from_slice(&[0; 8]);
    out
}

// <dns_sd.h>
type DNSServiceRef = *mut c_void;
const FLAG_MORE_COMING: u32 = 0x1;
const FLAG_ADD: u32 = 0x2;
const FLAG_RETURN_INTERMEDIATES: u32 = 0x1000;
const FLAG_TIMEOUT: u32 = 0x10000;
const ERR_NO_SUCH_RECORD: i32 = -65554;
const ERR_NO_SUCH_NAME: i32 = -65538;

type QueryRecordReply = extern "C" fn(
    sd: DNSServiceRef,
    flags: u32,
    interface: u32,
    error: i32,
    fullname: *const c_char,
    rrtype: u16,
    rrclass: u16,
    rdlen: u16,
    rdata: *const c_void,
    ttl: u32,
    context: *mut c_void,
);

unsafe extern "C" {
    fn DNSServiceQueryRecord(
        sd: *mut DNSServiceRef,
        flags: u32,
        interface: u32,
        fullname: *const c_char,
        rrtype: u16,
        rrclass: u16,
        callback: QueryRecordReply,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(sd: DNSServiceRef) -> i32;
    fn DNSServiceProcessResult(sd: DNSServiceRef) -> i32;
    fn DNSServiceRefDeallocate(sd: DNSServiceRef);
}

/// One question in flight, boxed so the callback's context pointer stays put.
struct Pending {
    sd: DNSServiceRef,
    guest_port: u16,
    query: Query,
    records: Vec<Record>,
    /// Set by the callback once the batch is complete.
    rcode: Option<u8>,
    deadline: Instant,
}

impl Drop for Pending {
    fn drop(&mut self) {
        // SAFETY: allocated by DNSServiceQueryRecord and released once, here; this also closes
        // its socket, which leaves the kqueue.
        unsafe { DNSServiceRefDeallocate(self.sd) };
    }
}

extern "C" fn on_record(
    _sd: DNSServiceRef,
    flags: u32,
    _interface: u32,
    error: i32,
    fullname: *const c_char,
    rrtype: u16,
    rrclass: u16,
    rdlen: u16,
    rdata: *const c_void,
    ttl: u32,
    context: *mut c_void,
) {
    // SAFETY: the context is the Pending this query was started with, alive until its ref is
    // deallocated, and callbacks run only inside DNSServiceProcessResult on netd's one thread.
    let p = unsafe { &mut *(context as *mut Pending) };
    if p.rcode.is_some() {
        return;
    }
    match error {
        0 if flags & FLAG_ADD != 0 && !fullname.is_null() => {
            // SAFETY: mDNSResponder passes a NUL-terminated name and `rdlen` bytes of rdata.
            let name = unsafe { CStr::from_ptr(fullname) }
                .to_string_lossy()
                .into_owned();
            let rdata = if rdlen == 0 || rdata.is_null() {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(rdata as *const u8, rdlen as usize) }.to_vec()
            };
            p.records.push(Record {
                name,
                rtype: rrtype,
                rclass: rrclass,
                ttl,
                rdata,
            });
        }
        0 | ERR_NO_SUCH_RECORD | ERR_NO_SUCH_NAME => {}
        e => {
            log::debug!("DNS {}: mDNSResponder error {e}", p.query.name);
            p.rcode = Some(RCODE_SERVFAIL);
            return;
        }
    }
    if flags & FLAG_MORE_COMING == 0 {
        p.rcode = Some(RCODE_NOERROR);
    }
}

/// The questions in flight, by their sockets.
#[derive(Default)]
pub struct Resolver {
    pending: HashMap<RawFd, Box<Pending>>,
}

/// A finished question: the guest port to answer and the DNS message to answer it with.
pub type Answer = (u16, Vec<u8>);

impl Resolver {
    /// Asks the host's resolver `query` for the guest's `guest_port`. When it cannot even be
    /// asked, the SERVFAIL comes back at once.
    pub fn ask(&mut self, guest_port: u16, query: Query, poller: &Poller) -> Option<Answer> {
        if self.pending.len() >= MAX_PENDING {
            return Some((guest_port, response(&query, RCODE_SERVFAIL, &[])));
        }
        let Ok(name) = CString::new(query.name.clone()) else {
            return Some((guest_port, response(&query, RCODE_FORMERR, &[])));
        };
        let (qtype, qclass) = (query.qtype, query.qclass);
        let mut p = Box::new(Pending {
            sd: std::ptr::null_mut(),
            guest_port,
            query,
            records: Vec::new(),
            rcode: None,
            deadline: Instant::now() + QUERY_TIMEOUT,
        });
        let mut sd = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name, our callback, and a context that outlives the ref (the
        // box is stored below and drops the ref before itself).
        let err = unsafe {
            DNSServiceQueryRecord(
                &mut sd,
                FLAG_RETURN_INTERMEDIATES | FLAG_TIMEOUT,
                0,
                name.as_ptr(),
                qtype,
                qclass,
                on_record,
                &mut *p as *mut Pending as *mut c_void,
            )
        };
        if err != 0 {
            log::debug!("DNS {}: cannot ask mDNSResponder ({err})", p.query.name);
            return Some((guest_port, response(&p.query, RCODE_SERVFAIL, &[])));
        }
        p.sd = sd;
        // SAFETY: a live ref.
        let fd = unsafe { DNSServiceRefSockFD(sd) };
        if fd < 0 || poller.add(fd).is_err() {
            return Some((guest_port, response(&p.query, RCODE_SERVFAIL, &[])));
        }
        self.pending.insert(fd, p);
        None
    }

    pub fn owns(&self, fd: RawFd) -> bool {
        self.pending.contains_key(&fd)
    }

    /// Reads what mDNSResponder sent on `fd`; the answer, once the question is complete.
    pub fn process(&mut self, fd: RawFd) -> Option<Answer> {
        let p = self.pending.get_mut(&fd)?;
        // SAFETY: a live ref whose socket is readable; runs `on_record` with `p` as its context.
        let err = unsafe { DNSServiceProcessResult(p.sd) };
        if err != 0 {
            p.rcode = Some(RCODE_SERVFAIL);
        }
        p.rcode?;
        let p = self.pending.remove(&fd)?;
        let rcode = p.rcode.unwrap_or(RCODE_SERVFAIL);
        log::debug!(
            "DNS: answered {} type {} with rcode {rcode} and {} records",
            p.query.name,
            p.query.qtype,
            p.records.len()
        );
        Some((p.guest_port, response(&p.query, rcode, &p.records)))
    }

    /// Fails the questions past their deadline.
    pub fn expire(&mut self, now: Instant) -> Vec<Answer> {
        let late: Vec<RawFd> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(fd, _)| *fd)
            .collect();
        late.into_iter()
            .filter_map(|fd| self.pending.remove(&fd))
            .map(|p| (p.guest_port, response(&p.query, RCODE_SERVFAIL, &[])))
            .collect()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.deadline).min()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A query as a stub resolver sends it, optionally with EDNS.
    pub(crate) fn query_msg(name: &[&[u8]], qtype: u16, edns: Option<u16>) -> Vec<u8> {
        let mut m = vec![
            0xab,
            0xcd,
            0x01,
            0x00,
            0,
            1,
            0,
            0,
            0,
            0,
            0,
            edns.is_some() as u8,
        ];
        for l in name {
            m.push(l.len() as u8);
            m.extend_from_slice(l);
        }
        m.push(0);
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&1u16.to_be_bytes());
        if let Some(size) = edns {
            m.push(0);
            m.extend_from_slice(&TYPE_OPT.to_be_bytes());
            m.extend_from_slice(&size.to_be_bytes());
            m.extend_from_slice(&[0; 6]);
        }
        m
    }

    #[test]
    fn a_question_is_parsed_and_its_name_escaped_for_mdnsresponder() {
        let q = parse_query(&query_msg(&[b"www", b"a.b\\c", b"com"], 1, Some(1232))).unwrap();
        assert_eq!(q.id, 0xabcd);
        assert!(q.recursion_desired);
        assert_eq!(q.name, "www.a\\.b\\\\c.com.");
        assert_eq!((q.qtype, q.qclass, q.edns), (1, 1, Some(1232)));
        let mut wire = Vec::new();
        encode_name(&q.name, &mut wire).unwrap();
        assert_eq!(wire, q.question[..q.question.len() - 4]);
    }

    #[test]
    fn malformed_questions_are_refused_or_dropped() {
        assert_eq!(parse_query(&[1]), Err(Refusal::Drop));
        let mut response = query_msg(&[b"x"], 1, None);
        response[2] |= 0x80;
        assert_eq!(parse_query(&response), Err(Refusal::Drop));
        let mut two = query_msg(&[b"x"], 1, None);
        two[5] = 2;
        assert_eq!(
            parse_query(&two),
            Err(Refusal::Answer {
                id: 0xabcd,
                rcode: RCODE_FORMERR
            })
        );
        let mut pointer = query_msg(&[b"x"], 1, None);
        pointer[12] = 0xc0;
        assert!(matches!(
            parse_query(&pointer),
            Err(Refusal::Answer {
                rcode: RCODE_FORMERR,
                ..
            })
        ));
    }

    #[test]
    fn a_response_carries_the_records_and_echoes_the_question() {
        let q = parse_query(&query_msg(&[b"www", b"github", b"com"], 1, None)).unwrap();
        let cname = Record {
            name: "www.github.com.".into(),
            rtype: 5,
            rclass: 1,
            ttl: 60,
            rdata: vec![
                6, b'g', b'i', b't', b'h', b'u', b'b', 3, b'c', b'o', b'm', 0,
            ],
        };
        let a = Record {
            name: "github.com.".into(),
            rtype: 1,
            rclass: 1,
            ttl: 60,
            rdata: vec![140, 82, 121, 4],
        };
        let r = response(&q, RCODE_NOERROR, &[cname, a]);
        assert_eq!(&r[0..2], &[0xab, 0xcd]);
        assert_eq!(u16_at(&r, 2), Some(0x8180));
        assert_eq!((u16_at(&r, 4), u16_at(&r, 6)), (Some(1), Some(2)));
        assert_eq!(&r[12..12 + q.question.len()], &q.question[..]);
        assert!(r.ends_with(&[0, 4, 140, 82, 121, 4]));
    }

    #[test]
    fn an_oversized_answer_is_truncated_for_a_stub_without_edns() {
        let q = parse_query(&query_msg(&[b"big"], 16, None)).unwrap();
        let txt = Record {
            name: "big.".into(),
            rtype: 16,
            rclass: 1,
            ttl: 1,
            rdata: vec![b'x'; 400],
        };
        let r = response(&q, RCODE_NOERROR, &[txt.clone(), txt.clone()]);
        assert!(r.len() <= 512);
        assert_eq!(u16_at(&r, 2).unwrap() & 0x0200, 0x0200, "TC set");
        assert_eq!(u16_at(&r, 6), Some(0));
        let q = parse_query(&query_msg(&[b"big"], 16, Some(4096))).unwrap();
        let r = response(&q, RCODE_NOERROR, &[txt.clone(), txt]);
        assert_eq!(u16_at(&r, 6), Some(2), "fits in the EDNS size");
    }

    /// The host resolver answers `localhost` itself, network or not.
    #[test]
    fn the_host_resolver_answers_through_the_poller() {
        let poller = Poller::new().unwrap();
        let mut resolver = Resolver::default();
        let q = parse_query(&query_msg(&[b"localhost"], 1, None)).unwrap();
        assert_eq!(resolver.ask(5353, q, &poller), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut fds = Vec::new();
        let answer = loop {
            assert!(Instant::now() < deadline, "no answer for localhost");
            poller
                .wait(Some(Duration::from_millis(500)), &mut fds)
                .unwrap();
            if let Some(a) = fds.iter().find_map(|e| resolver.process(e.fd)) {
                break a;
            }
        };
        assert_eq!(answer.0, 5353);
        assert_eq!(u16_at(&answer.1, 2).unwrap() & 0xf, RCODE_NOERROR as u16);
        assert!(answer.1.ends_with(&[0, 4, 127, 0, 0, 1]), "{:?}", answer.1);
        assert!(resolver.next_deadline().is_none());
    }
}
