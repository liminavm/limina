//! The worker's side of the Metal-level GPU transport: libkrun's virtio-gpu extension
//! (`devices::virtio::gpu::ext`) served by a `limina-replayer` process.
//!
//! The device decides where things go in guest-physical space and keeps the guest's books; this
//! decides what they are: contexts and arenas live in the replayer, arena pages are mapped here so
//! the device can hand them to the guest, and a doorbell store becomes a semaphore signal. Fences
//! are answered here too, by watching each ring's `completed` in the mapping of its arena.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use devices::virtio::gpu::ext::{ExtArena, GpuExtension, Signal};
use limina_gpu_wire::ring::RingControl;
use limina_replayer::client::{Doorbell, MappedArena, Replayer};
use limina_replayer::journal::ContextExport;

/// Off by default until a guest driver exists: a stock guest would only pay for an idle process.
const ENABLE_ENV: &str = "LIMINA_GPU_METAL";
/// Guest-physical space for arenas. Arenas are placed in it on demand; nothing is allocated up
/// front.
pub const ARENA_WINDOW: usize = 16 << 30;

struct ReplayerExtension {
    replayer: Replayer,
    /// Doorbell slot → that context's doorbell. Read on vCPU threads.
    doorbells: RwLock<Vec<Option<Doorbell>>>,
    /// ctx → its slot, to clear it on destroy.
    slots: Mutex<HashMap<u32, u32>>,
    /// (ctx, arena) → the worker's mapping of it, which the guest's mapping is made from. An
    /// import shares its export's mapping.
    arenas: Mutex<HashMap<(u32, u32), Arc<MappedArena>>>,
    /// Exported arenas by blob resource: each keeps its mapping (and, in the replayer, its
    /// arena) whatever happens to the context that made it.
    exports: Mutex<HashMap<u32, Arc<MappedArena>>>,
    fences: Arc<Fences>,
    /// A restore made the replayer context that holds arenas only their exports still hold.
    holding_orphans: Mutex<bool>,
}

/// The replayer context a restore puts orphaned exported arenas in, for as long as the restore
/// runs: the guest's contexts are never 0.
const ORPHANS: u32 = 0;

/// A context's ring control block: in the mapping of the arena it was attached in.
struct Ring {
    arena: Arc<MappedArena>,
    offset: usize,
}

impl Ring {
    fn control(&self) -> &RingControl {
        // SAFETY: `offset` was checked to leave a whole, aligned control block inside the
        // mapping, which `arena` keeps alive. The guest writes only the producer's fields.
        unsafe { &*((self.arena.mapping.addr() as usize + self.offset) as *const RingControl) }
    }
}

/// Fences waiting for their seqno, and the rings they wait on.
#[derive(Default)]
struct Fences {
    pending: Mutex<Vec<(u32, u64, Signal)>>,
    wake: Condvar,
    rings: Mutex<HashMap<u32, Ring>>,
}

/// How often the watch looks at the rings while a fence waits. Fences guard only buffers that
/// cross processes, so a few per frame at most.
const FENCE_POLL: Duration = Duration::from_micros(100);

