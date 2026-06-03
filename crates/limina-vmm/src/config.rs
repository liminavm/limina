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
///
/// Output-only is fine (the facade passes `input_fd = -1`); provide `input` — a FIFO
/// or pty — for an interactive console.
#[derive(Debug, Clone)]
pub struct ConsoleSpec {
    /// File to capture guest console output into.
    pub output: PathBuf,
    /// Optional source of guest console input.
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
}

/// Where the host sends presented guest frames.
#[derive(Debug, Clone)]
pub enum DisplaySink {
    /// Capture each frame to a PNG (latest wins) — the headless test oracle.
    CapturePng(PathBuf),
    /// Publish frames into a shared IOSurface and report it to the supervisor over this
    /// control fd (the window present path). `-1` = create the surface but send nothing.
    Window { control_fd: i32 },
}

/// virtio-input devices attached to the guest (M2). A keyboard and an absolute pointer,
/// each fed from an inherited socket the supervisor writes evdev events to. The supervisor
/// sets these up; the worker just registers the backends on the given fds.
#[derive(Debug, Clone)]
pub struct InputSpec {
    /// Read end of the keyboard event socket (worker inherits it).
    pub kbd_fd: i32,
    /// Read end of the pointer event socket (worker inherits it).
    pub ptr_fd: i32,
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
    /// Guest RAM in MiB (static; dynamic memory is a later milestone).
    pub ram_mib: usize,
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
    /// Optional virtio-gpu display (M2). None = headless (no GPU device).
    pub display: Option<DisplaySpec>,
    /// Optional virtio-input devices (M2). None = no keyboard/pointer.
    pub input: Option<InputSpec>,
}
