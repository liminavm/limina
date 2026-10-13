//! The command ring: a control block followed by a power-of-two data area, in arena memory.
//!
//! Positions are monotonic byte counts; `pos & (size - 1)` is the offset in the data area. Every
//! command starts with a [`CmdHeader`](crate::cmd::CmdHeader), is padded to [`CMD_ALIGN`], and
//! never straddles the end of the data area: a producer that would wrap writes a
//! [`PAD`](crate::cmd::op::PAD) command over the remainder first.
//!
//! The consumer trusts nothing in shared memory. It keeps its own `head`, takes the data size from
//! its own attach record rather than the control block, and copies each command into private memory
//! before validating it, so a guest rewriting the ring underneath cannot change what was checked.
//!
//! Waking: the consumer parks only after publishing [`STATE_PARKED`] and re-checking `tail`, and
//! the producer reads the state only after publishing `tail`. Both sides put a sequentially
//! consistent fence between their store and their load, so at least one of them sees the other's
//! store: a parked consumer is always kicked, and a running one never needs to be.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use crate::cmd::{self, CmdHeader, Command};

/// `magic` of an initialised control block.
pub const MAGIC: u32 = u32::from_le_bytes(*b"LMGR");
/// Byte offset of the data area from the start of the ring region.
pub const DATA_OFFSET: usize = 512;
/// Alignment and size granule of every command.
pub const CMD_ALIGN: usize = 8;
/// Smallest data area accepted.
pub const MIN_DATA_SIZE: u32 = 4096;

/// The consumer is draining or about to.
pub const STATE_RUNNING: u32 = 0;
/// The consumer is asleep and needs a doorbell to see new commands.
pub const STATE_PARKED: u32 = 1;

/// The ring's control block. Each 128-byte line has a single writer (Apple Silicon's cache line).
#[repr(C, align(128))]
pub struct RingControl {
    /// [`MAGIC`], written by the producer at init.
    pub magic: u32,
    /// [`VERSION`](crate::VERSION), written by the producer at init.
    pub version: u32,
    _r0: [u8; 120],
    /// Bytes produced. Written by the guest.
    pub tail: AtomicU64,
    _r1: [u8; 120],
    /// Bytes consumed. Written by the host.
    pub head: AtomicU64,
    /// Highest seqno whose commit has finished on the GPU. Written by the host.
    pub completed: AtomicU64,
    /// [`STATE_RUNNING`] or [`STATE_PARKED`]. Written by the host.
    pub state: AtomicU32,
    /// Zero, or the first [`Error`] the host hit; the context is lost once it is set.
    pub error: AtomicU32,
    /// Ring position of the command that set `error`.
    pub error_at: AtomicU64,
    _r2: [u8; 96],
}

const _: () = assert!(size_of::<RingControl>() == 384);
const _: () = assert!(size_of::<RingControl>() <= DATA_OFFSET);

impl RingControl {
    /// Records a finished seqno. Seqnos retire in order on one ring, but completion handlers may
    /// run on any thread, so this keeps the maximum.
    pub fn complete(&self, seqno: u64) {
        self.completed.fetch_max(seqno, Ordering::Release);
    }

    /// Records the first error; later ones are dropped, since the first is the cause.
    pub fn fail(&self, error: Error, at: u64) {
        if self
            .error
            .compare_exchange(0, error as u32, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            self.error_at.store(at, Ordering::Release);
        }
    }
}

/// Why a context was lost. Stored in [`RingControl::error`].
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// `tail` moved backwards or past the data area.
    Corrupt = 1,
    /// A header with a bad length, or a command that straddles the end.
    Malformed = 2,
    /// An opcode the host does not know.
    UnknownOp = 3,
    /// A payload too short for its command, or trailing bytes that do not add up.
    BadPayload = 4,
    /// A handle that names no object, or the wrong kind of object.
    BadHandle = 5,
    /// An arena id the context does not own, or a range outside the arena.
    BadRange = 6,
    /// An enum value or size Metal would reject.
    BadValue = 7,
    /// The MSL source did not compile.
    ShaderCompile = 8,
    /// Metal refused to create a pipeline.
    PipelineCreate = 9,
    /// A command arrived in the wrong encoder state.
    BadState = 10,
    /// The GPU reported a command-buffer error.
    GpuFault = 11,
    /// The host could not allocate what a command needed.
    OutOfMemory = 12,
    /// A snapshot could not carry the context: its work did not finish before the snapshot, or
    /// the restore could not rebuild it as the guest knew it.
    Restore = 13,
    /// The process serving the context exited: nothing it held survives, and nothing it owed will
    /// arrive. Written by the worker, not the GPU host.
    HostGone = 14,
}

