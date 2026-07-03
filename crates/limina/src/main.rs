// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina — the front-end / supervisor (decision D3).
//!
//! For M1 this is a CLI that resolves a VM config, spawns the entitled `limina-vmm`
//! worker as a dedicated child process, and supervises its lifecycle (graceful
//! power-off on Ctrl-C, force-kill on timeout, report when the VM stops). The
//! AppKit UI grows on top of this supervisor later.

mod balloon_policy;
mod center;
mod clipboard;
mod control;
mod gateway;
mod session;
mod supervisor;
mod venus_env;
mod vmlib;
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
    /// Managed-VM verbs (create/start/ls/stop/rm/center). Without a subcommand the
    /// flat flags below describe one ephemeral VM, exactly as before.
    #[command(subcommand)]
    cmd: Option<Cmd>,

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

    /// Display size as WIDTHxHEIGHT (e.g. 1280x800). Used with --display-capture; windowed
    /// boots derive their size from --display-resolution (this is only the last-resort
    /// fallback on a screen-less host).
    #[arg(long, default_value = "1280x800")]
    display_size: String,

    /// What resolution the guest display is driven to (windowed boots only): `host` (default)
    /// follows the screen the window is on, letterboxing the window as needed — fullscreen
    /// needs no modeset; `dynamic` makes the guest follow the window (drag-end resize, and
    /// guest modesets resize the window — the original behavior); WIDTHxHEIGHT fixes it
    /// (letterboxed, driven once at boot, never re-pushed).
    #[arg(
        long,
        default_value = "host",
        value_parser = parse_display_resolution_arg,
        conflicts_with = "display_capture"
    )]
    display_resolution: vmlib::schema::DisplayResolution,

    /// Persist + restore the VM window's frame (and dynamic-mode resolution) in this TOML
    /// file. Managed starts pass the bundle's state.toml automatically; ad-hoc --window runs
    /// opt in with an explicit path (no persistence otherwise).
    #[arg(long, requires = "window")]
    window_state_file: Option<PathBuf>,

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

    /// How hard the balloon reclaims idle guest memory (only meaningful with `--memory`).
    /// `moderate` (default) and `light` key the squeeze to HOST memory pressure and leave the
    /// guest a page-cache allowance while the host is fine; `aggressive` squeezes to the floor
    /// whenever the guest is idle; `disabled` never drives the balloon (free-page reporting
    /// still returns freed guest memory). See spikes/mem-overhead-2026-07-02 for the numbers.
    #[arg(long, value_enum, default_value_t = balloon_policy::ReclaimMode::Moderate)]
    reclaim: balloon_policy::ReclaimMode,

    /// Force the software-2D-only GPU (no virglrenderer/venus). Default is the coexist device
    /// (software-2D 2D + Venus 3D). Use for the capture oracle or the local-Terminal GPU-init hang.
    #[arg(long)]
    gpu_software_2d: bool,

    /// Do NOT mirror the host battery into the guest. By default the worker attaches a
    /// virtio-i2c SBS battery mirroring the host's whenever the host has one (stock guests
    /// pick it up via i2c-virtio + sbs-battery and show charge natively); desktops without
    /// a battery attach nothing either way.
    #[arg(long)]
    no_battery: bool,

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

    /// Guest MAC address for the NAT NIC (aa:bb:cc:dd:ee:ff). Managed VMs pass their persistent
    /// per-VM MAC from vm.toml; when omitted, the well-known vfkit MAC is used. The gateway's
    /// static .2 lease is rebound to this MAC so DHCP and the SSH forward keep working.
    #[arg(long, requires = "net", value_parser = parse_mac_arg)]
    net_mac: Option<String>,

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

    /// Title for the VM window (managed starts pass the VM's name; default "Limina").
    #[arg(long, hide = true)]
    window_title: Option<String>,
}

/// Managed-VM subcommands (docs/design/vm-definitions.md Phase 1): a VM definition
/// is a `.liminavm` bundle in the library; `start` resolves it to the same `Cli`
/// the flat flags produce, so both paths share every semantic downstream.
#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// Open the VM control center window (also the default when run with no arguments).
    Center,
    /// Create a managed VM in the library, optionally importing an existing disk image.
    Create(CreateArgs),
    /// Start a managed VM by name or .liminavm bundle path.
    Start(StartArgs),
    /// List managed VMs and their status.
    Ls,
    /// Ask a running managed VM to shut down (--force skips the guest-side grace).
    Stop(StopArgs),
    /// Delete a stopped managed VM's bundle (disks outside the bundle are never touched).
    Rm(RmArgs),
}

