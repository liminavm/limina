// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The limina ⇄ libkrun facade.
//!
//! This module is the *one* place that translates limina's typed [`VmSpec`] into
//! libkrun's `VmResources` and drives the microVM. It deliberately reimplements the
//! orchestration that libkrun's C entrypoint (`krun_start_enter`) does internally, so
//! that all coupling to libkrun's *internal* Rust API is concentrated here — an
//! upstream rebase that changes `VmResources`/`build_microvm` touches this module and
//! nothing else (architecture decision D2.1).

mod console;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::unbounded;
use devices::virtio::block::{CacheType, ImageType, SyncMode};
use devices::virtio::display::DisplayInfo;
use limina_display::CaptureConfig;
use polly::event_manager::EventManager;
use vmm::resources::VmResources;
use vmm::vmm_config::block::BlockDeviceConfig;
use vmm::vmm_config::external_kernel::{ExternalKernel, KernelFormat};
use vmm::vmm_config::firmware::FirmwareConfig;
use vmm::vmm_config::fs::FsDeviceConfig;
use vmm::vmm_config::machine_config::VmConfig;
use vmm::vmm_config::vsock::VsockDeviceConfig;

use crate::config::{BootSource, DiskSpec, DisplaySpec, FsShare, KernelSpec, VmSpec, VsockSpec};

/// Standard libkrun guest CID.
const GUEST_CID: u32 = 3;

// virglrenderer init flag bits (see docs/research/03 §1.3).
#[allow(dead_code)]
const VIRGLRENDERER_USE_EGL: u32 = 1 << 0;
#[allow(dead_code)]
const VIRGLRENDERER_THREAD_SYNC: u32 = 1 << 1;
#[allow(dead_code)]
const VIRGLRENDERER_VENUS: u32 = 1 << 6;
const VIRGLRENDERER_NO_VIRGL: u32 = 1 << 7;
#[allow(dead_code)]
const VIRGLRENDERER_USE_ASYNC_FENCE_CB: u32 = 1 << 8;
#[allow(dead_code)]
const VIRGLRENDERER_RENDER_SERVER: u32 = 1 << 9;

/// virgl_flags for the virtio-gpu device.
///
/// Tier 1 (current): `NO_VIRGL`. Our libkrun patch handles 2D scanout entirely in host
/// CPU memory (software 2D), so the renderer needs no virgl/GL context — which is good,
/// because virgl GL has no host context on macOS and the brew virglrenderer has no render
/// server for Venus. This gives a clean rutabaga init (no "falling back to safe defaults").
///
/// Tier 2 (later, accelerated): the macOS Venus mask — `USE_EGL | VENUS | RENDER_SERVER |
/// THREAD_SYNC | USE_ASYNC_FENCE_CB` (libkrun's `gui_vm` example) — once we build a
/// virglrenderer with the render server and a Venus-capable guest. Override at runtime
/// with `LIMINA_VIRGL_FLAGS` (e.g. `0x343`).
const GPU_VIRGL_FLAGS: u32 = VIRGLRENDERER_NO_VIRGL;

/// Translate a [`VmSpec`] into a libkrun [`VmResources`]. No VM is started yet.
pub fn build_resources(spec: &VmSpec) -> Result<VmResources> {
    let mut vmr = VmResources::default();

    vmr.set_vm_config(&VmConfig {
        vcpu_count: Some(spec.cpus),
        mem_size_mib: Some(spec.ram_mib),
        ht_enabled: Some(false),
        cpu_template: None,
    })
    .map_err(|e| anyhow!("set_vm_config: {e:?}"))?;

    match &spec.boot {
        // EFI boot: load the EDK2 firmware blob; the guest boots its own kernel off the
        // disk's ESP (Payload::Firmware). No bundled kernel, no root_disk_remount.
        BootSource::Firmware(path) => {
            vmr.set_firmware_config(FirmwareConfig { path: path.clone() });
        }
        // Direct kernel boot (Payload::ExternalKernel): our own raw Image + initramfs.
        BootSource::Kernel(kernel) => set_external_kernel(&mut vmr, kernel)?,
    }

    for disk in &spec.disks {
        add_disk(&mut vmr, disk)?;
    }

    for share in &spec.shares {
        add_fs_share(&mut vmr, share)?;
    }

    if let Some(vsock) = &spec.vsock {
        add_vsock(&mut vmr, vsock)?;
    }

    if let Some(display) = &spec.display {
        add_display(&mut vmr, display)?;
    }

    if let Some(console) = &spec.console {
        console::attach(&mut vmr, console).context("attaching serial console")?;
    }

    Ok(vmr)
}