impl Error {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => Self::Corrupt,
            2 => Self::Malformed,
            3 => Self::UnknownOp,
            4 => Self::BadPayload,
            5 => Self::BadHandle,
            6 => Self::BadRange,
            7 => Self::BadValue,
            8 => Self::ShaderCompile,
            9 => Self::PipelineCreate,
            10 => Self::BadState,
            11 => Self::GpuFault,
            12 => Self::OutOfMemory,
            13 => Self::Restore,
            14 => Self::HostGone,
            _ => return None,
        })
    }
}

/// Total bytes a ring with `data_size` bytes of data occupies.
pub const fn region_size(data_size: u32) -> usize {
    DATA_OFFSET + data_size as usize
}

/// Whether `data_size` is acceptable to both ends.
pub const fn valid_data_size(data_size: u32) -> bool {
    data_size >= MIN_DATA_SIZE && data_size.is_power_of_two()
}

const fn align_cmd(len: usize) -> usize {
    (len + CMD_ALIGN - 1) & !(CMD_ALIGN - 1)
}

/// The encoded length of `T` followed by `extra` bytes, header and padding included.
pub const fn encoded_len<T: Command>(extra: usize) -> usize {
    align_cmd(size_of::<CmdHeader>() + size_of::<T>() + extra)
}

/// Why a push did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Not enough free space right now; publish, kick if needed, wait for `head`, retry.
    Full,
    /// Larger than half the data area, so it could never fit with a wrap.
    TooLarge,
}

/// The guest end.
pub struct Producer {
    ctrl: NonNull<RingControl>,
    data: NonNull<u8>,
    size: u64,
    tail: u64,
    head_seen: u64,
}

// SAFETY: the producer owns its ring exclusively; the pointers are into shared memory that
// outlives it by the contract of `init`.
unsafe impl Send for Producer {}

impl Producer {
    /// Initialises the control block at `region` and returns its producer.
    ///
    /// # Safety
    /// `region` must be valid for [`region_size`]`(data_size)` bytes, aligned to 128, for as long
    /// as the producer lives, and no one else may produce into it.
    pub unsafe fn init(region: NonNull<u8>, data_size: u32) -> Self {
        assert!(valid_data_size(data_size));
        let ctrl = region.cast::<RingControl>();
        // SAFETY: the caller guarantees the region; zeroing the control block is its initial state.
        unsafe {
            core::ptr::write_bytes(region.as_ptr(), 0, DATA_OFFSET);
            (*ctrl.as_ptr()).magic = MAGIC;
            (*ctrl.as_ptr()).version = crate::VERSION;
        }
        Self {
            ctrl,
            // SAFETY: DATA_OFFSET is inside the region.
            data: unsafe { region.add(DATA_OFFSET) },
            size: data_size as u64,
            tail: 0,
            head_seen: 0,
        }
    }

    /// The same producer over a copy of its ring at `region`, as a snapshot restore leaves it.
    ///
    /// # Safety
    /// As for [`Self::init`], and `region` must hold a copy of the ring's bytes.
    pub unsafe fn moved_to(self, region: NonNull<u8>) -> Self {
        Self {
            ctrl: region.cast(),
            // SAFETY: DATA_OFFSET is inside the region.
            data: unsafe { region.add(DATA_OFFSET) },
            ..self
        }
    }

    pub fn control(&self) -> &RingControl {
        // SAFETY: valid for our lifetime by the contract of `init`.
        unsafe { self.ctrl.as_ref() }
    }

    /// Bytes pushed so far, published or not.
    pub fn position(&self) -> u64 {
        self.tail
    }

