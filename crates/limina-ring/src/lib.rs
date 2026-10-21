//! A single-producer, single-consumer ring of variable-length records in shared memory.
//!
//! A ring region is a control block followed by a power-of-two data area. Positions are monotonic
//! byte counts; `pos & (size - 1)` is the offset in the data area. Every record starts with a
//! [`RecordHeader`], is padded to [`RECORD_ALIGN`], and never straddles the end of the data area: a
//! producer that would wrap writes a [`TAG_PAD`] record over the remainder first.
//!
//! One side [`format`]s the region; the producer and the consumer then attach to it, in either
//! order and possibly from different processes. The producer attaches to an untouched ring and may
//! publish before the consumer arrives, which then starts at the first record. Each checks the
//! protocol number the region was formatted with, which is the user's: this crate carries no
//! protocol of its own.
//!
//! The consumer trusts nothing in shared memory. It keeps its own `head`, takes the data size from
//! its own attach call rather than the control block, and reads each record's header once into
//! private memory before checking it, so a producer rewriting the ring underneath cannot change
//! what was checked. A lying producer can corrupt the payloads it sends, which is its right, and
//! nothing else. A lying consumer can only stall the producer.
//!
//! Waking is the user's (a semaphore, a pipe, a doorbell register), and is needed only when the
//! consumer sleeps: it parks only after publishing [`STATE_PARKED`] and re-checking `tail`, and the
//! producer reads the state only after publishing `tail`. Both sides put a sequentially consistent
//! fence between their store and their load, so at least one of them sees the other's store: a
//! parked consumer is always kicked, and a running one never needs to be. The Metal transport's
//! command ring (`limina-gpu-wire`) works the same way.
#![no_std]

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// `magic` of a formatted control block.
pub const MAGIC: u32 = u32::from_le_bytes(*b"LMRR");
/// Byte offset of the data area from the start of the ring region.
pub const DATA_OFFSET: usize = 512;
/// Alignment and size granule of every record.
pub const RECORD_ALIGN: usize = 8;
/// Smallest data area accepted.
pub const MIN_DATA_SIZE: u32 = 4096;
/// The tag of the filler a producer writes before wrapping. Never handed to the consumer's user.
pub const TAG_PAD: u32 = 0;

/// The consumer is draining or about to.
pub const STATE_RUNNING: u32 = 0;
/// The consumer is asleep and needs a doorbell to see new records.
pub const STATE_PARKED: u32 = 1;

/// The ring's control block. Each 128-byte line has a single writer (Apple Silicon's cache line).
#[repr(C, align(128))]
pub struct Control {
    /// [`MAGIC`], written by [`format`].
    pub magic: u32,
    /// The user's protocol number, written by [`format`] and checked by both ends.
    pub protocol: u32,
    /// The data area's size, written by [`format`]. Informational: each end uses its own.
    pub data_size: u32,
    _r0: [u8; 116],
    /// Bytes produced. Written by the producer.
    pub tail: AtomicU64,
    _r1: [u8; 120],
    /// Bytes consumed. Written by the consumer.
    pub head: AtomicU64,
    /// [`STATE_RUNNING`] or [`STATE_PARKED`]. Written by the consumer.
    pub state: AtomicU32,
    _r2: [u8; 116],
}

const _: () = assert!(size_of::<Control>() == 384);
const _: () = assert!(size_of::<Control>() <= DATA_OFFSET);

/// What precedes every record in the data area.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordHeader {
    /// Payload bytes, padding excluded.
    pub len: u32,
    /// The user's record type; [`TAG_PAD`] is the ring's own.
    pub tag: u32,
}

/// Bytes of header in front of every record's payload.
pub const HEADER_LEN: usize = size_of::<RecordHeader>();
const _: () = assert!(HEADER_LEN == RECORD_ALIGN);

/// Why a ring could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The data size is not a power of two of at least [`MIN_DATA_SIZE`].
    BadSize,
    /// The control block is not a formatted ring of this protocol, or its positions are
    /// impossible.
    Corrupt,
    /// A record header points outside what was published.
    Malformed,
}

