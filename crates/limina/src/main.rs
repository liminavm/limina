// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina — the front-end / supervisor (decision D3).
//!
//! For M1 this is a CLI that resolves a VM config, spawns the entitled `limina-vmm`
//! worker as a dedicated child process, and supervises its lifecycle (graceful
//! power-off on Ctrl-C, force-kill on timeout, report when the VM stops). The
//! AppKit UI grows on top of this supervisor later.

mod clipboard;
mod control;
mod gateway;
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

    /// Raw disk image to attach as virtio-blk `vda` (optional for direct kernel boot).
    #[arg(long)]
    disk: Option<PathBuf>,

    /// Open the disk read-only (protects the image from guest writes).
    #[arg(long)]
    read_only: bool,

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
}

fn main() -> Result<()> {
    // Death-pact reaper mode (hidden, spawned by the gateway alongside gvproxy). Handled BEFORE
    // clap/logging/AppKit so it stays a tiny process that only watches a pipe and reaps gvproxy
    // when this supervisor dies. See gateway::spawn_death_pact_watcher / run_reaper.
    if std::env::args().nth(1).as_deref() == Some(gateway::REAP_GATEWAY_ARG) {
        gateway::run_reaper();
    }

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
    if let Some(disk) = &cli.disk {
        args.push("--disk".into());
        args.push(path_arg(disk)?);
    }
    if cli.read_only {
        args.push("--read-only".into());
    }
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
        match control::ControlPlane::start(&socket) {
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
        log::info!(
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
        return run_windowed(
            vmm_bin,
            args,
            grace,
            width,
            height,
            gateway,
            control,
            resize_socket,
        );
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

/// Windowed run: socketpair control channel, spawn the worker in `--display-window` mode
/// (inheriting the worker end), then run the AppKit window on the main thread while a
/// background thread monitors the worker. Never returns (exits with the worker's code).
/// One windowed worker plus the supervisor-side fds wiring the window to it. Re-created on a
/// guest reboot ([`spawn_windowed_worker`]); the new fds are swapped into the window's
/// [`window::WorkerConn`] so the same NSWindow keeps showing whichever worker is current.
struct WindowedWorker {
    child: std::process::Child,
    pid: i32,
    /// Worker→supervisor scanout control channel; once handed to `spawn_reader`, that reader
    /// owns it and closes it on EOF (so the relaunch path must NOT close it).
    sup_fd: libc::c_int,
    /// Supervisor→worker evdev input ends (the window writes here), and the shown-ack fd (a dup
    /// of `sup_fd`). These are the supervisor's to close when the worker is replaced.
    kbd_sup_fd: libc::c_int,
    ptr_sup_fd: libc::c_int,
    ack_fd: libc::c_int,
}

/// Spawn a worker in `--display-window` mode and wire fresh scanout/input/ack socketpairs to
/// it. `base_args` are the worker args *without* the windowed fd flags (added here, with the
/// new fd numbers), so this can be called again to relaunch after a guest reboot.
fn spawn_windowed_worker(
    vmm_bin: &std::path::Path,
    base_args: &[String],
    grace: Duration,
    width: u32,
    height: u32,
    surface_port_name: Option<&str>,
) -> Result<WindowedWorker> {
    // Control channel (worker→supervisor scanout): sup_fd stays here (CLOEXEC so the worker
    // can't inherit it); worker_fd is inherited and referenced by --control-fd. Stream type
    // since it carries newline-delimited text.
    let (sup_fd, worker_fd) = socketpair(libc::SOCK_STREAM)?;
    // Input channels (supervisor→worker evdev events): one datagram per event, so SOCK_DGRAM
    // preserves the 8-byte record boundaries the worker's backends rely on.
    let (kbd_sup_fd, kbd_worker_fd) = socketpair(libc::SOCK_DGRAM)?;
    let (ptr_sup_fd, ptr_worker_fd) = socketpair(libc::SOCK_DGRAM)?;
    // Input events are tiny (8 bytes) but bursty (a key chord, a fast drag). Give the
    // datagram pipes a deep buffer (~32k events) so a momentary worker lag never drops
    // input, and make the supervisor *send* ends non-blocking so a pathological full
    // socket (a stalled/dead worker input thread) can never block the AppKit main thread
    // and freeze the whole UI — send() returns EAGAIN and we drop instead (see input.rs).
    for fd in [kbd_sup_fd, kbd_worker_fd, ptr_sup_fd, ptr_worker_fd] {
        set_socket_buffer(fd, 256 * 1024);
    }
    set_nonblocking(kbd_sup_fd);
    set_nonblocking(ptr_sup_fd);

    let mut args = base_args.to_vec();
    args.push("--display-window".into());
    args.push("--control-fd".into());
    args.push(worker_fd.to_string());
    args.push("--display-size".into());
    args.push(format!("{width}x{height}"));
    args.push("--input-kbd-fd".into());
    args.push(kbd_worker_fd.to_string());
    args.push("--input-ptr-fd".into());
    args.push(ptr_worker_fd.to_string());
    if let Some(name) = surface_port_name {
        // Scoped scanouts: the worker hands its (non-global) IOSurfaces to our surface-port
        // receiver by Mach port instead of making them globally lookup-able.
        args.push("--surface-port-name".into());
        args.push(name.to_string());
    }

    let spec = WorkerSpec {
        vmm_bin: vmm_bin.to_path_buf(),
        args,
        shutdown_grace: grace,
    };
    let child = supervisor::spawn_worker(&spec, &[worker_fd, kbd_worker_fd, ptr_worker_fd])?;
    let pid = child.id() as i32;
    // The worker holds its own copies now; drop ours so it sees EOF / owns the read ends.
    unsafe {
        libc::close(worker_fd);
        libc::close(kbd_worker_fd);
        libc::close(ptr_worker_fd);
    }
    // Shown-ack write half (#8 leg 2): a dup of the control socketpair end. NOTE: a dup
    // shares the open file description — setting O_NONBLOCK here would make the reader
    // thread's blocking reads on sup_fd fail too. The ack writes use MSG_DONTWAIT per send.
    let ack_fd = unsafe { libc::dup(sup_fd) };
    Ok(WindowedWorker {
        child,
        pid,
        sup_fd,
        kbd_sup_fd,
        ptr_sup_fd,
        ack_fd,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_windowed(
    vmm_bin: PathBuf,
    base_args: Vec<String>,
    grace: Duration,
    width: u32,
    height: u32,
    gateway: Option<gateway::Gateway>,
    control: Option<control::ControlPlane>,
    resize_socket: Option<PathBuf>,
) -> Result<()> {
    use objc2::MainThreadMarker;

    let mtm = MainThreadMarker::new().context("the window must run on the main thread")?;

    // Capability-scope the scanout IOSurfaces: register a Mach surface-port receiver the worker
    // hands its NON-global scanouts to (so strangers can't IOSurfaceLookup the guest screen). On
    // failure, fall back to global surfaces (worker detects no receiver) so the VM still runs.
    let (surface_port_name, surface_map) = match window::surface_rendezvous() {
        Ok((name, map)) => (Some(name), map),
        Err(e) => {
            log::error!("surface-port rendezvous failed ({e}); scanout IOSurfaces will be GLOBAL");
            (None, window::empty_surface_map())
        }
    };

    // Spawn the first worker and wire its scanout/input/ack channels into a swappable conn.
    let w = spawn_windowed_worker(
        &vmm_bin,
        &base_args,
        grace,
        width,
        height,
        surface_port_name.as_deref(),
    )?;
    let conn = window::WorkerConn::new(w.pid, w.kbd_sup_fd, w.ptr_sup_fd, w.ack_fd);
    let shared = window::Shared::new();
    window::spawn_reader(w.sup_fd, shared.clone());

    // Monitor the worker on a background thread. A guest *reboot* (distinct exit code) relaunches
    // the worker — recycle gvproxy, spawn a fresh worker, swap its fds into `conn`, start a new
    // reader — keeping the SAME NSWindow open. Any other exit (power-off, error, Ctrl-C, or a
    // boot loop) marks the worker gone so the window loop quits. The gateway handle lives in this
    // thread (it owns gvproxy's restart) for the VM's life.
    let monitor_shared = shared.clone();
    let monitor_conn = conn.clone();
    let monitor_control = control.clone();
    let monitor_port_name = surface_port_name.clone();
    std::thread::spawn(move || {
        let mut child = w.child;
        // Supervisor-side fds of the CURRENT worker to retire when it's replaced. NOT sup_fd:
        // its reader thread owns it and closes it on EOF.
        let mut old_io = [w.kbd_sup_fd, w.ptr_sup_fd, w.ack_fd];
        let mut guard = supervisor::RebootGuard::new();
        loop {
            let started = std::time::Instant::now();
            let code = supervisor::monitor(child, grace, monitor_control.as_ref()).unwrap_or(0);
            if !guard.should_relaunch(code, started.elapsed()) {
                window::mark_worker_exited(&monitor_shared);
                break;
            }
            // Recycle gvproxy (single-connection vfkit socket) before the fresh worker dials it.
            if let Some(gw) = &gateway {
                if let Err(e) = gw.restart() {
                    log::error!(
                        "could not restart the NAT gateway for the reboot: {e:#}; stopping"
                    );
                    window::mark_worker_exited(&monitor_shared);
                    break;
                }
            }
            let next = match spawn_windowed_worker(
                &vmm_bin,
                &base_args,
                grace,
                width,
                height,
                monitor_port_name.as_deref(),
            ) {
                Ok(n) => n,
                Err(e) => {
                    log::error!("relaunch after guest reboot failed: {e:#}; stopping");
                    window::mark_worker_exited(&monitor_shared);
                    break;
                }
            };
            // Publish the new worker BEFORE closing the old fds (so a concurrent input send on
            // the main thread can't race a reused fd number), then start its reader and retire
            // the previous worker's input/ack fds.
            monitor_conn.swap(next.pid, next.kbd_sup_fd, next.ptr_sup_fd, next.ack_fd);
            window::spawn_reader(next.sup_fd, monitor_shared.clone());
            for fd in old_io {
                unsafe { libc::close(fd) };
            }
            log::info!(
                "guest rebooted → relaunched the windowed VM worker (pid {})",
                next.pid
            );
            child = next.child;
            old_io = [next.kbd_sup_fd, next.ptr_sup_fd, next.ack_fd];
        }
    });

    // Runs the AppKit window on this (main) thread; never returns — on quit it asks the guest
    // agent to power off (orderly), falling back to killing the current worker's process group
    // (`conn.pid()`). The window captures NSEvents and writes evdev events to the current
    // worker's input fds (read from `conn`). `resize_socket` lets the resize gesture push the
    // new window size to the worker (which forwards it to the live virtio-gpu).
    window::run(shared, mtm, conn, control, resize_socket, surface_map);
}

/// Create a socketpair of the given type; set CLOEXEC on the supervisor end (fd.0) so the
/// worker doesn't inherit it. Returns `(supervisor_end, worker_end)`.
fn socketpair(sock_type: libc::c_int) -> Result<(libc::c_int, libc::c_int)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, sock_type, 0, fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("socketpair: {}", std::io::Error::last_os_error());
    }
    let (sup_fd, worker_fd) = (fds[0], fds[1]);
    unsafe {
        let f = libc::fcntl(sup_fd, libc::F_GETFD);
        libc::fcntl(sup_fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
        // Writing to a socket whose peer (the worker) has exited must fail with an error,
        // not raise SIGPIPE and kill the supervisor (macOS has no MSG_NOSIGNAL).
        let on: libc::c_int = 1;
        libc::setsockopt(
            sup_fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok((sup_fd, worker_fd))
}

/// Mark `fd` non-blocking so a write to a full socket fails fast (EAGAIN) instead of
/// blocking the caller. Best-effort: a failure here only forfeits the non-blocking property.
fn set_nonblocking(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Request `bytes` of send and receive buffer on `fd` (best-effort; the kernel may clamp).
fn set_socket_buffer(fd: libc::c_int, bytes: libc::c_int) {
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &bytes as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
}
