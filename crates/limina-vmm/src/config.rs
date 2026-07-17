// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina's own resolved VM spec — the typed input to the [`crate::krun`] facade.
//!
//! This is deliberately *limina's* vocabulary, not libkrun's. For M1 it's built from
//! CLI args; later it comes from the serde VM-config schema resolved by the limina UI
//! and handed to this worker. Keeping it separate from `VmResources` is what lets the
//! facade absorb libkrun-internal API churn in one place (decision D2.1).

use std::path::PathBuf;

/// A disk to attach to the guest. Presented as virtio-blk (`vdaN`, in order).
#[derive(Debug, Clone)]
pub struct DiskSpec {
    /// Stable identifier for the block device.
    pub id: String,
    /// Host path to the raw disk image.
    pub path: PathBuf,
    /// Open the image read-only (protects it from guest writes).
    pub read_only: bool,
}

/// Where the guest's serial console is wired.
#[derive(Debug, Clone)]
pub enum ConsoleSpec {
    /// Capture guest console output to a file. Output-only is fine (the facade passes
    /// `input_fd = -1`); provide `input` — a FIFO — for a (non-interactive) input source.
    File {
        /// File to capture guest console output into.
        output: PathBuf,
        /// Optional source of guest console input.
        input: Option<PathBuf>,
    },
    /// Allocate a pseudo-terminal and wire the guest serial to it, for an interactive
    /// console. The slave device path is printed at startup (attach with `screen <path>`).
    Pty,
}

/// A virtio-console (`hvc0`) wired bidirectionally — a robust interactive data console.
///
/// This is the higher-throughput, queue-based channel for bulk console data. The PL011
/// serial (`ttyAMA0`) is *also* a working bidirectional tty now (via [`ConsoleSpec::File`],
/// the `arm,primecell` FDT node, and the HVF halfword-MMIO fix); it stays the
/// firmware/EFI/early-boot console and a serial debug shell, while hvc0 is the post-boot
/// data path. With `console=hvc0` on the cmdline this device becomes the guest's
/// `/dev/console`, carrying both kernel log and an interactive shell.
#[derive(Debug, Clone)]
pub struct VirtioConsoleSpec {
    /// File to capture guest hvc0 output into.
    pub output: PathBuf,
    /// Optional FIFO feeding guest hvc0 input (`None` = output-only).
    pub input: Option<PathBuf>,
}

/// Direct kernel boot: a raw aarch64 kernel `Image` loaded straight into guest RAM,
/// with our own initramfs and command line. No bootloader, no ESP. This is the L1 test
/// path (and the basis for our enhanced/custom-kernel tier).
#[derive(Debug, Clone)]
pub struct KernelSpec {
    /// Raw aarch64 kernel `Image` (libkrun `KernelFormat::Raw`).
    pub image: PathBuf,
    /// Optional initramfs (cpio) loaded alongside the kernel.
    pub initramfs: Option<PathBuf>,
    /// Optional kernel command line (e.g. `console=ttyAMA0`); a default is used if None.
    pub cmdline: Option<String>,
}

/// A host directory shared into the guest over virtio-fs.
///
/// The tag is the virtiofs mount tag. The special tag `/dev/root` makes this share the
/// guest's **root filesystem** (with `rootfstype=virtiofs` on the cmdline) — which is
/// how the L1 guest boots: no disk image, the rootfs is just a host directory.
#[derive(Debug, Clone)]
pub struct FsShare {
    /// virtiofs tag; `/dev/root` = root filesystem.
    pub tag: String,
    /// Host directory to share.
    pub path: PathBuf,
    /// Share read-only.
    pub read_only: bool,
}

/// A vsock channel between host and guest.
///
/// libkrun bridges the guest vsock port to a host UNIX socket. With our wiring the
/// **host listens** on `socket_path` and the **guest connects** to `CID_HOST(2):port`
/// — used by the L1 guest agent for structured, in-guest test assertions.
#[derive(Debug, Clone)]
pub struct VsockSpec {
    /// Guest vsock port the agent connects to (on CID_HOST).
    pub port: u32,
    /// Host UNIX socket path the host side listens on.
    pub socket_path: PathBuf,
}

/// A virtio-gpu display attached to the guest. `width`/`height` set the advertised mode
/// (EDID). The host sink is one of: PNG capture (the headless oracle) or a shared
/// IOSurface published to the supervisor window over a control fd. Tier-1 (software 2D)
/// today; the Tier-2 zero-copy path slots in behind the same device.
#[derive(Debug, Clone)]
pub struct DisplaySpec {
    /// Advertised display width in pixels.
    pub width: u32,
    /// Advertised display height in pixels.
    pub height: u32,
    /// The host sink for presented frames.
    pub sink: DisplaySink,
    /// Force the software-2D-only GPU (no virglrenderer/venus init). Default is the
    /// coexist device (software-2D 2D + Venus 3D, degrading to software-2D if venus init
    /// fails). Set this for the headless 2D capture oracle and to dodge the local-Terminal
    /// GPU-init hang. `LIMINA_VIRGL_FLAGS` overrides both (forces a specific renderer flag set).
    pub software_2d: bool,
    /// Optional UNIX-socket path for runtime display-resize requests. When set, the worker
    /// binds a listener there and applies `resize <w> <h>` lines (newline-delimited) to the
    /// live virtio-gpu via the libkrun [`DisplayResizeHandle`] — the guest then re-modesets.
    /// The supervisor's window-resize gesture and the test harness both connect here. This is
    /// decoupled from the present/ack channel on purpose. See docs/design/runtime-display-resize.md.
    pub control_socket: Option<PathBuf>,
}