    fn reserve(&mut self, len: usize) -> Result<NonNull<u8>, PushError> {
        let len = len as u64;
        if len > self.size / 2 {
            return Err(PushError::TooLarge);
        }
        let pos = self.tail & (self.size - 1);
        let to_end = self.size - pos;
        let need = if len > to_end { to_end + len } else { len };
        if self.tail + need - self.head_seen > self.size {
            self.head_seen = self.control().head.load(Ordering::Acquire);
            if self.tail + need - self.head_seen > self.size {
                return Err(PushError::Full);
            }
        }
        if len > to_end {
            let pad = CmdHeader {
                op: cmd::op::PAD,
                flags: 0,
                len: to_end as u32,
            };
            // SAFETY: `pos + 8 <= size` because commands are 8-aligned and `to_end > 0`.
            unsafe {
                self.data
                    .add(pos as usize)
                    .cast::<CmdHeader>()
                    .write_unaligned(pad)
            };
            self.tail += to_end;
        }
        // SAFETY: `tail & mask` is inside the data area and `len` fits before its end.
        Ok(unsafe { self.data.add((self.tail & (self.size - 1)) as usize) })
    }

    /// Appends `cmd` followed by the concatenation of `extra`. Not visible to the consumer until
    /// [`publish`](Self::publish).
    pub fn push<T: Command>(&mut self, cmd: &T, extra: &[&[u8]]) -> Result<(), PushError> {
        let extra_len: usize = extra.iter().map(|e| e.len()).sum();
        let len = encoded_len::<T>(extra_len);
        let mut p = self.reserve(len)?.as_ptr();
        let header = CmdHeader {
            op: T::OP,
            flags: 0,
            len: len as u32,
        };
        // SAFETY: `reserve` returned `len` writable bytes; `Command` types have no padding, so their
        // bytes are initialised.
        unsafe {
            p.cast::<CmdHeader>().write_unaligned(header);
            p = p.add(size_of::<CmdHeader>());
            core::ptr::copy_nonoverlapping((cmd as *const T).cast::<u8>(), p, size_of::<T>());
            p = p.add(size_of::<T>());
            for e in extra {
                core::ptr::copy_nonoverlapping(e.as_ptr(), p, e.len());
                p = p.add(e.len());
            }
            let written = size_of::<CmdHeader>() + size_of::<T>() + extra_len;
            core::ptr::write_bytes(p, 0, len - written);
        }
        self.tail += len as u64;
        Ok(())
    }

    /// Makes everything pushed visible. Returns whether the consumer is parked and so needs a
    /// doorbell.
    pub fn publish(&mut self) -> bool {
        let c = self.control();
        c.tail.store(self.tail, Ordering::Release);
        fence(Ordering::SeqCst);
        c.state.load(Ordering::Relaxed) == STATE_PARKED
    }

    /// Highest seqno the GPU has finished.
    pub fn completed(&self) -> u64 {
        self.control().completed.load(Ordering::Acquire)
    }

    /// The error that lost the context, if any.
    pub fn error(&self) -> Option<(u32, u64)> {
        let c = self.control();
        match c.error.load(Ordering::Acquire) {
            0 => None,
            e => Some((e, c.error_at.load(Ordering::Acquire))),
        }
    }
}

/// The host end.
pub struct Consumer {
    ctrl: NonNull<RingControl>,
    data: NonNull<u8>,
    size: u64,
    head: u64,
}

// SAFETY: as for `Producer`.
unsafe impl Send for Consumer {}

/// One command copied out of the ring.
pub struct Next<'a> {
    pub op: u16,
    /// Ring position of its header, for error reports.
    pub at: u64,
    /// The bytes after the header, padding included.
    pub payload: &'a [u8],
}

impl Consumer {
    /// Attaches to a ring the producer has initialised, and resets the host-written fields.
    ///
    /// # Safety
    /// `region` must be valid for [`region_size`]`(data_size)` bytes, aligned to 128, for as long
    /// as the consumer lives.
    pub unsafe fn attach(region: NonNull<u8>, data_size: u32) -> Result<Self, Error> {
        if !valid_data_size(data_size) {
            return Err(Error::BadValue);
        }
        let ctrl = region.cast::<RingControl>();
        // SAFETY: by the caller's contract.
        let c = unsafe { ctrl.as_ref() };
        // Plain reads of guest-written words; a torn value only fails the check.
        // SAFETY: in bounds and aligned.
        let (magic, version) = unsafe {
            (
                core::ptr::read_volatile(&c.magic),
                core::ptr::read_volatile(&c.version),
            )
        };
        if magic != MAGIC || version != crate::VERSION {
            return Err(Error::Corrupt);
        }
        let head = c.tail.load(Ordering::Acquire);
        if head != 0 {
            // A ring is attached before anything is pushed into it.
            return Err(Error::Corrupt);
        }
        c.head.store(0, Ordering::Relaxed);
        c.completed.store(0, Ordering::Relaxed);
        c.error.store(0, Ordering::Relaxed);
        c.error_at.store(0, Ordering::Relaxed);
        c.state.store(STATE_RUNNING, Ordering::Release);
        Ok(Self {
            ctrl,
            // SAFETY: inside the region.
            data: unsafe { region.add(DATA_OFFSET) },
            size: data_size as u64,
            head: 0,
        })
    }

