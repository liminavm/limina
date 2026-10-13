//! The worker's side of the Metal-level GPU transport: libkrun's virtio-gpu extension
//! (`devices::virtio::gpu::ext`) served by a `limina-gpu-host` process.
//!
//! The device decides where things go in guest-physical space and keeps the guest's books; this
//! decides what they are: contexts and arenas live in the GPU host, arena pages are mapped here so
//! the device can hand them to the guest, and a doorbell store becomes a semaphore signal. Fences
//! are answered here too, by watching each ring's `completed` in the mapping of its arena.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use devices::virtio::gpu::ext::{ExtArena, GpuExtension, Signal};
use limina_gpu_host::client::{Doorbell, GpuHost, MappedArena};
use limina_gpu_host::journal::ContextExport;
use limina_gpu_wire::ring::{self, RingControl};

/// Turns the transport on like `--gpu-metal`, for runs that cannot add a flag (the L2 tests).
/// Off by default: a guest without the vendor drivers would only pay for an idle process.
const ENABLE_ENV: &str = "LIMINA_GPU_METAL";
/// Guest-physical space for arenas. Arenas are placed in it on demand; nothing is allocated up
/// front.
pub const ARENA_WINDOW: usize = 16 << 30;

struct GpuHostExtension {
    host: GpuHost,
    /// Doorbell slot → that context's doorbell. Read on vCPU threads.
    doorbells: RwLock<Vec<Option<Doorbell>>>,
    /// ctx → its slot, to clear it on destroy.
    slots: Mutex<HashMap<u32, u32>>,
    /// (ctx, arena) → the worker's mapping of it, which the guest's mapping is made from. An
    /// import shares its export's mapping.
    arenas: Mutex<HashMap<(u32, u32), Arc<MappedArena>>>,
    /// Exported arenas by blob resource: each keeps its mapping (and, in the GPU host, its
    /// arena) whatever happens to the context that made it.
    exports: Mutex<HashMap<u32, Arc<MappedArena>>>,
    fences: Arc<Fences>,
    /// A restore made the GPU host context that holds arenas only their exports still hold.
    holding_orphans: Mutex<bool>,
}

/// The GPU host context a restore puts orphaned exported arenas in, for as long as the restore
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
    /// The GPU host exited: nothing will complete any more.
    gone: AtomicBool,
}

/// How often the watch looks at the rings while a fence waits. Fences guard only buffers that
/// cross processes, so a few per frame at most.
const FENCE_POLL: Duration = Duration::from_micros(100);

impl Fences {
    /// The GPU host exited. Every ring is lost, so the guest's driver sees its device gone, and
    /// every fence is answered, now and from now on: the work it waits for will never finish.
    fn host_gone(&self) {
        self.gone.store(true, Ordering::Release);
        for r in self
            .rings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            let c = r.control();
            c.fail(ring::Error::HostGone, c.head.load(Ordering::Acquire));
        }
        // Taken so the notify cannot fall between the watch's check and its wait.
        drop(self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        self.wake.notify_one();
    }

