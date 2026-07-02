// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina — the front-end / supervisor (decision D3).
//!
//! For M1 this is a CLI that resolves a VM config, spawns the entitled `limina-vmm`
//! worker as a dedicated child process, and supervises its lifecycle (graceful
//! power-off on Ctrl-C, force-kill on timeout, report when the VM stops). The
//! AppKit UI grows on top of this supervisor later.

mod balloon_policy;
mod clipboard;
mod control;
mod gateway;
mod session;
mod supervisor;
mod venus_env;
mod window;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use crate::supervisor::WorkerSpec;

/// Run a Linux guest on limina (supervises the limina-vmm worker).
#[derive(Parser, Debug)]
#[command(name = "limina", about, version)]
struct Cli {
    /// EFI firmware blob (EDK2 .fd) to load into guest RAM. The stock-baseline boot
    /// path; mutually exclusive with --kernel. Optional under --window: windowed boots
    /// default to the GOP firmware so EFI/GRUB/early-kernel render in the window (see
    /// `resolve_windowed_firmware`). A headless boot still needs --firmware or --kernel.
    #[arg(long, conflicts_with = "kernel")]
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

    /// Attach a disk to the guest as virtio-blk (repeatable): `PATH[:ro][:create=SIZE]`. The
    /// first `--disk` is the boot disk (`vda`), the next `vdb`, and so on — attach order is
    /// device order. `:ro` opens it read-only; `:create=SIZE` (e.g. `50G`, `512M`) creates a
    /// blank sparse raw image at PATH if it doesn't already exist, then attaches it. Optional
    /// for a direct kernel boot.
    #[arg(long, value_name = "PATH[:ro][:create=SIZE]")]
    disk: Vec<String>,

    /// Open the FIRST `--disk` read-only (back-compat alias; prefer a per-disk `:ro` suffix).
    #[arg(long)]
    read_only: bool,

    /// Attach a read-only ISO / CD-ROM image (repeatable). Sugar for `--disk PATH:ro`, appended
    /// after the data disks; the guest sees a read-only `/dev/vdX` (mount it with `mount -o ro`).
    #[arg(long, value_name = "PATH")]
    cdrom: Vec<PathBuf>,

    /// Share a host directory into the guest (repeatable): `[NAME=]PATH[:ro]`. NAME
    /// defaults to the directory's basename; the share is tagged `limina-NAME` over
    /// virtio-fs and the guest agent auto-mounts it at /media/NAME (a guest without
    /// the agent can `mount -t virtiofs limina-NAME <dir>` by hand).
    #[arg(long = "share", value_name = "[NAME=]PATH[:ro]")]
    share: Vec<String>,

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

    /// Bind the supervisor-owned control plane at this path instead of the private
    /// default (lets a test harness join the plane as a peer). Unlike --vsock-*, the
    /// supervisor still serves the protocol itself.
    #[arg(long, conflicts_with = "vsock_socket")]
    control_socket: Option<PathBuf>,

    /// Capture the guest serial console to this file.
    #[arg(long)]
    console: Option<PathBuf>,

    /// Optional FIFO/file feeding the guest serial console input (pairs with --console).
    #[arg(long, requires = "console")]
    console_input: Option<PathBuf>,

    /// Wire the guest serial to an interactive pseudo-terminal instead of a file; the
    /// worker prints the slave device path to attach with `screen <path>`.
    #[arg(long, conflicts_with = "console")]
    console_pty: bool,

    /// Capture the guest virtio-console (`hvc0`) output to this file — the robust
    /// bidirectional console. Pair with `console=hvc0` on the cmdline and
    /// --virtio-console-input to make hvc0 the guest's interactive `/dev/console`.
    #[arg(long)]
    virtio_console: Option<PathBuf>,

    /// Optional FIFO/file feeding the guest virtio-console (`hvc0`) input.
    #[arg(long, requires = "virtio_console")]
    virtio_console_input: Option<PathBuf>,

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

    /// UNIX-socket path for runtime display-resize requests, forwarded to the worker. The
    /// window-resize gesture and the test harness connect here to reflow the guest resolution.
    #[arg(long)]
    display_control_socket: Option<PathBuf>,

    /// Dynamic memory range as `MIN..MAX` (M6), e.g. `2G..12G` or `2048..12288` (bare = MiB).
    /// `MAX` is the RAM libkrun allocates (overrides --ram-mib); the balloon policy shrinks
    /// effective guest RAM toward `MIN` under low pressure and grows it back toward `MAX` under
    /// load. Implies a balloon control socket. Omitted ⇒ static `--ram-mib`.
    #[arg(long)]
    memory: Option<String>,

    /// UNIX-socket path for runtime balloon control (M6), forwarded to the worker. The
    /// dynamic-memory policy and the test harness connect here to drive the target / read `stats`.
    /// Auto-allocated when `--memory` is given.
    #[arg(long)]
    balloon_control_socket: Option<PathBuf>,

