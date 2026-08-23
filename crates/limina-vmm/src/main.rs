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

mod bracket;
mod config;
mod fido_usb;
mod krun;
mod moc_usb;
mod power;
mod restart;
mod shutdown;
mod snapshot;
mod suspend;
mod usb_kbd;
mod wake;

use std::path::PathBuf;

use anyhow::{Context, Result};
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

    /// Publish an SMBIOS Type 11 "OEM String" to the guest (repeatable). **EFI boot only** —
    /// libkrun writes SMBIOS only on the firmware path, so this conflicts with `--kernel`
    /// rather than being silently dropped. Chiefly a systemd-credential channel:
    /// `io.systemd.credential:passwd.hashed-password.root=<hash>`.
    #[arg(
        long = "smbios-oem-string",
        value_name = "STRING",
        conflicts_with = "kernel"
    )]
    smbios_oem_string: Vec<String>,

    /// Host directory to serve as the guest root over virtio-fs (tag `/dev/root`).
    /// Pair with `--cmdline "... rootfstype=virtiofs rw init=/init"`. The L1 path.
    #[arg(long)]
    rootfs: Option<PathBuf>,

    /// Share a host directory into the guest over virtio-fs as `tag=path` (repeatable).
    /// The guest mounts it with `mount -t virtiofs <tag> <dir>`; tags prefixed `limina-`
    /// are auto-mounted under /media by the guest agent. Append `:ro` for read-only.
    #[arg(long = "share", value_name = "TAG=PATH[:ro]")]
    share: Vec<String>,

    /// Disk to attach as virtio-blk, `PATH[:ro]` (repeatable). Attach order is device order:
    /// the first `--disk` is `vda`, the next `vdb`, … `:ro` opens it read-only. (The `limina`
    /// supervisor validates/creates images and forwards them here; this worker just attaches
    /// each in order, assigning positional block ids — `disks[0]`=`"root"`.)
    #[arg(long, value_name = "PATH[:ro]")]
    disk: Vec<String>,

    /// Open the FIRST `--disk` read-only (back-compat alias; prefer a per-disk `:ro` suffix).
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

    /// fd of the guest's SPICE agent port (`com.redhat.spice.0`), created and brokered by
    /// the supervisor. The device is attached only when this is present, so a worker run
    /// by hand simply has no agent port.
    #[arg(long)]
    spice_fd: Option<i32>,

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

    /// How many virtio-gpu scanouts to configure (the *pool*). `num_scanouts` is device
    /// config-space state and cannot grow at runtime, so a display the user enables later
    /// has to already exist as a disconnected scanout. Slot 0 is the one that starts
    /// connected; every other slot boots disconnected and is a `display id=N connected=1`
    /// away from being a monitor. See docs/design/stable-edid-hotplug.md.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=16))]
    display_pool: u32,

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

    /// Do NOT mirror the host battery into the guest. By default a virtio-i2c SBS
    /// battery mirroring the host's is attached when the host has one (or
    /// LIMINA_BATTERY_FAKE is set); this turns the device off entirely.
    #[arg(long)]
    no_battery: bool,

    /// Do NOT attach the native virtio-snd audio device. By default a virtio-snd card
    /// (device ID 25) driving host CoreAudio is attached; the guest's stock virtio_snd
    /// driver binds it with no guest-side components. This turns the device off entirely.
    #[arg(long)]
    no_snd: bool,

    /// Advertise the mic-capture input stream on the virtio-snd device so the guest can
    /// record the host microphone. Opt-in and OFF by default for privacy (unlike audio
    /// output). No effect with `--no-snd`. First guest capture triggers the macOS mic TCC
    /// prompt (needs the app bundle's NSMicrophoneUsageDescription).
    #[arg(long)]
    mic: bool,

    /// Attach the emulated xHCI USB controller (platform `generic-xhci`). Opt-in and OFF
    /// by default; a stock guest binds it with its own xhci-plat driver and brings the
    /// root hub up with no guest-side components. Stage A is a bring-up skeleton (the
    /// controller enumerates but carries no devices yet).
    #[arg(long)]
    usb: bool,

    /// UNIX-socket path the worker binds for the stock-tier FIDO USB gadget (M14 Stage C).
    /// The worker cold-plugs a HID report-pipe gadget with the FIDO identity and shuttles
    /// CTAPHID frames over this socket to the supervisor's authenticator (SEP/Touch ID lives
    /// there). Only meaningful with `--usb`; absent → no FIDO gadget.
    #[arg(long, requires = "usb")]
    fido_socket: Option<PathBuf>,

    /// UNIX-socket path the worker binds for the stock-tier fingerprint reader gadget (M14 wave 3).
    /// The worker cold-plugs a bulk-pipe gadget with the elanmoc identity and shuttles bulk packets
    /// over this socket to the supervisor's protocol engine (Touch ID / template store live there).
    /// Only meaningful with `--usb`; absent → no fingerprint gadget.
    #[arg(long, requires = "usb")]
    moc_socket: Option<PathBuf>,

    /// Advertise `VIRTIO_BALLOON_F_REPORTING` (free-page-reporting fast reclaim) to the guest.
    /// OFF by default: a stock Linux guest with page-reporting enabled crashes on suspend-to-idle
    /// (upstream `virtballoon_freeze` frees the reporting virtqueue while its non-freezable worker is
    /// still live). Enable only for enhanced-tier guests that carry the kernel fix; stock guests keep
    /// the coarser inflate-time reclaim and suspend safely.
    #[arg(long)]
    balloon_free_page_reporting: bool,

    /// Advertise `VIRTIO_BALLOON_F_DEFLATE_ON_OOM` to the guest. OFF by default: the bit makes
    /// Linux keep ballooned pages inside `MemTotal` (transparent accounting), so an inflated
    /// dynamic VM looks almost out of memory and systemd-oomd starts killing; the supervisor's
    /// PSI policy is the release path instead. Escape hatch for guests that need the in-kernel
    /// deflate-at-OOM net (see the docs/design/m6-dynamic-memory.md addendum).
    #[arg(long)]
    balloon_deflate_on_oom: bool,

    /// Attach a user-mode NAT NIC (`eth0`) connected to a gvproxy gateway listening on
    /// this vfkit unixgram socket. The supervisor spawns gvproxy and guarantees it is up
    /// before the guest activates the device.
    #[arg(long)]
    net_gvproxy: Option<PathBuf>,

    /// Guest MAC for the NAT NIC (aa:bb:cc:dd:ee:ff). Default: the well-known vfkit MAC.
    /// The supervisor keeps gvproxy's static .2 lease in sync with this.
    #[arg(long, requires = "net_gvproxy")]
    net_mac: Option<String>,

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

    /// Write a VM snapshot to this path on a SIGUSR1 suspend trigger (M9 suspend/resume). When set,
    /// the worker installs a SIGUSR1 handler; on signal it quiesces the vCPUs, serializes
    /// vCPU+GIC+RAM to the file, and exits 126 ("snapshotted"). The supervisor drives the trigger
    /// and owns the path; resuming is a separate `--restore` boot.
    #[arg(long)]
    snapshot_file: Option<PathBuf>,

    /// Resume from a VM snapshot written by an earlier `--snapshot-file` suspend (M9), instead of a
    /// cold boot. Pass the SAME machine config (kernel/cmdline/cpus/ram) as the suspended VM plus
    /// this flag; the worker rebuilds the machine and restores RAM + every vCPU + the GIC so the
    /// guest continues mid-execution. Device state is not in the snapshot (re-established on resume).
    #[arg(long)]
    restore: Option<PathBuf>,

    /// What to do with the guest when the HOST goes to sleep. `s2idle` (default): pulse the guest
    /// sleep button on `willSleep` (holding the ack while it quiesces) and wake it on `didWake` —
    /// its own thaw re-reads the host-anchored RTC, so the wall clock is correct on every tier.
    /// `ignore`: leave it running with a frozen counter across host sleep (the old behavior).
    #[arg(long, default_value = "s2idle")]
    on_host_sleep: String,
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

