// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The limina host↔guest control-plane wire protocol (roadmap M5 task 1, decision D8).
//!
//! One multiplexed connection (vsock: guest connects out to `CID_HOST`, the host listens
//! on a unix socket libkrun bridges) carries every control-plane conversation. Each
//! message is a **16-byte frame header + a CBOR payload**:
//!
//! ```text
//! offset  size  field
//!      0     4  magic   b"LIMI"
//!      4     1  version (PROTOCOL_VERSION)
//!      5     1  msg_type
//!      6     2  flags   (reserved, 0; little-endian)
//!      8     4  channel (0 = control; future: clipboard, display, …; little-endian)
//!     12     4  payload length (little-endian; <= MAX_PAYLOAD)
//! ```
//!
//! Robustness rules (the load-bearing ones):
//! - **Unknown message types are NEVER fatal**: they decode to [`Message::Unknown`] and the
//!   receiver replies [`Message::Error`] with [`ERR_UNSUPPORTED`], keeping the channel up —
//!   that's what lets old agents talk to new hosts and vice versa.
//! - Capability negotiation happens in HELLO/WELCOME (string capability sets), not via
//!   header-version bumps; the header version only changes if the *framing* changes.
//! - Payload length is bounded by [`MAX_PAYLOAD`] so a corrupt header can't OOM either end.
//!
//! First conversation (the M5 task-1 set):
//! guest → HELLO (agent id, capabilities, facts) → host replies WELCOME (its capabilities);
//! guest sends periodic HEARTBEAT; host may send SHUTDOWN, the guest acks with SHUTDOWN_ACK
//! and powers itself off (the orderly window-close path; `krun_get_shutdown_eventfd` remains
//! the forcing fallback).
//!
//! This crate is shared by the macOS host and the static musl guest binary — std-portable
//! code only, tiny dependency tree (minicbor).

use std::io::{self, Read, Write};

use minicbor::{Decode, Encode};

/// Frame magic: the first four bytes of every frame.
pub const MAGIC: [u8; 4] = *b"LIMI";
/// Framing version (bump only if the 16-byte header layout itself changes).
pub const PROTOCOL_VERSION: u8 = 1;
/// Size of the fixed frame header.
pub const HEADER_LEN: usize = 16;
/// Upper bound on a payload, so a corrupt/hostile length can't trigger huge allocations.
pub const MAX_PAYLOAD: u32 = 1 << 20;
/// The control channel (HELLO/WELCOME/HEARTBEAT/SHUTDOWN live here).
pub const CHANNEL_CONTROL: u32 = 0;
/// The clipboard channel (CLIP_OFFER/CLIP_REQUEST/CLIP_DATA live here).
pub const CHANNEL_CLIPBOARD: u32 = 1;

/// Channel for the virtual FIDO authenticator bridge (M14): raw CTAP HID reports
/// between the guest's uhid device (agent side) and the host authenticator.
pub const CHANNEL_FIDO: u32 = 2;
/// The well-known guest vsock port of the control plane (`b"LIMI"` big-endian — vsock
/// ports are a flat u32 space, so a distinctive value just avoids collisions). The
/// supervisor listens here by default; agents connect to `CID_HOST:CONTROL_PORT`.
pub const CONTROL_PORT: u32 = u32::from_be_bytes(MAGIC);

/// Error code: the peer received a message type it does not implement (not fatal).
pub const ERR_UNSUPPORTED: u32 = 1;
/// Error code: the requested content exceeds what a frame can carry (not fatal — the
/// receiver keeps its current clipboard; chunking is the future fix, not a bigger frame).
pub const ERR_TOO_LARGE: u32 = 2;

/// Upper bound on clipboard content in a [`ClipData`] frame: [`MAX_PAYLOAD`] minus
/// headroom for the CBOR envelope (serial + mime string + tags). A sender whose content
/// exceeds this replies [`ERR_TOO_LARGE`] instead of attempting a doomed frame.
pub const MAX_CLIP_DATA: usize = (MAX_PAYLOAD as usize) - 4096;