/// Why a reservation did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Not enough free space right now; publish, kick if needed, wait for `head`, retry.
    Full,
    /// Larger than [`Producer::max_record`], so it could never fit with a wrap.
    TooLarge,
}

/// Total bytes a ring with `data_size` bytes of data occupies.
pub const fn region_size(data_size: u32) -> usize {
    DATA_OFFSET + data_size as usize
}

/// Whether `data_size` is acceptable to both ends.
pub const fn valid_data_size(data_size: u32) -> bool {
    data_size >= MIN_DATA_SIZE && data_size.is_power_of_two()
}

/// The space a record with `len` payload bytes takes in the data area.
pub const fn record_space(len: usize) -> usize {
    (HEADER_LEN + len + RECORD_ALIGN - 1) & !(RECORD_ALIGN - 1)
}

/// Writes an empty ring of `data_size` bytes, tagged with `protocol`, at `region`.
///
/// # Safety
/// `region` must be valid for [`region_size`]`(data_size)` bytes and aligned to 128, and neither
/// end may be attached to it yet.
pub unsafe fn format(region: NonNull<u8>, data_size: u32, protocol: u32) -> Result<(), Error> {
    if !valid_data_size(data_size) {
        return Err(Error::BadSize);
    }
    let ctrl = region.cast::<Control>().as_ptr();
    // SAFETY: the caller guarantees the region; zero is every field's initial value.
    unsafe {
        core::ptr::write_bytes(region.as_ptr(), 0, DATA_OFFSET);
        (*ctrl).magic = MAGIC;
        (*ctrl).protocol = protocol;
        (*ctrl).data_size = data_size;
    }
    fence(Ordering::Release);
    Ok(())
}

/// Checks the control block at `region` and returns it, with what its producer has published.
/// Plain reads of words the other side may have written; a torn value only fails the check.
///
/// # Safety
/// As for [`format`], minus the attachment rule.
unsafe fn checked(
    region: NonNull<u8>,
    data_size: u32,
    protocol: u32,
) -> Result<(NonNull<Control>, u64), Error> {
    if !valid_data_size(data_size) {
        return Err(Error::BadSize);
    }
    let ctrl = region.cast::<Control>();
    // SAFETY: by the caller's contract.
    let c = unsafe { ctrl.as_ref() };
    // SAFETY: in bounds and aligned.
    let (magic, proto) = unsafe {
        (
            core::ptr::read_volatile(&c.magic),
            core::ptr::read_volatile(&c.protocol),
        )
    };
    if magic != MAGIC || proto != protocol {
        return Err(Error::Corrupt);
    }
    Ok((ctrl, c.tail.load(Ordering::Acquire)))
}

/// The writing end.
pub struct Producer {
    ctrl: NonNull<Control>,
    data: NonNull<u8>,
    size: u64,
    tail: u64,
    head_seen: u64,
}

// SAFETY: the producer owns its end exclusively; the pointers are into shared memory that outlives
// it by the contract of `attach`.
unsafe impl Send for Producer {}

impl Producer {
    /// Attaches the producer to a ring [`format`]ted with `protocol`.
    ///
    /// # Safety
    /// `region` must be valid for [`region_size`]`(data_size)` bytes, aligned to 128, for as long
    /// as the producer lives, and no one else may produce into it.
    pub unsafe fn attach(
        region: NonNull<u8>,
        data_size: u32,
        protocol: u32,
    ) -> Result<Self, Error> {
        // SAFETY: by the caller's contract.
        let (ctrl, tail) = unsafe { checked(region, data_size, protocol) }?;
        if tail != 0 {
            // Another producer has been here.
            return Err(Error::Corrupt);
        }
        Ok(Self {
            ctrl,
            // SAFETY: DATA_OFFSET is inside the region.
            data: unsafe { region.add(DATA_OFFSET) },
            size: data_size as u64,
            tail: 0,
            head_seen: 0,
        })
    }