impl Fences {
    /// Answers each fence whose ring has completed its seqno, lost its context, or gone.
    fn watch(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            while pending.is_empty() {
                pending = self.wake.wait(pending).unwrap_or_else(|e| e.into_inner());
            }
            let mut done = Vec::new();
            {
                let rings = self.rings.lock().unwrap_or_else(|e| e.into_inner());
                let mut i = 0;
                while i < pending.len() {
                    let (ctx, seqno, _) = pending[i];
                    let answered = rings.get(&ctx).is_none_or(|r| {
                        let c = r.control();
                        c.completed.load(Ordering::Acquire) >= seqno
                            || c.error.load(Ordering::Acquire) != 0
                    });
                    if answered {
                        done.push(pending.swap_remove(i).2);
                    } else {
                        i += 1;
                    }
                }
            }
            if !done.is_empty() {
                drop(pending);
                for signal in done {
                    signal();
                }
                pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                continue;
            }
            pending = self
                .wake
                .wait_timeout(pending, FENCE_POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

fn info(a: &MappedArena) -> ExtArena {
    ExtArena {
        host_addr: a.mapping.addr(),
        size: a.mapping.size(),
        gpu_base: a.gpu_base,
    }
}

impl GpuExtension for ReplayerExtension {
    fn create_context(&self, ctx: u32, doorbell: u32) -> Result<(), String> {
        let bell = self
            .replayer
            .create_context(ctx)
            .map_err(|e| e.to_string())?;
        let mut bells = self.doorbells.write().unwrap_or_else(|e| e.into_inner());
        let slot = doorbell as usize;
        if bells.len() <= slot {
            bells.resize_with(slot + 1, || None);
        }
        bells[slot] = Some(bell);
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ctx, doorbell);
        Ok(())
    }

    fn destroy_context(&self, ctx: u32) {
        if let Some(slot) = self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ctx)
            && let Some(b) = self
                .doorbells
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(slot as usize)
        {
            *b = None;
        }
        self.arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|&(c, _), _| c != ctx);
        self.fences
            .rings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ctx);
        self.fences.wake.notify_one();
        if let Err(e) = self.replayer.destroy_context(ctx) {
            log::warn!("metal ext: destroy context {ctx}: {e}");
        }
    }

    fn create_arena(&self, ctx: u32, arena: u32, size: u64) -> Result<ExtArena, String> {
        let a = self
            .replayer
            .create_arena(ctx, arena, size)
            .map_err(|e| e.to_string())?;
        let i = info(&a);
        self.arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((ctx, arena), Arc::new(a));
        Ok(i)
    }

    fn destroy_arena(&self, ctx: u32, arena: u32) {
        self.arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(ctx, arena));
        if let Err(e) = self.replayer.destroy_arena(ctx, arena) {
            log::warn!("metal ext: destroy arena {ctx}/{arena}: {e}");
        }
    }

    fn attach_ring(&self, ctx: u32, arena: u32, offset: u64, data_size: u32) -> Result<(), String> {
        let a = self
            .arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(ctx, arena))
            .cloned()
            .ok_or("no such arena")?;
        let end = offset.checked_add(size_of::<RingControl>() as u64);
        if !offset.is_multiple_of(align_of::<RingControl>() as u64)
            || end.is_none_or(|e| e > a.mapping.size())
        {
            return Err("ring control block outside its arena".into());
        }
        self.replayer
            .attach_ring(ctx, arena, offset, data_size)
            .map_err(|e| e.to_string())?;
        self.fences
            .rings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                ctx,
                Ring {
                    arena: a,
                    offset: offset as usize,
                },
            );
        Ok(())
    }

    fn fence(&self, ctx: u32, seqno: u64, signal: Signal) {
        self.fences
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((ctx, seqno, signal));
        self.fences.wake.notify_one();
    }

    fn export_arena(&self, ctx: u32, arena: u32, export: u32) -> Result<ExtArena, String> {
        let a = self
            .arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(ctx, arena))
            .cloned()
            .ok_or("no such arena")?;
        self.replayer
            .export_arena(ctx, arena, export)
            .map_err(|e| e.to_string())?;
        let i = info(&a);
        self.exports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(export, a);
        Ok(i)
    }

    fn import_arena(&self, ctx: u32, arena: u32, export: u32) -> Result<ExtArena, String> {
        let a = self
            .exports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&export)
            .cloned()
            .ok_or("no such export")?;
        self.replayer
            .import_arena(ctx, arena, export)
            .map_err(|e| e.to_string())?;
        let i = info(&a);
        self.arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((ctx, arena), a);
        Ok(i)
    }

    fn release_export(&self, export: u32) {
        self.exports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&export);
        if let Err(e) = self.replayer.release_export(export) {
            log::warn!("metal ext: release export {export}: {e}");
        }
    }

    fn bind_surface(&self, ctx: u32, surface: u32, port: u32) -> Result<(), String> {
        self.replayer
            .bind_surface(ctx, surface, port)
            .map_err(|e| e.to_string())
    }

    fn release_surface(&self, ctx: u32, surface: u32) {
        if let Err(e) = self.replayer.release_surface(ctx, surface) {
            log::warn!("metal ext: release surface {surface} of {ctx}: {e}");
        }
    }

    fn export_context(&self, ctx: u32) -> Result<Vec<u8>, String> {
        self.replayer.export_context(ctx).map_err(|e| e.to_string())
    }

    fn restore_arena(
        &self,
        ctx: u32,
        arena: u32,
        size: u64,
        gpu_base: u64,
    ) -> Result<ExtArena, String> {
        if ctx == ORPHANS {
            let mut holding = self
                .holding_orphans
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !*holding {
                self.replayer
                    .create_context(ORPHANS)
                    .map_err(|e| e.to_string())?;
                *holding = true;
            }
        }
        let a = self
            .replayer
            .restore_arena(ctx, arena, size, gpu_base)
            .map_err(|e| e.to_string())?;
        let i = info(&a);
        self.arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((ctx, arena), Arc::new(a));
        Ok(i)
    }

    fn restore_ring(
        &self,
        ctx: u32,
        arena: u32,
        offset: u64,
        data_size: u32,
        export: &[u8],
    ) -> Result<(), String> {
        let a = self
            .arenas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(ctx, arena))
            .cloned()
            .ok_or("no such arena")?;
        let end = offset.checked_add(size_of::<RingControl>() as u64);
        if !offset.is_multiple_of(align_of::<RingControl>() as u64)
            || end.is_none_or(|e| e > a.mapping.size())
        {
            return Err("ring control block outside its arena".into());
        }
        let ring = Ring {
            arena: a,
            offset: offset as usize,
        };
        // A context the snapshot could not export restarts lost, where its consumer stopped.
        let lost;
        let export = if export.is_empty() {
            lost = ContextExport {
                position: ring.control().head.load(Ordering::Acquire),
                lost: true,
                ..Default::default()
            }
            .encode();
            &lost
        } else {
            export
        };
        self.replayer
            .restore_context(ctx, arena, offset, data_size, export)
            .map_err(|e| e.to_string())?;
        self.fences
            .rings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ctx, ring);
        Ok(())
    }

    fn restore_done(&self) {
        if let Err(e) = self.replayer.restore_done() {
            log::warn!("metal ext: restore done: {e}");
        }
        let mut holding = self
            .holding_orphans
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if std::mem::take(&mut *holding) {
            // Their exports keep the arenas, here and in the replayer.
            self.arenas
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|&(c, _), _| c != ORPHANS);
            if let Err(e) = self.replayer.destroy_context(ORPHANS) {
                log::warn!("metal ext: orphan holder: {e}");
            }
        }
    }

    fn doorbell(&self, doorbell: u32) {
        if let Some(Some(b)) = self
            .doorbells
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(doorbell as usize)
        {
            b.ring();
        }
    }
}