/// Where the host sends presented guest frames.
#[derive(Debug, Clone)]
pub enum DisplaySink {
    /// Capture each frame to a PNG (latest wins) — the headless test oracle.
    CapturePng(PathBuf),
    /// Publish frames into a shared IOSurface and report it to the supervisor over this
    /// control fd (the window present path). `-1` = create the surface but send nothing.
    Window {
        control_fd: i32,
        /// Bootstrap name of the supervisor's surface-port receiver. When set, scanout/cursor
        /// IOSurfaces are NON-global and handed over by Mach port (so strangers can't
        /// `IOSurfaceLookup` the guest screen). `None` ⇒ legacy global surfaces.
        surface_port_name: Option<String>,
    },
}

/// A user-mode NAT network interface (M3) backed by a **gvproxy** gateway over a
/// vfkit-style UNIX *datagram* socket. gvproxy provides DHCP, DNS and the NAT to the
/// host network entirely in userspace — no root, no entitlement.
///
/// The gateway process is spawned and supervised by the limina supervisor (it must be
/// listening on `gvproxy_socket` before the guest's virtio-net activates); the worker
/// just connects libkrun's virtio-net backend to that socket (sending the `VFKT` magic).
/// Presented to the guest as `eth0`.
#[derive(Debug, Clone)]
pub struct NetSpec {
    /// Path to gvproxy's vfkit unixgram socket (`gvproxy -listen-vfkit unixgram://<path>`).
    pub gvproxy_socket: PathBuf,
    /// Guest MAC for the NIC. `None` = the well-known vfkit MAC (gvproxy's default static
    /// .2 lease); managed VMs pass their persistent per-VM MAC (the supervisor then rebinds
    /// gvproxy's lease to match).
    pub mac: Option<[u8; 6]>,
}

/// virtio-input devices attached to the guest (M2). A keyboard, an absolute pointer, and —
/// optionally — a relative pointer (mouse) for capture mode (M8). Each is fed from an inherited
/// socket the supervisor writes evdev events to. The supervisor sets these up; the worker just
/// registers the backends on the given fds.
#[derive(Debug, Clone)]
pub struct InputSpec {
    /// Read end of the keyboard event socket (worker inherits it).
    pub kbd_fd: i32,
    /// Read end of the absolute-pointer event socket (worker inherits it).
    pub ptr_fd: i32,
    /// Read end of the relative-pointer (mouse) event socket for capture mode, or `-1` if no
    /// relative device is attached (e.g. headless display-capture runs).
    pub rel_ptr_fd: i32,
}

/// How the guest boots.
#[derive(Debug, Clone)]
pub enum BootSource {
    /// EFI firmware blob (EDK2 `.fd`) loaded into guest RAM; the guest's own
    /// bootloader/kernel then boots off a disk's ESP. The stock-baseline path.
    Firmware(PathBuf),
    /// Direct kernel boot — the fast L1 path and the custom-kernel tier.
    Kernel(KernelSpec),
}

/// A fully-resolved VM specification.
#[derive(Debug, Clone)]
pub struct VmSpec {
    /// Number of vCPUs.
    pub cpus: u8,
    /// Guest RAM in MiB. With dynamic memory (M6) this is the **max** — what libkrun allocates and
    /// the guest sees; the supervisor's balloon policy shrinks effective RAM toward its min via the
    /// control socket (the worker is mechanism-only and doesn't know the min).
    pub ram_mib: usize,
    /// Where the worker binds the balloon control socket (newline `target <bytes>` / `stats`),
    /// driven by the supervisor's dynamic-memory policy (M6). `None` = no runtime balloon control.
    pub balloon_control_socket: Option<PathBuf>,
    /// How the guest boots (EFI firmware or a direct kernel).
    pub boot: BootSource,
    /// Disks to attach, in order.
    pub disks: Vec<DiskSpec>,
    /// virtio-fs shares (a `/dev/root`-tagged share becomes the root filesystem).
    pub shares: Vec<FsShare>,
    /// Optional vsock channel (host<->guest agent).
    pub vsock: Option<VsockSpec>,
    /// Optional serial console wiring.
    pub console: Option<ConsoleSpec>,
    /// Optional virtio-console (`hvc0`) for a robust interactive bidirectional console.
    pub virtio_console: Option<VirtioConsoleSpec>,
    /// Optional virtio-gpu display (M2). None = headless (no GPU device).
    pub display: Option<DisplaySpec>,
    /// Optional virtio-input devices (M2). None = no keyboard/pointer.
    pub input: Option<InputSpec>,
    /// Optional user-mode NAT NIC (M3). None = no network.
    pub net: Option<NetSpec>,
    /// Mirror the host battery into the guest (virtio-i2c SBS battery). Even when
    /// true the device only attaches if the host actually has a battery (or
    /// `LIMINA_BATTERY_FAKE` is set) — desktops correctly show none.
    pub battery: bool,
    /// Attach the native virtio-snd audio device (device ID 25) driving host audio.
    /// On by default; the guest's stock virtio_snd driver binds it (no guest components).
    pub snd: bool,
    /// Advertise the mic-capture input stream on the virtio-snd device. Opt-in and
    /// default-off for privacy (unlike playback); only meaningful when `snd` is also on.
    pub mic: bool,
}