    pub fn control(&self) -> &Control {
        // SAFETY: valid for our lifetime by the contract of `attach`.
        unsafe { self.ctrl.as_ref() }
    }

    /// Bytes pushed so far, published or not.
    pub fn position(&self) -> u64 {
        self.tail
    }

    /// The largest payload a record can carry: what fits in half the data area.
    pub fn max_record(&self) -> usize {
        self.size as usize / 2 - HEADER_LEN
    }

    /// Whether `space` more bytes fit, refreshing the consumer's `head` when they seem not to. A
    /// `head` that is ahead of `tail` or more than a ring behind is a lie, and reads as full.
    fn fits(&mut self, space: u64) -> bool {
        let fits = |head: u64| {
            let used = self.tail.wrapping_sub(head);
            used <= self.size && space <= self.size - used
        };
        if fits(self.head_seen) {
            return true;
        }
        self.head_seen = self.control().head.load(Ordering::Acquire);
        fits(self.head_seen)
    }

    /// Reserves room for a record of up to `len` payload bytes. Nothing is visible to the consumer
    /// until the reservation is [committed](Reservation::commit) and [published](Self::publish);
    /// a reservation dropped uncommitted costs nothing but, at most, a wrap.
    pub fn reserve(&mut self, len: usize) -> Result<Reservation<'_>, PushError> {
        if len > self.max_record() {
            return Err(PushError::TooLarge);
        }
        let space = record_space(len) as u64;
        let pos = self.tail & (self.size - 1);
        let to_end = self.size - pos;
        let need = if space > to_end {
            to_end + space
        } else {
            space
        };
        if !self.fits(need) {
            return Err(PushError::Full);
        }
        if space > to_end {
            let pad = RecordHeader {
                len: (to_end as usize - HEADER_LEN) as u32,
                tag: TAG_PAD,
            };
            // SAFETY: `pos + HEADER_LEN <= size` because records are aligned and `to_end > 0`.
            unsafe {
                self.data
                    .add(pos as usize)
                    .cast::<RecordHeader>()
                    .write_volatile(pad)
            };
            self.tail += to_end;
        }
        // SAFETY: `tail & mask` is inside the data area and `space` fits before its end.
        let at = unsafe { self.data.add((self.tail & (self.size - 1)) as usize) };
        Ok(Reservation {
            producer: self,
            at,
            cap: len,
        })
    }

    /// Appends a record of `tag` whose payload is the concatenation of `parts`.
    pub fn push(&mut self, tag: u32, parts: &[&[u8]]) -> Result<(), PushError> {
        let len = parts.iter().map(|p| p.len()).sum();
        let mut r = self.reserve(len)?;
        let mut off = 0;
        for p in parts {
            r.write_at(off, p);
            off += p.len();
        }
        r.commit(tag, len);
        Ok(())
    }

    /// Makes every committed record visible. Returns whether the consumer is parked and so needs a
    /// doorbell.
    pub fn publish(&mut self) -> bool {
        let c = self.control();
        c.tail.store(self.tail, Ordering::Release);
        fence(Ordering::SeqCst);
        c.state.load(Ordering::Relaxed) == STATE_PARKED
    }
}

/// Room for one record, held until it is committed or dropped.
pub struct Reservation<'a> {
    producer: &'a mut Producer,
    /// Where the record's header goes.
    at: NonNull<u8>,
    cap: usize,
}