/// Attach a virtio-gpu display. The GPU device is created iff `gpu_virgl_flags` is set,
/// so this is what turns the display on. Tier 1: one 2D scanout at the requested mode,
/// with our capture backend as the host sink (PNG oracle). With no `capture_png` the
/// builder falls back to a no-op backend, so the device exists but frames go nowhere.
fn add_display(vmr: &mut VmResources, display: &DisplaySpec) -> Result<()> {
    // Allow a quick flag sweep without recompiling (e.g. LIMINA_VIRGL_FLAGS=0x103).
    let flags = std::env::var("LIMINA_VIRGL_FLAGS")
        .ok()
        .and_then(|s| {
            let s = s.trim();
            s.strip_prefix("0x")
                .map(|h| u32::from_str_radix(h, 16))
                .unwrap_or_else(|| s.parse::<u32>())
                .ok()
        })
        .unwrap_or(GPU_VIRGL_FLAGS);
    log::info!("virtio-gpu virgl_flags = {flags:#x}");
    vmr.set_gpu_virgl_flags(flags);
    vmr.displays
        .push(DisplayInfo::new(display.width, display.height));

    if let Some(png_path) = &display.capture_png {
        vmr.display_backend = Some(limina_display::capture_backend(CaptureConfig {
            png_path: png_path.clone(),
        }));
    }

    Ok(())
}

/// Configure a direct kernel boot (raw aarch64 `Image` + optional cpio initramfs).
///
/// libkrun loads the Image at `0x8000_0000`, the initramfs just below the FDT, and
/// passes our cmdline through the device tree. `initramfs_size` must be the real file
/// size — libkrun reserves guest RAM for it from that figure.
fn set_external_kernel(vmr: &mut VmResources, kernel: &KernelSpec) -> Result<()> {
    let initramfs_size = match &kernel.initramfs {
        Some(path) => std::fs::metadata(path)
            .with_context(|| format!("stat initramfs {path:?}"))?
            .len(),
        None => 0,
    };

    vmr.set_external_kernel(ExternalKernel {
        path: kernel.image.clone(),
        format: KernelFormat::Raw,
        initramfs_path: kernel.initramfs.clone(),
        initramfs_size,
        cmdline: kernel.cmdline.clone(),
    });

    Ok(())
}

/// Share a host directory into the guest over virtio-fs. A `/dev/root`-tagged share
/// becomes the guest root (paired with `rootfstype=virtiofs` on the cmdline).
fn add_fs_share(vmr: &mut VmResources, share: &FsShare) -> Result<()> {
    let path = share
        .path
        .to_str()
        .with_context(|| format!("share path is not valid UTF-8: {:?}", share.path))?
        .to_string();

    vmr.add_fs_device(FsDeviceConfig {
        fs_id: share.tag.clone(),
        shared_dir: Some(path),
        shm_size: None,
        read_only: share.read_only,
        virtual_entries: Vec::new(),
    });

    Ok(())
}

