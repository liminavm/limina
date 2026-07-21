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

mod battery;
mod console;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::unbounded;
use devices::virtio::block::{CacheType, ImageType, SyncMode};
use devices::virtio::display::DisplayInfo;
use devices::virtio::{BalloonControlHandle, DisplayResizeHandle};
use limina_display::{CaptureConfig, WindowConfig};
use polly::event_manager::EventManager;
use vmm::resources::VmResources;
use vmm::vmm_config::block::BlockDeviceConfig;
use vmm::vmm_config::external_kernel::{ExternalKernel, KernelFormat};
use vmm::vmm_config::firmware::FirmwareConfig;
use vmm::vmm_config::fs::FsDeviceConfig;
use vmm::vmm_config::machine_config::VmConfig;
use vmm::vmm_config::net::NetworkInterfaceConfig;
use vmm::vmm_config::vsock::VsockDeviceConfig;

use devices::virtio::net::device::VirtioNetBackend;

use crate::config::{
    BootSource, DiskSpec, DisplaySink, DisplaySpec, FsShare, InputSpec, KernelSpec, NetSpec,
    VmSpec, VsockSpec,
};

/// Standard libkrun guest CID.
const GUEST_CID: u32 = 3;

// virtio-net feature bits (mirrors libkrun's C-API `NET_FEATURE_*` so we don't depend on
// private consts). We request the IPv4 offload set `NET_COMPAT_FEATURES` — the same baseline
// krunkit uses with gvproxy. TSO6/UFO6 are reachable but left off until verified non-corrupting
// (see roadmap M3).
const NET_FEATURE_CSUM: u32 = 1 << 0;
const NET_FEATURE_GUEST_CSUM: u32 = 1 << 1;
const NET_FEATURE_GUEST_TSO4: u32 = 1 << 7;
const NET_FEATURE_GUEST_UFO: u32 = 1 << 10;
const NET_FEATURE_HOST_TSO4: u32 = 1 << 11;
const NET_FEATURE_HOST_UFO: u32 = 1 << 14;

/// virtio-net offload features for the gvproxy NAT NIC (IPv4-only baseline).
const NET_COMPAT_FEATURES: u32 = NET_FEATURE_CSUM
    | NET_FEATURE_GUEST_CSUM
    | NET_FEATURE_GUEST_TSO4
    | NET_FEATURE_GUEST_UFO
    | NET_FEATURE_HOST_TSO4
    | NET_FEATURE_HOST_UFO;

/// Guest MAC for the NAT NIC: the well-known vfkit/krunkit MAC. gvproxy's default config
/// has a *static* DHCP lease binding this MAC to `192.168.127.2` and a built-in port-forward
/// `127.0.0.1:2222 → 192.168.127.2:22`. Using it (rather than an arbitrary locally-administered
/// MAC, which gets a dynamic .3 lease) makes the guest IP deterministic and gives inbound SSH
/// (`ssh -p 2222 user@127.0.0.1`) for free — no gvproxy REST forwarding needed. (`5a` =
/// locally-administered, unicast.)
const NET_GUEST_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];

// virglrenderer init flag bits (see docs/research/03 §1.3).
const VIRGLRENDERER_USE_EGL: u32 = 1 << 0;
#[allow(dead_code)]
const VIRGLRENDERER_THREAD_SYNC: u32 = 1 << 1;
const VIRGLRENDERER_USE_SURFACELESS: u32 = 1 << 3;
const VIRGLRENDERER_USE_GLES: u32 = 1 << 4;
const VIRGLRENDERER_VENUS: u32 = 1 << 6;
#[allow(dead_code)]
const VIRGLRENDERER_NO_VIRGL: u32 = 1 << 7;
#[allow(dead_code)]
const VIRGLRENDERER_USE_ASYNC_FENCE_CB: u32 = 1 << 8;
const VIRGLRENDERER_RENDER_SERVER: u32 = 1 << 9;