/// Message type ids (the `msg_type` header byte).
pub mod msg_type {
    pub const HELLO: u8 = 1;
    pub const WELCOME: u8 = 2;
    pub const HEARTBEAT: u8 = 3;
    pub const SHUTDOWN: u8 = 4;
    pub const SHUTDOWN_ACK: u8 = 5;
    pub const ERROR: u8 = 6;
    /// Guest → host memory-pressure report (M6 dynamic memory / PSI autoballoon).
    pub const MEM_PRESSURE: u8 = 7;
    /// Host → guest authoritative wallclock (guest-clock sync across host sleep/restore).
    pub const TIME_SYNC: u8 = 8;
    pub const CLIP_OFFER: u8 = 16;
    pub const CLIP_REQUEST: u8 = 17;
    pub const CLIP_DATA: u8 = 18;

    pub const FIDO_REPORT: u8 = 32;
}

/// Guest → host greeting: who the agent is, what it can do, and cheap boot-time facts.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Hello {
    /// Agent identification, e.g. `limina-init/0.1`.
    #[n(0)]
    pub agent: String,
    /// Capability strings (e.g. `shutdown`, `heartbeat`). Unknown ones are ignored.
    #[n(1)]
    pub caps: Vec<String>,
    /// The guest's page size — the first structured fact the host cares about (16 KiB
    /// guests get the enhanced memory path).
    #[n(2)]
    pub pagesize: u64,
}

/// Host → guest reply to [`Hello`]: the host's capability set.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Welcome {
    #[n(0)]
    pub caps: Vec<String>,
}

/// Guest → host liveness beacon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct Heartbeat {
    /// Monotonic per-connection sequence number.
    #[n(0)]
    pub seq: u64,
}

/// Guest → host memory-pressure report for the M6 PSI autoballoon policy. Sent on an interval over
/// [`CHANNEL_CONTROL`]; the host moves the balloon target between min and max with hysteresis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Encode, Decode)]
pub struct MemPressure {
    /// PSI `some` memory pressure over the 10s/60s windows, in hundredths of a percent (`avg*100`,
    /// so 0..=10000). Both `some_*` and `full_*` are 0 when `/proc/pressure/memory` is unavailable
    /// (kernel `psi=0`); the host then falls back to the `mem_available_kib` watermark.
    #[n(0)]
    pub some_avg10: u32,
    #[n(1)]
    pub some_avg60: u32,
    /// PSI `full` memory pressure (10s/60s windows), in hundredths of a percent.
    #[n(2)]
    pub full_avg10: u32,
    #[n(3)]
    pub full_avg60: u32,
    /// `/proc/meminfo` `MemAvailable` in KiB (the always-available fallback signal).
    #[n(4)]
    pub mem_available_kib: u64,
    /// `/proc/meminfo` `MemTotal` in KiB.
    #[n(5)]
    pub mem_total_kib: u64,
    /// `/proc/meminfo` `MemFree` in KiB. 0 means "not reported" (an agent predating the field, or
    /// an unreadable meminfo) — consumers MUST treat 0 as absent, never as an empty free pool
    /// (a live guest's MemFree is never exactly 0).
    #[n(6)]
    #[cbor(default)]
    pub mem_free_kib: u64,
    /// PSI `full` **IO** pressure (10s/60s windows), hundredths of a percent, from
    /// `/proc/pressure/io`. Cache starvation shows up here rather than in memory PSI (a guest
    /// squeezed of page cache thrashes on re-reads with io-full pegged while memory-PSI stays
    /// modest). 0 when absent.
    #[n(7)]
    #[cbor(default)]
    pub io_full_avg10: u32,
    #[n(8)]
    #[cbor(default)]
    pub io_full_avg60: u32,
}

