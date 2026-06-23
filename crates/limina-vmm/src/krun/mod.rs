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
use devices::virtio::DisplayResizeHandle;
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

    if let Some(vc) = &spec.virtio_console {
        console::attach_virtio(&mut vmr, vc).context("attaching virtio-console")?;
    }

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
        "virtio-input: keyboard fd={}, pointer fd={}",
        input.kbd_fd,
        input.ptr_fd
    );
    vmr.input_backends
        .push(limina_input::backends::keyboard_backends(input.kbd_fd));
    vmr.input_backends
        .push(limina_input::backends::pointer_backends(input.ptr_fd));
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
        mac: NET_GUEST_MAC,
        features: NET_COMPAT_FEATURES,
    })
    .map_err(|e| anyhow!("add_network_interface(gvproxy): {e:?}"))?;
    vmr.dhcp_client = true;
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