#[derive(clap::Args, Debug)]
struct CreateArgs {
    /// Name of the new VM (the bundle becomes <name>.liminavm in the library).
    name: String,
    /// Existing disk image to import as the VM's boot disk.
    #[arg(long)]
    disk: Option<PathBuf>,
    /// Reference the disk where it is instead of cloning/copying it into the bundle.
    #[arg(long, requires = "disk")]
    in_place: bool,
    /// Create a blank sparse boot disk of this size (e.g. "40G") instead of importing one.
    #[arg(long, conflicts_with = "disk", value_name = "SIZE")]
    blank: Option<String>,
    /// Attach an installer/media ISO (referenced in place, read-only). With --blank and
    /// no bootable disk, the firmware boots the ISO — the OS-install path.
    #[arg(long, value_name = "PATH")]
    cdrom: Option<PathBuf>,
    /// Number of vCPUs.
    #[arg(long, default_value_t = 4)]
    cpus: u8,
    /// Maximum memory (e.g. "4G", "8GiB"; managed VMs are always dynamic with a 1 GiB
    /// floor — the reclaim mode governs how hard the balloon claws idle memory back).
    #[arg(long, default_value = "4G")]
    memory: String,
    /// Pin the inbound-SSH host port (default 0 = auto-allocate from 2222 up).
    #[arg(long, default_value_t = 0)]
    ssh_port: u16,
    /// Create the VM headless (no window when started).
    #[arg(long)]
    no_window: bool,
    /// Create the bundle in this directory instead of the default library.
    #[arg(long)]
    dir: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct StartArgs {
    /// VM name (library lookup) or path to a .liminavm bundle.
    vm: String,
    #[command(flatten)]
    overrides: StartOverrides,
}

/// One-shot overrides for `limina start` — `None`/unset means "use the definition".
/// Deliberately NOT a flattened `Cli`: its `default_value_t` fields can't distinguish
/// "user passed the default" from "user passed nothing".
#[derive(clap::Args, Debug, Default)]
struct StartOverrides {
    /// Open a window even if the definition says headless.
    #[arg(long, overrides_with = "no_window")]
    window: bool,
    /// Run headless even if the definition wants a window.
    #[arg(long, overrides_with = "window")]
    no_window: bool,
    /// Override the vCPU count for this run.
    #[arg(long)]
    cpus: Option<u8>,
    /// Override the maximum memory for this run (e.g. "8G"; managed VMs are always dynamic
    /// with a 1 GiB floor — use --reclaim disabled for static-like behavior).
    #[arg(long)]
    memory: Option<String>,
    /// Override the memory reclaim mode for this run (disabled/light/moderate/aggressive).
    #[arg(long, value_enum)]
    reclaim: Option<balloon_policy::ReclaimMode>,
    /// Override the inbound-SSH host port for this run.
    #[arg(long)]
    ssh_port: Option<u16>,
    /// Override the boot firmware for this run.
    #[arg(long)]
    firmware: Option<PathBuf>,
    /// Override the display resolution mode for this run (host, dynamic, or WIDTHxHEIGHT).
    #[arg(long, value_parser = parse_display_resolution_arg)]
    display_resolution: Option<vmlib::schema::DisplayResolution>,
    /// Path to the limina-vmm worker binary (default: sibling of this executable).
    #[arg(long)]
    vmm_bin: Option<PathBuf>,
    /// Seconds to wait for an orderly guest power-off before force-killing.
    #[arg(long)]
    shutdown_grace_secs: Option<u64>,
    /// Capture the guest serial console to this file (harness/debug).
    #[arg(long)]
    console: Option<PathBuf>,
    /// Bind the control plane at this path (lets a test harness join as a peer).
    #[arg(long)]
    control_socket: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct StopArgs {
    /// VM name or .liminavm bundle path.
    vm: String,
    /// Skip the guest-side grace: kill the VM immediately (a second SIGTERM to the
    /// supervisor, which escalates straight to SIGKILL).
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args, Debug)]
struct RmArgs {
    /// VM name or .liminavm bundle path.
    vm: String,
    /// Force-stop the VM first if it is running.
    #[arg(long)]
    force: bool,
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

/// Raise RLIMIT_NOFILE's soft limit toward the hard cap. A Dock/Finder-launched process
/// inherits launchd's 256-fd soft default, and the venus render server spends one fd per
/// guest shm blob — a GNOME login's context burst exhausts 256, every subsequent blob or
/// context create fails EMFILE, guest venus init dies and gnome-shell aborts (2026-07-02
/// dogfood-guest login crash; the worker idled 6 fds from the ceiling). Terminal launches
/// inherit a high shell limit, which is why dev runs never hit it. Called first thing in
/// main so every child (center → `limina start` → limina-vmm + gvproxy) inherits the
/// raised limit; the worker also raises its own. macOS rejects an unbounded setrlimit
/// (rlim_max reads RLIM_INFINITY but the kernel caps at kern.maxfilesperproc), so clamp
/// to OPEN_MAX (10240), which is always accepted and plenty for any session.
fn raise_fd_limit() {
    const TARGET: libc::rlim_t = 10240; // OPEN_MAX
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let want = TARGET.min(lim.rlim_max); // RLIM_INFINITY compares greater
        if lim.rlim_cur >= want {
            return;
        }
        lim.rlim_cur = want;
        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
    }
}

fn main() -> Result<()> {
    raise_fd_limit();

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

    let mut cli = Cli::parse();
    match cli.cmd.take() {
        Some(Cmd::Center) => center::run(),
        Some(Cmd::Create(args)) => cmd_create(args),
        Some(Cmd::Start(args)) => cmd_start(args),
        Some(Cmd::Ls) => cmd_ls(),
        Some(Cmd::Stop(args)) => cmd_stop(args),
        Some(Cmd::Rm(args)) => cmd_rm(args),
        // Bare `limina` — a double-clicked limina.app or a plain terminal launch —
        // opens the control center. Any flag at all means the flat ephemeral-VM CLI.
        None if std::env::args_os().len() == 1 => center::run(),
        None => run_vm(cli),
    }
}

fn cmd_create(args: CreateArgs) -> Result<()> {
    let memory = vmlib::schema::Memory::parse(&args.memory).context("--memory")?;
    let dest = args.dir.unwrap_or_else(vmlib::bundle::library_dir);
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("creating the VM library {}", dest.display()))?;
    let opts = vmlib::import::CreateOpts {
        name: args.name,
        disk: args.disk,
        import_mode: if args.in_place {
            vmlib::import::ImportMode::ReferenceInPlace
        } else {
            vmlib::import::ImportMode::CloneIntoBundle
        },
        blank_size: args
            .blank
            .as_deref()
            .map(parse_disk_size)
            .transpose()
            .context("--blank")?,
        cdrom: args.cdrom,
        cpus: args.cpus,
        memory,
        ssh_port: args.ssh_port,
        window: !args.no_window,
    };
    let bundle = vmlib::import::create(&opts, &dest)?;
    println!("created {}", bundle.path.display());
    Ok(())
}