/// virgl_flags for the **coexist** virtio-gpu (tier-2, the default): venus (Vulkan) **and** vrend
/// (GL via zink-on-KosmicKrisp) on top of the unconditional software-2D scanout.
///
/// `VENUS | USE_EGL | USE_GLES | USE_SURFACELESS | THREAD_SYNC | ASYNC_FENCE_CB | RENDER_SERVER`:
/// - `VENUS` (0x40): Vulkan passthrough (enhanced/16k tier) → KosmicKrisp → Metal.
/// - `USE_EGL` (0x1) + `USE_SURFACELESS` (0x8) + `USE_GLES` (0x10): bring up vrend's own surfaceless
///   EGL winsys → **zink-on-KK** → Metal, giving accelerated **GL** for the stock-4k baseline via
///   virgl's copy model (no `hv_vm_map`, so immune to the 16k/4k page wall). GLES (not desktop GL):
///   on macOS epoxy routes desktop-GL calls to Apple's system OpenGL framework (→ glFlush crash with
///   no CGL context); GLES dlopens `libGLESv2` → resolves to our zink-on-KK Mesa. Guest GL is
///   unaffected — virglrenderer translates guest GL → host GLES. (Proven: `spikes/virgl-zink-kk`.)
///   This needs a virglrenderer built with `-Dplatforms=egl` (our `third_party/virgl-prefix`) and a
///   small patch wiring the no-GBM EGL path on macOS (`patches/virglrenderer-vrend-egl-no-gbm-macos`).
///   `NO_VIRGL` (0x80) was historically forced ON because Apple Silicon has no host GL; zink-on-KK
///   removes that constraint, so it's now OFF and vrend is enabled.
/// - `RENDER_SERVER` (0x200): our virglrenderer is built with `render-server-mode=thread`; 1.3.0
///   only initializes Venus when this is set (`proxy_renderer_init` is gated on it).
/// - `THREAD_SYNC` (0x2) + `ASYNC_FENCE_CB` (0x100): the render-server model retires venus fences
///   ASYNCHRONOUSLY (fence eventfd + sync thread → `write_context_fence` → rutabaga → guest IRQ);
///   without them the guest hangs forever in `glFinish`/`vkQueueWaitIdle`.
///
/// The software-2D path still serves all 2D/scanout commands (unconditional); venus + vrend add 3D
/// contexts on top. Two-tier safety: if renderer init fails, libkrun degrades to software-2D.
/// Override at runtime with `LIMINA_VIRGL_FLAGS` (e.g. for experiments).
const GPU_COEXIST_FLAGS: u32 = VIRGLRENDERER_VENUS
    | VIRGLRENDERER_USE_EGL
    | VIRGLRENDERER_USE_GLES
    | VIRGLRENDERER_USE_SURFACELESS
    | VIRGLRENDERER_THREAD_SYNC
    | VIRGLRENDERER_USE_ASYNC_FENCE_CB
    | VIRGLRENDERER_RENDER_SERVER;

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

    if let Some(input) = &spec.input {
        add_input(&mut vmr, input);
    }

    if let Some(net) = &spec.net {
        add_net(&mut vmr, net)?;
    }

    if let Some(console) = &spec.console {
        console::attach(&mut vmr, console).context("attaching serial console")?;
    }

    // Host battery mirror (virtio-i2c SBS battery): only when asked AND the host
    // actually has a battery to mirror (or the fake hook is set) — a desktop Mac
    // attaches nothing and the guest correctly shows no battery.
    if spec.battery {
        if let Some(provider) = battery::provider() {
            log::info!("battery: mirroring the host battery into the guest");
            vmr.battery_provider = Some(provider);
        }
    }

    if let Some(vc) = &spec.virtio_console {
        console::attach_virtio(&mut vmr, vc).context("attaching virtio-console")?;
    }

    // Native virtio-snd audio device (device ID 25). On by default; the guest's stock
    // virtio_snd driver binds it and exposes an ALSA card with no guest components.
    vmr.balloon_free_page_reporting = spec.free_page_reporting;
    vmr.balloon_deflate_on_oom = spec.deflate_on_oom;
    vmr.snd = spec.snd;
    // Mic capture is opt-in (default off, privacy); only acts when snd is also on.
    vmr.snd_capture = spec.mic;

    Ok(vmr)
}