impl MemPressure {
    /// Parse a snapshot from the raw `/proc/pressure/memory`, `/proc/pressure/io`, and
    /// `/proc/meminfo` contents (the guest agent reads the files; this is the pure, host-testable
    /// parser). PSI fields are 0 when the corresponding `pressure` text is empty (kernel `psi=0`
    /// or unreadable file); MemAvailable/MemTotal/MemFree come from `meminfo`. Tolerant of missing
    /// fields (default 0).
    pub fn from_proc(pressure: &str, io_pressure: &str, meminfo: &str) -> MemPressure {
        // A PSI line: "some avg10=1.23 avg60=4.56 avg300=0.00 total=12345"; value is a percent.
        fn avg(line: &str, key: &str) -> u32 {
            line.split_whitespace()
                .find_map(|tok| tok.strip_prefix(key)?.strip_prefix('='))
                .and_then(|v| v.parse::<f64>().ok())
                .map(|f| (f * 100.0).round() as u32)
                .unwrap_or(0)
        }
        // A meminfo line: "MemTotal:       6029312 kB".
        fn kib(meminfo: &str, key: &str) -> u64 {
            meminfo
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        }
        let some = pressure
            .lines()
            .find(|l| l.starts_with("some"))
            .unwrap_or("");
        let full = pressure
            .lines()
            .find(|l| l.starts_with("full"))
            .unwrap_or("");
        let io_full = io_pressure
            .lines()
            .find(|l| l.starts_with("full"))
            .unwrap_or("");
        MemPressure {
            some_avg10: avg(some, "avg10"),
            some_avg60: avg(some, "avg60"),
            full_avg10: avg(full, "avg10"),
            full_avg60: avg(full, "avg60"),
            mem_available_kib: kib(meminfo, "MemAvailable:"),
            mem_total_kib: kib(meminfo, "MemTotal:"),
            mem_free_kib: kib(meminfo, "MemFree:"),
            io_full_avg10: avg(io_full, "avg10"),
            io_full_avg60: avg(io_full, "avg60"),
        }
    }
}

/// Host → guest: the host's authoritative wallclock. The guest kernel's CLOCK_REALTIME is
/// CNTVCT-anchored and CNTVCT freezes while the host sleeps (mach_absolute_time), so a host
/// nap lags the running guest's clock by the nap's length — and a snapshot restore lags it
/// by the save→restore gap. The supervisor sends this on agent connect (covers boot and the
/// post-restore reconnect), when it detects the host slept (oversleep), and periodically;
/// the agent steps the guest clock when the delta is large and leaves small deltas to the
/// guest's own NTP. Backward steps are allowed (a wrapped/late clock must come back).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct TimeSync {
    /// Host `CLOCK_REALTIME` in nanoseconds since the Unix epoch.
    #[n(0)]
    pub unix_ns: u64,
}

/// Host → guest: please power off cleanly (the orderly window-close path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct Shutdown {
    /// How long the host will wait before forcing teardown; advisory.
    #[n(0)]
    pub grace_ms: u64,
}

/// Either direction: "my side's clipboard changed; I have these formats" (the clipboard
/// conversation is symmetric — a guest copy offers to the host, a host copy offers to the
/// guest). Eager-pull model: the receiver answers with [`ClipRequest`] when it wants the
/// content; only the **newest** serial seen from a peer is honored, so a stale in-flight
/// request can never resurrect an older clipboard.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ClipOffer {
    /// Sender-side monotonic offer number.
    #[n(0)]
    pub serial: u64,
    /// Formats available, e.g. `text/plain;charset=utf-8` (M5: text only).
    #[n(1)]
    pub mime_types: Vec<String>,
}

/// Either direction: "send me the data of your offer `serial` as `mime_type`."
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ClipRequest {
    /// The [`ClipOffer::serial`] this answers.
    #[n(0)]
    pub serial: u64,
    #[n(1)]
    pub mime_type: String,
}

/// Reply to [`ClipRequest`]: the content, inline ([`MAX_PAYLOAD`]-bounded — fine for M5
/// text; large payloads grow chunking later, not a bigger bound).
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ClipData {
    /// The [`ClipRequest::serial`] (== offer serial) this answers.
    #[n(0)]
    pub serial: u64,
    #[n(1)]
    pub mime_type: String,
    #[cbor(n(2), with = "minicbor::bytes")]
    pub data: Vec<u8>,
}

/// Either direction: one CTAP HID report for the virtual FIDO authenticator (M14).
/// Guest→host carries what an application wrote to the uhid device (an OUTPUT
/// report); host→guest carries the authenticator's reply (delivered as an INPUT
/// report). Always a full 64-byte HID frame — CTAPHID framing (channel ids,
/// sequencing, reassembly) lives inside `data`, owned by the host authenticator
/// and the guest's CTAP stack, never by this transport.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct FidoReport {
    #[cbor(n(0), with = "minicbor::bytes")]
    pub data: Vec<u8>,
}