    /// Force the software-2D-only GPU (no virglrenderer/venus). Default is the coexist device
    /// (software-2D 2D + Venus 3D). Use for the capture oracle or the local-Terminal GPU-init hang.
    #[arg(long)]
    gpu_software_2d: bool,

    /// Attach a user-mode NAT NIC: spawn and supervise a gvproxy gateway (DHCP/DNS/NAT,
    /// no root) and connect the guest's virtio-net to it. The guest gets an IP and outbound
    /// internet automatically (e.g. for SSH).
    #[arg(long)]
    net: bool,

    /// Capture the gvproxy gateway's `-debug` packet log to this file (DHCP/DNS/NAT — the
    /// host-side network oracle). Requires --net; without it gvproxy logs quietly.
    #[arg(long, requires = "net")]
    net_log: Option<PathBuf>,

    /// Host port for inbound SSH to the guest (gvproxy's built-in `127.0.0.1:<port> → guest:22`
    /// forward). When omitted, the first free port from 2222 up is auto-allocated, so two or more
    /// VMs run in parallel without colliding — the resolved port is logged at startup. Pass an
    /// explicit value to pin it (errors if that port is busy). Requires --net; must be 1024–65535.
    #[arg(long, requires = "net")]
    ssh_port: Option<u16>,

    /// Seconds to wait for an orderly guest power-off before force-killing.
    #[arg(long, default_value_t = 20)]
    shutdown_grace_secs: u64,

    /// Path to the limina-vmm worker binary (default: sibling of this executable;
    /// override with $LIMINA_VMM_BIN).
    #[arg(long)]
    vmm_bin: Option<PathBuf>,

    /// Swap the Command and Option keys for the guest: Command acts as Alt, Option acts as
    /// Meta/Super (the common ask for PC-style muscle memory). This is the **default**; the flag
    /// is kept for back-compat / explicitness and to override an earlier --no-swap-cmd-opt.
    /// Host-side keymap policy; the guest still owns the keyboard layout. Only meaningful with
    /// --window.
    #[arg(long, overrides_with = "no_swap_cmd_opt")]
    swap_cmd_opt: bool,

    /// Keep the Mac-native Command/Option mapping instead of the default swap (Command stays
    /// Meta/Super, Option stays Alt). The opt-out for --swap-cmd-opt; if both appear the last one
    /// on the command line wins.
    #[arg(long, overrides_with = "swap_cmd_opt")]
    no_swap_cmd_opt: bool,
}

impl Cli {
    /// Effective Command/Option swap policy. Swap is **on by default** (PC-style muscle memory);
    /// `--no-swap-cmd-opt` opts out. `--swap-cmd-opt` and `--no-swap-cmd-opt` override each other
    /// (last-wins, via clap `overrides_with`), so this OR is exact for every combination and
    /// reads both fields (no dead-code on the back-compat flag).
    fn swap_cmd_opt_enabled(&self) -> bool {
        self.swap_cmd_opt || !self.no_swap_cmd_opt
    }
}

fn main() -> Result<()> {
    // Death-pact reaper mode (hidden, spawned by the gateway alongside gvproxy). Handled BEFORE
    // clap/logging/AppKit so it stays a tiny process that only watches a pipe and reaps gvproxy
    // when this supervisor dies. See gateway::spawn_death_pact_watcher / run_reaper.
    if std::env::args().nth(1).as_deref() == Some(gateway::REAP_GATEWAY_ARG) {
        gateway::run_reaper();
    }

    // Default to warn-and-up so a production run is quiet; RUST_LOG overrides it (RUST_LOG=info
    // restores the lifecycle log). User-facing output (e.g. the SSH-forward hint below) is
    // printed directly, not via the logger, so it survives the default level.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = Cli::parse();
    run_vm(cli)
}

