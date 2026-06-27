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
    BootSource, ConsoleSpec, DiskSpec, DisplaySink, DisplaySpec, FsShare, InputSpec, KernelSpec,
    NetSpec, VirtioConsoleSpec, VmSpec, VsockSpec,
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

    /// Share a host directory into the guest over virtio-fs as `tag=path` (repeatable).
    /// The guest mounts it with `mount -t virtiofs <tag> <dir>`; tags prefixed `limina-`
    /// are auto-mounted under /media by the guest agent. Append `:ro` for read-only.
    #[arg(long = "share", value_name = "TAG=PATH[:ro]")]
    share: Vec<String>,

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

    /// Wire the guest serial to an interactive pseudo-terminal instead of a file; the
    /// slave device path is printed at startup (attach with `screen <path>`).
    #[arg(long, conflicts_with_all = ["console", "console_input"])]
    console_pty: bool,

    /// Capture the guest virtio-console (`hvc0`) output to this file. Pair with
    /// `console=hvc0` on the cmdline to make hvc0 the guest's interactive `/dev/console`
    /// (the robust bidirectional path; PL011 stays the firmware/early-boot console).
    #[arg(long)]
    virtio_console: Option<PathBuf>,

    /// Optional FIFO/file feeding the guest virtio-console (`hvc0`) input.
    #[arg(long, requires = "virtio_console")]
    virtio_console_input: Option<PathBuf>,

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

    /// Bootstrap name of the supervisor's surface-port receiver. When set, scanout/cursor
    /// IOSurfaces are NON-global and handed to the supervisor by Mach port (so strangers can't
    /// `IOSurfaceLookup` the guest screen). Omitted ⇒ legacy global surfaces.
    #[arg(long, requires = "display_window")]
    surface_port_name: Option<String>,

    /// Display mode as WIDTHxHEIGHT (e.g. 1280x800). Used with the display flags.
    #[arg(long, default_value = "1280x800")]
    display_size: String,

    /// UNIX-socket path for runtime display-resize requests. The worker binds a listener
    /// here and applies newline-delimited `resize <w> <h>` commands to the live virtio-gpu
    /// (the guest re-modesets). The supervisor window and the test harness connect to it.
    #[arg(long)]
    display_control_socket: Option<PathBuf>,

    /// Force the software-2D-only GPU (no virglrenderer/venus). Default is the coexist
    /// device (software-2D 2D + Venus 3D). Use for the capture oracle or the local-Terminal
    /// GPU-init hang.
    #[arg(long)]
    gpu_software_2d: bool,

    /// UNIX-socket path for runtime balloon control (M6). The worker binds a listener here and
    /// applies newline-delimited `target <bytes>` commands to the live virtio-balloon (replying to
    /// `stats` with `actual=<bytes> reclaimed=<bytes>`). The supervisor policy and the test harness
    /// connect to it.
    #[arg(long)]
    balloon_control_socket: Option<PathBuf>,

    /// Attach a user-mode NAT NIC (`eth0`) connected to a gvproxy gateway listening on
    /// this vfkit unixgram socket. The supervisor spawns gvproxy and guarantees it is up
    /// before the guest activates the device.
    #[arg(long)]
    net_gvproxy: Option<PathBuf>,

    /// fd of the keyboard event socket (supervisor→worker). Enables the virtio-keyboard.
    /// Requires --input-ptr-fd (the pointer comes as a pair).
    #[arg(long, requires = "input_ptr_fd")]
    input_kbd_fd: Option<i32>,

    /// fd of the pointer event socket (supervisor→worker). Enables the virtio-pointer.
    #[arg(long, requires = "input_kbd_fd")]
    input_ptr_fd: Option<i32>,

    /// fd of the relative-pointer (mouse) event socket (supervisor→worker). Enables the
    /// virtio-mouse used in pointer-capture mode (M8). Optional; requires the pointer pair.
    #[arg(long, requires = "input_ptr_fd")]
    input_rel_ptr_fd: Option<i32>,
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

/// Parse a `--share TAG=PATH[:ro]` spec. The `=` split is first-match so the path may
/// contain `=`; the `:ro` suffix is only treated as a flag when it is exactly `:ro`.
fn parse_share(spec: &str) -> Result<FsShare> {
    let (tag, rest) = spec
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--share must be TAG=PATH[:ro], got {spec:?}"))?;
    anyhow::ensure!(!tag.is_empty(), "--share has an empty tag: {spec:?}");
    let (path, read_only) = match rest.strip_suffix(":ro") {
        Some(p) => (p, true),
        None => (rest, false),
    };
    anyhow::ensure!(!path.is_empty(), "--share has an empty path: {spec:?}");
    Ok(FsShare {
        tag: tag.to_string(),
        path: PathBuf::from(path),
        read_only,
    })
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

    let mut shares: Vec<FsShare> = cli
        .rootfs
        .map(|path| FsShare {
            tag: "/dev/root".to_string(),
            path,
            read_only: false,
        })
        .into_iter()
        .collect();
    for spec in &cli.share {
        shares.push(parse_share(spec)?);
    }

    let vsock = match (cli.vsock_port, cli.vsock_socket) {
        (Some(port), Some(socket_path)) => Some(VsockSpec { port, socket_path }),
        _ => None,
    };

    let display = {
        let sink = if cli.display_window {
            let control_fd = cli.control_fd.unwrap_or(-1);
            if control_fd >= 0 {
                // #8 leg 2: the GPU device reads the supervisor's "shown <id>" acks off
                // the control socketpair (same fd; the display backend only writes).
                // The fd number is process-wide — env is just the in-process rendezvous.
                std::env::set_var("LIMINA_SHOWN_ACK_FD", control_fd.to_string());
            }
            // The venus zero-copy scanouts are created deep inside virglrenderer (in-process),
            // which can't see our CLI args — hand it the receiver name via the environment so it
            // creates its IOSurfaces non-global and publishes their Mach ports too (the sw2d path
            // uses the WindowConfig below). Set before any GPU/renderer init.
            if let Some(name) = cli.surface_port_name.as_deref() {
                std::env::set_var("LIMINA_SURFACE_PORT_NAME", name);
            }
            Some(DisplaySink::Window {
                control_fd,
                surface_port_name: cli.surface_port_name.clone(),
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
                    software_2d: cli.gpu_software_2d,
                    control_socket: cli.display_control_socket,
                })
            }
            None => None,
        }
    };

    let input = match (cli.input_kbd_fd, cli.input_ptr_fd) {
        (Some(kbd_fd), Some(ptr_fd)) => Some(InputSpec {
            kbd_fd,
            ptr_fd,
            rel_ptr_fd: cli.input_rel_ptr_fd.unwrap_or(-1),
        }),
        _ => None,
    };

    let net = cli
        .net_gvproxy
        .map(|gvproxy_socket| NetSpec { gvproxy_socket });

    let spec = VmSpec {
        cpus: cli.cpus,
        ram_mib: cli.ram_mib,
        balloon_control_socket: cli.balloon_control_socket,
        boot,
        disks,
        shares,
        vsock,
        console: if cli.console_pty {
            Some(ConsoleSpec::Pty)
        } else {
            cli.console.map(|output| ConsoleSpec::File {
                output,
                input: cli.console_input,
            })
        },
        virtio_console: cli.virtio_console.map(|output| VirtioConsoleSpec {
            output,
            input: cli.virtio_console_input,
        }),
        display,
        input,
        net,
    };

    krun::boot(&spec)
}