impl Reservation<'_> {
    /// Payload bytes reserved.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Copies `bytes` into the payload at `offset`.
    ///
    /// # Panics
    /// If the bytes run past the reservation.
    pub fn write_at(&mut self, offset: usize, bytes: &[u8]) {
        assert!(offset <= self.cap && bytes.len() <= self.cap - offset);
        // SAFETY: inside the reserved payload, which the consumer does not read until committed.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.at.add(HEADER_LEN + offset).as_ptr(),
                bytes.len(),
            )
        };
    }

    /// The payload, for a fill that cannot go through [`Self::write_at`] (a read from a socket or
    /// from guest memory straight into the ring).
    ///
    /// # Safety
    /// The consumer must not touch the ring past the published `tail`, which a well-behaved one
    /// never does; a hostile one that does races with the fill.
    pub unsafe fn payload_mut(&mut self) -> &mut [u8] {
        // SAFETY: `cap` bytes past the header are reserved; the race is the caller's to accept.
        unsafe { core::slice::from_raw_parts_mut(self.at.add(HEADER_LEN).as_ptr(), self.cap) }
    }

    /// Ends the record at `len` payload bytes, with type `tag`.
    ///
    /// # Panics
    /// If `len` exceeds the reservation or `tag` is [`TAG_PAD`].
    pub fn commit(self, tag: u32, len: usize) {
        assert!(len <= self.cap, "a record cannot outgrow its reservation");
        assert_ne!(tag, TAG_PAD, "the pad tag is the ring's own");
        let header = RecordHeader {
            len: len as u32,
            tag,
        };
        // SAFETY: the header slot is reserved.
        unsafe { self.at.cast::<RecordHeader>().write_volatile(header) };
        self.producer.tail += record_space(len) as u64;
    }
}

/// The reading end.
pub struct Consumer {
    ctrl: NonNull<Control>,
    data: NonNull<u8>,
    size: u64,
    head: u64,
}

// SAFETY: as for `Producer`.
unsafe impl Send for Consumer {}

impl Consumer {
    /// Attaches the consumer to a ring [`format`]ted with `protocol`.
    ///
    /// # Safety
    /// `region` must be valid for [`region_size`]`(data_size)` bytes, aligned to 128, for as long
    /// as the consumer lives, and no one else may consume from it.
    pub unsafe fn attach(
        region: NonNull<u8>,
        data_size: u32,
        protocol: u32,
    ) -> Result<Self, Error> {
        // SAFETY: by the caller's contract.
        let (ctrl, tail) = unsafe { checked(region, data_size, protocol) }?;
        // Nothing has been consumed, so the producer cannot have published more than a ring.
        if tail > data_size as u64 || !tail.is_multiple_of(RECORD_ALIGN as u64) {
            return Err(Error::Corrupt);
        }
        // SAFETY: valid, by the same contract.
        let c = unsafe { ctrl.as_ref() };
        c.head.store(0, Ordering::Relaxed);
        c.state.store(STATE_RUNNING, Ordering::Release);
        Ok(Self {
            ctrl,
            // SAFETY: inside the region.
            data: unsafe { region.add(DATA_OFFSET) },
            size: data_size as u64,
            head: 0,
        })
    }

    pub fn control(&self) -> &Control {
        // SAFETY: valid for our lifetime by the contract of `attach`.
        unsafe { self.ctrl.as_ref() }
    }

    /// Bytes consumed.
    pub fn position(&self) -> u64 {
        self.head
    }

