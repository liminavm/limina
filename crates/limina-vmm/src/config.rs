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

/// A fully-resolved VM specification. M1 boot path: EFI firmware + raw disk.
#[derive(Debug, Clone)]
pub struct VmSpec {
    /// Number of vCPUs.
    pub cpus: u8,
    /// Guest RAM in MiB (static; dynamic memory is a later milestone).
    pub ram_mib: usize,
    /// EFI firmware blob (EDK2 `.fd`) loaded into guest RAM; the guest's own
    /// bootloader/kernel then boots off the disk's ESP.
    pub firmware: PathBuf,
    /// Disks to attach, in order.
    pub disks: Vec<DiskSpec>,
    /// Optional serial console wiring.
    pub console: Option<ConsoleSpec>,
}