/// Wire a vsock device so the guest agent can reach the host. `listen = false` means
/// the host listens on `socket_path` and the guest connects to `CID_HOST(2):port`;
/// libkrun bridges the two.
fn add_vsock(vmr: &mut VmResources, vsock: &VsockSpec) -> Result<()> {
    let mut unix_ipc_port_map = std::collections::HashMap::new();
    unix_ipc_port_map.insert(vsock.port, (vsock.socket_path.clone(), false));

    vmr.set_vsock_device(VsockDeviceConfig {
        vsock_id: "vsock0".to_string(),
        guest_cid: GUEST_CID,
        host_port_map: None,
        unix_ipc_port_map: Some(unix_ipc_port_map),
        tsi_flags: devices::virtio::TsiFlags::empty(),
    })
    .map_err(|e| anyhow!("set_vsock_device: {e:?}"))?;

    Ok(())
}

fn add_disk(vmr: &mut VmResources, disk: &DiskSpec) -> Result<()> {
    let path = disk
        .path
        .to_str()
        .with_context(|| format!("disk path is not valid UTF-8: {:?}", disk.path))?
        .to_string();

    vmr.add_block_device(BlockDeviceConfig {
        block_id: disk.id.clone(),
        cache_type: CacheType::Writeback,
        disk_image_path: path,
        disk_image_format: ImageType::Raw,
        is_disk_read_only: disk.read_only,
        direct_io: false,
        sync_mode: SyncMode::Full,
    })
    .map_err(|e| anyhow!("add_block_device({}): {e:?}", disk.id))?;

    Ok(())
}

/// Build and run the microVM. Blocks indefinitely running our own event loop.
///
/// On guest power-off, libkrun's `Vmm::stop` calls `libc::exit` and tears down this
/// process — which is exactly why the VMM runs as a dedicated worker rather than
/// in-process with the UI (decision D3). So on a clean shutdown this never returns;
/// it returns `Err` only on a build/run failure.
pub fn boot(spec: &VmSpec) -> Result<()> {
    let vmr = build_resources(spec).context("building VmResources")?;

    // Guest shutdown eventfd + SIGTERM/SIGINT handlers (graceful power-off request).
    let shutdown_efd = crate::shutdown::install().context("installing shutdown handler")?;

    let mut event_manager = EventManager::new().map_err(|e| anyhow!("EventManager::new: {e:?}"))?;
    // The worker channel carries virtio-gpu blob-mapping messages (macOS): the GPU
    // device asks the VMM to map host GPU memory into guest space. We service it on a
    // dedicated thread below, exactly as `krun_start_enter` does (required once the GPU
    // device exists; harmless otherwise).
    let (worker_tx, worker_rx) = unbounded();

    let boot = match &spec.boot {
        BootSource::Firmware(_) => "EFI firmware",
        BootSource::Kernel(_) => "direct kernel",
    };
    log::info!(
        "building microVM: {} vcpu(s), {} MiB, {} disk(s), {} share(s), display={}, boot={boot}",
        spec.cpus,
        spec.ram_mib,
        spec.disks.len(),
        spec.shares.len(),
        spec.display.is_some(),
    );
    let vmm = vmm::builder::build_microvm(&vmr, &mut event_manager, Some(shutdown_efd), worker_tx)
        .map_err(|e| anyhow!("build_microvm: {e:?}"))?;

    // Start the GPU worker-message servicer when a display is attached (mirrors
    // krun_start_enter's `if gpu_virgl_flags.is_some()`). Without it, a guest blob map
    // would block the GPU worker forever waiting on a reply.
    if spec.display.is_some() {
        vmm::worker::start_worker_thread(vmm.clone(), worker_rx)
            .map_err(|e| anyhow!("start_worker_thread: {e:?}"))?;
    }
    log::info!("microVM running; entering event loop (SIGTERM/SIGINT → guest power-off)");

    // Keep the Vmm alive for the lifetime of the event loop.
    let _vmm = vmm;
    loop {
        event_manager
            .run()
            .map_err(|e| anyhow!("event loop: {e:?}"))?;
    }
}