    /// Answers each fence whose ring has completed its seqno, lost its context, or gone.
    fn watch(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            while pending.is_empty() {
                pending = self.wake.wait(pending).unwrap_or_else(|e| e.into_inner());
            }
            let mut done = Vec::new();
            if self.gone.load(Ordering::Acquire) {
                done.extend(pending.drain(..).map(|(_, _, signal)| signal));
            } else {
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

impl GpuExtension for GpuHostExtension {
    fn wire_version(&self) -> u32 {
        limina_gpu_wire::VERSION
    }

    fn create_context(&self, ctx: u32, doorbell: u32) -> Result<(), String> {
        let bell = self.host.create_context(ctx).map_err(|e| e.to_string())?;
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
        if let Err(e) = self.host.destroy_context(ctx) {
            log::warn!("metal ext: destroy context {ctx}: {e}");
        }
    }

    fn create_arena(&self, ctx: u32, arena: u32, size: u64) -> Result<ExtArena, String> {
        let a = self
            .host
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
        if let Err(e) = self.host.destroy_arena(ctx, arena) {
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
        self.host
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
        self.host
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
        self.host
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
        if let Err(e) = self.host.release_export(export) {
            log::warn!("metal ext: release export {export}: {e}");
        }
    }

    fn bind_surface(&self, ctx: u32, surface: u32, port: u32) -> Result<(), String> {
        self.host
            .bind_surface(ctx, surface, port)
            .map_err(|e| e.to_string())
    }

    fn release_surface(&self, ctx: u32, surface: u32) {
        if let Err(e) = self.host.release_surface(ctx, surface) {
            log::warn!("metal ext: release surface {surface} of {ctx}: {e}");
        }
    }

    fn export_context(&self, ctx: u32) -> Result<Vec<u8>, String> {
        self.host.export_context(ctx).map_err(|e| e.to_string())
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
                self.host
                    .create_context(ORPHANS)
                    .map_err(|e| e.to_string())?;
                *holding = true;
            }
        }
        let a = self
            .host
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
        self.host
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
        if let Err(e) = self.host.restore_done() {
            log::warn!("metal ext: restore done: {e}");
        }
        let mut holding = self
            .holding_orphans
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if std::mem::take(&mut *holding) {
            // Their exports keep the arenas, here and in the GPU host.
            self.arenas
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|&(c, _), _| c != ORPHANS);
            if let Err(e) = self.host.destroy_context(ORPHANS) {
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

/// The GPU host binary: next to this one, as cargo and the app bundle both lay them out.
fn gpu_host_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("limina-gpu-host"))
}

/// Starts a GPU host and returns the extension it serves, when `enabled` or [`ENABLE_ENV`] asks.
/// A failure leaves the VM without the extension, which a guest sees as a plain virtio-gpu.
pub fn start(enabled: bool) -> Option<Arc<dyn GpuExtension>> {
    if !enabled && std::env::var_os(ENABLE_ENV).is_none() {
        return None;
    }
    let exe = gpu_host_exe()?;
    match GpuHost::spawn(&exe) {
        Ok(host) => {
            log::info!(
                "metal ext: GPU host {} started (pid {})",
                exe.display(),
                host.pid()
            );
            let fences = Arc::new(Fences::default());
            let dead = Arc::downgrade(&fences);
            if let Err(e) = host.on_exit(move || {
                log::error!("metal ext: the GPU host exited; every context it served is lost");
                if let Some(f) = dead.upgrade() {
                    f.host_gone();
                }
            }) {
                log::warn!("metal ext: cannot watch the GPU host: {e}");
                return None;
            }
            let watched = fences.clone();
            if let Err(e) = std::thread::Builder::new()
                .name("metal-fences".into())
                .spawn(move || watched.watch())
            {
                log::warn!("metal ext: fence watch did not start: {e}");
                return None;
            }
            Some(Arc::new(GpuHostExtension {
                fences,
                host,
                doorbells: RwLock::new(Vec::new()),
                slots: Mutex::new(HashMap::new()),
                arenas: Mutex::new(HashMap::new()),
                exports: Mutex::new(HashMap::new()),
                holding_orphans: Mutex::new(false),
            }))
        }
        Err(e) => {
            log::warn!("metal ext: GPU host {} did not start: {e}", exe.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use limina_gpu_host::mach::{self, Mapping};
    use limina_gpu_wire::ring::Error;

    use super::*;

    /// A ring control block in memory mapped the way the worker maps arenas.
    fn ring() -> Ring {
        let page = 16 << 10;
        let blob = mach::Anonymous::new(page).expect("memory");
        let (entry, _) = mach::make_memory_entry(blob.addr(), blob.size()).expect("entry");
        let mapping = Mapping::map(&entry, page).expect("mapping");
        Ring {
            arena: Arc::new(MappedArena {
                mapping,
                gpu_base: 0,
            }),
            offset: 0,
        }
    }

    /// A GPU host that dies answers every fence still waiting on it, and its rings say why, so
    /// the guest sees its device lost rather than waiting for work that will never complete.
    #[test]
    fn a_dead_gpu_host_answers_its_fences_and_loses_its_rings() {
        let fences = Arc::new(Fences::default());
        fences.rings.lock().unwrap().insert(1, ring());
        let watched = fences.clone();
        std::thread::spawn(move || watched.watch());
        let (tx, rx) = mpsc::channel();
        for seqno in [1, 2] {
            let tx = tx.clone();
            fences.pending.lock().unwrap().push((
                1,
                seqno,
                Box::new(move || tx.send(seqno).expect("the test waits for this")),
            ));
        }
        fences.wake.notify_one();
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "nothing completed yet"
        );

        fences.host_gone();

        let mut answered: Vec<u64> = (0..2)
            .map(|_| {
                rx.recv_timeout(Duration::from_secs(1))
                    .expect("a fence was left waiting")
            })
            .collect();
        answered.sort();
        assert_eq!(answered, [1, 2]);
        let rings = fences.rings.lock().unwrap();
        let control = rings[&1].control();
        assert_eq!(
            control.error.load(Ordering::Acquire),
            Error::HostGone as u32
        );
        drop(rings);
        // A fence asked for after the host died is answered too.
        let (tx, rx) = mpsc::channel();
        fences.pending.lock().unwrap().push((
            1,
            3,
            Box::new(move || tx.send(()).expect("the test waits for this")),
        ));
        fences.wake.notify_one();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("a fence after the host died was left waiting");
    }
}