/// Attach a virtio-gpu display at the requested mode, with the chosen host sink (PNG capture
/// oracle or shared-IOSurface window). Selects the GPU mode: the coexist device by default
/// (software-2D 2D/scanout + Venus 3D, degrading to software-2D if venus init fails), or
/// software-2D-only when forced via `--gpu-software-2d`.
fn add_display(vmr: &mut VmResources, display: &DisplaySpec) -> Result<()> {
    // Power-user escape hatch: LIMINA_VIRGL_FLAGS=0x.. forces a specific renderer flag set
    // (and thus the renderer/coexist path) without recompiling — for experiments/sweeps.
    let virgl_override = std::env::var("LIMINA_VIRGL_FLAGS").ok().and_then(|s| {
        let s = s.trim();
        s.strip_prefix("0x")
            .map(|h| u32::from_str_radix(h, 16))
            .unwrap_or_else(|| s.parse::<u32>())
            .ok()
    });
    // GPU mode (precedence: env override > --gpu-software-2d > default coexist):
    //  - coexist (DEFAULT): software-2D serves 2D/scanout, a VENUS|NO_VIRGL rutabaga adds 3D.
    //    If venus init fails, libkrun degrades to software-2D (no panic), so making this the
    //    default is safe — the enhancement is additive and self-degrading.
    //  - --gpu-software-2d: force software-2D only (the 2D capture oracle; the local-Terminal
    //    GPU-init hang, which graceful degradation can't catch because it's a block not an error).
    let (software_2d, flags) = match (virgl_override, display.software_2d) {
        (Some(f), _) => (false, f),
        (None, true) => (true, GPU_COEXIST_FLAGS), // flags unused when software_2d (no rutabaga)
        (None, false) => (false, GPU_COEXIST_FLAGS),
    };
    log::info!(
        "virtio-gpu virgl_flags = {flags:#x}, software_2d = {software_2d} (coexist = {})",
        !software_2d
    );
    vmr.set_gpu_virgl_flags(flags);
    vmr.set_gpu_software_2d(software_2d);
    vmr.displays
        .push(DisplayInfo::new(display.width, display.height));

    vmr.display_backend = Some(match &display.sink {
        DisplaySink::CapturePng(png_path) => limina_display::capture_backend(CaptureConfig {
            png_path: png_path.clone(),
        }),
        DisplaySink::Window {
            control_fd,
            surface_port_name,
        } => limina_display::window_backend(WindowConfig {
            control_fd: *control_fd,
            surface_port_name: surface_port_name.clone(),
        }),
    });

    Ok(())
}