/// Either direction: a non-fatal protocol error report.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ErrorMsg {
    /// One of the `ERR_*` codes.
    #[n(0)]
    pub code: u32,
    /// The `msg_type` byte this error refers to (0 if not applicable).
    #[n(1)]
    pub ref_type: u8,
    /// Human-readable detail (for logs; never parsed).
    #[n(2)]
    pub detail: String,
}

/// A decoded control-plane message. `Unknown` carries anything from a newer peer — the
/// receiver answers with `ERR_UNSUPPORTED` instead of dropping the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Welcome(Welcome),
    Heartbeat(Heartbeat),
    MemPressure(MemPressure),
    TimeSync(TimeSync),
    Shutdown(Shutdown),
    /// Empty payload: acknowledges a [`Shutdown`]; the guest powers off right after.
    ShutdownAck,
    ClipOffer(ClipOffer),
    ClipRequest(ClipRequest),
    ClipData(ClipData),
    FidoReport(FidoReport),
    Error(ErrorMsg),
    Unknown {
        msg_type: u8,
        payload: Vec<u8>,
    },
}

impl Message {
    /// The wire `msg_type` byte for this message.
    pub fn msg_type(&self) -> u8 {
        match self {
            Message::Hello(_) => msg_type::HELLO,
            Message::Welcome(_) => msg_type::WELCOME,
            Message::Heartbeat(_) => msg_type::HEARTBEAT,
            Message::MemPressure(_) => msg_type::MEM_PRESSURE,
            Message::TimeSync(_) => msg_type::TIME_SYNC,
            Message::Shutdown(_) => msg_type::SHUTDOWN,
            Message::ShutdownAck => msg_type::SHUTDOWN_ACK,
            Message::ClipOffer(_) => msg_type::CLIP_OFFER,
            Message::ClipRequest(_) => msg_type::CLIP_REQUEST,
            Message::ClipData(_) => msg_type::CLIP_DATA,
            Message::FidoReport(_) => msg_type::FIDO_REPORT,
            Message::Error(_) => msg_type::ERROR,
            Message::Unknown { msg_type, .. } => *msg_type,
        }
    }

    /// The convenience ERROR reply for an unsupported message type.
    pub fn unsupported(ref_type: u8) -> Message {
        Message::Error(ErrorMsg {
            code: ERR_UNSUPPORTED,
            ref_type,
            detail: format!("unsupported message type {ref_type}"),
        })
    }