/// Parse a `--disk PATH[:ro]` attach spec. The `:ro` suffix is a flag only when it's the exact
/// trailing token; interior colons stay in the path. (`:create` is resolved by the supervisor,
/// so the worker only ever sees `PATH[:ro]`.)
fn parse_disk_attach(spec: &str) -> Result<(PathBuf, bool)> {
    let (path, read_only) = match spec.strip_suffix(":ro") {
        Some(p) => (p, true),
        None => (spec, false),
    };
    anyhow::ensure!(!path.is_empty(), "--disk has an empty path: {spec:?}");
    Ok((PathBuf::from(path), read_only))
}

/// Take a non-blocking advisory exclusive lock (`flock(LOCK_EX)`) on each writable disk's
/// backing file and return the held fds (drop = unlock). libkrun opens its own fd to the same
/// file for I/O; this guard fd only carries the lock, so a second VM attaching the same image
/// read-write fails fast instead of silently corrupting it. Read-only disks aren't locked.
fn lock_writable_disks(disks: &[DiskSpec]) -> Result<Vec<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let mut guards = Vec::new();
    for disk in disks {
        if disk.read_only {
            continue;
        }
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk.path)
            .with_context(|| format!("opening disk {:?} to lock it", disk.path))?;
        // SAFETY: `f` owns a valid fd for the duration of the call.
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::ensure!(
                err.raw_os_error() != Some(libc::EWOULDBLOCK),
                "disk {:?} is already attached read-write to another running VM; \
                 attach it `:ro` to share it, or stop the other VM",
                disk.path
            );
            return Err(err).with_context(|| format!("flock {:?}", disk.path));
        }
        guards.push(f);
    }
    Ok(guards)
}