/// Attach a virtio-keyboard and a virtio-absolute-pointer. Each reads evdev events the
/// supervisor writes (as 8-byte datagrams) to an inherited socket fd; we register the
/// native Rust backends (D2.1) on those fds. libkrun attaches the devices iff
/// `input_backends` is non-empty.
fn add_input(vmr: &mut VmResources, input: &InputSpec) {
    log::info!(
        "virtio-input: keyboard fd={}, pointer fd={}, rel-pointer fd={}",
        input.kbd_fd,
        input.ptr_fd,
        input.rel_ptr_fd
    );
    vmr.input_backends
        .push(limina_input::backends::keyboard_backends(input.kbd_fd));
    vmr.input_backends
        .push(limina_input::backends::pointer_backends(input.ptr_fd));
    // The relative-pointer (mouse) device for capture mode is optional (M8).
    if input.rel_ptr_fd >= 0 {
        vmr.input_backends
            .push(limina_input::backends::rel_pointer_backends(
                input.rel_ptr_fd,
            ));
    }
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

/// Attach the user-mode NAT NIC: a virtio-net device whose backend is a vfkit-style UNIX
/// datagram connection to a gvproxy gateway. The connection (and the `VFKT` handshake) is
/// established lazily when the guest activates the device during boot, so gvproxy only needs
/// to be listening on the socket by then — the supervisor guarantees that ordering.
///
/// `dhcp_client = true` adds `KRUN_DHCP=1` to the cmdline; that drives libkrun's *own* guest
/// init (the L1/krun-guest path). A stock Fedora guest ignores it and runs DHCP via
/// NetworkManager against gvproxy's built-in DHCP server either way.
fn add_net(vmr: &mut VmResources, net: &NetSpec) -> Result<()> {
    vmr.add_network_interface(NetworkInterfaceConfig {
        iface_id: "eth0".to_string(),
        backend: VirtioNetBackend::UnixgramPath(
            net.gvproxy_socket.clone(),
            /* vfkit magic */ true,
        ),
        mac: net.mac.unwrap_or(NET_GUEST_MAC),
        features: NET_COMPAT_FEATURES,
    })
    .map_err(|e| anyhow!("add_network_interface(gvproxy): {e:?}"))?;
    vmr.dhcp_client = true;
    Ok(())
}

/// Detect the disk image format by its magic, so a user needn't tag `--disk foo.qcow2` (M10
/// Phase 4). qcow2 files start with `"QFI\xfb"`; everything else we treat as raw — libkrun's only
/// other always-present format here. A short or unreadable file → raw (the safe default; libkrun
/// surfaces a genuine open failure later). Auto-detect supersedes the design's planned explicit
/// `:qcow2` suffix: it removes the silent-corruption footgun of opening a qcow2 as raw, and a real
/// qcow2 always carries the magic so detection is unambiguous.
fn detect_image_type(path: &std::path::Path) -> ImageType {
    use std::io::Read;
    const QCOW_MAGIC: [u8; 4] = [0x51, 0x46, 0x49, 0xfb]; // "QFI\xfb"
    let mut buf = [0u8; 4];
    match std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) if buf == QCOW_MAGIC => ImageType::Qcow2,
        _ => ImageType::Raw,
    }
}