fn cmd_start(args: StartArgs) -> Result<()> {
    let bundle = vmlib::bundle::resolve(&args.vm)?;
    let cfg = bundle.load()?;
    let cli = cli_from_definition(&cfg, &bundle, &args.overrides)?;
    // Exclusive run lock + pidfile: taken before the VM comes up so a double-start
    // fails fast, leaked deliberately because every supervisor exit path is
    // process::exit — the kernel releases the flock with the process.
    let lock = vmlib::runtime::acquire(&bundle)?;
    vmlib::runtime::write_pidfile(&bundle)?;
    std::mem::forget(lock);
    run_vm(cli)
}

fn cmd_ls() -> Result<()> {
    let all = vmlib::bundle::list()?;
    if all.is_empty() {
        println!(
            "no VMs in {} (create one with `limina create`)",
            vmlib::bundle::library_dir().display()
        );
        return Ok(());
    }
    println!(
        "{:<24} {:<16} {:>4}  {:<12} {:<6}",
        "NAME", "STATUS", "CPUS", "MEMORY", "SSH"
    );
    for bundle in all {
        let status = match vmlib::runtime::status(&bundle) {
            vmlib::runtime::VmStatus::Running { pid } => format!("running (pid {pid})"),
            vmlib::runtime::VmStatus::Stopped => "stopped".to_string(),
        };
        match bundle.load() {
            Ok(cfg) => {
                let mem = cfg.hardware.memory.0.clone();
                let ssh = match cfg.networks.first() {
                    Some(n) if n.ssh_port != 0 => n.ssh_port.to_string(),
                    Some(_) => "auto".to_string(),
                    None => "-".to_string(),
                };
                println!(
                    "{:<24} {:<16} {:>4}  {:<12} {:<6}",
                    cfg.identity.name, status, cfg.hardware.cpus, mem, ssh
                );
            }
            Err(e) => {
                println!("{:<24} broken: {e:#}", bundle.dir_name());
            }
        }
    }
    Ok(())
}

fn cmd_stop(args: StopArgs) -> Result<()> {
    let bundle = vmlib::bundle::resolve(&args.vm)?;
    let pid = match vmlib::runtime::status(&bundle) {
        vmlib::runtime::VmStatus::Stopped => {
            println!("{} is not running", bundle.dir_name());
            return Ok(());
        }
        vmlib::runtime::VmStatus::Running { pid } => pid,
    };
    vmlib::runtime::signal_stop(pid, args.force)?;
    // The graceful ladder is bounded (agent grace + shutdown grace + SIGKILL), so a
    // healthy supervisor always releases the lock; 60s covers a slow guest shutdown.
    if vmlib::runtime::wait_stopped(&bundle, Duration::from_secs(60)) {
        println!("{} stopped", bundle.dir_name());
        Ok(())
    } else {
        anyhow::bail!(
            "{} did not stop within 60s (supervisor pid {pid})",
            bundle.dir_name()
        )
    }
}

