// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina-vmm — the entitled VMM worker process.
//!
//! Boots a Linux guest on libkrun + Hypervisor.framework via libkrun's *internal*
//! Rust API (decision D2.1 — no C ABI). For M1 it takes a resolved spec on the CLI
//! and boots to a serial console; later the limina UI spawns it with config over an fd
//! and supervises it (decision D3 — the VMM is a dedicated child process).
//!
//! The binary MUST be codesigned with `com.apple.security.hypervisor` (see `sign.sh`)
//! or `hv_vm_create` fails with `Error::VmCreate`.

mod config;
mod krun;
mod shutdown;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::config::{ConsoleSpec, DiskSpec, VmSpec};

/// Boot a Linux guest on libkrun + Hypervisor.framework.
#[derive(Parser, Debug)]
#[command(name = "limina-vmm", about, version)]
struct Cli {
    /// EFI firmware blob (EDK2 .fd) to load into guest RAM.
    #[arg(long)]
    firmware: PathBuf,

    /// Raw disk image to boot (attached as virtio-blk `vda`).
    #[arg(long)]
    disk: PathBuf,

    /// Open the disk read-only (protects the image from guest writes).
    #[arg(long)]
    read_only: bool,

    /// Number of vCPUs.
    #[arg(long, default_value_t = 4)]
    cpus: u8,

    /// Guest RAM in MiB (static; dynamic memory is a later milestone).
    #[arg(long, default_value_t = 4096)]
    ram_mib: usize,

    /// Capture the guest serial console to this file.
    #[arg(long)]
    console: Option<PathBuf>,

    /// Optional FIFO/file feeding the guest serial console input.
    #[arg(long, requires = "console")]
    console_input: Option<PathBuf>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    let cli = Cli::parse();

    let spec = VmSpec {
        cpus: cli.cpus,
        ram_mib: cli.ram_mib,
        firmware: cli.firmware,
        disks: vec![DiskSpec {
            id: "root".to_string(),
            path: cli.disk,
            read_only: cli.read_only,
        }],
        console: cli.console.map(|output| ConsoleSpec {
            output,
            input: cli.console_input,
        }),
    };

    krun::boot(&spec)
}