fn add_disk(vmr: &mut VmResources, disk: &DiskSpec) -> Result<()> {
    let format = detect_image_type(&disk.path);
    let path = disk
        .path
        .to_str()
        .with_context(|| format!("disk path is not valid UTF-8: {:?}", disk.path))?
        .to_string();

    vmr.add_block_device(BlockDeviceConfig {
        block_id: disk.id.clone(),
        cache_type: CacheType::Writeback,
        disk_image_path: path,
        disk_image_format: format,
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

    // Guest suspend eventfd + SIGUSR2 handler (M9 freeze trigger, stock tier): pulses the
    // GPIO KEY_SLEEP button so a stock guest's logind enters suspend-to-idle, no agent needed.
    let suspend_efd = crate::suspend::install().context("installing suspend handler")?;

    // Guest restart eventfd + SIGHUP handler: pulses the GPIO KEY_RESTART button so a stock
    // guest's logind reboots gracefully (worker exits 125 → supervisor relaunches a fresh worker).
    let restart_efd = crate::restart::install().context("installing restart handler")?;

    // Guest wake eventfd (M9 restore): pulses the GPIO KEY_WAKEUP (wakeup-source) button to bring a
    // suspended guest out of s2idle. Injected internally on restore; SIGWINCH is an out-of-band seam.
    let wake_efd = crate::wake::install().context("installing wake handler")?;

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
    let vmm = vmm::builder::build_microvm(
        &vmr,
        &mut event_manager,
        Some(shutdown_efd),
        Some(suspend_efd),
        Some(restart_efd),
        Some(wake_efd),
        worker_tx,
        spec.restore_file.clone(),
    )
    .map_err(|e| anyhow!("build_microvm: {e:?}"))?;
    if let Some(path) = &spec.restore_file {
        log::info!("restoring from snapshot {path:?}");
    }

    // Runtime display resize: if a control socket was requested, wire it to the live virtio-gpu
    // device's resize handle. This is a dedicated UNIX socket, decoupled from the present/ack
    // control channel on purpose (see docs/design/runtime-display-resize.md).
    if let Some(path) = spec
        .display
        .as_ref()
        .and_then(|d| d.control_socket.as_ref())
    {
        match vmm.lock().unwrap().gpu_resize_handle() {
            Some(handle) => install_resize_listener(path.clone(), handle)
                .context("installing the display-resize listener")?,
            None => log::error!(
                "--display-control-socket set but the GPU resize handle is unavailable; \
                 runtime display resize is disabled"
            ),
        }
    }

    // Runtime balloon control (M6 dynamic memory): if a control socket was requested, wire it to
    // the live virtio-balloon's control handle. A dedicated UNIX socket, mirroring the display
    // resize one. libkrun stays mechanism-only; the target policy lives in the supervisor.
    if let Some(path) = spec.balloon_control_socket.as_ref() {
        match vmm.lock().unwrap().balloon_control_handle() {
            Some(handle) => install_balloon_listener(path.clone(), handle)
                .context("installing the balloon control listener")?,
            None => log::error!(
                "--balloon-control-socket set but the balloon control handle is unavailable; \
                 dynamic memory is disabled"
            ),
        }
    }

    // VM snapshot trigger (M9 suspend/resume): when a snapshot path was requested, install the
    // SIGUSR1 handler + a trigger thread. On signal it locks the Vmm, quiesces the vCPUs, writes
    // the snapshot, and exits 126 ("snapshotted") — the supervisor reports the VM suspended and,
    // unlike a reboot, does NOT relaunch. `save_snapshot` self-quiesces (the per-vCPU Snapshot
    // event saves-then-parks each vCPU), so we do NOT pause first — pausing would make the vCPUs
    // discard the Snapshot event and time out. A FAILED save is now recoverable: the parked vCPUs are
    // resumed (`resume_parked_vcpus`) and the guest woken, so the VM lives on (only if that resume
    // itself fails is it fatal, exit 1).
    if let Some(path) = spec.snapshot_file.clone() {
        let trigger = crate::snapshot::install().context("installing the snapshot trigger")?;
        let vmm_for_snapshot = vmm.clone();
        let snap_path = path.clone();
        std::thread::Builder::new()
            .name("snapshot-trigger".into())
            .spawn(move || {
                // Block until SIGUSR1 writes the trigger eventfd.
                if let Err(e) = trigger.read() {
                    log::error!("snapshot: trigger read failed: {e}; suspend disabled");
                    return;
                }
                log::info!("snapshot: SIGUSR1 received, capturing VM state to {snap_path:?}");
                let mut guard = vmm_for_snapshot.lock().unwrap();
                match guard.save_snapshot(&snap_path) {
                    Ok(()) => {
                        log::info!("snapshot: complete; exiting 126 (snapshotted)");
                        guard.stop(126); // runs exit observers, then libc::_exit — never returns
                    }
                    Err(e) => {
                        // Abort path: the vCPUs are parked (snapshot_vcpus). Resume them so the guest
                        // returns to execution instead of wedging, then inject the wake so a
                        // bracket-suspended guest comes back out of s2idle. Non-fatal — the VM lives on
                        // and the supervisor just sees the suspend request fail.
                        log::error!("snapshot: save failed: {e:?}; resuming the guest (abort, non-fatal)");
                        match guard.resume_parked_vcpus() {
                            Ok(()) => {
                                drop(guard);
                                crate::wake::pulse();
                                log::warn!(
                                    "snapshot: aborted and guest resumed; suspend request failed"
                                );
                            }
                            Err(re) => {
                                log::error!(
                                    "snapshot: resume after failed save also failed: {re:?}; the guest \
                                     is wedged — exiting 1"
                                );
                                guard.stop(1);
                            }
                        }
                    }
                }
            })
            .map_err(|e| anyhow!("spawning the snapshot trigger thread: {e}"))?;

        // M9.2 suspend bracket (the PRODUCTION suspend path). On SIGTSTP: pulse the guest suspend
        // button, wait for the guest to s2idle-quiesce (every virtio device reset to INIT — the
        // `is_quiesced` oracle), then snapshot + exit 126. Unlike the raw SIGUSR1 path above it
        // NEVER snapshots a non-quiesced guest, so a restore always resumes cleanly. If the guest
        // never quiesces within the timeout (e.g. a virtiofs mount refuses s2idle), the bracket
        // wakes it back out of suspend and keeps the VM running — the supervisor sees no exit-126
        // and reports the suspend failed. See `crate::bracket` and docs/design/m9.2-*.md.
        let bracket_path = path.clone();
        let trigger =
            crate::bracket::install().context("installing the suspend-bracket trigger")?;
        let vmm_for_bracket = vmm.clone();
        std::thread::Builder::new()
            .name("suspend-bracket".into())
            .spawn(move || loop {
                if let Err(e) = trigger.read() {
                    log::error!("bracket: trigger read failed: {e}; suspend bracket disabled");
                    return;
                }
                // LIMINA_BRACKET_NO_BUTTON: the caller already arranged the guest-side suspend
                // (e.g. the L2 test schedules `systemctl suspend` in-guest because a fresh seated
                // session can swallow the button). Pulsing the button TOO means the guest gets two
                // suspend requests; the one that lands after userspace freezes replays on resume
                // and re-suspends the restored guest into an unwakeable sleep (run-11 MMIO trace:
                // thaw renegotiates every device, then everything freezes back to 0).
                if std::env::var_os("LIMINA_BRACKET_NO_BUTTON").is_some() {
                    log::info!("bracket: SIGTSTP received; button pulse suppressed (env), waiting for guest-initiated quiesce");
                } else {
                    log::info!("bracket: SIGTSTP received; pulsing the guest suspend button");
                    crate::suspend::pulse();
                }

                // Poll the quiesce oracle until every virtio device has reset to INIT, or give up.
                const POLL: std::time::Duration = std::time::Duration::from_millis(250);
                const QUIESCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
                let deadline = std::time::Instant::now() + QUIESCE_TIMEOUT;
                let quiesced = loop {
                    if vmm_for_bracket.lock().unwrap().is_quiesced() {
                        break true;
                    }
                    if std::time::Instant::now() >= deadline {
                        break false;
                    }
                    std::thread::sleep(POLL);
                };

                if !quiesced {
                    let holdouts = vmm_for_bracket.lock().unwrap().quiesce_holdouts();
                    log::warn!(
                        "bracket: guest did not quiesce within {QUIESCE_TIMEOUT:?} \
                         (holdouts: {holdouts:?}); waking it and aborting the suspend (VM lives on)"
                    );
                    crate::wake::pulse();
                    continue; // re-arm: the next SIGTSTP gets a fresh bracket
                }

                log::info!("bracket: guest quiesced; capturing snapshot to {bracket_path:?}");
                let mut guard = vmm_for_bracket.lock().unwrap();
                match guard.save_snapshot(&bracket_path) {
                    Ok(()) => {
                        log::info!("bracket: snapshot complete; exiting 126 (suspended)");
                        guard.stop(126); // exit observers, then libc::_exit — never returns
                    }
                    Err(e) => {
                        // Same abort as the raw path: the vCPUs were parked by save_snapshot; resume
                        // them and wake the guest so the VM lives on. Only a failed resume is fatal.
                        log::error!(
                            "bracket: save failed: {e:?}; resuming the guest (abort, non-fatal)"
                        );
                        match guard.resume_parked_vcpus() {
                            Ok(()) => {
                                drop(guard);
                                crate::wake::pulse();
                                log::warn!(
                                    "bracket: aborted and guest resumed; suspend request failed"
                                );
                            }
                            Err(re) => {
                                log::error!(
                                    "bracket: resume after failed save also failed: {re:?}; the \
                                     guest is wedged — exiting 1"
                                );
                                guard.stop(1);
                            }
                        }
                    }
                }
            })
            .map_err(|e| anyhow!("spawning the suspend-bracket thread: {e}"))?;
    }

    // Host-sleep integration (docs/design/host-sleep-s2idle.md §4): s2idle the guest around
    // host sleep so its own thaw restores the wall clock (0088 RTC) and its apps see an honest
    // suspend — and wake it on host wake, but only if WE put it to sleep. Works for any guest
    // with zero guest components; independent of the snapshot machinery above.
    if spec.host_sleep_s2idle {
        crate::power::start(vmm.clone());
    }

    // Start the GPU worker-message servicer when a display is attached (mirrors
    // krun_start_enter's `if gpu_virgl_flags.is_some()`). Without it, a guest blob map
    // would block the GPU worker forever waiting on a reply.
    if spec.display.is_some() {
        vmm::worker::start_worker_thread(vmm.clone(), worker_rx)
            .map_err(|e| anyhow!("start_worker_thread: {e:?}"))?;
    }
    log::info!("microVM running; entering event loop (SIGTERM/SIGINT → guest power-off)");

    // M9 restore: a suspended guest was snapshotted parked in s2idle. Now that its vCPUs are live
    // behind the restored GIC + PL061 registers, inject the wake (KEY_WAKEUP) so it runs its own
    // s2idle resume path and re-initialises its virtio devices (the 0058 thaw ladder). The wake_efd
    // is level-triggered, so the first event-loop iteration below delivers it to the GPIO device;
    // the raised SPI is latched pending in the restored GIC, so it wakes the vCPU regardless of
    // exact timing. Harmless if the snapshot was of a running (non-suspended) guest.
    if spec.restore_file.is_some() {
        log::info!("restore: injecting guest wake (KEY_WAKEUP) to resume from s2idle");
        crate::wake::pulse();
    }

    // Keep the Vmm alive for the lifetime of the event loop.
    let _vmm = vmm;
    loop {
        event_manager
            .run()
            .map_err(|e| anyhow!("event loop: {e:?}"))?;
    }
}

/// Bind a UNIX-socket listener that applies newline-delimited `resize <w> <h>` commands to the
/// live GPU via `handle` (display 0). Each accepted connection is read to EOF — the supervisor
/// keeps one long-lived connection; the test harness connects per call. Runs on a detached
/// thread for the VMM's lifetime. See docs/design/runtime-display-resize.md.
fn install_resize_listener(path: std::path::PathBuf, handle: DisplayResizeHandle) -> Result<()> {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    // A stale socket from a previous run (or a relaunched worker) blocks bind(); clear it first.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind display-control socket {path:?}"))?;
    log::info!("display-resize: listening on {path:?}");
    std::thread::Builder::new()
        .name("display-resize".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("display-resize: accept failed: {e}");
                        continue;
                    }
                };
                for line in std::io::BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    match parse_resize(&line) {
                        Some((w, h)) => {
                            log::info!("display-resize: request {w}x{h}");
                            handle.request(0, w, h);
                        }
                        None => log::warn!("display-resize: ignoring control line {line:?}"),
                    }
                }
            }
        })
        .context("spawning the display-resize listener thread")?;
    Ok(())
}