/// Boot and supervise one VM described by a fully-resolved `Cli` — the flat-flag
/// invocation today, or (later) one synthesized from a managed VM definition. On
/// success it never returns to the caller: the windowed path runs the AppKit loop
/// forever and the headless path ends in `process::exit`.
fn run_vm(cli: Cli) -> Result<()> {
    // Resolve the Command/Option swap policy up front (default ON; --no-swap-cmd-opt opts out)
    // before any field of `cli` is moved out below, so the windowed path can use it freely.
    let swap_cmd_opt = cli.swap_cmd_opt_enabled();
    let vmm_bin = resolve_vmm_bin(cli.vmm_bin).context("locating the limina-vmm worker binary")?;

    // Dynamic memory (M6): --memory MIN..MAX overrides --ram-mib with MAX (what libkrun allocates)
    // and keeps MIN for the balloon policy. The balloon control socket is auto-allocated below.
    let mem_range = cli.memory.as_deref().map(parse_memory_range).transpose()?;
    let ram_mib = mem_range.map(|(_, max)| max).unwrap_or(cli.ram_mib);

    // Balloon control socket (M6): explicit flag wins; else auto-allocate when --memory is given.
    let balloon_socket = cli.balloon_control_socket.clone().or_else(|| {
        mem_range.is_some().then(|| {
            std::env::temp_dir().join(format!("limina-balloon-{}.sock", std::process::id()))
        })
    });

    // The PSI autoballoon policy runs in the supervisor when a range + socket both exist: it
    // consumes guest MemPressure over the control plane and drives the balloon target.
    let balloon_policy = match (mem_range, balloon_socket.clone()) {
        (Some((min, max)), Some(sock)) => {
            log::info!("dynamic memory: {min}..{max} MiB (balloon policy shrinks toward {min})");
            Some(balloon_policy::BalloonPolicy::new(
                min as u32 * balloon_policy::PAGES_PER_MIB,
                max as u32 * balloon_policy::PAGES_PER_MIB,
                sock,
            ))
        }
        _ => None,
    };

    // Forward the VM options to the worker's CLI.
    let mut args: Vec<String> = vec![
        "--cpus".into(),
        cli.cpus.to_string(),
        "--ram-mib".into(),
        ram_mib.to_string(),
    ];
    // Boot source. clap guarantees firmware and kernel don't both appear; we resolve the
    // rest: an explicit --firmware is honored, and a windowed boot with neither given
    // defaults to the GOP firmware so EFI/GRUB/early-kernel render in the window (M2.5
    // Phase 3). A headless boot still needs an explicit --firmware or --kernel.
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
    } else {
        let firmware = match &cli.firmware {
            Some(f) => f.clone(),
            None => {
                anyhow::ensure!(
                    cli.window,
                    "one of --firmware or --kernel is required (a default firmware is only \
                     resolved for windowed boots)"
                );
                let exe_dir = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
                    .unwrap_or_else(|| PathBuf::from("."));
                let env_override = std::env::var_os("LIMINA_GOP_FIRMWARE").map(PathBuf::from);
                let (path, is_gop) =
                    resolve_windowed_firmware(&exe_dir, env_override, |p| p.exists()).context(
                        "no --firmware given and no default firmware found; pass --firmware or \
                         build the GOP firmware with `GOP=1 scripts/build-krun-efi.sh`",
                    )?;
                if is_gop {
                    log::info!(
                        "windowed boot: using GOP firmware {} (EFI/GRUB render in the window)",
                        path.display()
                    );
                } else {
                    log::warn!(
                        "windowed boot: GOP firmware not found; using silent firmware {} \
                         (serial-only boot console — build the GOP firmware with \
                         `GOP=1 scripts/build-krun-efi.sh` for a graphical boot console)",
                        path.display()
                    );
                }
                path
            }
        };
        args.push("--firmware".into());
        args.push(path_arg(&firmware)?);
    }
    if let Some(rootfs) = &cli.rootfs {
        args.push("--rootfs".into());
        args.push(path_arg(rootfs)?);
    }
    // Disks (M10): repeatable `--disk PATH[:ro][:create=SIZE]` + `--cdrom PATH`. See build_disk_args.
    args.extend(build_disk_args(&cli.disk, cli.read_only, &cli.cdrom)?);
    {
        let mut tags = std::collections::HashSet::new();
        for spec in &cli.share {
            let share = parse_share(spec)?;
            anyhow::ensure!(
                tags.insert(share.tag.clone()),
                "duplicate share name (tag {}): pass NAME=PATH to disambiguate",
                share.tag
            );
            args.push("--share".into());
            args.push(format!(
                "{}={}{}",
                share.tag,
                share
                    .path
                    .to_str()
                    .with_context(|| format!("share path is not valid UTF-8: {:?}", share.path))?,
                if share.read_only { ":ro" } else { "" }
            ));
        }
    }
    // Guest-agent control plane (M5/D8). Two modes:
    //  - explicit --vsock-* given (the test harness driving the protocol itself): pass the
    //    raw plumbing through and do NOT run our own host side;
    //  - otherwise (the product default): the supervisor owns the channel — bind a private
    //    control socket, wire the worker's vsock device at the well-known CONTROL_PORT,
    //    and serve HELLO/WELCOME/heartbeats. Guests without an agent simply never connect;
    //    guests with one get orderly SHUTDOWN on window-close/SIGTERM.
    let control = if let (Some(port), Some(socket)) = (&cli.vsock_port, &cli.vsock_socket) {
        args.push("--vsock-port".into());
        args.push(port.to_string());
        args.push("--vsock-socket".into());
        args.push(path_arg(socket)?);
        None
    } else {
        let socket = cli.control_socket.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("limina-ctrl-{}.sock", std::process::id()))
        });
        match control::ControlPlane::start(&socket, balloon_policy) {
            Ok(cp) => {
                args.push("--vsock-port".into());
                args.push(limina_proto::CONTROL_PORT.to_string());
                args.push("--vsock-socket".into());
                args.push(path_arg(&socket)?);
                Some(cp)
            }
            Err(e) => {
                // The VM is fully usable without the control plane; degrade, don't die.
                log::warn!("control plane disabled: {e:#}");
                None
            }
        }
    };
    if let Some(console) = &cli.console {
        args.push("--console".into());
        args.push(path_arg(console)?);
    }
    if let Some(console_input) = &cli.console_input {
        args.push("--console-input".into());
        args.push(path_arg(console_input)?);
    }
    if let Some(virtio_console) = &cli.virtio_console {
        args.push("--virtio-console".into());
        args.push(path_arg(virtio_console)?);
    }
    if let Some(virtio_console_input) = &cli.virtio_console_input {
        args.push("--virtio-console-input".into());
        args.push(path_arg(virtio_console_input)?);
    }
    if cli.console_pty {
        args.push("--console-pty".into());
    }
    let grace = Duration::from_secs(cli.shutdown_grace_secs);

    // GPU mode applies to both the windowed and capture display paths (worker default is the
    // coexist device; this forwards the software-2D-only override).
    if cli.gpu_software_2d {
        args.push("--gpu-software-2d".into());
    }

    // Runtime display-resize control socket: forwarded to the worker (which binds it). Used by
    // the windowed resize gesture and any external controller (e.g. the test harness). For a
    // windowed boot we auto-allocate one if not given, so dragging the window Just Works.
    let resize_socket = cli.display_control_socket.clone().or_else(|| {
        cli.window.then(|| {
            std::env::temp_dir().join(format!("limina-resize-{}.sock", std::process::id()))
        })
    });
    if let Some(path) = &resize_socket {
        args.push("--display-control-socket".into());
        args.push(path_arg(path)?);
    }

    // Runtime balloon control socket (M6): forwarded to the worker (which binds it). The path was
    // resolved above (explicit flag, else auto-allocated under --memory) and shared with the policy.
    if let Some(path) = &balloon_socket {
        args.push("--balloon-control-socket".into());
        args.push(path_arg(path)?);
    }

    // User-mode NAT: spawn + supervise a gvproxy gateway and connect the guest NIC to it.
    // Kept alive for the VM's lifetime; cleaned up on both exit paths (Drop here, the global
    // gateway::cleanup() in the windowed timer's process::exit).
    let gateway = if cli.net {
        // Validate only an *explicit* port; omitting --ssh-port auto-allocates a free one (so two
        // VMs can run in parallel without the user hand-picking non-colliding ports).
        if let Some(p) = cli.ssh_port {
            anyhow::ensure!(
                p >= gateway::SSH_PORT_MIN,
                "--ssh-port must be between {} and 65535 (gvproxy's range), got {p}",
                gateway::SSH_PORT_MIN
            );
        }
        let gw = gateway::start(cli.net_log.as_deref(), cli.ssh_port)
            .context("starting the gvproxy NAT gateway")?;
        // Surface the resolved port — with auto-allocation the user can't know it in advance.
        // User-facing output: print directly so it shows at the default warn level.
        println!(
            "guest SSH forward ready: ssh -p {} <user>@127.0.0.1",
            gw.ssh_port()
        );
        args.push("--net-gvproxy".into());
        args.push(path_arg(gw.socket_path())?);
        Some(gw)
    } else {
        None
    };

    // Windowed mode: open a native window in the supervisor and stream the guest scanout
    // from the worker over a control socketpair (the worker publishes shared IOSurfaces).
    if cli.window {
        let (width, height) = parse_display_size(&cli.display_size)?;
        return session::run_windowed(session::SessionConfig {
            vmm_bin,
            base_args: args,
            grace,
            width,
            height,
            gateway,
            control,
            resize_socket,
            remap: limina_input::keymap::KeyRemap { swap_cmd_opt },
        });
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

    let code = supervisor::run(&spec, control.as_ref(), gateway.as_ref())?;
    // Explicit: process::exit skips destructors, so tear the gateway + control socket
    // down before exiting.
    drop(gateway);
    control::cleanup();
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

/// krunkit's serial-only EDK2 blob — the degraded fallback when no GOP firmware is found.
/// A guest booted on it still works; only the pre-DRM boot console (firmware/GRUB/early
/// kernel) is invisible in the window (it goes to serial instead).
const SILENT_FIRMWARE: &str = "/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd";

/// Candidate GOP-firmware locations for a windowed boot, in priority order:
///   1. `env_override` (`$LIMINA_GOP_FIRMWARE`) — explicit operator choice;
///   2. the app bundle's Resources (`limina.app/Contents/MacOS/limina` → `../Resources/…`)
///      — the productized location once we bundle the firmware;
///   3. the in-repo dev build artifact (`target/<profile>/limina` → `../krun-efi/…`),
///      produced by `scripts/build-krun-efi.sh`.
///
/// Pure (no I/O) so the ordering is unit-testable; existence is checked by the caller.
fn windowed_firmware_candidates(exe_dir: &Path, env_override: Option<PathBuf>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = env_override {
        v.push(p);
    }
    v.push(exe_dir.join("../Resources/KRUN_EFI.gop.fd"));
    v.push(exe_dir.join("../krun-efi/KRUN_EFI.gop.fd"));
    v
}

/// Resolve the firmware for a windowed boot when the user gave no explicit `--firmware`.
/// Prefers the GOP (graphical) firmware so the full boot renders in the window; degrades
/// to the silent firmware if none is found. Returns `(path, is_gop)`, or `None` if not
/// even the silent firmware exists. `exists` is injected so the selection is unit-testable.
fn resolve_windowed_firmware(
    exe_dir: &Path,
    env_override: Option<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Option<(PathBuf, bool)> {
    for p in windowed_firmware_candidates(exe_dir, env_override) {
        if exists(&p) {
            return Some((p, true));
        }
    }
    let silent = PathBuf::from(SILENT_FIRMWARE);
    exists(&silent).then_some((silent, false))
}

fn path_arg(p: &std::path::Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .with_context(|| format!("path is not valid UTF-8: {p:?}"))
}

/// Parse a `MIN..MAX` dynamic-memory range (M6) into `(min_mib, max_mib)`. Each bound is a size with
/// an optional `G`/`M` suffix (case-insensitive); a bare number is MiB. e.g. `2G..12G`, `2048..12288`.
fn parse_memory_range(s: &str) -> Result<(usize, usize)> {
    let (min_s, max_s) = s
        .split_once("..")
        .with_context(|| format!("--memory must be MIN..MAX (e.g. 2G..12G), got {s:?}"))?;
    let min = parse_size_mib(min_s)?;
    let max = parse_size_mib(max_s)?;
    anyhow::ensure!(
        min > 0 && min <= max,
        "--memory MIN ({min} MiB) must be > 0 and <= MAX ({max} MiB)"
    );
    Ok((min, max))
}

/// Parse a memory size into MiB. Optional `G`/`M` suffix (case-insensitive); a bare number is MiB.
fn parse_size_mib(s: &str) -> Result<usize> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix(['G', 'g']) {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1)
    } else {
        (s, 1)
    };
    let v: usize = num
        .trim()
        .parse()
        .with_context(|| format!("invalid memory size {s:?}"))?;
    Ok(v * mult)
}