    /// The next record, in place. It is consumed, and its space freed, when the [`Record`] drops.
    /// `Ok(None)` when the ring is empty.
    pub fn read(&mut self) -> Result<Option<Record<'_>>, Error> {
        loop {
            let tail = self.control().tail.load(Ordering::Acquire);
            if tail == self.head {
                return Ok(None);
            }
            let avail = tail.wrapping_sub(self.head);
            if avail > self.size || !avail.is_multiple_of(RECORD_ALIGN as u64) {
                return Err(Error::Corrupt);
            }
            let at = self.head;
            let pos = (self.head & (self.size - 1)) as usize;
            // SAFETY: `pos + HEADER_LEN <= size`: positions are aligned inside the data area. One
            // read, so what is checked is what is used.
            let header = unsafe { self.data.add(pos).cast::<RecordHeader>().read_volatile() };
            let space = record_space(header.len as usize);
            if space as u64 > avail || pos + space > self.size as usize {
                return Err(Error::Malformed);
            }
            if header.tag == TAG_PAD {
                self.head += space as u64;
                self.release();
                continue;
            }
            return Ok(Some(Record {
                // SAFETY: inside the data area, checked above.
                payload: unsafe { self.data.add(pos + HEADER_LEN) },
                consumer: self,
                at,
                header,
                space: space as u64,
            }));
        }
    }

    /// Copies the next record's payload into `buf` and consumes it, returning its tag and length.
    /// `Ok(None)` when the ring is empty; [`Error::Malformed`] if the record is longer than `buf`.
    pub fn read_into(&mut self, buf: &mut [u8]) -> Result<Option<(u32, usize)>, Error> {
        let Some(r) = self.read()? else {
            return Ok(None);
        };
        let len = r.len();
        if len > buf.len() {
            return Err(Error::Malformed);
        }
        r.copy_to(0, &mut buf[..len]);
        Ok(Some((r.tag(), len)))
    }

    /// Publishes `head`, freeing the space of everything consumed.
    fn release(&self) {
        self.control().head.store(self.head, Ordering::Release);
    }

    /// Announces that the consumer is about to sleep. Returns `false` if records arrived in the
    /// meantime, in which case the consumer must not sleep (and is marked running again).
    pub fn park(&self) -> bool {
        let c = self.control();
        c.state.store(STATE_PARKED, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        if c.tail.load(Ordering::Relaxed) != self.head {
            c.state.store(STATE_RUNNING, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Marks the consumer running after a wake.
    pub fn unpark(&self) {
        self.control().state.store(STATE_RUNNING, Ordering::Relaxed);
    }

    /// Whether records are waiting.
    pub fn has_work(&self) -> bool {
        self.control().tail.load(Ordering::Acquire) != self.head
    }
}

/// One record, still in the ring. Its payload is read by copying, never by reference, because the
/// producer may be rewriting it; its header was read once and checked.
pub struct Record<'a> {
    consumer: &'a mut Consumer,
    payload: NonNull<u8>,
    at: u64,
    header: RecordHeader,
    space: u64,
}

impl Record<'_> {
    pub fn tag(&self) -> u32 {
        self.header.tag
    }

    /// Payload bytes.
    pub fn len(&self) -> usize {
        self.header.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.header.len == 0
    }

    /// Ring position of the record's header, for error reports.
    pub fn at(&self) -> u64 {
        self.at
    }

    /// Copies payload bytes from `offset` into `dst`.
    ///
    /// # Panics
    /// If the bytes run past the payload.
    pub fn copy_to(&self, offset: usize, dst: &mut [u8]) {
        assert!(offset <= self.len() && dst.len() <= self.len() - offset);
        // SAFETY: inside the checked payload.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.payload.add(offset).as_ptr(),
                dst.as_mut_ptr(),
                dst.len(),
            )
        };
    }

    /// The payload's address, for a copy that cannot go through [`Self::copy_to`] (straight into
    /// guest memory). Valid for [`Self::len`] bytes until the record drops; the producer may
    /// rewrite them meanwhile, so read them with raw copies only.
    pub fn as_ptr(&self) -> *const u8 {
        self.payload.as_ptr()
    }
}