    fn encode_payload(&self) -> io::Result<Vec<u8>> {
        fn cbor<T: Encode<()>>(v: &T) -> io::Result<Vec<u8>> {
            minicbor::to_vec(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        match self {
            Message::Hello(m) => cbor(m),
            Message::Welcome(m) => cbor(m),
            Message::Heartbeat(m) => cbor(m),
            Message::MemPressure(m) => cbor(m),
            Message::TimeSync(m) => cbor(m),
            Message::Shutdown(m) => cbor(m),
            Message::ShutdownAck => Ok(Vec::new()),
            Message::ClipOffer(m) => cbor(m),
            Message::ClipRequest(m) => cbor(m),
            Message::ClipData(m) => cbor(m),
            Message::FidoReport(m) => cbor(m),
            Message::Error(m) => cbor(m),
            Message::Unknown { payload, .. } => Ok(payload.clone()),
        }
    }

    fn decode_payload(msg_type_byte: u8, payload: Vec<u8>) -> io::Result<Message> {
        fn cbor<'b, T: Decode<'b, ()>>(bytes: &'b [u8]) -> io::Result<T> {
            minicbor::decode(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        Ok(match msg_type_byte {
            msg_type::HELLO => Message::Hello(cbor(&payload)?),
            msg_type::WELCOME => Message::Welcome(cbor(&payload)?),
            msg_type::HEARTBEAT => Message::Heartbeat(cbor(&payload)?),
            msg_type::MEM_PRESSURE => Message::MemPressure(cbor(&payload)?),
            msg_type::TIME_SYNC => Message::TimeSync(cbor(&payload)?),
            msg_type::SHUTDOWN => Message::Shutdown(cbor(&payload)?),
            msg_type::SHUTDOWN_ACK => Message::ShutdownAck,
            msg_type::CLIP_OFFER => Message::ClipOffer(cbor(&payload)?),
            msg_type::CLIP_REQUEST => Message::ClipRequest(cbor(&payload)?),
            msg_type::CLIP_DATA => Message::ClipData(cbor(&payload)?),
            msg_type::FIDO_REPORT => Message::FidoReport(cbor(&payload)?),
            msg_type::ERROR => Message::Error(cbor(&payload)?),
            other => Message::Unknown {
                msg_type: other,
                payload,
            },
        })
    }
}

/// The fixed 16-byte header of every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    pub msg_type: u8,
    pub flags: u16,
    pub channel: u32,
    pub len: u32,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&MAGIC);
        b[4] = self.version;
        b[5] = self.msg_type;
        b[6..8].copy_from_slice(&self.flags.to_le_bytes());
        b[8..12].copy_from_slice(&self.channel.to_le_bytes());
        b[12..16].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; HEADER_LEN]) -> io::Result<FrameHeader> {
        if b[0..4] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame magic (not a limina control stream?)",
            ));
        }
        let h = FrameHeader {
            version: b[4],
            msg_type: b[5],
            flags: u16::from_le_bytes([b[6], b[7]]),
            channel: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            len: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        };
        if h.len > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame payload {} exceeds MAX_PAYLOAD {MAX_PAYLOAD}", h.len),
            ));
        }
        Ok(h)
    }
}

/// Write one message as a frame. A frame is written with a single `write_all` of the
/// header followed by one of the payload — small messages on a SOCK_STREAM byte stream;
/// the reader reassembles via `read_exact` regardless.
pub fn write_message(w: &mut impl Write, channel: u32, msg: &Message) -> io::Result<()> {
    let payload = msg.encode_payload()?;
    if payload.len() as u64 > MAX_PAYLOAD as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload exceeds MAX_PAYLOAD",
        ));
    }
    let header = FrameHeader {
        version: PROTOCOL_VERSION,
        msg_type: msg.msg_type(),
        flags: 0,
        channel,
        len: payload.len() as u32,
    };
    w.write_all(&header.encode())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Read one message (blocking until a full frame arrives). Unknown message types come
