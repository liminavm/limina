//! The worker↔replayer control protocol: Mach messages, one request and one reply each.
//!
//! This is the slow, privileged path: contexts, arenas and rings are created here, by the worker,
//! on the guest kernel's behalf. Commands never travel this way; they go through the rings.

/// Replayer → worker, once, right after start: carries a send right to the replayer's request port.
pub const HELLO: i32 = 0x4c47_0000;
/// `{ctx}` → `{status}` + the context's doorbell semaphore.
pub const CREATE_CONTEXT: i32 = 0x4c47_0001;
/// `{ctx}` → `{status}`.
pub const DESTROY_CONTEXT: i32 = 0x4c47_0002;
/// [`CreateArena`] → [`ArenaReply`] + a memory entry for the arena's pages.
pub const CREATE_ARENA: i32 = 0x4c47_0003;
/// `{ctx, arena}` → `{status}`.
pub const DESTROY_ARENA: i32 = 0x4c47_0004;
/// [`AttachRing`] → `{status}`.
pub const ATTACH_RING: i32 = 0x4c47_0005;

/// Request statuses.
pub const OK: i32 = 0;
pub const ERR_MALFORMED: i32 = 1;
pub const ERR_NO_CONTEXT: i32 = 2;
pub const ERR_EXISTS: i32 = 3;
pub const ERR_NO_ARENA: i32 = 4;
pub const ERR_BAD_VALUE: i32 = 5;
pub const ERR_NO_MEMORY: i32 = 6;
pub const ERR_INTERNAL: i32 = 7;

/// Plain-old-data messages. Every bit pattern is valid and there is no implicit padding.
///
/// # Safety
/// As stated: the type is `repr(C)`, fully initialised by its fields, and valid for any bytes.
pub unsafe trait Pod: Copy + Default + 'static {
    fn bytes(&self) -> &[u8] {
        // SAFETY: by the trait contract.
        unsafe { std::slice::from_raw_parts((self as *const Self).cast::<u8>(), size_of::<Self>()) }
    }

    fn read(data: &[u8]) -> Option<Self> {
        if data.len() < size_of::<Self>() {
            return None;
        }
        // SAFETY: in bounds; valid for any bytes by the trait contract.
        Some(unsafe { data.as_ptr().cast::<Self>().read_unaligned() })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CtxRequest {
    pub ctx: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    pub status: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateArena {
    pub ctx: u32,
    pub arena: u32,
    pub size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaReply {
    pub status: i32,
    pub _pad: u32,
    /// The arena's size as allocated (rounded up to the host page).
    pub size: u64,
    /// GPU address of the arena's first byte.
    pub gpu_base: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaRequest {
    pub ctx: u32,
    pub arena: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AttachRing {
    pub ctx: u32,
    pub arena: u32,
    pub offset: u64,
    pub data_size: u32,
    pub _pad: u32,
}

// SAFETY: all of these are repr(C) integers with explicit padding.
unsafe impl Pod for CtxRequest {}
unsafe impl Pod for Status {}
unsafe impl Pod for CreateArena {}
unsafe impl Pod for ArenaReply {}
unsafe impl Pod for ArenaRequest {}
unsafe impl Pod for AttachRing {}