impl Drop for Record<'_> {
    fn drop(&mut self) {
        self.consumer.head += self.space;
        self.consumer.release();
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;
    use std::vec;
    use std::vec::Vec;

    const PROTO: u32 = 0x6e65_7464;
    const SIZE: u32 = 4096;

    #[repr(C, align(128))]
    struct Region([u8; DATA_OFFSET + SIZE as usize]);

    /// The region leaks; tests are short-lived.
    fn region() -> NonNull<u8> {
        let r = Box::leak(Box::new(Region([0; DATA_OFFSET + SIZE as usize])));
        NonNull::new(r.0.as_mut_ptr()).unwrap()
    }

    fn ring() -> (NonNull<u8>, Producer, Consumer) {
        let r = region();
        unsafe { format(r, SIZE, PROTO) }.unwrap();
        let p = unsafe { Producer::attach(r, SIZE, PROTO) }.unwrap();
        let c = unsafe { Consumer::attach(r, SIZE, PROTO) }.unwrap();
        (r, p, c)
    }

    fn payload(i: u32) -> Vec<u8> {
        let len = (i as usize * 37) % 700;
        (0..len).map(|b| (b as u32 ^ i) as u8).collect()
    }

    #[test]
    fn round_trips_records_of_every_length_across_wraps() {
        let (_r, mut p, mut c) = ring();
        let mut buf = vec![0u8; 2048];
        for i in 1..5000u32 {
            let want = payload(i);
            let (a, b) = want.split_at(want.len() / 3);
            p.push(i, &[a, b]).unwrap();
            p.publish();
            assert_eq!(c.read_into(&mut buf).unwrap(), Some((i, want.len())));
            assert_eq!(&buf[..want.len()], &want[..]);
            assert_eq!(c.read_into(&mut buf).unwrap(), None);
        }
    }

    #[test]
    fn a_record_is_consumed_only_when_it_drops() {
        let (_r, mut p, mut c) = ring();
        p.push(7, &[&[1, 2, 3]]).unwrap();
        p.publish();
        {
            let r = c.read().unwrap().unwrap();
            assert_eq!((r.tag(), r.len()), (7, 3));
            assert_eq!(p.control().head.load(Ordering::Acquire), 0);
            let mut out = [0u8; 2];
            r.copy_to(1, &mut out);
            assert_eq!(out, [2, 3]);
        }
        assert_eq!(c.control().head.load(Ordering::Acquire), 16);
    }

    #[test]
    fn a_reservation_commits_shorter_than_it_reserved() {
        let (_r, mut p, mut c) = ring();
        let mut r = p.reserve(1500).unwrap();
        r.write_at(0, b"frame");
        r.commit(3, 5);
        p.publish();
        let mut buf = [0u8; 16];
        assert_eq!(c.read_into(&mut buf).unwrap(), Some((3, 5)));
        assert_eq!(&buf[..5], b"frame");
        assert_eq!(c.position(), record_space(5) as u64);
    }

    #[test]
    fn a_full_ring_refuses_until_the_consumer_frees_space() {
        let (_r, mut p, mut c) = ring();
        let mut n = 0;
        while p.push(1, &[&[0u8; 500]]).is_ok() {
            n += 1;
        }
        assert_eq!(n, SIZE as usize / record_space(500));
        assert_eq!(p.push(1, &[&[0u8; 500]]), Err(PushError::Full));
        p.publish();
        drop(c.read().unwrap());
        assert_eq!(p.push(1, &[&[0u8; 500]]), Ok(()));
        assert_eq!(
            p.reserve(p.max_record() + 1).err(),
            Some(PushError::TooLarge)
        );
    }

    #[test]
    fn parking_is_seen_by_the_producer_and_refused_with_work_pending() {
        let (_r, mut p, c) = ring();
        assert!(c.park());
        p.push(1, &[]).unwrap();
        assert!(p.publish(), "a parked consumer must be kicked");
        c.unpark();
        assert!(!c.park(), "work is pending");
        assert!(!p.publish());
    }

    #[test]
    fn attach_checks_magic_protocol_and_size() {
        let r = region();
        assert_eq!(
            unsafe { Consumer::attach(r, SIZE, PROTO) }.err(),
            Some(Error::Corrupt)
        );
        unsafe { format(r, SIZE, PROTO) }.unwrap();
        assert_eq!(
            unsafe { Consumer::attach(r, SIZE, PROTO + 1) }.err(),
            Some(Error::Corrupt)
        );
        assert_eq!(
            unsafe { Producer::attach(r, 3000, PROTO) }.err(),
            Some(Error::BadSize)
        );
        assert_eq!(unsafe { format(r, 2048, PROTO) }, Err(Error::BadSize));
    }

    #[test]
    fn a_consumer_attaching_late_finds_what_was_published() {
        let r = region();
        unsafe { format(r, SIZE, PROTO) }.unwrap();
        let mut p = unsafe { Producer::attach(r, SIZE, PROTO) }.unwrap();
        p.push(4, &[b"dhcp"]).unwrap();
        p.publish();
        assert_eq!(
            unsafe { Producer::attach(r, SIZE, PROTO) }.err(),
            Some(Error::Corrupt),
            "a second producer"
        );
        let mut c = unsafe { Consumer::attach(r, SIZE, PROTO) }.unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(c.read_into(&mut buf).unwrap(), Some((4, 4)));
        assert_eq!(&buf[..4], b"dhcp");
    }

    #[test]
    fn a_lying_producer_is_caught() {
        let (r, mut p, mut c) = ring();
        p.push(1, &[&[0u8; 8]]).unwrap();
        p.publish();
        // A length running past the published tail.
        unsafe { r.add(DATA_OFFSET).cast::<u32>().write(64) };
        assert_eq!(c.read().err(), Some(Error::Malformed));
        // A tail more than a ring ahead, and one that is not aligned.
        let ctl = unsafe { r.cast::<Control>().as_ref() };
        ctl.tail.store(SIZE as u64 * 2, Ordering::Release);
        assert_eq!(c.read().err(), Some(Error::Corrupt));
        ctl.tail.store(3, Ordering::Release);
        assert_eq!(c.read().err(), Some(Error::Corrupt));
    }

    #[test]
    fn a_lying_consumer_only_stalls_the_producer() {
        let (r, mut p, _c) = ring();
        let ctl = unsafe { r.cast::<Control>().as_ref() };
        p.push(1, &[&[0u8; 8]]).unwrap();
        // A head past the tail, and one more than a ring behind it.
        ctl.head.store(1 << 40, Ordering::Release);
        while p.push(1, &[&[0u8; 500]]).is_ok() {}
        assert_eq!(p.push(1, &[]), Err(PushError::Full));
        assert!(p.position() <= SIZE as u64);
    }

    /// Producer and consumer on two threads, the consumer parking on a condvar doorbell: every
    /// record arrives intact and in order, and no wakeup is lost (a lost one hangs, and the wait
    /// fails after its timeout).
    #[test]
    fn two_threads_with_a_doorbell_lose_nothing() {
        struct Bell(Mutex<bool>, Condvar);
        let bell = Arc::new(Bell(Mutex::new(false), Condvar::new()));
        let (r, mut p, mut c) = ring();
        let _ = r;
        const N: u32 = 200_000;
        let b = bell.clone();
        let consumer = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let mut want = 1u32;
            while want < N {
                while let Some((tag, len)) = c.read_into(&mut buf).unwrap() {
                    assert_eq!(tag, want);
                    assert_eq!(&buf[..len], &payload(want)[..]);
                    want += 1;
                }
                if want == N {
                    break;
                }
                if c.park() {
                    let mut rung = b.0.lock().unwrap();
                    while !*rung {
                        let (g, t) = b.1.wait_timeout(rung, Duration::from_secs(10)).unwrap();
                        assert!(!t.timed_out(), "lost wakeup at record {want}");
                        rung = g;
                    }
                    *rung = false;
                    drop(rung);
                    c.unpark();
                }
            }
        });
        for i in 1..N {
            let want = payload(i);
            while p.push(i, &[&want]).is_err() {
                if p.publish() {
                    *bell.0.lock().unwrap() = true;
                    bell.1.notify_one();
                }
                std::thread::yield_now();
            }
            if i % 7 == 0 && p.publish() {
                *bell.0.lock().unwrap() = true;
                bell.1.notify_one();
            }
        }
        if p.publish() {
            *bell.0.lock().unwrap() = true;
            bell.1.notify_one();
        }
        consumer.join().unwrap();
    }
}