    /// Picks a ring up at `position`, where a consumer before a snapshot left it. The host-written
    /// fields are the ring's own, restored with it.
    ///
    /// # Safety
    /// As for [`Self::attach`].
    pub unsafe fn resume(
        region: NonNull<u8>,
        data_size: u32,
        position: u64,
    ) -> Result<Self, Error> {
        if !valid_data_size(data_size) || !position.is_multiple_of(CMD_ALIGN as u64) {
            return Err(Error::BadValue);
        }
        let ctrl = region.cast::<RingControl>();
        // SAFETY: by the caller's contract.
        let c = unsafe { ctrl.as_ref() };
        // SAFETY: in bounds and aligned.
        let (magic, version) = unsafe {
            (
                core::ptr::read_volatile(&c.magic),
                core::ptr::read_volatile(&c.version),
            )
        };
        let tail = c.tail.load(Ordering::Acquire);
        if magic != MAGIC
            || version != crate::VERSION
            || tail < position
            || tail - position > data_size as u64
        {
            return Err(Error::Corrupt);
        }
        c.head.store(position, Ordering::Release);
        Ok(Self {
            ctrl,
            // SAFETY: inside the region.
            data: unsafe { region.add(DATA_OFFSET) },
            size: data_size as u64,
            head: position,
        })
    }

    pub fn control(&self) -> &RingControl {
        // SAFETY: valid for our lifetime by the contract of `attach`.
        unsafe { self.ctrl.as_ref() }
    }

    /// A pointer to the control block that outlives borrows of the consumer, for completion
    /// handlers. Valid as long as the ring's memory is.
    pub fn control_ptr(&self) -> NonNull<RingControl> {
        self.ctrl
    }

    /// Size of the data area; a command's payload is at most half of it.
    pub fn data_size(&self) -> u64 {
        self.size
    }

    /// Bytes consumed.
    pub fn position(&self) -> u64 {
        self.head
    }