/// Parse `aa:bb:cc:dd:ee:ff` into MAC bytes.
fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let mut mac = [0u8; 6];
    let parts: Vec<&str> = s.split(':').collect();
    anyhow::ensure!(
        parts.len() == 6,
        "--net-mac must be six colon-separated hex octets, got {s:?}"
    );
    for (b, p) in mac.iter_mut().zip(parts) {
        *b = u8::from_str_radix(p, 16)
            .map_err(|_| anyhow::anyhow!("--net-mac has a non-hex octet {p:?} in {s:?}"))?;
    }
    Ok(mac)
}

/// Raise RLIMIT_NOFILE's soft limit toward the hard cap (clamped to OPEN_MAX, which macOS
/// always accepts). The venus render server spends one fd per guest shm blob; under the
/// launchd 256-fd default a GNOME login's context burst hits EMFILE and every blob/context
/// create fails (a guest-login crash class). The supervisor raises before spawning us, but
/// raise here too so direct/dev invocations of the worker are covered. Mirrors
/// limina::raise_fd_limit — keep the two in sync.
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

/// Worker logging. Default filter is warn-and-up so a production run is quiet (the
/// per-frame GPU device chatter lives at trace/debug); RUST_LOG overrides it — e.g.
/// RUST_LOG=debug for venus host-init, RUST_LOG=trace for the present-path DIAGs.
///
/// The sink is NON-BLOCKING by default: log writes land in a large bounded channel
/// drained by a writer thread, and when the channel is full new lines are DROPPED
/// (and counted) instead of stalling the emitter. Synchronous stderr writes from the
/// GPU worker/fence threads measurably destabilize frame pacing — RUST_LOG=trace
/// benchmarking produced ~2k blocking writes/s and a 40–60 fps ping-pong;
/// with log-heavy debugging the *logging* must not become the stall
/// under investigation. Drops are reported to stderr (directly, bypassing the
/// channel) by a watcher thread, so a lossy capture is always self-identifying.
/// `LIMINA_LOG_BLOCKING=1` restores the plain synchronous logger for the runs where
/// every line matters more than pacing.
fn init_worker_logging() {
    let env = env_logger::Env::default().default_filter_or("warn");
    if std::env::var_os("LIMINA_LOG_BLOCKING").is_some() {
        // Microsecond timestamps: per-frame work (present, fences) is sub-millisecond —
        // second-resolution stamps make inter-event timing unreadable in trace captures.
        env_logger::Builder::from_env(env)
            .format_timestamp_micros()
            .init();
        return;
    }
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .buffered_lines_limit(128_000)
        .finish(std::io::stderr());
    // Worker-lifetime logger: the guard's Drop would flush the channel tail at exit,
    // but the worker's exit paths are process-terminal (PSCI teardown / exec) and a
    // static guard is simpler than threading it through. Tail lines at exit may drop.
    std::mem::forget(guard);
    let errors = writer.error_counter();
    let _ = std::thread::Builder::new()
        .name("log-drop-watch".into())
        .spawn(move || {
            let mut last = 0usize;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let now = errors.dropped_lines();
                if now > last {
                    // Direct stderr, NOT the logger: the report must get out exactly
                    // when the channel is the thing that's full.
                    eprintln!(
                        "worker log: {} lines dropped by the non-blocking logger \
                         (total {now}); set LIMINA_LOG_BLOCKING=1 if every line matters",
                        now - last
                    );
                    last = now;
                }
            }
        });
    env_logger::Builder::from_env(env)
        .target(env_logger::Target::Pipe(Box::new(writer)))
        .format_timestamp_micros()
        .init();
}