/// back as [`Message::Unknown`] — the caller decides to reply `ERR_UNSUPPORTED`, never
/// treats it as a stream error. Returns `(channel, message)`.
pub fn read_message(r: &mut impl Read) -> io::Result<(u32, Message)> {
    let mut hbuf = [0u8; HEADER_LEN];
    r.read_exact(&mut hbuf)?;
    let header = FrameHeader::decode(&hbuf)?;
    let mut payload = vec![0u8; header.len as usize];
    r.read_exact(&mut payload)?;
    let msg = Message::decode_payload(header.msg_type, payload)?;
    Ok((header.channel, msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn round_trip(msg: Message) -> (u32, Message) {
        let mut buf = Vec::new();
        write_message(&mut buf, CHANNEL_CONTROL, &msg).expect("encode");
        read_message(&mut Cursor::new(buf)).expect("decode")
    }

    #[test]
    fn mem_pressure_round_trips() {
        let mp = MemPressure {
            some_avg10: 123,
            some_avg60: 456,
            full_avg10: 50,
            full_avg60: 0,
            mem_available_kib: 5151232,
            mem_total_kib: 6029312,
            mem_free_kib: 204800,
            io_full_avg10: 4400,
            io_full_avg60: 1200,
        };
        let (_, decoded) = round_trip(Message::MemPressure(mp));
        assert_eq!(decoded, Message::MemPressure(mp));
    }

    #[test]
    fn mem_pressure_parses_psi_and_meminfo() {
        let pressure = "some avg10=1.23 avg60=4.56 avg300=0.00 total=999\n\
                        full avg10=0.50 avg60=0.00 avg300=0.00 total=10\n";
        let io_pressure = "some avg10=9.99 avg60=8.88 avg300=0.00 total=999\n\
                           full avg10=44.00 avg60=12.34 avg300=0.00 total=10\n";
        let meminfo = "MemTotal:       6029312 kB\nMemFree:         123 kB\n\
                       MemAvailable:    5151232 kB\n";
        let mp = MemPressure::from_proc(pressure, io_pressure, meminfo);
        assert_eq!(mp.some_avg10, 123); // 1.23% -> 123 hundredths
        assert_eq!(mp.some_avg60, 456);
        assert_eq!(mp.full_avg10, 50);
        assert_eq!(mp.full_avg60, 0);
        assert_eq!(mp.mem_available_kib, 5151232);
        assert_eq!(mp.mem_total_kib, 6029312);
        assert_eq!(mp.mem_free_kib, 123);
        assert_eq!(mp.io_full_avg10, 4400);
        assert_eq!(mp.io_full_avg60, 1234);
    }

    #[test]
    fn mem_pressure_psi_absent_keeps_meminfo() {
        // psi=0 kernel: /proc/pressure/{memory,io} missing -> empty; meminfo still parsed.
        let mp = MemPressure::from_proc("", "", "MemTotal: 100 kB\nMemAvailable: 80 kB\n");
        assert_eq!(mp.some_avg10, 0);
        assert_eq!(mp.full_avg60, 0);
        assert_eq!(mp.io_full_avg10, 0);
        assert_eq!(mp.mem_available_kib, 80);
        assert_eq!(mp.mem_total_kib, 100);
        assert_eq!(mp.mem_free_kib, 0); // absent from meminfo -> 0 = "not reported"
    }

    /// Old-agent compat: a payload encoded WITHOUT the `#[cbor(default)]` tail fields
    /// (`mem_free_kib`, `io_full_*`) must decode into the grown struct with those fields 0.
    /// `OldMemPressure` is a byte-exact twin of the struct as shipped before the fields grew.
    #[test]
    fn mem_pressure_decodes_old_agent_payload() {
        #[derive(Encode)]
        struct OldMemPressure {
            #[n(0)]
            some_avg10: u32,
            #[n(1)]
            some_avg60: u32,
            #[n(2)]
            full_avg10: u32,
            #[n(3)]
            full_avg60: u32,
            #[n(4)]
            mem_available_kib: u64,
            #[n(5)]
            mem_total_kib: u64,
        }
        let old = OldMemPressure {
            some_avg10: 123,
            some_avg60: 456,
            full_avg10: 50,
            full_avg60: 7,
            mem_available_kib: 5151232,
            mem_total_kib: 6029312,
        };
        let bytes = minicbor::to_vec(&old).expect("encode old");
        let mp: MemPressure = minicbor::decode(&bytes).expect("decode old payload into new struct");
        assert_eq!(mp.some_avg10, 123);
        assert_eq!(mp.mem_total_kib, 6029312);
        assert_eq!(mp.mem_free_kib, 0);
        assert_eq!(mp.io_full_avg10, 0);
        assert_eq!(mp.io_full_avg60, 0);
    }

    /// New-agent → old-supervisor compat: a payload encoded WITH the new tail fields must
    /// still decode into the pre-growth struct shape (the decoder skips unknown indices).
    /// A mid-upgrade dogfood host hits exactly this state.
    #[test]
    fn mem_pressure_new_payload_decodes_into_old_shape() {
        #[derive(Debug, PartialEq, Decode)]
        struct OldMemPressure {
            #[n(0)]
            some_avg10: u32,
            #[n(1)]
            some_avg60: u32,
            #[n(2)]
            full_avg10: u32,
            #[n(3)]
            full_avg60: u32,
            #[n(4)]
            mem_available_kib: u64,
            #[n(5)]
            mem_total_kib: u64,
        }
        let new = MemPressure {
            some_avg10: 123,
            some_avg60: 456,
            full_avg10: 50,
            full_avg60: 7,
            mem_available_kib: 5151232,
            mem_total_kib: 6029312,
            mem_free_kib: 204800,
            io_full_avg10: 4400,
            io_full_avg60: 1200,
        };
        let bytes = minicbor::to_vec(new).expect("encode new");
        let old: OldMemPressure =
            minicbor::decode(&bytes).expect("decode new payload into old shape");
        assert_eq!(old.some_avg10, 123);
        assert_eq!(old.full_avg60, 7);
        assert_eq!(old.mem_available_kib, 5151232);
        assert_eq!(old.mem_total_kib, 6029312);
    }

    #[test]
    fn time_sync_round_trips() {
        let ts = TimeSync {
            unix_ns: 1_789_000_000_123_456_789,
        };
        let (_, decoded) = round_trip(Message::TimeSync(ts));
        assert_eq!(decoded, Message::TimeSync(ts));
    }

    #[test]
    fn header_round_trip() {
        let h = FrameHeader {
            version: PROTOCOL_VERSION,
            msg_type: msg_type::HELLO,
            flags: 0,
            channel: 7,
            len: 42,
        };
        assert_eq!(FrameHeader::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn messages_round_trip() {
        let msgs = [
            Message::Hello(Hello {
                agent: "limina-init/0.1".into(),
                caps: vec!["shutdown".into(), "heartbeat".into()],
                pagesize: 16384,
            }),
            Message::Welcome(Welcome {
                caps: vec!["shutdown".into()],
            }),
            Message::Heartbeat(Heartbeat { seq: 3 }),
            Message::Shutdown(Shutdown { grace_ms: 5000 }),
            Message::ShutdownAck,
            Message::ClipOffer(ClipOffer {
                serial: 7,
                mime_types: vec!["text/plain;charset=utf-8".into()],
            }),
            Message::ClipRequest(ClipRequest {
                serial: 7,
                mime_type: "text/plain;charset=utf-8".into(),
            }),
            Message::ClipData(ClipData {
                serial: 7,
                mime_type: "text/plain;charset=utf-8".into(),
                data: "höst→guest ⌘V".as_bytes().to_vec(),
            }),
            Message::Error(ErrorMsg {
                code: ERR_UNSUPPORTED,
                ref_type: 200,
                detail: "unsupported message type 200".into(),
            }),
        ];
        for msg in msgs {
            let (channel, decoded) = round_trip(msg.clone());
            assert_eq!(channel, CHANNEL_CONTROL);
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn unknown_type_is_not_an_error() {
        let (_, decoded) = round_trip(Message::Unknown {
            msg_type: 250,
            payload: vec![1, 2, 3],
        });
        match decoded {
            Message::Unknown { msg_type, payload } => {
                assert_eq!(msg_type, 250);
                assert_eq!(payload, vec![1, 2, 3]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn channel_is_preserved() {
        let mut buf = Vec::new();
        write_message(&mut buf, 9, &Message::Heartbeat(Heartbeat { seq: 1 })).unwrap();
        let (channel, _) = read_message(&mut Cursor::new(buf)).unwrap();
        assert_eq!(channel, 9);
    }

    #[test]
    fn bad_magic_is_invalid_data() {
        let mut buf = Vec::new();
        write_message(&mut buf, 0, &Message::ShutdownAck).unwrap();
        buf[0] = b'X';
        let err = read_message(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_length_is_rejected_without_allocating() {
        let header = FrameHeader {
            version: PROTOCOL_VERSION,
            msg_type: msg_type::HEARTBEAT,
            flags: 0,
            channel: 0,
            len: MAX_PAYLOAD + 1,
        };
        // Encode bypasses the check (it never produces such a frame); decode must reject.
        let mut bytes = header.encode();
        bytes[12..16].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        let err = read_message(&mut Cursor::new(bytes.to_vec())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_payload_is_unexpected_eof() {
        let mut buf = Vec::new();
        write_message(&mut buf, 0, &Message::Heartbeat(Heartbeat { seq: 1234567 })).unwrap();
        buf.truncate(buf.len() - 1);
        let err = read_message(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn unsupported_reply_references_the_offender() {
        match Message::unsupported(250) {
            Message::Error(e) => {
                assert_eq!(e.code, ERR_UNSUPPORTED);
                assert_eq!(e.ref_type, 250);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
