//! The worker's side of the Metal-level GPU transport: libkrun's virtio-gpu extension
//! (`devices::virtio::gpu::ext`) served by a `limina-replayer` process.
//!
//! The device decides where things go in guest-physical space and keeps the guest's books; this
//! decides what they are: contexts and arenas live in the replayer, arena pages are mapped here so
//! the device can hand them to the guest, and a doorbell store becomes a semaphore signal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use devices::virtio::gpu::ext::{ExtArena, GpuExtension};
use limina_replayer::client::{Doorbell, MappedArena, Replayer};

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
        self.replayer
            .attach_ring(ctx, arena, offset, data_size)
            .map_err(|e| e.to_string())
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
            Some(Arc::new(ReplayerExtension {
                replayer,
                doorbells: RwLock::new(Vec::new()),
                slots: Mutex::new(HashMap::new()),
                arenas: Mutex::new(HashMap::new()),
                exports: Mutex::new(HashMap::new()),
            }))
        }
        Err(e) => {
            log::warn!("metal ext: replayer {} did not start: {e}", exe.display());
            None
        }
    }
}