fn cmd_rm(args: RmArgs) -> Result<()> {
    let bundle = vmlib::bundle::resolve(&args.vm)?;
    if let vmlib::runtime::VmStatus::Running { pid } = vmlib::runtime::status(&bundle) {
        anyhow::ensure!(
            args.force,
            "{} is running; stop it first or pass --force",
            bundle.dir_name()
        );
        vmlib::runtime::signal_stop(pid, true)?;
        anyhow::ensure!(
            vmlib::runtime::wait_stopped(&bundle, Duration::from_secs(30)),
            "{} did not stop; not deleting",
            bundle.dir_name()
        );
    }
    vmlib::import::remove(&bundle)?;
    println!("deleted {}", bundle.path.display());
    Ok(())
}

/// Resolve a VM definition to the `Cli` the flat flags would have produced — the
/// definition layer is pure policy over the existing invocation shape, so disks,
/// firmware resolution, the gateway, the control plane and the reboot relaunch all
/// behave identically for managed and ephemeral VMs. Overrides win over the
/// definition; everything else takes the flat CLI's defaults.
fn cli_from_definition(
    cfg: &vmlib::schema::VmConfig,
    bundle: &vmlib::bundle::VmBundle,
    ov: &StartOverrides,
) -> Result<Cli> {
    use vmlib::schema::{memory_to_cli, GpuMode, Memory};

    let (ram_mib, memory) = match &ov.memory {
        Some(s) => memory_to_cli(&Memory::parse(s).context("--memory")?)?,
        None => memory_to_cli(&cfg.hardware.memory)?,
    };

    let mut disk = Vec::new();
    for d in &cfg.disks {
        let p = bundle.resolve_path(&d.path);
        let s = path_arg(&p)?;
        disk.push(format!("{s}{}", if d.ro { ":ro" } else { "" }));
    }
    let cdrom: Vec<PathBuf> = cfg
        .cdroms
        .iter()
        .map(|c| bundle.resolve_path(&c.path))
        .collect();
    let mut share = Vec::new();
    for s in &cfg.shares {
        let p = path_arg(&s.path)?;
        let ro = if s.ro { ":ro" } else { "" };
        share.push(match &s.name {
            Some(n) => format!("{n}={p}{ro}"),
            None => format!("{p}{ro}"),
        });
    }

    let net = !cfg.networks.is_empty();
    let ssh_port = ov.ssh_port.or_else(|| {
        cfg.networks
            .first()
            .and_then(|n| (n.ssh_port != 0).then_some(n.ssh_port))
    });
    anyhow::ensure!(
        net || ssh_port.is_none(),
        "--ssh-port needs a [[network]] in the definition"
    );

    let window = if ov.window {
        true
    } else if ov.no_window {
        false
    } else {
        cfg.display.window
    };
    let firmware = ov
        .firmware
        .clone()
        .or_else(|| cfg.boot.firmware.as_ref().map(|f| bundle.resolve_path(f)));

    Ok(Cli {
        cmd: None,
        firmware,
        kernel: None,
        initramfs: None,
        cmdline: None,
        rootfs: None,
        disk,
        read_only: false,
        cdrom,
        share,
        cpus: ov.cpus.unwrap_or(cfg.hardware.cpus),
        ram_mib,
        vsock_port: None,
        vsock_socket: None,
        control_socket: ov.control_socket.clone(),
        console: ov.console.clone(),
        console_input: None,
        console_pty: false,
        virtio_console: None,
        virtio_console_input: None,
        window,
        display_capture: None,
        // Dynamic-mode first-boot fallback only; the real initial size is derived per
        // display mode (and remembered state) in run_vm's windowed branch.
        display_size: "1280x800".into(),
        display_resolution: ov.display_resolution.unwrap_or(cfg.display.resolution),
        window_state_file: window.then(|| bundle.state_toml()),
        display_control_socket: None,
        memory,
        balloon_control_socket: None,
        reclaim: ov.reclaim.unwrap_or(cfg.hardware.reclaim),
        gpu_software_2d: cfg.display.gpu == GpuMode::Software2d,
        no_battery: !cfg.hardware.battery,
        net,
        net_log: None,
        net_mac: cfg.networks.first().map(|n| n.mac.clone()),
        ssh_port,
        shutdown_grace_secs: ov.shutdown_grace_secs.unwrap_or(20),
        vmm_bin: ov.vmm_bin.clone(),
        // Encode the definition's swap policy as the equivalent flag pair, so
        // swap_cmd_opt_enabled() resolves to exactly cfg.input.swap_cmd_opt.
        swap_cmd_opt: cfg.input.swap_cmd_opt,
        no_swap_cmd_opt: !cfg.input.swap_cmd_opt,
        window_title: Some(cfg.identity.name.clone()),
    })
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

    // The PSI autoballoon policy runs in the supervisor when a range + socket both exist (and
    // reclaim isn't disabled): it consumes guest MemPressure over the control plane, samples
    // host memory pressure, and drives the balloon target per the reclaim mode.
    let balloon_policy = match (mem_range, balloon_socket.clone()) {
        (Some((min, max)), Some(sock)) if cli.reclaim != balloon_policy::ReclaimMode::Disabled => {
            log::info!(
                "dynamic memory: {min}..{max} MiB (reclaim {:?})",
                cli.reclaim
            );
            Some(balloon_policy::BalloonPolicy::new(
                min as u32 * balloon_policy::PAGES_PER_MIB,
                max as u32 * balloon_policy::PAGES_PER_MIB,
                cli.reclaim,
                sock,
            ))
        }
        (Some((min, max)), Some(_)) => {
            log::info!("dynamic memory: {min}..{max} MiB (reclaim disabled — balloon idle)");
            None
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
    if cli.no_battery {
        args.push("--no-battery".into());
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
        let gw = gateway::start(cli.net_log.as_deref(), cli.ssh_port, cli.net_mac.as_deref())
            .context("starting the gvproxy NAT gateway")?;
        // Surface the resolved port — with auto-allocation the user can't know it in advance.
        // User-facing output: print directly so it shows at the default warn level.
        println!(
            "guest SSH forward ready: ssh -p {} <user>@127.0.0.1",
            gw.ssh_port()
        );
        args.push("--net-gvproxy".into());
        args.push(path_arg(gw.socket_path())?);
        if let Some(mac) = &cli.net_mac {
            args.push("--net-mac".into());
            args.push(mac.clone());
        }
        Some(gw)
    } else {
        None
    };

    // Windowed mode: open a native window in the supervisor and stream the guest scanout
    // from the worker over a control socketpair (the worker publishes shared IOSurfaces).
    if cli.window {
        // Remembered window state (managed VMs: the bundle's state.toml). Loaded before the
        // worker spawns because the initial --display-size derives from it (dynamic mode) or
        // from the screen the remembered frame is on (host mode).
        let win_state = cli
            .window_state_file
            .as_deref()
            .and_then(vmlib::state::load)
            .and_then(|s| s.window);
        let restore_frame = win_state.map(|w| w.frame);
        let screen = window::screen_info_for_frame(restore_frame);
        // Dynamic first-boot (nothing remembered): the half-area default at the SCREEN's
        // aspect — the same rule that sizes the first window, so window == guest with no
        // early re-modeset. --display-size only backstops a screen-less host.
        let fallback = screen
            .as_ref()
            .map(|s| {
                window::fit::default_window_content(
                    s.frame,
                    (f64::from(s.frame.0), f64::from(s.frame.1)),
                    s.visible,
                )
            })
            .unwrap_or(parse_display_size(&cli.display_size)?);
        let (width, height) = initial_display_size(
            cli.display_resolution,
            win_state.map(|w| w.content),
            screen.as_ref().map(|s| s.frame),
            fallback,
        );
        // First-appearance window: half the display's area at the guest's aspect, so the
        // window is neither tiny, nor screen-filling, nor letterboxed on first show.
        let default_content = screen
            .as_ref()
            .map(|s| {
                window::fit::default_window_content(
                    (width, height),
                    (f64::from(s.frame.0), f64::from(s.frame.1)),
                    s.visible,
                )
            })
            .unwrap_or((width, height));
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
            title: cli.window_title.clone().unwrap_or_else(|| "Limina".into()),
            mode: cli.display_resolution,
            state_path: cli.window_state_file.clone(),
            restore_frame,
            default_content,
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

/// clap value parser for `--display-resolution` (host | dynamic | WIDTHxHEIGHT).
fn parse_display_resolution_arg(s: &str) -> Result<vmlib::schema::DisplayResolution> {
    s.parse()
}

/// The guest resolution a windowed boot starts at, by display mode: host → the target
/// screen's point size (`None` on a screen-less host → the fallback); dynamic → the
/// remembered window content size, else the fallback (the half-area screen-aspect
/// default, or `--display-size` on a screen-less host); fixed → the configured
/// resolution. Pure — the screen size is injected — so it tests headless.
fn initial_display_size(
    mode: vmlib::schema::DisplayResolution,
    remembered: Option<(u32, u32)>,
    screen_points: Option<(u32, u32)>,
    fallback: (u32, u32),
) -> (u32, u32) {
    use vmlib::schema::DisplayResolution::*;
    match mode {
        Host => screen_points.unwrap_or(fallback),
        Dynamic => remembered.unwrap_or(fallback),
        Fixed(w, h) => (w, h),
    }
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

/// clap value-parser for `--net-mac`: validate + normalize `aa:bb:cc:dd:ee:ff` to lowercase.
fn parse_mac_arg(s: &str) -> Result<String, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6
        || parts
            .iter()
            .any(|p| p.len() != 2 || u8::from_str_radix(p, 16).is_err())
    {
        return Err(format!(
            "MAC must be six colon-separated hex octets (aa:bb:cc:dd:ee:ff), got {s:?}"
        ));
    }
    Ok(s.to_ascii_lowercase())
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

/// Parse a memory size into MiB. Optional suffix, case-insensitive: `G`/`GB`/`GiB` (all GiB —
/// RAM sizes are binary) or `M`/`MB`/`MiB`; a bare number is MiB.
fn parse_size_mib(s: &str) -> Result<usize> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let (num, mult) = if let Some(n) = lower
        .strip_suffix("gib")
        .or_else(|| lower.strip_suffix("gb"))
        .or_else(|| lower.strip_suffix('g'))
    {
        (n, 1024)
    } else if let Some(n) = lower
        .strip_suffix("mib")
        .or_else(|| lower.strip_suffix("mb"))
        .or_else(|| lower.strip_suffix('m'))
    {
        (n, 1)
    } else {
        (lower.as_str(), 1)
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

    /// Guards the launchd-256-fd login crash (2026-07-02): with the soft limit dropped to
    /// the Finder-launch default, raise_fd_limit must lift it to OPEN_MAX (or the hard cap
    /// if lower). The real reproducer is launchd-only (12 concurrent venus contexts all
    /// EMFILE'd); this pins the mechanism the fix relies on.
    #[test]
    fn raise_fd_limit_lifts_the_launchd_default() {
        unsafe {
            let mut orig = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut orig), 0);
            let lowered = libc::rlimit {
                rlim_cur: 256,
                rlim_max: orig.rlim_max,
            };
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &lowered), 0);

            raise_fd_limit();

            let mut now = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut now), 0);
            let want = 10240u64.min(orig.rlim_max);
            assert_eq!(now.rlim_cur, want, "soft fd limit not raised");

            // Restore the original soft limit for the rest of the test process.
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &orig);
        }
    }

    #[test]
    fn memory_range_parses_suffixes_and_bare_mib() {
        assert_eq!(parse_memory_range("2G..12G").unwrap(), (2048, 12288));
        assert_eq!(parse_memory_range("512M..4096").unwrap(), (512, 4096));
        // GB/GiB/MB/MiB spellings (all binary), any case.
        assert_eq!(parse_size_mib("8GB").unwrap(), 8192);
        assert_eq!(parse_size_mib("8GiB").unwrap(), 8192);
        assert_eq!(parse_size_mib("8gib").unwrap(), 8192);
        assert_eq!(parse_size_mib("512MB").unwrap(), 512);
        assert_eq!(parse_size_mib("512MiB").unwrap(), 512);
        assert_eq!(parse_size_mib(" 4 G ").unwrap(), 4096);
        assert!(parse_size_mib("8QB").is_err());
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

    /// A definition + empty overrides must map onto the exact `Cli` the equivalent
    /// flat invocation would produce — the definition layer is policy, not mechanism.
    #[test]
    fn definition_maps_onto_the_flat_cli() {
        use crate::vmlib::bundle::VmBundle;
        use crate::vmlib::schema::*;

        let bundle = VmBundle::new("/lib/Fedora.liminavm");
        let cfg = VmConfig {
            config_version: CONFIG_VERSION,
            identity: Identity {
                name: "Fedora".into(),
                uuid: "u".into(),
                created: "t".into(),
            },
            hardware: Hardware {
                cpus: 6,
                memory: Memory("8G".into()),
                ..Hardware::default()
            },
            disks: vec![
                DiskEntry {
                    path: "disks/root.raw".into(),
                    ro: false,
                },
                DiskEntry {
                    path: "/elsewhere/data.raw".into(),
                    ro: true,
                },
            ],
            cdroms: vec![CdromEntry {
                path: "media.iso".into(),
            }],
            networks: vec![NetworkEntry {
                mode: NetMode::Nat,
                mac: "5a:00:00:00:00:01".into(),
                ssh_port: 2299,
            }],
            shares: vec![ShareEntry {
                name: Some("Projects".into()),
                path: "/Users/x/Projects".into(),
                ro: true,
            }],
            boot: BootCfg {
                firmware: Some("fw/KRUN_EFI.fd".into()),
            },
            display: DisplayCfg {
                window: false,
                gpu: GpuMode::Software2d,
                resolution: DisplayResolution::Fixed(1600, 1000),
            },
            input: InputCfg {
                swap_cmd_opt: false,
            },
        };

        let cli = cli_from_definition(&cfg, &bundle, &StartOverrides::default()).unwrap();
        assert_eq!(cli.cpus, 6);
        assert_eq!(cli.ram_mib, 8192, "the max is what libkrun allocates");
        assert_eq!(
            cli.memory.as_deref(),
            Some("1024..8192"),
            "managed VMs always boot dynamic, floored at 1 GiB"
        );
        assert_eq!(
            cli.disk,
            vec![
                "/lib/Fedora.liminavm/disks/root.raw".to_string(),
                "/elsewhere/data.raw:ro".to_string(),
            ],
            "bundle-relative resolved, order preserved, ro suffixed"
        );
        assert_eq!(
            cli.cdrom,
            vec![PathBuf::from("/lib/Fedora.liminavm/media.iso")]
        );
        assert_eq!(cli.share, vec!["Projects=/Users/x/Projects:ro".to_string()]);
        assert!(cli.net);
        assert_eq!(cli.ssh_port, Some(2299));
        assert_eq!(
            cli.net_mac.as_deref(),
            Some("5a:00:00:00:00:01"),
            "the persistent per-VM MAC reaches the worker + gateway"
        );
        assert!(!cli.window);
        assert!(cli.gpu_software_2d);
        assert_eq!(
            cli.firmware,
            Some(PathBuf::from("/lib/Fedora.liminavm/fw/KRUN_EFI.fd"))
        );
        assert!(!cli.swap_cmd_opt_enabled(), "definition's swap=false wins");
        assert_eq!(cli.shutdown_grace_secs, 20, "flat default");
        assert_eq!(cli.window_title.as_deref(), Some("Fedora"));
        assert_eq!(cli.display_resolution, DisplayResolution::Fixed(1600, 1000));
        assert_eq!(
            cli.window_state_file, None,
            "a headless start carries no window state"
        );
        assert!(cli.cmd.is_none());
    }

    /// The display mode flows definition → Cli (override wins), and a windowed managed
    /// start carries the bundle's state.toml path so the window can remember its frame.
    #[test]
    fn display_resolution_flows_from_definition_and_overrides() {
        use crate::vmlib::bundle::VmBundle;
        use crate::vmlib::schema::*;

        let bundle = VmBundle::new("/lib/vm.liminavm");
        let mut cfg = VmConfig {
            config_version: CONFIG_VERSION,
            identity: Identity {
                name: "vm".into(),
                uuid: "u".into(),
                created: "t".into(),
            },
            hardware: Hardware::default(),
            disks: vec![],
            cdroms: vec![],
            networks: vec![],
            shares: vec![],
            boot: BootCfg::default(),
            display: DisplayCfg::default(), // window = true, resolution = host
            input: InputCfg::default(),
        };

        let cli = cli_from_definition(&cfg, &bundle, &StartOverrides::default()).unwrap();
        assert_eq!(
            cli.display_resolution,
            DisplayResolution::Host,
            "match-host is the default"
        );
        assert_eq!(
            cli.window_state_file,
            Some(PathBuf::from("/lib/vm.liminavm/state.toml")),
            "windowed managed starts persist window state in the bundle"
        );

        cfg.display.resolution = DisplayResolution::Dynamic;
        let ov = StartOverrides {
            display_resolution: Some(DisplayResolution::Fixed(1024, 768)),
            ..Default::default()
        };
        let cli = cli_from_definition(&cfg, &bundle, &ov).unwrap();
        assert_eq!(
            cli.display_resolution,
            DisplayResolution::Fixed(1024, 768),
            "the one-shot override beats the definition"
        );
    }

    /// The boot resolution per display mode: host follows the screen (fallback on a
    /// screen-less host), dynamic follows the remembered window (fallback on first boot),
    /// fixed is verbatim.
    #[test]
    fn initial_display_size_derivation() {
        use crate::vmlib::schema::DisplayResolution::*;

        let screen = Some((1728, 1117));
        let remembered = Some((1400, 900));
        let fallback = (1280, 800);

        assert_eq!(
            initial_display_size(Host, remembered, screen, fallback),
            (1728, 1117),
            "host mode ignores the remembered content size"
        );
        assert_eq!(initial_display_size(Host, None, None, fallback), fallback);
        assert_eq!(
            initial_display_size(Dynamic, remembered, screen, fallback),
            (1400, 900)
        );
        assert_eq!(
            initial_display_size(Dynamic, None, screen, fallback),
            fallback
        );
        assert_eq!(
            initial_display_size(Fixed(1920, 1080), remembered, screen, fallback),
            (1920, 1080)
        );
    }

    #[test]
    fn start_overrides_beat_the_definition() {
        use crate::vmlib::bundle::VmBundle;
        use crate::vmlib::schema::*;

        let bundle = VmBundle::new("/lib/vm.liminavm");
        let cfg = VmConfig {
            config_version: CONFIG_VERSION,
            identity: Identity {
                name: "vm".into(),
                uuid: "u".into(),
                created: "t".into(),
            },
            hardware: Hardware::default(), // cpus 4, 4G max
            disks: vec![],
            cdroms: vec![],
            networks: vec![NetworkEntry {
                mode: NetMode::Nat,
                mac: "m".into(),
                ssh_port: 0,
            }],
            shares: vec![],
            boot: BootCfg::default(),
            display: DisplayCfg::default(), // window = true
            input: InputCfg::default(),
        };

        let ov = StartOverrides {
            no_window: true,
            cpus: Some(2),
            memory: Some("1G".into()),
            ssh_port: Some(2444),
            firmware: Some("/fw.fd".into()),
            shutdown_grace_secs: Some(5),
            ..Default::default()
        };
        let cli = cli_from_definition(&cfg, &bundle, &ov).unwrap();
        assert!(!cli.window, "--no-window override beats window=true");
        assert_eq!(cli.cpus, 2);
        assert_eq!(cli.ram_mib, 1024);
        assert_eq!(
            cli.memory.as_deref(),
            Some("1024..1024"),
            "a max at the floor degrades to a no-op range (room 0)"
        );
        assert_eq!(cli.ssh_port, Some(2444));
        assert_eq!(cli.firmware, Some(PathBuf::from("/fw.fd")));
        assert_eq!(cli.shutdown_grace_secs, 5);

        // An ssh-port override without any [[network]] is an error, not silence.
        let mut no_net = cfg.clone();
        no_net.networks.clear();
        let ov = StartOverrides {
            ssh_port: Some(2444),
            ..Default::default()
        };
        assert!(cli_from_definition(&no_net, &bundle, &ov).is_err());
    }

    #[test]
    fn subcommands_parse_and_flat_cli_is_untouched() {
        // Flat invocation (the harness shape) still parses with no subcommand.
        let cli = Cli::try_parse_from(["limina", "--window", "--cpus", "2"]).unwrap();
        assert!(cli.cmd.is_none());
        assert!(cli.window);
        assert_eq!(cli.cpus, 2);

        // start with overrides.
        let cli = Cli::try_parse_from(["limina", "start", "fedora", "--cpus", "2", "--no-window"])
            .unwrap();
        match cli.cmd {
            Some(Cmd::Start(a)) => {
                assert_eq!(a.vm, "fedora");
                assert_eq!(a.overrides.cpus, Some(2));
                assert!(a.overrides.no_window);
                assert!(!a.overrides.window);
            }
            other => panic!("expected start, got {other:?}"),
        }

        // create with an import.
        let cli = Cli::try_parse_from([
            "limina", "create", "dev", "--disk", "/i/f.raw", "--memory", "8G",
        ])
        .unwrap();
        match cli.cmd {
            Some(Cmd::Create(a)) => {
                assert_eq!(a.name, "dev");
                assert_eq!(a.disk, Some(PathBuf::from("/i/f.raw")));
                assert_eq!(a.memory, "8G");
                assert!(!a.in_place);
                assert_eq!(a.ssh_port, 0);
            }
            other => panic!("expected create, got {other:?}"),
        }

        // --in-place requires --disk.
        assert!(Cli::try_parse_from(["limina", "create", "dev", "--in-place"]).is_err());

        // A blank disk + ISO (the OS-install shape); --blank and --disk conflict.
        let cli = Cli::try_parse_from([
            "limina", "create", "inst", "--blank", "40G", "--cdrom", "/i/f.iso",
        ])
        .unwrap();
        match cli.cmd {
            Some(Cmd::Create(a)) => {
                assert_eq!(a.blank.as_deref(), Some("40G"));
                assert_eq!(a.cdrom, Some(PathBuf::from("/i/f.iso")));
                assert!(a.disk.is_none());
            }
            other => panic!("expected create, got {other:?}"),
        }
        assert!(Cli::try_parse_from([
            "limina", "create", "x", "--blank", "1G", "--disk", "/d.raw"
        ])
        .is_err());

        // stop/rm/ls/center parse.
        assert!(matches!(
            Cli::try_parse_from(["limina", "stop", "dev", "--force"])
                .unwrap()
                .cmd,
            Some(Cmd::Stop(StopArgs { force: true, .. }))
        ));
        assert!(matches!(
            Cli::try_parse_from(["limina", "rm", "dev"]).unwrap().cmd,
            Some(Cmd::Rm(RmArgs { force: false, .. }))
        ));
        assert!(matches!(
            Cli::try_parse_from(["limina", "ls"]).unwrap().cmd,
            Some(Cmd::Ls)
        ));
        assert!(matches!(
            Cli::try_parse_from(["limina", "center"]).unwrap().cmd,
            Some(Cmd::Center)
        ));
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
