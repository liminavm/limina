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
use polly::event_manager::EventManager;
use vmm::resources::VmResources;
use vmm::vmm_config::block::BlockDeviceConfig;
use vmm::vmm_config::external_kernel::{ExternalKernel, KernelFormat};
use vmm::vmm_config::firmware::FirmwareConfig;
use vmm::vmm_config::fs::FsDeviceConfig;
use vmm::vmm_config::machine_config::VmConfig;
use vmm::vmm_config::vsock::VsockDeviceConfig;

use crate::config::{BootSource, DiskSpec, FsShare, KernelSpec, VmSpec, VsockSpec};

/// Standard libkrun guest CID.
const GUEST_CID: u32 = 3;

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

    if let Some(console) = &spec.console {
        console::attach(&mut vmr, console).context("attaching serial console")?;
    }

    Ok(vmr)
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
    // The worker channel carries gpu/virgl messages; unused until we enable the GPU.
    let (worker_tx, _worker_rx) = unbounded();

    let boot = match &spec.boot {
        BootSource::Firmware(_) => "EFI firmware",
        BootSource::Kernel(_) => "direct kernel",
    };
    log::info!(
        "building microVM: {} vcpu(s), {} MiB, {} disk(s), {} share(s), boot={boot}",
        spec.cpus,
        spec.ram_mib,
        spec.disks.len(),
        spec.shares.len()
    );
    let _vmm = vmm::builder::build_microvm(&vmr, &mut event_manager, Some(shutdown_efd), worker_tx)
        .map_err(|e| anyhow!("build_microvm: {e:?}"))?;
    log::info!("microVM running; entering event loop (SIGTERM/SIGINT → guest power-off)");

    loop {
        event_manager
            .run()
            .map_err(|e| anyhow!("event loop: {e:?}"))?;
    }
}