/// Default ceiling, in MiB, on host GPU memory held on the guest's behalf.
///
/// The guest cannot see this memory. A guest that leaks `VkDeviceMemory` grows the *host*
/// worker, so its own OOM killer never fires and the guest's memory graphs stay flat;
/// what happens instead is that macOS eventually picks the worker as the largest
/// compressed process and SIGKILLs it, killing the VM with no guest backtrace, no crash
/// report, and at a moment unrelated to the allocation at fault. (2026-08-06: a Vulkan
/// compositor re-allocated a 4K backdrop texture instead of reusing it — ~51 GB/hour —
/// and the VM died at 142 GB. See `spikes/wallpaper-backdrop-leak/`.)
///
/// Capping it trades that for one guest client losing its GPU context, with the culprit
/// and its allocation histogram named in the worker log, while the VM and every other
/// client keep running. It is a context kill rather than a returned error because venus
/// submits `vkAllocateMemory` asynchronously and never reads the host's result — full
/// reasoning in `vkr_budget.h`.
///
/// Deliberately generous: this is a runaway backstop, not a working-set limit. A healthy
/// desktop guest holds well under its own RAM in host GPU memory, and the two-tier
/// guarantee means a stock guest must never trip this in normal use — so the number is
/// set far above any legitimate workload while staying far below the footprint that gets
/// a worker killed.
fn default_gpu_mem_budget_mib(ram_mib: usize) -> usize {
    const FLOOR_MIB: usize = 8192;
    std::cmp::max(FLOOR_MIB, ram_mib.saturating_mul(2))
}

