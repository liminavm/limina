//! GPU memory for buffers and command-buffer scratch, carved out of arenas.
//!
//! A context has at most 4096 arenas, ids chosen by the guest below that, and a loaded model has
//! thousands of buffers. So, as KosmicKrisp's bridge does with its heaps, regions of up to
//! [`POOLED_MAX`] come out of shared [`POOL_ARENA`]-byte arenas, and larger ones get an arena of
//! their own. Arena ids are reused once their arena is destroyed.
//!
//! A freed region is reused at once. That is Metal's contract with heaps and untracked resources,
//! which MLX keeps: it frees a buffer only once no command buffer still in flight uses it. An
//! arena is destroyed later, once every commit made before the free has completed: the host frees
//! its memory and reuses its guest-physical range at once, and the destroy travels on the control
//! queue, not the ring.

use std::collections::BTreeMap;
use std::fs::File;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use limina_gpu_client::{Arena, MIB, destroy_arena};

pub const POOL_ARENA: u64 = 64 * MIB;
pub const POOLED_MAX: u64 = 16 * MIB;
/// The main arena, which holds the ring.
const MAIN_ARENA: u32 = 1;
/// `VIRTGPU_LIMINA_MAX_ARENAS`.
const MAX_ARENAS: u32 = 4096;
/// The host page: an arena of its own is whole pages of it.
const HOST_PAGE: u64 = 16384;

/// The seqno of the latest commit, so a free can tell which commits it must wait for.
pub static COMMITTED: AtomicU64 = AtomicU64::new(0);

/// GPU memory: `size` bytes at `ptr` in this process and at `gpu` for the GPU.
pub struct Region {
    pub ptr: *mut u8,
    pub gpu: u64,
    pub size: u64,
    pub arena: u32,
    pub offset: u64,
    /// The mapping, for a region with an arena of its own.
    own: Option<Arena>,
}

// SAFETY: a region names memory, not thread-bound state; whoever holds it decides who touches it.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Drop for Region {
    fn drop(&mut self) {
        crate::metal::pool().lock().unwrap().free(self);
    }
}

struct Pooled {
    id: u32,
    arena: Arena,
    /// Free ranges: offset to length.
    free: BTreeMap<u64, u64>,
    used: u64,
}

pub struct Pool {
    file: File,
    /// The ring's completed seqno, which the host writes: in the main arena, which lives as long
    /// as the process.
    completed: &'static AtomicU64,
    align: u64,
    next_id: u32,
    free_ids: Vec<u32>,
    pooled: Vec<Pooled>,
    /// Arenas no region uses any more, unmapped, with the seqno that must complete first.
    doomed: Vec<(u64, u32)>,
}

// SAFETY: the arenas' mappings are plain memory; the pool is only reached through a Mutex.
unsafe impl Send for Pool {}

impl Pool {
    /// A pool over the context of `file`, whose ring's completed seqno is `completed`, aligning
    /// regions to `align` bytes.
    pub fn new(file: File, completed: &'static AtomicU64, align: u64) -> Mutex<Self> {
        Mutex::new(Self {
            file,
            completed,
            align: align.max(256),
            next_id: MAIN_ARENA,
            free_ids: Vec::new(),
            pooled: Vec::new(),
            doomed: Vec::new(),
        })
    }

    fn id(&mut self) -> std::io::Result<u32> {
        let start = Instant::now();
        loop {
            if let Some(id) = self.free_ids.pop() {
                return Ok(id);
            }
            if self.next_id + 1 < MAX_ARENAS {
                self.next_id += 1;
                return Ok(self.next_id);
            }
            // Every id is in use or doomed. The doomed ones come back once the commits they wait
            // for complete, which the completion thread may not have seen yet.
            if self.doomed.is_empty() || start.elapsed() > Duration::from_secs(10) {
                return Err(std::io::Error::other(format!(
                    "all {MAX_ARENAS} arenas of the context are in use"
                )));
            }
            self.reap(self.completed.load(Ordering::Acquire));
            if self.free_ids.is_empty() {
                std::thread::sleep(Duration::from_micros(50));
            }
        }
    }

    fn create(&mut self, size: u64) -> std::io::Result<(u32, Arena)> {
        let id = self.id()?;
        match Arena::create(&self.file, id, size) {
            Ok(a) => Ok((id, a)),
            Err(e) => {
                self.free_ids.push(id);
                Err(e)
            }
        }
    }

    /// `len` bytes of GPU memory.
    pub fn alloc(&mut self, len: u64) -> std::io::Result<Region> {
        let size = len.max(1).next_multiple_of(self.align);
        if size > POOLED_MAX {
            let (id, arena) = self.create(size.next_multiple_of(HOST_PAGE))?;
            return Ok(Region {
                ptr: arena.ptr.as_ptr(),
                gpu: arena.gpu_base,
                size,
                arena: id,
                offset: 0,
                own: Some(arena),
            });
        }
        let found = self
            .pooled
            .iter()
            .position(|p| p.free.values().any(|&l| l >= size));
        let i = match found {
            Some(i) => i,
            None => {
                let (id, arena) = self.create(POOL_ARENA)?;
                self.pooled.push(Pooled {
                    id,
                    arena,
                    free: BTreeMap::from([(0, POOL_ARENA)]),
                    used: 0,
                });
                self.pooled.len() - 1
            }
        };
        let p = &mut self.pooled[i];
        let (&offset, &free) = p.free.iter().find(|&(_, &l)| l >= size).unwrap();
        p.free.remove(&offset);
        if free > size {
            p.free.insert(offset + size, free - size);
        }
        p.used += size;
        Ok(Region {
            // SAFETY: inside the arena's mapping.
            ptr: unsafe { p.arena.ptr.as_ptr().add(offset as usize) },
            gpu: p.arena.gpu_base + offset,
            size,
            arena: p.id,
            offset,
            own: None,
        })
    }

    fn free(&mut self, r: &mut Region) {
        let after = COMMITTED.load(Ordering::Acquire);
        if let Some(arena) = r.own.take() {
            drop(arena);
            self.doomed.push((after, r.arena));
            return;
        }
        let Some(i) = self.pooled.iter().position(|p| p.id == r.arena) else {
            return;
        };
        let p = &mut self.pooled[i];
        let (mut offset, mut size) = (r.offset, r.size);
        if let Some((&o, &l)) = p.free.range(..offset).next_back()
            && o + l == offset
        {
            p.free.remove(&o);
            offset = o;
            size += l;
        }
        if let Some(l) = p.free.remove(&(offset + size)) {
            size += l;
        }
        p.free.insert(offset, size);
        p.used -= r.size;
        // Keep one empty pooled arena for the next allocation; destroy any other.
        let empty = self.pooled.iter().filter(|p| p.used == 0).count();
        if self.pooled[i].used == 0 && empty > 1 {
            let p = self.pooled.swap_remove(i);
            drop(p.arena);
            self.doomed.push((after, p.id));
        }
    }

    /// Destroys the doomed arenas whose commits have all completed, up to seqno `done`.
    pub fn reap(&mut self, done: u64) {
        let (ready, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.doomed)
            .into_iter()
            .partition(|&(after, _)| after <= done);
        self.doomed = kept;
        for (_, id) in ready {
            match destroy_arena(&self.file, id) {
                Ok(()) => self.free_ids.push(id),
                // The id stays out of use rather than being handed out twice.
                Err(e) => eprintln!("limina-metal: destroying arena {id}: {e}"),
            }
        }
    }
}
