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

use crate::config::{
    BootSource, ConsoleSpec, DiskSpec, DisplaySink, DisplaySpec, FsShare, KernelSpec, VmSpec,
    VsockSpec,
};

/// Boot a Linux guest on libkrun + Hypervisor.framework.
#[derive(Parser, Debug)]
#[command(name = "limina-vmm", about, version)]
struct Cli {
    /// EFI firmware blob (EDK2 .fd) to load into guest RAM. The stock-baseline boot
    /// path; mutually exclusive with --kernel.
    #[arg(long, conflicts_with = "kernel", required_unless_present = "kernel")]
    firmware: Option<PathBuf>,

    /// Raw aarch64 kernel Image for direct kernel boot (the fast L1 path).
    #[arg(long)]
    kernel: Option<PathBuf>,

    /// Initramfs (cpio) for direct kernel boot.
    #[arg(long, requires = "kernel")]
    initramfs: Option<PathBuf>,

    /// Kernel command line for direct kernel boot.
    #[arg(long, requires = "kernel")]
    cmdline: Option<String>,

    /// Host directory to serve as the guest root over virtio-fs (tag `/dev/root`).
    /// Pair with `--cmdline "... rootfstype=virtiofs rw init=/init"`. The L1 path.
    #[arg(long)]
    rootfs: Option<PathBuf>,

    /// Raw disk image to attach as virtio-blk `vda` (optional for direct kernel boot).
    #[arg(long)]
    disk: Option<PathBuf>,

    /// Open the disk read-only (protects the image from guest writes).
    #[arg(long)]
    read_only: bool,

    /// Number of vCPUs.
    #[arg(long, default_value_t = 4)]
    cpus: u8,

    /// Guest RAM in MiB (static; dynamic memory is a later milestone).
    #[arg(long, default_value_t = 4096)]
    ram_mib: usize,

    /// Guest vsock port for the guest agent (host listens on --vsock-socket).
    #[arg(long, requires = "vsock_socket")]
    vsock_port: Option<u32>,

    /// Host UNIX socket path the host side listens on for the guest agent.
    #[arg(long, requires = "vsock_port")]
    vsock_socket: Option<PathBuf>,

    /// Capture the guest serial console to this file.
    #[arg(long)]
    console: Option<PathBuf>,

    /// Optional FIFO/file feeding the guest serial console input.
    #[arg(long, requires = "console")]
    console_input: Option<PathBuf>,

    /// Attach a virtio-gpu display and capture presented frames to this PNG path
    /// (the headless display oracle). Mutually exclusive with --display-window.
    #[arg(long, conflicts_with = "display_window")]
    display_capture: Option<PathBuf>,

    /// Attach a virtio-gpu display and publish frames as a shared IOSurface to the
    /// supervisor window. Pair with --control-fd for the worker→supervisor channel.
    #[arg(long)]
    display_window: bool,

    /// fd of the worker→supervisor control channel (used with --display-window).
    #[arg(long, requires = "display_window")]
    control_fd: Option<i32>,

    /// Display mode as WIDTHxHEIGHT (e.g. 1280x800). Used with the display flags.
    #[arg(long, default_value = "1280x800")]
    display_size: String,
}

/// Parse a `WIDTHxHEIGHT` display mode string into `(width, height)`.
fn parse_display_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("display size must be WIDTHxHEIGHT, got {s:?}"))?;
    let width = w
        .trim()
        .parse::<u32>()
        .map_err(|e| anyhow::anyhow!("invalid display width {w:?}: {e}"))?;
    let height = h
        .trim()
        .parse::<u32>()
        .map_err(|e| anyhow::anyhow!("invalid display height {h:?}: {e}"))?;
    Ok((width, height))
}

fn main() -> Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    let cli = Cli::parse();

    // clap guarantees exactly one of --firmware / --kernel is present.
    let boot = match cli.kernel {
        Some(image) => BootSource::Kernel(KernelSpec {
            image,
            initramfs: cli.initramfs,
            cmdline: cli.cmdline,
        }),
        None => BootSource::Firmware(cli.firmware.expect("clap requires firmware or kernel")),
    };

    let disks = cli
        .disk
        .map(|path| DiskSpec {
            id: "root".to_string(),
            path,
            read_only: cli.read_only,
        })
        .into_iter()
        .collect();

    let shares = cli
        .rootfs
        .map(|path| FsShare {
            tag: "/dev/root".to_string(),
            path,
            read_only: false,
        })
        .into_iter()
        .collect();

    let vsock = match (cli.vsock_port, cli.vsock_socket) {
        (Some(port), Some(socket_path)) => Some(VsockSpec { port, socket_path }),
        _ => None,
    };

    let display = {
        let sink = if cli.display_window {
            Some(DisplaySink::Window {
                control_fd: cli.control_fd.unwrap_or(-1),
            })
        } else {
            cli.display_capture.map(DisplaySink::CapturePng)
        };
        match sink {
            Some(sink) => {
                let (width, height) = parse_display_size(&cli.display_size)?;
                Some(DisplaySpec {
                    width,
                    height,
                    sink,
                })
            }
            None => None,
        }
    };

    let spec = VmSpec {
        cpus: cli.cpus,
        ram_mib: cli.ram_mib,
        boot,
        disks,
        shares,
        vsock,
        console: cli.console.map(|output| ConsoleSpec {
            output,
            input: cli.console_input,
        }),
        display,
    };

    krun::boot(&spec)
}