/// A parsed `--share [NAME=]PATH[:ro]` spec, normalized to the worker's `tag=path` form.
struct ShareSpec {
    tag: String,
    path: PathBuf,
    read_only: bool,
}

/// Parse a `--share [NAME=]PATH[:ro]` spec. NAME defaults to the directory basename; the
/// virtiofs tag is `limina-NAME` (the prefix the guest agent auto-mounts under /media).
/// virtio-fs tags are capped at 36 bytes on the wire, hence the length check.
fn parse_share(spec: &str) -> Result<ShareSpec> {
    let (name, rest) = match spec.split_once('=') {
        Some((n, r)) => (Some(n.to_string()), r),
        None => (None, spec),
    };
    let (path_str, read_only) = match rest.strip_suffix(":ro") {
        Some(p) => (p, true),
        None => (rest, false),
    };
    anyhow::ensure!(!path_str.is_empty(), "--share has an empty path: {spec:?}");
    let path = PathBuf::from(path_str);
    anyhow::ensure!(path.is_dir(), "share path is not a directory: {path:?}");

    let name = match name {
        Some(n) => n,
        None => path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("cannot derive a share name from {path:?}; pass NAME=PATH"))?
            .to_string(),
    };
    // The name doubles as the guest mount-point basename and rides the virtiofs tag:
    // keep it filesystem- and tag-safe.
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    anyhow::ensure!(
        !name.is_empty(),
        "share name is empty after sanitizing: {spec:?}"
    );
    let tag = format!("limina-{name}");
    anyhow::ensure!(
        tag.len() <= 36,
        "share name too long ({} bytes; virtiofs tags are capped at 36): {tag}",
        tag.len()
    );
    Ok(ShareSpec {
        tag,
        path,
        read_only,
    })
}