/// Parse a `resize <w> <h>` control line into `(width, height)`.
fn parse_resize(line: &str) -> Option<(u32, u32)> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "resize" {
        return None;
    }
    let w = parts.next()?.parse::<u32>().ok()?;
    let h = parts.next()?.parse::<u32>().ok()?;
    Some((w, h))
}

/// Bind a UNIX-socket listener that drives the live virtio-balloon via `handle` (M6 dynamic
/// memory). Newline-delimited commands:
/// - `target <bytes>` → set the balloon target (rounded to 4 KiB pages); the guest inflates/deflates.
/// - `stats`          → reply `target=<bytes> actual=<bytes> reclaimed=<bytes>` (the last commanded
///   target, the guest's self-reported balloon size, and total reclaimed) on the same connection.
///
/// Runs on a detached thread for the VMM's lifetime; the supervisor policy and the test harness
/// connect to it. Mirrors [`install_resize_listener`].
fn install_balloon_listener(path: std::path::PathBuf, handle: BalloonControlHandle) -> Result<()> {
    use std::os::unix::net::UnixListener;

    // A stale socket from a previous run (or a relaunched worker) blocks bind(); clear it first.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind balloon-control socket {path:?}"))?;
    log::info!("balloon: control socket listening on {path:?}");
    // The last commanded target (bytes), shared across connections so `stats` can report it.
    // Targets only ever arrive through this socket, so this is authoritative.
    let target_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    std::thread::Builder::new()
        .name("balloon-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("balloon: accept failed: {e}");
                        continue;
                    }
                };
                // One thread per connection: the supervisor's policy holds a long-lived connection,
                // so a single-threaded accept loop would block all other clients (e.g. a `stats`
                // query) behind it. Each connection is independent and cheap.
                let handle = handle.clone();
                let target_bytes = target_bytes.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("balloon-conn".into())
                    .spawn(move || serve_balloon_conn(stream, handle, target_bytes))
                {
                    log::error!("balloon: cannot spawn connection thread: {e}");
                }
            }
        })
        .context("spawning the balloon control listener thread")?;
    Ok(())
}

