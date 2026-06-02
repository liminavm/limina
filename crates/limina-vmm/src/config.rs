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
    /// Optional serial console wiring.
    pub console: Option<ConsoleSpec>,
}