fn main() -> Result<()> {
    raise_fd_limit();

    // Before anything else: the worker inherits the supervisor's stderr, so a Dock-launched
    // Limina.app silences the worker's panics exactly as it silences the supervisor's. Both
    // append to the same log; each record names its own binary and pid.
    limina_paniclog::install(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

    init_worker_logging();

    // Upstream KosmicKrisp gates its VK_EXT_custom_border_color (+ border_color_swizzle)
    // implementation behind MESA_KK_EXPERIMENTAL=custom_border, default OFF. zink-on-KK
    // needs the extension for its GL 3.2+ tier, and since the MTL4 rebase we
    // adopt upstream's implementation instead of carrying our own advertise (mechanism
    // upstream, policy here). Respect an explicit override from the environment.
    if std::env::var_os("MESA_KK_EXPERIMENTAL").is_none() {
        std::env::set_var("MESA_KK_EXPERIMENTAL", "custom_border");
    }

    let cli = Cli::parse();

    // Bound the host GPU memory the guest can hold (mechanism: virglrenderer's
    // `vkr_budget.c`, which reads this once at its first allocation). Policy is here
    // because guest RAM is here. An explicit setting always wins; `0` keeps the ledger
    // and its diagnostics but lifts the cap.
    if std::env::var_os("LIMINA_GPU_MEM_BUDGET_MIB").is_none() {
        std::env::set_var(
            "LIMINA_GPU_MEM_BUDGET_MIB",
            default_gpu_mem_budget_mib(cli.ram_mib).to_string(),
        );
    }

    // clap guarantees exactly one of --firmware / --kernel is present.
    let boot = match cli.kernel {
        Some(image) => BootSource::Kernel(KernelSpec {
            image,
            initramfs: cli.initramfs,
            cmdline: cli.cmdline,
        }),
        None => BootSource::Firmware(cli.firmware.expect("clap requires firmware or kernel")),
    };

    // Disks attach in order: the first `--disk` is `vda` (block id "root", preserved so a
    // pre-M10 single-disk snapshot still matches), the next `vdb` ("disk1"), … `--read-only`
    // is a back-compat alias for `:ro` on the first disk.
    let disks: Vec<DiskSpec> = cli
        .disk
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let (path, mut read_only) = parse_disk_attach(spec)?;
            if i == 0 && cli.read_only {
                read_only = true;
            }
            let id = if i == 0 {
                "root".to_string()
            } else {
                format!("disk{i}")
            };
            Ok(DiskSpec {
                id,
                path,
                read_only,
            })
        })
        .collect::<Result<_>>()?;

    // Take a host-side advisory exclusive lock on every WRITABLE backing file and hold it for
    // the VM's lifetime, so the same image can't be attached read-write to two running VMs (a
    // silent corruption footgun). Read-only disks may be shared. Held in `main` scope across
    // `krun::boot` (which never returns on a clean shutdown).
    let _disk_locks = lock_writable_disks(&disks)?;

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
                    pool: cli.display_pool,
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

    let host_sleep_s2idle = match cli.on_host_sleep.as_str() {
        "s2idle" => true,
        "ignore" => false,
        other => anyhow::bail!("--on-host-sleep must be 's2idle' or 'ignore', got {other:?}"),
    };

    let net_mac = cli.net_mac.as_deref().map(parse_mac).transpose()?;
    let net = cli.net_gvproxy.map(|gvproxy_socket| NetSpec {
        gvproxy_socket,
        mac: net_mac,
    });

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
        spice_fd: cli.spice_fd,
        display,
        input,
        net,
        battery: !cli.no_battery,
        snd: !cli.no_snd,
        mic: cli.mic,
        usb: cli.usb,
        fido_socket: cli.fido_socket,
        moc_socket: cli.moc_socket,
        free_page_reporting: cli.balloon_free_page_reporting,
        deflate_on_oom: cli.balloon_deflate_on_oom,
        snapshot_file: cli.snapshot_file,
        restore_file: cli.restore,
        host_sleep_s2idle,
        smbios_oem_strings: cli.smbios_oem_string,
    };

    krun::boot(&spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn gpu_mem_budget_is_a_runaway_backstop_not_a_working_set_limit() {
        // Small guests get the floor, not 2x — 2 GiB of guest RAM does not mean 4 GiB is
        // a sane ceiling on host GPU memory (one 4K swapchain plus a browser's textures
        // already runs to hundreds of MiB, and headroom is what keeps a legitimate
        // workload from ever meeting the cap).
        assert_eq!(default_gpu_mem_budget_mib(2048), 8192);
        assert_eq!(default_gpu_mem_budget_mib(4096), 8192);
        // Past the floor it tracks guest RAM, so a big VM gets proportionate headroom.
        assert_eq!(default_gpu_mem_budget_mib(8192), 16384);
        assert_eq!(default_gpu_mem_budget_mib(16384), 32768);
        // The leak that motivated this reached 142 GB; every default is far below it.
        assert!(default_gpu_mem_budget_mib(65536) < 142_000);
        // Never zero — zero means "no cap" to the renderer, so the degenerate input must
        // not silently disable enforcement.
        assert!(default_gpu_mem_budget_mib(0) > 0);
    }

    #[test]
    fn parse_mac_accepts_colon_hex_and_rejects_garbage() {
        assert_eq!(
            parse_mac("5a:94:ef:e4:0c:ee").unwrap(),
            [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee]
        );
        assert_eq!(
            parse_mac("AA:BB:CC:00:11:22").unwrap(),
            [0xaa, 0xbb, 0xcc, 0x00, 0x11, 0x22]
        );
        assert!(parse_mac("5a:94:ef:e4:0c").is_err()); // five octets
        assert!(parse_mac("zz:94:ef:e4:0c:ee").is_err()); // non-hex
        assert!(parse_mac("5a94efe40cee").is_err()); // no colons
    }

    #[test]
    fn parse_disk_attach_ro_suffix() {
        let (p, ro) = parse_disk_attach("/d/a.raw").unwrap();
        assert_eq!(p, PathBuf::from("/d/a.raw"));
        assert!(!ro);
        let (p, ro) = parse_disk_attach("/d/a.raw:ro").unwrap();
        assert_eq!(p, PathBuf::from("/d/a.raw"));
        assert!(ro);
        // Interior colon is part of the path.
        let (p, ro) = parse_disk_attach("/weird:dir/a.raw").unwrap();
        assert_eq!(p, PathBuf::from("/weird:dir/a.raw"));
        assert!(!ro);
        assert!(parse_disk_attach(":ro").is_err());
    }

    /// The id scheme: first disk keeps the historical "root" id (so a pre-M10 single-disk
    /// snapshot still matches), the rest are positional "disk1", "disk2", … This mirrors the
    /// id assignment in `main`.
    #[test]
    fn positional_block_ids_keep_root_first() {
        let ids: Vec<String> = (0..3)
            .map(|i| {
                if i == 0 {
                    "root".to_string()
                } else {
                    format!("disk{i}")
                }
            })
            .collect();
        assert_eq!(ids, ["root", "disk1", "disk2"]);
    }

    #[test]
    fn lock_writable_disks_blocks_a_second_rw_attach_but_allows_ro() {
        let dir = std::env::temp_dir().join(format!("limina-vmm-locktest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.raw");
        std::fs::write(&path, [0u8; 4096]).unwrap();

        let rw = DiskSpec {
            id: "root".into(),
            path: path.clone(),
            read_only: false,
        };
        let ro = DiskSpec {
            id: "root".into(),
            path: path.clone(),
            read_only: true,
        };

        // First writable attach takes the lock.
        let guard = lock_writable_disks(std::slice::from_ref(&rw)).unwrap();
        assert_eq!(guard.len(), 1);
        // A second writable attach of the SAME file is refused.
        assert!(lock_writable_disks(std::slice::from_ref(&rw)).is_err());
        // A read-only attach is allowed (and takes no lock).
        assert!(lock_writable_disks(std::slice::from_ref(&ro))
            .unwrap()
            .is_empty());

        // Dropping the guard releases the lock so it can be taken again.
        drop(guard);
        let again = lock_writable_disks(std::slice::from_ref(&rw)).unwrap();
        // Sanity: the lock fd is real.
        assert!(again[0].as_raw_fd() >= 0);
        drop(again);

        std::fs::remove_dir_all(&dir).ok();
    }
}