/// Serve one balloon control connection: `target <bytes>` drives the live balloon, `stats` replies
/// with `target=<bytes> actual=<bytes> reclaimed=<bytes>` (the commanded target vs the guest's
/// self-reported size — a gap between the two is the guest failing/refusing to inflate). Reads
/// until EOF.
fn serve_balloon_conn(
    stream: std::os::unix::net::UnixStream,
    handle: BalloonControlHandle,
    target_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use std::io::{BufRead, Write};
    use std::sync::atomic::Ordering;
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            log::error!("balloon: failed to clone control stream: {e}");
            return;
        }
    };
    for line in std::io::BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("target") => match parts.next().and_then(|s| s.parse::<u64>().ok()) {
                Some(bytes) => {
                    let pages = (bytes >> 12).min(u32::MAX as u64) as u32;
                    log::info!("balloon: target {bytes} bytes ({pages} pages)");
                    target_bytes.store(bytes, Ordering::Relaxed);
                    handle.set_target_pages(pages);
                }
                None => log::warn!("balloon: ignoring malformed target line {line:?}"),
            },
            Some("stats") => {
                let stats = handle.get_stats();
                let actual_bytes = (stats.actual_pages as u64) << 12;
                if let Err(e) = writeln!(
                    writer,
                    "target={} actual={actual_bytes} reclaimed={}",
                    target_bytes.load(Ordering::Relaxed),
                    stats.reclaimed_bytes
                )
                .and_then(|()| writer.flush())
                {
                    log::warn!("balloon: failed to reply to stats: {e}");
                    break;
                }
            }
            _ => log::warn!("balloon: ignoring control line {line:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{detect_image_type, ImageType};

    #[test]
    fn detect_image_type_by_magic() {
        let dir = std::env::temp_dir().join(format!("limina-fmttest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let qcow = dir.join("x.qcow2");
        let raw = dir.join("x.raw");
        let tiny = dir.join("tiny");
        std::fs::write(&qcow, [0x51, 0x46, 0x49, 0xfb, 0, 0]).unwrap(); // "QFI\xfb" + payload
        std::fs::write(&raw, [0u8; 16]).unwrap(); // zeros (a fresh sparse raw)
        std::fs::write(&tiny, [0x51, 0x46]).unwrap(); // <4 bytes — short read

        assert!(matches!(detect_image_type(&qcow), ImageType::Qcow2));
        assert!(matches!(detect_image_type(&raw), ImageType::Raw));
        assert!(matches!(detect_image_type(&tiny), ImageType::Raw));
        // A missing file falls back to raw (libkrun surfaces the real open error).
        assert!(matches!(
            detect_image_type(&dir.join("nope")),
            ImageType::Raw
        ));

        std::fs::remove_dir_all(&dir).ok();
    }
}