/// A parsed `--disk PATH[:ro][:create=SIZE]` option.
#[derive(Debug, PartialEq, Eq)]
struct DiskOpt {
    path: PathBuf,
    read_only: bool,
    /// When set, create a blank sparse raw of this many bytes at `path` if it's absent.
    create: Option<u64>,
}

/// Parse `--disk PATH[:ro][:create=SIZE]`. Recognized `:`-suffixes (`:ro`, `:create=SIZE`) are
/// stripped from the right in any order; everything left is the path, so interior colons in a
/// path are fine. The one un-escapable edge (shared with `--share`): a path that literally ends
/// in `:ro` is read as the flag. `SIZE` accepts a `K`/`M`/`G`/`T` binary suffix (bare = bytes).
fn parse_disk(spec: &str) -> Result<DiskOpt> {
    let mut rest = spec;
    let mut read_only = false;
    let mut create = None;
    loop {
        let Some(colon) = rest.rfind(':') else { break };
        let token = &rest[colon + 1..];
        if token == "ro" {
            anyhow::ensure!(!read_only, "--disk {spec:?}: ':ro' given twice");
            read_only = true;
        } else if let Some(size) = token.strip_prefix("create=") {
            anyhow::ensure!(create.is_none(), "--disk {spec:?}: ':create=' given twice");
            create =
                Some(parse_disk_size(size).with_context(|| format!("--disk {spec:?}: bad size"))?);
        } else {
            break;
        }
        rest = &rest[..colon];
    }
    anyhow::ensure!(!rest.is_empty(), "--disk has an empty path: {spec:?}");
    Ok(DiskOpt {
        path: PathBuf::from(rest),
        read_only,
        create,
    })
}

