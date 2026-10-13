//! The worker↔GPU host control protocol: Mach messages, one request and one reply each.
//!
//! This is the slow, privileged path: contexts, arenas and rings are created here, by the worker,
//! on the guest kernel's behalf. Commands never travel this way; they go through the rings.

/// The GPU host → worker, once, right after start: carries a send right to the GPU host's request port.
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
/// [`ExportArena`] → `{status}`: `{ctx, arena}` becomes reachable as `export` from any context,
/// and stays alive while exported even if its context goes.
pub const EXPORT_ARENA: i32 = 0x4c47_0006;
/// [`ImportArena`] → [`ArenaReply`]: arena `arena` of `ctx` is export `export`'s memory.
pub const IMPORT_ARENA: i32 = 0x4c47_0007;
/// `{export}` → `{status}`.
pub const RELEASE_EXPORT: i32 = 0x4c47_0008;
/// [`SurfaceRequest`] + a send right for an IOSurface → `{status}`: the context may make textures
/// over the surface under that id (`CREATE_TEXTURE_FROM_SURFACE`).
pub const BIND_SURFACE: i32 = 0x4c47_0009;
/// [`SurfaceRequest`] → `{status}`. Textures already made over the surface keep it.
pub const RELEASE_SURFACE: i32 = 0x4c47_000a;
/// `{ctx}` → [`ExportReply`] + a memory entry holding the context as it travels across a snapshot
/// ([`crate::journal::ContextExport`], encoded, `len` bytes). Answered once the context's ring is
/// idle and its committed work has finished, or given up on (the export says lost).
pub const EXPORT_CONTEXT: i32 = 0x4c47_000b;
/// [`RestoreArena`] → [`ArenaReply`] + a memory entry: an arena made at `gpu_base`, the address a
/// restored guest holds for it. Arenas are restored in address order, before anything else; holes
/// below the address are held by placeholders until [`RESTORE_DONE`].
pub const RESTORE_ARENA: i32 = 0x4c47_000c;
/// `{}` → `{status}`: every arena and context stands; the placeholders go.
pub const RESTORE_DONE: i32 = 0x4c47_000d;
/// [`RestoreContext`] + a memory entry holding an encoded
/// [`ContextExport`](crate::journal::ContextExport) → `{status}`. Starts the context's ring at
/// the exported position with its objects rebuilt, after its arenas are restored. A context that
/// cannot be rebuilt starts lost; the request still succeeds.
pub const RESTORE_CONTEXT: i32 = 0x4c47_000e;

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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ExportArena {
    pub ctx: u32,
    pub arena: u32,
    pub export: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ImportArena {
    pub ctx: u32,
    pub arena: u32,
    pub export: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ExportRequest {
    pub export: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RestoreArena {
    pub ctx: u32,
    pub arena: u32,
    pub size: u64,
    pub gpu_base: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RestoreContext {
    /// Where the ring is, as for [`AttachRing`].
    pub ring: AttachRing,
    /// Bytes of the export in the memory entry.
    pub len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ExportReply {
    pub status: i32,
    pub _pad: u32,
    pub len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SurfaceRequest {
    pub ctx: u32,
    pub surface: u32,
}

// SAFETY: all of these are repr(C) integers with explicit padding.
unsafe impl Pod for CtxRequest {}
unsafe impl Pod for Status {}
unsafe impl Pod for CreateArena {}
unsafe impl Pod for ArenaReply {}
unsafe impl Pod for ArenaRequest {}
unsafe impl Pod for AttachRing {}
unsafe impl Pod for ExportArena {}
unsafe impl Pod for ImportArena {}
unsafe impl Pod for ExportRequest {}
unsafe impl Pod for SurfaceRequest {}
unsafe impl Pod for ExportReply {}
unsafe impl Pod for RestoreArena {}
unsafe impl Pod for RestoreContext {}