    /// Copies the next command into `scratch` and consumes it. `Ok(None)` when the ring is empty.
    /// `scratch` must hold at least half the data area.
    pub fn next<'s>(&mut self, scratch: &'s mut [u8]) -> Result<Option<Next<'s>>, Error> {
        loop {
            let tail = self.control().tail.load(Ordering::Acquire);
            if tail == self.head {
                return Ok(None);
            }
            let avail = tail.wrapping_sub(self.head);
            if avail > self.size || !avail.is_multiple_of(CMD_ALIGN as u64) {
                return Err(Error::Corrupt);
            }
            let at = self.head;
            let pos = (self.head & (self.size - 1)) as usize;
            // SAFETY: `pos + 8 <= size`: positions are 8-aligned inside the data area.
            let header = unsafe { self.data.add(pos).cast::<CmdHeader>().read_volatile() };
            let len = header.len as usize;
            if len < size_of::<CmdHeader>()
                || !len.is_multiple_of(CMD_ALIGN)
                || len as u64 > avail
                || pos + len > self.size as usize
            {
                return Err(Error::Malformed);
            }
            self.head += len as u64;
            if header.op == cmd::op::PAD {
                self.release();
                continue;
            }
            let body = len - size_of::<CmdHeader>();
            if body > scratch.len() {
                return Err(Error::Malformed);
            }
            // SAFETY: `pos + len` is inside the data area; the copy is what makes later checks
            // stick, since the guest can rewrite the ring at any time.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.data.add(pos + size_of::<CmdHeader>()).as_ptr(),
                    scratch.as_mut_ptr(),
                    body,
                )
            };
            self.release();
            return Ok(Some(Next {
                op: header.op,
                at,
                payload: &scratch[..body],
            }));
        }
    }

    /// Publishes `head`, freeing the space of everything consumed.
    fn release(&self) {
        self.control().head.store(self.head, Ordering::Release);
    }

    /// Announces that the consumer is about to sleep. Returns `false` if commands arrived in the
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

    /// Whether commands are waiting.
    pub fn has_work(&self) -> bool {
        self.control().tail.load(Ordering::Acquire) != self.head
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::cmd::{Commit, FillBuffer, decode};
    use std::vec;
    use std::vec::Vec;

    #[repr(C, align(128))]
    struct Region([u8; DATA_OFFSET + 4096]);

    /// The region leaks; tests are short-lived.
    fn ring() -> (*mut u8, Producer, Consumer) {
        let r = std::boxed::Box::leak(std::boxed::Box::new(Region([0; DATA_OFFSET + 4096])));
        let p = NonNull::new(r.0.as_mut_ptr()).unwrap();
        let prod = unsafe { Producer::init(p, 4096) };
        let cons = unsafe { Consumer::attach(p, 4096) }.unwrap();
        (p.as_ptr(), prod, cons)
    }

    #[test]
    fn round_trips_commands_with_trailing_bytes_across_wraps() {
        let (_r, mut prod, mut cons) = ring();
        let mut scratch = vec![0u8; 2048];
        let mut seen: Vec<u64> = Vec::new();
        for i in 0..1000u64 {
            let name = [i as u8; 13];
            prod.push(
                &Commit {
                    seqno: i,
                    ..Default::default()
                },
                &[&name],
            )
            .unwrap();
            prod.push(
                &FillBuffer {
                    buffer: crate::cmd::BufRef {
                        arena: 1,
                        _pad: 0,
                        offset: i,
                    },
                    length: 64,
                    value: 7,
                    _pad: 0,
                },
                &[],
            )
            .unwrap();
            prod.publish();
            while let Some(n) = cons.next(&mut scratch).unwrap() {
                match n.op {
                    crate::cmd::op::COMMIT => {
                        let (c, rest) = decode::<Commit>(n.payload).unwrap();
                        assert_eq!(&rest[..13], &[c.seqno as u8; 13]);
                        seen.push(c.seqno);
                    }
                    crate::cmd::op::FILL_BUFFER => {
                        let (f, _) = decode::<FillBuffer>(n.payload).unwrap();
                        assert_eq!(f.buffer.offset, *seen.last().unwrap());
                    }
                    op => panic!("op {op}"),
                }
            }
        }
        assert_eq!(seen, (0..1000).collect::<Vec<_>>());
    }

    #[test]
    fn a_full_ring_refuses_until_the_consumer_frees_space() {
        let (_r, mut prod, mut cons) = ring();
        let mut n = 0;
        while prod
            .push(
                &Commit {
                    seqno: n,
                    ..Default::default()
                },
                &[],
            )
            .is_ok()
        {
            n += 1;
        }
        assert_eq!(
            prod.push(
                &Commit {
                    seqno: n,
                    ..Default::default()
                },
                &[]
            ),
            Err(PushError::Full)
        );
        prod.publish();
        let mut scratch = vec![0u8; 2048];
        assert!(cons.next(&mut scratch).unwrap().is_some());
        assert!(
            prod.push(
                &Commit {
                    seqno: n,
                    ..Default::default()
                },
                &[]
            )
            .is_ok()
        );
    }

    #[test]
    fn parking_is_seen_by_the_producer_and_refused_with_work_pending() {
        let (_r, mut prod, cons) = ring();
        assert!(cons.park());
        prod.push(
            &Commit {
                seqno: 1,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        assert!(prod.publish(), "a parked consumer must be kicked");
        cons.unpark();
        assert!(!cons.park(), "work is pending");
        assert!(!prod.publish());
    }

    #[test]
    fn a_lying_header_is_malformed() {
        let (base, mut prod, mut cons) = ring();
        prod.push(
            &Commit {
                seqno: 1,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        prod.publish();
        // Rewrite the length to run past the published tail.
        unsafe { base.add(DATA_OFFSET + 4).cast::<u32>().write(64) };
        let mut scratch = vec![0u8; 2048];
        assert_eq!(cons.next(&mut scratch).err(), Some(Error::Malformed));
    }
}