/// Build the `--disk PATH[:ro]` args to forward to the worker, in declared order: the data disks
/// (`--read-only` folds into the first), then each `--cdrom` as a read-only disk appended after
/// them. The first disk becomes the boot disk (`vda`/block id `"root"`), the rest `vdb`, `vdc`, …
/// in order. Does the host-side work: `:create=SIZE` makes a blank sparse image, every path is
/// validated (regular file or block device), and a path attached twice — across disks *and* cdroms
/// — is rejected (canonicalized, since a writable image attached twice to one VM corrupts it).
fn build_disk_args(disks: &[String], read_only: bool, cdroms: &[PathBuf]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut check_unique = |path: &Path| -> Result<()> {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        anyhow::ensure!(
            seen.insert(key),
            "disk path attached more than once: {path:?}"
        );
        Ok(())
    };
    for (i, spec) in disks.iter().enumerate() {
        let mut disk = parse_disk(spec)?;
        if i == 0 && read_only {
            disk.read_only = true;
        }
        if let Some(size) = disk.create {
            create_disk_image(&disk.path, size)?;
        }
        validate_disk_path(&disk.path)?;
        check_unique(&disk.path)?;
        out.push("--disk".into());
        out.push(format!(
            "{}{}",
            path_arg(&disk.path)?,
            if disk.read_only { ":ro" } else { "" }
        ));
    }
    // `--cdrom PATH` is sugar for a read-only `--disk`, appended after the data disks. No
    // `:create`/`:ro` suffix parsing — it's always an existing, read-only image.
    for iso in cdroms {
        validate_disk_path(iso)?;
        check_unique(iso)?;
        out.push("--disk".into());
        out.push(format!("{}:ro", path_arg(iso)?));
    }
    Ok(out)
}

/// Parse a disk size into bytes. `50G`/`512M`/`1T`/`100K` (binary, ×1024); bare = bytes.
fn parse_disk_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult): (&str, u64) = if let Some(n) = s.strip_suffix(['T', 't']) {
        (n, 1024 * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['G', 'g']) {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n, 1024)
    } else {
        (s, 1)
    };
    let v: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid disk size {s:?}"))?;
    anyhow::ensure!(v > 0, "disk size must be greater than zero: {s:?}");
    v.checked_mul(mult)
        .with_context(|| format!("disk size overflows u64: {s:?}"))
}

/// Create a blank sparse raw image of `size` bytes at `path` if it doesn't exist. Idempotent
/// when the file is already present at exactly that size; refuses to touch a file of a
/// different size (never resizes/clobbers an existing disk).
fn create_disk_image(path: &Path, size: u64) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        anyhow::ensure!(
            meta.is_file(),
            "--disk :create target exists and is not a regular file: {path:?}"
        );
        anyhow::ensure!(
            meta.len() == size,
            "--disk :create target {path:?} already exists at {} bytes (requested {size}); \
             refusing to resize — remove it or drop :create to attach it as-is",
            meta.len()
        );
        return Ok(());
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating disk image {path:?}"))?;
    f.set_len(size) // sparse: allocates no blocks until written
        .with_context(|| format!("sizing new disk image {path:?} to {size} bytes"))?;
    Ok(())
}

