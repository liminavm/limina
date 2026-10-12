//! What a context's host objects were made from, and the form a context travels in across a
//! snapshot (`docs/design/metal-transport-snapshot.md`).
//!
//! Every host object owns the [`Record`] of its creation: the command as the guest sent it and
//! what it answered. A record holds the records it referenced, so the creates of objects the
//! guest has since destroyed still replay; references are made through those records, never
//! through guest handles, which the guest reuses.

use std::sync::Arc;

/// The command that made a host object, and what it answered.
#[derive(Debug)]
pub struct Record {
    pub op: u16,
    pub payload: Box<[u8]>,
    pub value: [u64; 6],
    /// The records of the objects the command named, in the order it named them.
    pub refs: Vec<Arc<Record>>,
}

/// One object of an export. `refs` index earlier objects of the same export.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectOut {
    /// The guest handle the object answers to, or 0 for one kept only because another object's
    /// create named it.
    pub handle: u32,
    pub op: u16,
    pub payload: Vec<u8>,
    pub value: [u64; 6],
    pub refs: Vec<u32>,
}

/// A context as it travels across a snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextExport {
    /// The ring position the consumer resumes at.
    pub position: u64,
    /// The highest seqno committed.
    pub last_seqno: u64,
    /// The context was lost, or could not be quiesced; it is restored lost.
    pub lost: bool,
    /// Every object, each after the objects it names.
    pub objects: Vec<ObjectOut>,
    /// Argument tables' addresses, by handle.
    pub tables: Vec<(u32, Vec<u64>)>,
    /// Events' values, by handle: what they had been signaled to.
    pub events: Vec<(u32, u64)>,
    /// Shared events' values, by handle.
    pub shared_events: Vec<(u32, u64)>,
    /// The commands of command buffers begun and not yet committed, run again after the rest.
    pub recording: Vec<(u16, Vec<u8>)>,
}

const MAGIC: u32 = 0x4c4d_4a31; // "LMJ1"

struct Writer(Vec<u8>);

impl Writer {
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }
    /// A count of items each at least `min` bytes long, refused if the rest cannot hold them.
    fn count(&mut self, min: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        (n.checked_mul(min)? <= self.0.len()).then_some(n)
    }
}

impl ContextExport {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(MAGIC);
        w.u64(self.position);
        w.u64(self.last_seqno);
        w.u32(self.lost as u32);
        w.u32(self.objects.len() as u32);
        for o in &self.objects {
            w.u32(o.handle);
            w.u16(o.op);
            w.bytes(&o.payload);
            for v in o.value {
                w.u64(v);
            }
            w.u32(o.refs.len() as u32);
            for &r in &o.refs {
                w.u32(r);
            }
        }
        w.u32(self.tables.len() as u32);
        for (h, addrs) in &self.tables {
            w.u32(*h);
            w.u32(addrs.len() as u32);
            for &a in addrs {
                w.u64(a);
            }
        }
        for list in [&self.events, &self.shared_events] {
            w.u32(list.len() as u32);
            for &(h, v) in list {
                w.u32(h);
                w.u64(v);
            }
        }
        w.u32(self.recording.len() as u32);
        for (op, p) in &self.recording {
            w.u16(*op);
            w.bytes(p);
        }
        w.0
    }

    /// `None` for anything malformed, including an object naming one that does not precede it.
    pub fn decode(data: &[u8]) -> Option<Self> {
        let mut r = Reader(data);
        if r.u32()? != MAGIC {
            return None;
        }
        let mut e = Self {
            position: r.u64()?,
            last_seqno: r.u64()?,
            lost: r.u32()? != 0,
            ..Default::default()
        };
        for i in 0..r.count(4 + 2 + 4 + 48 + 4)? {
            let handle = r.u32()?;
            let op = r.u16()?;
            let payload = r.bytes()?;
            let mut value = [0; 6];
            for v in &mut value {
                *v = r.u64()?;
            }
            let mut refs = Vec::new();
            for _ in 0..r.count(4)? {
                let at = r.u32()?;
                if at as usize >= i {
                    return None;
                }
                refs.push(at);
            }
            e.objects.push(ObjectOut {
                handle,
                op,
                payload,
                value,
                refs,
            });
        }
        for _ in 0..r.count(8)? {
            let h = r.u32()?;
            let mut addrs = Vec::new();
            for _ in 0..r.count(8)? {
                addrs.push(r.u64()?);
            }
            e.tables.push((h, addrs));
        }
        for list in [&mut e.events, &mut e.shared_events] {
            for _ in 0..r.count(12)? {
                list.push((r.u32()?, r.u64()?));
            }
        }
        for _ in 0..r.count(6)? {
            let op = r.u16()?;
            e.recording.push((op, r.bytes()?));
        }
        r.0.is_empty().then_some(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ContextExport {
        ContextExport {
            position: 4096,
            last_seqno: 7,
            lost: false,
            objects: vec![
                ObjectOut {
                    handle: 0,
                    op: 0x103,
                    payload: b"library".to_vec(),
                    value: [0; 6],
                    refs: vec![],
                },
                ObjectOut {
                    handle: 9,
                    op: 0x105,
                    payload: vec![1, 2, 3],
                    value: [64, 0, 0, 0, 0, 0],
                    refs: vec![0],
                },
            ],
            tables: vec![(4, vec![0x1500000000, 0])],
            events: vec![(5, 3)],
            shared_events: vec![(6, 11)],
            recording: vec![(0x200, vec![]), (0x201, vec![9; 16])],
        }
    }

    #[test]
    fn an_export_survives_its_encoding() {
        let e = sample();
        assert_eq!(ContextExport::decode(&e.encode()), Some(e));
    }

    #[test]
    fn a_reference_to_a_later_object_is_refused() {
        let mut e = sample();
        e.objects[1].refs = vec![1];
        assert_eq!(ContextExport::decode(&e.encode()), None);
    }

    #[test]
    fn truncated_or_padded_bytes_are_refused() {
        let bytes = sample().encode();
        for n in 0..bytes.len() {
            assert_eq!(ContextExport::decode(&bytes[..n]), None, "{n}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(ContextExport::decode(&long), None);
    }
}