/// The replayer binary: next to this one, as cargo and the app bundle both lay them out.
fn replayer_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("limina-replayer"))
}

/// Starts a replayer and returns the extension it serves, when enabled. A failure leaves the VM
/// without the extension, which a guest sees as a plain virtio-gpu.
pub fn start() -> Option<Arc<dyn GpuExtension>> {
    std::env::var_os(ENABLE_ENV)?;
    let exe = replayer_exe()?;
    match Replayer::spawn(&exe) {
        Ok(replayer) => {
            log::info!(
                "metal ext: replayer {} started (pid {})",
                exe.display(),
                replayer.pid()
            );
            let fences = Arc::new(Fences::default());
            let watched = fences.clone();
            if let Err(e) = std::thread::Builder::new()
                .name("metal-fences".into())
                .spawn(move || watched.watch())
            {
                log::warn!("metal ext: fence watch did not start: {e}");
                return None;
            }
            Some(Arc::new(ReplayerExtension {
                fences,
                replayer,
                doorbells: RwLock::new(Vec::new()),
                slots: Mutex::new(HashMap::new()),
                arenas: Mutex::new(HashMap::new()),
                exports: Mutex::new(HashMap::new()),
                holding_orphans: Mutex::new(false),
            }))
        }
        Err(e) => {
            log::warn!("metal ext: replayer {} did not start: {e}", exe.display());
            None
        }
    }
}