/// Validate a `--disk` backing path: it must exist and be a regular file or a block device.
/// Distinguishes "not found" (likely a typo, or wanted `:create`) from "permission denied".
fn validate_disk_path(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::metadata(path) {
        Ok(m) => {
            anyhow::ensure!(
                m.is_file() || m.file_type().is_block_device(),
                "--disk path is not a regular file or block device: {path:?}"
            );
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow::anyhow!(
            "--disk path not found: {path:?} (pass :create=SIZE to make a new disk)"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(anyhow::anyhow!(
            "--disk path not accessible (permission denied): {path:?}"
        )),
        Err(e) => Err(e).with_context(|| format!("stat --disk path {path:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn memory_range_parses_suffixes_and_bare_mib() {
        assert_eq!(parse_memory_range("2G..12G").unwrap(), (2048, 12288));
        assert_eq!(parse_memory_range("512M..4096").unwrap(), (512, 4096));
        assert_eq!(parse_memory_range("2048..12288").unwrap(), (2048, 12288));
        assert_eq!(parse_memory_range(" 1g .. 2g ").unwrap(), (1024, 2048));
        // min must be > 0 and <= max
        assert!(parse_memory_range("0..4G").is_err());
        assert!(parse_memory_range("8G..2G").is_err());
        // malformed
        assert!(parse_memory_range("4G").is_err());
        assert!(parse_memory_range("xx..2G").is_err());
    }

    #[test]
    fn firmware_candidates_are_env_then_bundle_then_dev() {
        let exe = Path::new("/app/Contents/MacOS");
        let c = windowed_firmware_candidates(exe, Some(PathBuf::from("/custom/fw.fd")));
        assert_eq!(c[0], PathBuf::from("/custom/fw.fd"));
        assert_eq!(c[1], exe.join("../Resources/KRUN_EFI.gop.fd"));
        assert_eq!(c[2], exe.join("../krun-efi/KRUN_EFI.gop.fd"));
        // Without an env override the bundle path leads.
        let c = windowed_firmware_candidates(exe, None);
        assert_eq!(c[0], exe.join("../Resources/KRUN_EFI.gop.fd"));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn resolves_dev_gop_artifact_when_present() {
        let exe = Path::new("/repo/target/debug");
        let dev = exe.join("../krun-efi/KRUN_EFI.gop.fd");
        let present: HashSet<PathBuf> = [dev.clone()].into_iter().collect();
        let (p, is_gop) =
            resolve_windowed_firmware(exe, None, |q| present.contains(q)).expect("resolves");
        assert_eq!(p, dev);
        assert!(is_gop, "the GOP artifact must be reported as GOP");
    }

    #[test]
    fn bundled_gop_wins_over_dev_artifact() {
        let exe = Path::new("/app/Contents/MacOS");
        let bundle = exe.join("../Resources/KRUN_EFI.gop.fd");
        let dev = exe.join("../krun-efi/KRUN_EFI.gop.fd");
        let present: HashSet<PathBuf> = [bundle.clone(), dev].into_iter().collect();
        let (p, _) =
            resolve_windowed_firmware(exe, None, |q| present.contains(q)).expect("resolves");
        assert_eq!(p, bundle);
    }

    #[test]
    fn env_override_wins_over_everything() {
        let exe = Path::new("/repo/target/debug");
        let custom = PathBuf::from("/custom/fw.fd");
        let dev = exe.join("../krun-efi/KRUN_EFI.gop.fd");
        let present: HashSet<PathBuf> = [custom.clone(), dev].into_iter().collect();
        let (p, is_gop) =
            resolve_windowed_firmware(exe, Some(custom.clone()), |q| present.contains(q))
                .expect("resolves");
        assert_eq!(p, custom);
        assert!(is_gop);
    }

    #[test]
    fn falls_back_to_silent_when_no_gop_present() {
        let exe = Path::new("/repo/target/debug");
        let silent = PathBuf::from(SILENT_FIRMWARE);
        let present: HashSet<PathBuf> = [silent.clone()].into_iter().collect();
        let (p, is_gop) =
            resolve_windowed_firmware(exe, None, |q| present.contains(q)).expect("resolves");
        assert_eq!(p, silent);
        assert!(!is_gop, "the silent firmware must be reported as non-GOP");
    }

    #[test]
    fn none_when_no_firmware_exists_at_all() {
        let exe = Path::new("/repo/target/debug");
        assert!(resolve_windowed_firmware(exe, None, |_| false).is_none());
    }

    #[test]
    fn parse_disk_plain_path() {
        let d = parse_disk("/images/fedora.raw").unwrap();
        assert_eq!(d.path, PathBuf::from("/images/fedora.raw"));
        assert!(!d.read_only);
        assert_eq!(d.create, None);
    }

    #[test]
    fn parse_disk_ro_and_create_in_either_order() {
        let a = parse_disk("/d/data.raw:ro:create=50G").unwrap();
        let b = parse_disk("/d/data.raw:create=50G:ro").unwrap();
        for d in [&a, &b] {
            assert_eq!(d.path, PathBuf::from("/d/data.raw"));
            assert!(d.read_only);
            assert_eq!(d.create, Some(50 * 1024 * 1024 * 1024));
        }
        assert_eq!(a, b);
    }

    #[test]
    fn parse_disk_keeps_interior_colons_in_path() {
        // A colon that isn't a recognized trailing option stays part of the path.
        let d = parse_disk("/weird:dir/disk.raw").unwrap();
        assert_eq!(d.path, PathBuf::from("/weird:dir/disk.raw"));
        assert!(!d.read_only);
        // …but with a recognized suffix, only that suffix is stripped.
        let d = parse_disk("/weird:dir/disk.raw:ro").unwrap();
        assert_eq!(d.path, PathBuf::from("/weird:dir/disk.raw"));
        assert!(d.read_only);
    }

    #[test]
    fn parse_disk_rejects_dupes_and_empty() {
        assert!(parse_disk(":ro").is_err()); // empty path
        assert!(parse_disk("/d/a.raw:ro:ro").is_err());
        assert!(parse_disk("/d/a.raw:create=1G:create=2G").is_err());
        assert!(parse_disk("/d/a.raw:create=notasize").is_err());
    }

    #[test]
    fn disk_size_units() {
        assert_eq!(parse_disk_size("512").unwrap(), 512);
        assert_eq!(parse_disk_size("100K").unwrap(), 100 * 1024);
        assert_eq!(parse_disk_size("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_disk_size("50G").unwrap(), 50 * 1024 * 1024 * 1024);
        assert_eq!(
            parse_disk_size("2T").unwrap(),
            2 * 1024 * 1024 * 1024 * 1024
        );
        assert_eq!(parse_disk_size(" 1g ").unwrap(), 1024 * 1024 * 1024);
        assert!(parse_disk_size("0").is_err());
        assert!(parse_disk_size("xx").is_err());
        assert!(parse_disk_size("").is_err());
    }

    #[test]
    fn create_disk_image_is_sparse_and_idempotent() {
        let dir = std::env::temp_dir().join(format!("limina-disktest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("new.raw");
        let _ = std::fs::remove_file(&p);

        // Creates a file of the requested size…
        create_disk_image(&p, 64 * 1024 * 1024).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 64 * 1024 * 1024);
        // …and validates as a regular file.
        validate_disk_path(&p).unwrap();
        // Idempotent at the same size.
        create_disk_image(&p, 64 * 1024 * 1024).unwrap();
        // Refuses a different size rather than clobbering.
        assert!(create_disk_image(&p, 32 * 1024 * 1024).is_err());

        std::fs::remove_file(&p).ok();
        // A path that doesn't exist fails validation with the create hint.
        assert!(validate_disk_path(&p).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_disk_args_orders_disks_then_cdroms_folds_ro_and_dedups() {
        let dir = std::env::temp_dir().join(format!("limina-cdromtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("data.raw");
        let iso = dir.join("media.iso");
        std::fs::write(&data, b"x").unwrap();
        std::fs::write(&iso, b"y").unwrap();
        let ds = data.to_str().unwrap().to_string();

        // A data disk + a --cdrom: the cdrom is appended after, read-only.
        let args =
            build_disk_args(std::slice::from_ref(&ds), false, std::slice::from_ref(&iso)).unwrap();
        assert_eq!(
            args,
            vec![
                "--disk".to_string(),
                path_arg(&data).unwrap(),
                "--disk".to_string(),
                format!("{}:ro", path_arg(&iso).unwrap()),
            ]
        );

        // `--read-only` folds `:ro` into the FIRST disk only.
        let args = build_disk_args(std::slice::from_ref(&ds), true, &[]).unwrap();
        assert_eq!(
            args,
            vec![
                "--disk".to_string(),
                format!("{}:ro", path_arg(&data).unwrap())
            ]
        );

        // A path given as both `--disk` and `--cdrom` (or twice) is rejected.
        assert!(build_disk_args(
            std::slice::from_ref(&ds),
            false,
            std::slice::from_ref(&data)
        )
        .is_err());

        // `:create=SIZE` makes a missing data disk, then forwards it read-write (no `:ro`).
        let made = dir.join("created.raw");
        let _ = std::fs::remove_file(&made);
        let spec = format!("{}:create=8M", made.to_str().unwrap());
        let args = build_disk_args(std::slice::from_ref(&spec), false, &[]).unwrap();
        assert_eq!(args, vec!["--disk".to_string(), path_arg(&made).unwrap()]);
        assert_eq!(std::fs::metadata(&made).unwrap().len(), 8 * 1024 * 1024);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Command/Option swap is ON by default; `--no-swap-cmd-opt` opts out; the two flags
    /// override each other last-wins. Parses real argv through clap so the `overrides_with`
    /// wiring is exercised (not just the boolean expression). `--window` is required for the
    /// windowed path but the swap policy is parsed regardless, so we keep argv minimal.
    fn swap_for(extra: &[&str]) -> bool {
        let mut argv = vec!["limina"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv)
            .expect("parsing swap flags")
            .swap_cmd_opt_enabled()
    }

    #[test]
    fn cmd_opt_swap_is_default_on_with_opt_out() {
        // Default: ON (the new behavior — PC-style muscle memory out of the box).
        assert!(swap_for(&[]), "swap should default ON");
        // Explicit on stays on (back-compat: the original --swap-cmd-opt still parses).
        assert!(swap_for(&["--swap-cmd-opt"]));
        // Opt-out turns it off.
        assert!(
            !swap_for(&["--no-swap-cmd-opt"]),
            "--no-swap-cmd-opt should disable"
        );
        // Both given → last one on the line wins (clap overrides_with).
        assert!(
            !swap_for(&["--swap-cmd-opt", "--no-swap-cmd-opt"]),
            "last flag (--no) wins"
        );
        assert!(
            swap_for(&["--no-swap-cmd-opt", "--swap-cmd-opt"]),
            "last flag (--swap) wins"
        );
    }
}
