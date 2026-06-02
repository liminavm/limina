// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina — the front-end / supervisor (decision D3).
//!
//! For M1 this is a CLI that resolves a VM config, spawns the entitled `limina-vmm`
//! worker as a dedicated child process, and supervises its lifecycle (graceful
//! power-off on Ctrl-C, force-kill on timeout, report when the VM stops). The
//! AppKit UI grows on top of this supervisor later.

mod supervisor;
mod window;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use crate::supervisor::WorkerSpec;

/// Run a Linux guest on limina (supervises the limina-vmm worker).
#[derive(Parser, Debug)]
#[command(name = "limina", about, version)]
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

    /// Guest RAM in MiB.
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

    /// Open a native window showing the guest display (the worker streams its scanout as
    /// a shared IOSurface). Mutually exclusive with --display-capture.
    #[arg(long, conflicts_with = "display_capture")]
    window: bool,

    /// Attach a virtio-gpu display and capture presented frames to this PNG path.
    #[arg(long)]
    display_capture: Option<PathBuf>,

    /// Display mode as WIDTHxHEIGHT (e.g. 1280x800). Used only with --display-capture.
    #[arg(long, default_value = "1280x800")]
    display_size: String,

    /// Seconds to wait for an orderly guest power-off before force-killing.
    #[arg(long, default_value_t = 20)]
    shutdown_grace_secs: u64,

    /// Path to the limina-vmm worker binary (default: sibling of this executable;
    /// override with $LIMINA_VMM_BIN).
    #[arg(long)]
    vmm_bin: Option<PathBuf>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    let cli = Cli::parse();
    let vmm_bin = resolve_vmm_bin(cli.vmm_bin).context("locating the limina-vmm worker binary")?;

    // Forward the VM options to the worker's CLI.
    let mut args: Vec<String> = vec![
        "--cpus".into(),
        cli.cpus.to_string(),
        "--ram-mib".into(),
        cli.ram_mib.to_string(),
    ];
    // Boot source (clap guarantees exactly one of firmware / kernel).
    if let Some(kernel) = &cli.kernel {
        args.push("--kernel".into());
        args.push(path_arg(kernel)?);
        if let Some(initramfs) = &cli.initramfs {
            args.push("--initramfs".into());
            args.push(path_arg(initramfs)?);
        }
        if let Some(cmdline) = &cli.cmdline {
            args.push("--cmdline".into());
            args.push(cmdline.clone());
        }
    } else if let Some(firmware) = &cli.firmware {
        args.push("--firmware".into());
        args.push(path_arg(firmware)?);
    }
    if let Some(rootfs) = &cli.rootfs {
        args.push("--rootfs".into());
        args.push(path_arg(rootfs)?);
    }
    if let Some(disk) = &cli.disk {
        args.push("--disk".into());
        args.push(path_arg(disk)?);
    }
    if cli.read_only {
        args.push("--read-only".into());
    }
    if let (Some(port), Some(socket)) = (&cli.vsock_port, &cli.vsock_socket) {
        args.push("--vsock-port".into());
        args.push(port.to_string());
        args.push("--vsock-socket".into());
        args.push(path_arg(socket)?);
    }
    if let Some(console) = &cli.console {
        args.push("--console".into());
        args.push(path_arg(console)?);
    }
    let grace = Duration::from_secs(cli.shutdown_grace_secs);

    // Windowed mode: open a native window in the supervisor and stream the guest scanout
    // from the worker over a control socketpair (the worker publishes shared IOSurfaces).
    if cli.window {
        let (width, height) = parse_display_size(&cli.display_size)?;
        return run_windowed(vmm_bin, args, grace, width, height);
    }

    if let Some(display_capture) = &cli.display_capture {
        args.push("--display-capture".into());
        args.push(path_arg(display_capture)?);
        args.push("--display-size".into());
        args.push(cli.display_size.clone());
    }

    let spec = WorkerSpec {
        vmm_bin,
        args,
        shutdown_grace: grace,
    };

    let code = supervisor::run(&spec)?;
    std::process::exit(code);
}

/// Windowed run: socketpair control channel, spawn the worker in `--display-window` mode
/// (inheriting the worker end), then run the AppKit window on the main thread while a
/// background thread monitors the worker. Never returns (exits with the worker's code).
fn run_windowed(
    vmm_bin: PathBuf,
    mut args: Vec<String>,
    grace: Duration,
    width: u32,
    height: u32,
) -> Result<()> {
    use objc2::MainThreadMarker;

    let mtm = MainThreadMarker::new().context("the window must run on the main thread")?;

    // Control channel: sup_fd stays here (CLOEXEC so the worker can't inherit it); worker_fd
    // is inherited by the worker and referenced by --control-fd.
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("socketpair: {}", std::io::Error::last_os_error());
    }
    let (sup_fd, worker_fd) = (fds[0], fds[1]);
    unsafe {
        let f = libc::fcntl(sup_fd, libc::F_GETFD);
        libc::fcntl(sup_fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
    }

    args.push("--display-window".into());
    args.push("--control-fd".into());
    args.push(worker_fd.to_string());
    args.push("--display-size".into());
    args.push(format!("{width}x{height}"));

    let spec = WorkerSpec {
        vmm_bin,
        args,
        shutdown_grace: grace,
    };
    let child = supervisor::spawn_worker(&spec, &[worker_fd])?;
    // The worker holds its own copy now; drop ours so the reader sees EOF when it exits.
    unsafe { libc::close(worker_fd) };

    let shared = window::Shared::new();
    window::spawn_reader(sup_fd, shared.clone());

    let shared_for_monitor = shared.clone();
    let monitor = std::thread::spawn(move || {
        let code = supervisor::monitor(child, grace).unwrap_or(1);
        window::mark_worker_exited(&shared_for_monitor);
        code
    });

    window::run(shared, mtm); // blocks until the worker exits
    let code = monitor.join().unwrap_or(1);
    std::process::exit(code);
}

/// Parse a `WIDTHxHEIGHT` string into `(width, height)`.
fn parse_display_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("display size must be WIDTHxHEIGHT, got {s:?}"))?;
    Ok((
        w.trim().parse().context("display width")?,
        h.trim().parse().context("display height")?,
    ))
}

/// Resolve the worker binary: explicit flag, then $LIMINA_VMM_BIN, then a sibling of
/// this executable (the common cargo `target/<profile>/` layout).
fn resolve_vmm_bin(flag: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("LIMINA_VMM_BIN") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let sibling = exe
        .parent()
        .context("executable has no parent dir")?
        .join("limina-vmm");
    anyhow::ensure!(
        sibling.exists(),
        "limina-vmm not found next to {exe:?}; pass --vmm-bin or set LIMINA_VMM_BIN"
    );
    Ok(sibling)
}

fn path_arg(p: &std::path::Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .with_context(|| format!("path is not valid UTF-8: {p:?}"))
}
