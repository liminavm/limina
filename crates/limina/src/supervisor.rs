// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Child-process supervision for the limina-vmm worker (decision D3).
//!
//! `krun_start_enter`'s equivalent (our event loop) blocks forever and the guest
//! power-off path calls `libc::exit` inside `krun-vmm`, tearing the worker down. So
//! the worker is a disposable child and this supervisor — which becomes the limina UI's
//! process — must survive it, drive shutdown, and report the outcome.
//!
//! Lifecycle:
//! - spawn the worker in its **own process group** so a terminal Ctrl-C (SIGINT to
//!   the foreground group) hits only us, not the worker; we forward shutdown explicitly.
//! - on SIGINT/SIGTERM: ask the guest to power off (SIGTERM → worker → shutdown eventfd).
//! - if the guest doesn't power off within the grace period, say so and keep waiting: an
//!   ordinary stop never SIGKILLs a running guest. Only an explicit force does (a second stop
//!   signal — double Ctrl-C, `limina stop --force` — or Force Stop in the window's menu).
//! - map the worker's exit to a VM-stopped outcome and report it.
//!
//! Reboot: libkrun is single-shot — a guest reboot (PSCI `SYSTEM_RESET`) tears the worker
//! process down just like a power-off. Our libkrun patch makes it exit with a *distinct* code
//! ([`WORKER_EXIT_REBOOT`]) so [`run`] can tell the two apart and **relaunch the worker** (a
//! fresh boot) on reboot, while the supervisor — and the resources it owns (gvproxy, the
//! control plane) — survive. A boot-loop guard stops endless relaunches.

use std::ffi::OsStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use std::os::unix::process::{CommandExt, ExitStatusExt};

/// Worker process exit code meaning "the guest rebooted" (PSCI `SYSTEM_RESET`) — mirrors
/// libkrun's `FC_EXIT_CODE_REBOOT` (a clean power-off exits 0). Kept in sync by hand: limina
/// spawns the worker as a subprocess and reads only its exit status, so it can't import the
/// constant from `krun-vmm`.
pub const WORKER_EXIT_REBOOT: i32 = 125;

/// Worker process exit code meaning "the guest was snapshotted" (M9 suspend/resume): the worker
/// wrote its VM snapshot to `--snapshot-file` on a SIGUSR1 trigger and tore itself down. Unlike a
/// reboot this does **not** relaunch — suspend is teardown + resume-on-next-start, and the decision
/// to resume is durable per-VM policy (a persisted `Suspended{snapshot}` status read at start),
/// never an in-memory supervisor relaunch. So [`should_relaunch`](RebootGuard::should_relaunch)
/// leaves it alone (it only ever relaunches [`WORKER_EXIT_REBOOT`]); we just report it distinctly.
pub const WORKER_EXIT_SNAPSHOT: i32 = 126;

/// A worker that exits with [`WORKER_EXIT_REBOOT`] after running less than this is treated as a
/// boot loop, not a healthy reboot.
const MIN_HEALTHY_UPTIME: Duration = Duration::from_secs(5);
/// Give up relaunching after this many back-to-back rapid reboots (boot-loop backstop).
const MAX_RAPID_REBOOTS: u32 = 5;

/// Bound on the whole M9.2 suspend bracket (worker: pulse suspend button → wait the guest to
/// s2idle-quiesce [≤20s] → snapshot [seconds for a 1–2 GB image]). If the worker hasn't exited 126
/// by now the guest could not quiesce; the supervisor gives up and the VM keeps running.
pub(crate) const SUSPEND_BRACKET_TIMEOUT: Duration = Duration::from_secs(60);

/// Decides whether a worker exit should relaunch the VM (a guest reboot) or end it, capping
/// runaway boot loops. Shared by the headless [`run`] loop and the windowed relaunch loop so
/// the reboot policy lives in one place.
#[derive(Default)]
pub struct RebootGuard {
    rapid_reboots: u32,
}

impl RebootGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Given a finished worker's exit `code` and how long it ran (`uptime`), return whether to
    /// relaunch. True only for a guest reboot that isn't a stop-in-progress and hasn't degenerated
    /// into a boot loop (too many reboots each under [`MIN_HEALTHY_UPTIME`]).
    pub fn should_relaunch(&mut self, code: i32, uptime: Duration) -> bool {
        if code != WORKER_EXIT_REBOOT || stop_requested() {
            return false;
        }
        if uptime < MIN_HEALTHY_UPTIME {
            self.rapid_reboots += 1;
            if self.rapid_reboots >= MAX_RAPID_REBOOTS {
                log::error!(
                    "guest rebooted {} times in under {MIN_HEALTHY_UPTIME:?} each — stopping the \
                     VM (boot loop?)",
                    self.rapid_reboots
                );
                return false;
            }
        } else {
            self.rapid_reboots = 0;
        }
        true
    }
}

/// Set by the SIGINT/SIGTERM handler; observed by the monitor loop.
static STOP: AtomicBool = AtomicBool::new(false);
/// Counts stop signals: the FIRST asks for the graceful ladder, a SECOND means
/// "skip the remaining grace, kill now" (`limina stop --force`, an impatient
/// double Ctrl-C). Only ever incremented from the handler.
static SIG_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

extern "C" fn on_signal(_sig: libc::c_int) {
    SIG_COUNT.fetch_add(1, Ordering::SeqCst);
    STOP.store(true, Ordering::SeqCst);
}

/// Set by the SIGTSTP handler (a `limina suspend` request relayed to the supervisor pid); observed
/// by the monitor loop, which relays it to the worker as the M9.2 suspend bracket. Distinct from
/// [`STOP`] — suspend is snapshot-and-teardown-to-resume, not power-off.
static SUSPEND: AtomicBool = AtomicBool::new(false);

extern "C" fn on_suspend(_sig: libc::c_int) {
    SUSPEND.store(true, Ordering::SeqCst);
}

/// Ask for the same graceful stop a SIGTERM triggers — used by the windowed
/// session's quit-Apple-event handler (osascript "quit", logout), which must never
/// exit the supervisor abruptly and orphan the worker.
pub fn request_stop() {
    STOP.store(true, Ordering::SeqCst);
}

/// Ask for the M9.2 suspend bracket exactly as `limina suspend`'s SIGTSTP does — used by the
/// windowed session's close-to-suspend policy (M9.4). Observed by the monitor loop, which
/// relays SIGTSTP to the worker; on success the worker exits [`WORKER_EXIT_SNAPSHOT`].
pub fn request_suspend() {
    SUSPEND.store(true, Ordering::SeqCst);
}

/// Is a suspend request pending (bracket running or about to)? The window polls this to
/// show the suspend overlay for CLI-triggered (`limina suspend`) suspends too, not just
/// close-triggered ones. Cleared by the monitor when a bracket times out.
pub fn suspend_requested() -> bool {
    SUSPEND.load(Ordering::SeqCst)
}

/// Clear a satisfied suspend request. A successful bracket leaves the flag set (the worker
/// exits before the monitor loop clears anything); the parked window (task #18) must clear
/// it before resuming, or `monitor()` would SIGTSTP the freshly-respawned worker straight
/// back into a suspend.
pub fn clear_suspend_request() {
    SUSPEND.store(false, Ordering::SeqCst);
}

/// Ask for an IMMEDIATE forceful stop (SIGKILL, no grace) — the window menu's Force Stop.
/// Equivalent to a stop request plus the impatient second signal, which every ladder site
/// already honors via [`force_stop_requested`].
pub fn request_force_stop() {
    SIG_COUNT.fetch_add(2, Ordering::SeqCst);
    STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() -> Result<()> {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut()) != 0
            || libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0
        {
            anyhow::bail!(
                "installing signal handlers: {}",
                std::io::Error::last_os_error()
            );
        }

        // SIGTSTP → suspend request (M9.2). Its default disposition is *stop the process* (job
        // control); a handler overrides that. The supervisor runs in its own process group with no
        // controlling terminal, so SIGTSTP only ever arrives from `limina suspend` relaying it.
        let mut sus: libc::sigaction = std::mem::zeroed();
        sus.sa_sigaction = on_suspend as *const () as usize;
        libc::sigemptyset(&mut sus.sa_mask);
        sus.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGTSTP, &sus, std::ptr::null_mut()) != 0 {
            anyhow::bail!(
                "installing SIGTSTP handler: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

/// What to launch and how patiently to shut it down.
pub struct WorkerSpec {
    /// Path to the (codesigned) limina-vmm binary.
    pub vmm_bin: PathBuf,
    /// Arguments forwarded to the worker. NEVER contains `--restore`: resume is decided
    /// per-spawn by [`take_pending_resume`] inside [`spawn_worker`], precisely so a reboot
    /// relaunch (which reuses these args) can't re-apply a consumed snapshot.
    pub args: Vec<String>,
    /// How long to wait for an orderly guest power-off before reporting that the guest is
    /// still running. Not a kill deadline — see [`monitor`].
    pub shutdown_grace: Duration,
    /// Armed suspend snapshot path (mirrors the worker's `--snapshot-file`). Consulted at
    /// every spawn: a snapshot present here IS a pending resume. `None` = suspend not armed.
    pub snapshot_file: Option<PathBuf>,
    /// Managed-VM `state.toml` whose `[suspended]` record is cleared when a pending resume
    /// is consumed (the record is UI status; the snapshot file is the source of truth).
    pub suspend_state_file: Option<PathBuf>,
}

/// Borrowed view of the suspend/auto-resume paths, for handing a spawn site both halves of
/// [`WorkerSpec`]'s resume state in one argument.
#[derive(Clone, Copy)]
pub struct ResumePaths<'a> {
    /// See [`WorkerSpec::snapshot_file`].
    pub snapshot_file: Option<&'a std::path::Path>,
    /// See [`WorkerSpec::suspend_state_file`].
    pub suspend_state_file: Option<&'a std::path::Path>,
}

/// M9.4 auto-resume: the armed snapshot path IS the resume-pending record. If `snapshot`
/// exists, the VM was suspended and this boot MUST restore from it (cold-booting would leave a
/// stale snapshot behind that a later start would apply over an advanced disk); if it doesn't,
/// the boot MUST be cold (there is nothing valid to restore). There is deliberately **no other
/// way** to request a restore — no `--restore` flag on `limina` — because every mismatch
/// between "a suspend exists" and "we are restoring" destroys data in one direction or the
/// other.
///
/// Called at EVERY worker spawn — first boot and reboot relaunch alike — and CONSUMES the
/// snapshot when found: rename to `.consumed` + clear the `[suspended]` record, so the next
/// spawn in the same session (a guest reboot) finds nothing and cold-boots. This
/// one-shot-by-construction shape replaced a `--restore` argv flag after that argv rode a
/// reboot relaunch and re-applied the stale snapshot over the advanced disk (btrfs "parent
/// transid verify failed" — destroyed a dogfood guest's filesystem).
///
/// Returns the path the worker must restore from (normally the `.consumed` name; the
/// canonical name only if the rename failed and we degrade to restoring in place).
pub fn take_pending_resume(
    snapshot: &std::path::Path,
    state_file: Option<&std::path::Path>,
) -> Option<PathBuf> {
    if !snapshot.exists() {
        // Reconcile a stale [suspended] record pointing at a missing snapshot so status
        // stops claiming a resume that can't happen.
        if let Some(state) = state_file {
            if crate::vmlib::state::load(state)
                .and_then(|s| s.suspended)
                .is_some()
            {
                log::warn!(
                    "state.toml records a suspend but snapshot {} is missing; cold-booting",
                    snapshot.display()
                );
                let _ = crate::vmlib::state::set_suspended(state, None);
            }
        }
        return None;
    }
    log::info!("resume pending: restoring from {}", snapshot.display());
    if let Some(state) = state_file {
        if let Err(e) = crate::vmlib::state::set_suspended(state, None) {
            log::warn!("clearing the suspended state failed: {e}; continuing");
        }
    }
    // SINGLE-USE enforcement (M9.4-1b): rename the snapshot out of its canonical name before
    // the worker reads it. A snapshot is only valid against the disk EXACTLY as the suspend
    // left it — the resumed guest immediately advances the disk, and a second restore of the
    // same snapshot writes stale fs metadata over it. After the rename, nothing — a stale
    // state.toml copy, a reboot relaunch, a re-run — can find the consumed snapshot at its
    // canonical path. The next suspend writes the canonical name fresh.
    let consumed = snapshot.with_extension("bin.consumed");
    match std::fs::rename(snapshot, &consumed) {
        Ok(()) => Some(consumed),
        Err(e) => {
            // Degrade: restore from the canonical path (worse invalidation, still a cleared
            // state record) rather than failing the resume.
            log::warn!("marking the snapshot consumed failed: {e}; restoring in place");
            Some(snapshot.to_path_buf())
        }
    }
}

/// Create a socketpair of the given type; set CLOEXEC on the supervisor end (fd.0) so the
/// worker doesn't inherit it. Returns `(supervisor_end, worker_end)` as **owned** fds, so
/// every path that drops them (notably an error return before the spawn completes) closes
/// them instead of leaking.
pub fn socketpair(sock_type: libc::c_int) -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, sock_type, 0, fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("socketpair: {}", std::io::Error::last_os_error());
    }
    // SAFETY: on success the kernel handed us two fresh, open fds; we take sole ownership.
    let (sup_fd, worker_fd) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    unsafe {
        let f = libc::fcntl(sup_fd.as_raw_fd(), libc::F_GETFD);
        libc::fcntl(sup_fd.as_raw_fd(), libc::F_SETFD, f | libc::FD_CLOEXEC);
        // Writing to a socket whose peer (the worker) has exited must fail with an error,
        // not raise SIGPIPE and kill the supervisor (macOS has no MSG_NOSIGNAL).
        let on: libc::c_int = 1;
        libc::setsockopt(
            sup_fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok((sup_fd, worker_fd))
}

/// The vCPU scheduling policy the worker runs under unless the environment says otherwise.
///
/// Mechanism is libkrun's (`macos/vcpu_sched.rs`); which policy to run is ours. `rt+dyn` puts a
/// vCPU thread into a real-time band only while that thread is mostly idle — the state where a
/// punctual timer wake is what a guest needs, and the state where the reservation costs the host
/// nothing. Without it an idle guest's frame clock slips a whole refresh at a time
/// (docs/hardening-backlog.md, "An idle guest misses frame deadlines").
const DEFAULT_VCPU_SCHED: &str = "rt+dyn";

/// What to set `LIMINA_VCPU_SCHED` to for the worker, given what the environment already carries.
///
/// Anything explicit wins, including an empty value — that is how a run turns the band off — and
/// the older `LIMINA_VCPU_RT` spelling counts as explicit too, or setting it would silently gain a
/// dynamic policy it never asked for.
fn worker_vcpu_sched(sched: Option<&OsStr>, legacy_rt: Option<&OsStr>) -> Option<&'static str> {
    if sched.is_some() || legacy_rt.is_some() {
        None
    } else {
        Some(DEFAULT_VCPU_SCHED)
    }
}

/// A spawned worker, plus the host ends of the channels this function created for it.
pub struct Spawned {
    pub child: std::process::Child,
    /// Host end of the guest's `com.redhat.spice.0` port — hand it to
    /// [`crate::control::ControlPlane::attach_vdagent`] to get a clipboard.
    pub spice_host: OwnedFd,
    /// Host end of the guest's `org.qemu.guest_agent.0` port — hand it to
    /// [`crate::control::ControlPlane::attach_qga`] to reach a stock guest's
    /// `qemu-guest-agent`.
    pub qga_host: OwnedFd,
}

/// Spawn the worker in its own process group. `inherit_fds` are extra file descriptors
/// the child should keep open across exec (the windowed control channel) — Rust sets
/// `O_CLOEXEC` on fds it doesn't know about, so we clear it via `pre_exec`.
///
/// The two stock-agent ports (SPICE's `com.redhat.spice.0`, QEMU's
/// `org.qemu.guest_agent.0`) are created **here**, for every spawn, rather than at each call
/// site. Two reasons: the guest's device topology must not depend on how the VM was
/// started (a headless run and a windowed run that differ in device count would restore
/// each other's snapshots wrong), and "one port per spawn" is what makes the broker's
/// announce-once-per-open rule fall out for free on reboot and resume.
pub fn spawn_worker(spec: &WorkerSpec, inherit_fds: &[i32]) -> Result<Spawned> {
    install_signal_handlers()?;
    install_panic_kill_hook();
    let mut cmd = Command::new(&spec.vmm_bin);
    cmd.args(&spec.args).process_group(0);

    if let Some(sched) = worker_vcpu_sched(
        std::env::var_os("LIMINA_VCPU_SCHED").as_deref(),
        std::env::var_os("LIMINA_VCPU_RT").as_deref(),
    ) {
        cmd.env("LIMINA_VCPU_SCHED", sched);
    }

    let (spice_host, spice_worker) = socketpair(libc::SOCK_STREAM)?;
    cmd.arg("--spice-fd")
        .arg(spice_worker.as_raw_fd().to_string());
    let (qga_host, qga_worker) = socketpair(libc::SOCK_STREAM)?;
    cmd.arg("--qga-fd").arg(qga_worker.as_raw_fd().to_string());
    // Auto-resume (M9.4): decided HERE, per spawn, never via spec.args — see
    // `take_pending_resume` for why (a reboot relaunch must cold-boot, not re-restore).
    if let Some(snap) = &spec.snapshot_file {
        if let Some(pending) = take_pending_resume(snap, spec.suspend_state_file.as_deref()) {
            cmd.arg("--restore").arg(&pending);
        }
    }

    // When running from an assembled limina.app, hand the worker the bundle-relative venus
    // env (KK ICD + zink-on-KK Mesa selectors). In a dev/cargo run no bundle is present, so
    // this is a no-op and the inherited env (boot-seated-kk.sh) stands.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(envs) = crate::venus_env::bundle_venus_env(dir, |p| p.exists()) {
                log::info!(
                    "limina.app: using bundled venus stack ({} vars)",
                    envs.len()
                );
                for (k, v) in envs {
                    cmd.env(k, v);
                }
            }
        }
    }
    // KosmicKrisp's allocator-pool snapshot. The driver writes its current state to this path
    // every few encoder closes, truncating each time, so a worker that dies leaves the pool's
    // shape behind — the stderr report can be thousands of closes stale when a rare fault lands.
    // Policy lives here rather than in the driver: managed VMs get it in their own logs dir, and
    // an explicit LIMINA_KK_POOL_SNAPSHOT always wins.
    if std::env::var_os("LIMINA_KK_POOL_SNAPSHOT").is_none() {
        let logs = spec
            .suspend_state_file
            .as_deref()
            .and_then(|st| st.parent())
            .map(|bundle| bundle.join("logs"))
            .filter(|logs| logs.is_dir());
        if let Some(logs) = logs {
            cmd.env("LIMINA_KK_POOL_SNAPSHOT", logs.join("kk-pool.txt"));
        }
    }

    {
        let mut fds = inherit_fds.to_vec();
        fds.push(spice_worker.as_raw_fd());
        fds.push(qga_worker.as_raw_fd());
        // SAFETY: only async-signal-safe fcntl calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                for &fd in &fds {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawning worker {:?}", spec.vmm_bin))?;
    log::info!(
        "VM worker started (pid {}); Ctrl-C to power off",
        child.id()
    );
    // The worker holds its own copy now; dropping ours means the broker's reader sees EOF
    // when the worker exits, which is how a reboot relaunch unblocks the old reader thread.
    drop(spice_worker);
    drop(qga_worker);
    WORKER_PID.store(child.id() as i32, Ordering::Release);
    Ok(Spawned {
        child,
        spice_host,
        qga_host,
    })
}

/// The live worker's pid — the leader of its own process group — for the panic hook. Zero
/// when no worker is running; cleared by [`monitor`] the moment the child is reaped, so a
/// recycled pid can never be mistaken for ours.
static WORKER_PID: AtomicI32 = AtomicI32::new(0);
static PANIC_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Make a supervisor panic take the VM down with it. The worker runs in its own process group
/// (so it survives a supervisor *relaunch*), which means a supervisor that merely panics
/// leaves a headless, orphaned guest running. The pointer stack asserts its invariants with
/// `assert!` on purpose — a crash is the loud, early signal wanted there — so a crash has to
/// mean the whole VM, not a ghost. Runs the default hook first (message + backtrace, and — since
/// `main` installs [`crate::panic_log`] before this one — the append to the panic log, which is
/// the only copy a Dock-launched app keeps), then SIGKILLs the worker's process group, then
/// aborts: a panicking non-main thread must not leave the window limping on without the
/// invariant that just failed.
fn install_panic_kill_hook() {
    if PANIC_HOOK_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let pid = WORKER_PID.swap(0, Ordering::AcqRel);
        if pid > 0 {
            eprintln!("limina: supervisor panic — killing the VM worker (process group {pid})");
            // SAFETY: plain signal send to the process group we spawned and still own.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        std::process::abort();
    }));
}

/// How long the guest gets to answer the GPIO power button before the stock guest agent is
/// asked instead. Sized against the default 20 s grace: agent 0–5 s, button 5–10 s, guest agent
/// from 10 s.
pub const BUTTON_GRACE: Duration = Duration::from_secs(5);

/// How much longer the guest gets once its stock agent has **accepted** a `guest-shutdown`.
///
/// The operator's grace is about a guest that will not answer; a guest that just took a real
/// shutdown request is answering, and SIGKILLing it mid-`systemd-shutdown` is how filesystems
/// get hurt. Measured on a seated F44 desktop, 2026-08-26: `shutdown -P +0` needs ~28 s from
/// request to the VM being gone (session teardown, unmounts, btrfs commit).
pub const QGA_GRACE: Duration = Duration::from_secs(45);

/// Is it time for the guest-agent rung? `elapsed` is measured from the shutdown request, and
/// `grace` is when the supervisor gives up *reporting* progress (it no longer kills).
///
/// Pure so the ordering is testable: the rung comes **after** the power button has had
/// [`BUTTON_GRACE`], and only when at least [`crate::control::AGENT_GRACE`] of the grace is
/// left — a rung asked in the last moment of a deliberately tight grace (the test harness uses
/// 3 s) is theatre.
fn qga_rung_due(elapsed: Duration, grace: Duration) -> bool {
    let due = crate::control::AGENT_GRACE + BUTTON_GRACE;
    elapsed >= due && due + crate::control::AGENT_GRACE <= grace
}

/// Monitor an already-spawned worker until it exits, honoring the stop signal and grace
/// period. Returns the process exit code (or `128 + signal`).
///
/// Shutdown ladder on SIGINT/SIGTERM, every rung a *request*: ask the guest **agent** over the
/// control plane (orderly, the guest runs its own shutdown path) → after
/// [`crate::control::AGENT_GRACE`] fall back to SIGTERM → worker → shutdown eventfd → GPIO power
/// button → after [`BUTTON_GRACE`] ask the stock `qemu-guest-agent` ([`QGA_GRACE`] more if it
/// takes it) → and then **wait**, saying once that the guest is still running.
///
/// Nothing here SIGKILLs a guest that simply refuses to power off. The kill is reserved for an
/// explicit force ([`force_stop_requested`]): a second stop signal, `limina stop --force`, or
/// Force Stop in the window's menu. A stop that killed on a timer would cost the user unsaved
/// work they never agreed to risk.
pub fn monitor(
    mut child: std::process::Child,
    grace: Duration,
    control: Option<&crate::control::ControlPlane>,
) -> Result<i32> {
    let pid = child.id() as libc::pid_t;
    let mut shutdown_at: Option<Instant> = None;
    let mut sigterm_sent = false;
    let mut qga_asked = false;
    let mut overrun_warned = false;
    // Extra time granted because the stock guest agent accepted a power-off.
    let mut agent_extra = Duration::ZERO;
    let mut suspend_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait().context("polling worker")? {
            return Ok(report_exit(status));
        }

        // M9.2 suspend bracket: relay a `limina suspend` (SIGTSTP to us) to the worker, which
        // pulses the guest suspend button, waits for the guest to s2idle-quiesce, snapshots it, and
        // exits 126 (caught by `try_wait` above → we return 126, the caller persists `[suspended]`).
        // If the guest can't quiesce (e.g. a virtiofs mount refuses s2idle) the worker wakes it and
        // keeps running — never exiting 126 — so we bound the wait and, on timeout, give up and let
        // the VM keep running, clearing the request so a later suspend can retry.
        if SUSPEND.load(Ordering::SeqCst) && suspend_at.is_none() && shutdown_at.is_none() {
            log::info!("suspend requested → running the suspend bracket (SIGTSTP → worker)");
            unsafe {
                libc::kill(pid, libc::SIGTSTP);
            }
            suspend_at = Some(Instant::now());
        }
        if let Some(t) = suspend_at {
            if t.elapsed() >= SUSPEND_BRACKET_TIMEOUT {
                log::warn!(
                    "suspend bracket did not complete within {SUSPEND_BRACKET_TIMEOUT:?} (guest \
                     could not quiesce — e.g. a virtiofs mount); the VM keeps running"
                );
                SUSPEND.store(false, Ordering::SeqCst);
                suspend_at = None;
            }
        }

        if STOP.load(Ordering::SeqCst) && shutdown_at.is_none() {
            let orderly = control
                .map(|c| c.request_shutdown(crate::control::AGENT_GRACE))
                .unwrap_or(false);
            if orderly {
                log::info!("shutdown requested → asking the guest agent to power off");
            } else {
                log::info!("shutdown requested → asking guest to power off");
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
                sigterm_sent = true;
            }
            shutdown_at = Some(Instant::now());
        }

        if let Some(t) = shutdown_at {
            if !sigterm_sent && t.elapsed() >= crate::control::AGENT_GRACE {
                log::warn!(
                    "agent did not power the guest off within {:?}; falling back to the power button",
                    crate::control::AGENT_GRACE
                );
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
                sigterm_sent = true;
            }
            // The last rung we have: a guest that ignored the power button may
            // still take `guest-shutdown` from its own stock agent, which runs `shutdown -P` as
            // root and so is not subject to the logind inhibitors a seated desktop holds. Only
            // when an agent has already answered — probing here would eat the remaining grace
            // on a guest that has none.
            if !qga_asked && sigterm_sent && qga_rung_due(t.elapsed(), grace) {
                qga_asked = true;
                if control.map(|c| c.request_qga_shutdown()).unwrap_or(false) {
                    // It accepted, so it is shutting down — hold the "still running" report
                    // until it has had time to finish.
                    agent_extra = QGA_GRACE;
                    log::warn!(
                        "the power button did not take within {:?}; asked the stock guest agent \
                         to power off instead (giving it {:?} more)",
                        BUTTON_GRACE,
                        QGA_GRACE
                    );
                }
            }

            // A guest that will not power off is NOT killed by an ordinary stop. Every rung
            // above is a request; if the guest ignores them all, the VM keeps running and says
            // so, and ending it stays an explicit human act (a second stop signal — double
            // Ctrl-C, `limina stop --force` — or Force Stop in the window's menu). Killing a
            // running guest because a timer expired is data loss the user never asked for.
            if !force_stop_requested() && t.elapsed() >= grace + agent_extra && !overrun_warned {
                overrun_warned = true;
                log::warn!(
                    "the guest has not powered off within {:?} and is still running. A stop \
                     never kills a VM on its own — stop again (double Ctrl-C, `limina stop \
                     --force`, or Force Stop in the menu) to SIGKILL it.",
                    grace + agent_extra
                );
            }

            if force_stop_requested() {
                log::warn!("force stop requested (second signal); SIGKILL");
                let _ = child.kill();
                let status = child.wait().context("waiting on worker after SIGKILL")?;
                return Ok(report_exit(status));
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

/// True once a SIGINT/SIGTERM has asked us to stop (observed by the window loop).
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// True once a SECOND stop signal has arrived: skip whatever grace remains and
/// kill immediately. Observed by both the headless monitor ladder and the
/// windowed quit check.
pub fn force_stop_requested() -> bool {
    SIG_COUNT.load(Ordering::SeqCst) >= 2
}

/// Spawn and supervise the worker until the VM stops (headless/non-windowed path).
///
/// A guest **power-off** (or signal-driven stop) returns the worker's exit code. A guest
/// **reboot** ([`WORKER_EXIT_REBOOT`]) instead **relaunches** the worker — a fresh boot of the
/// same VM — so the supervisor and its gvproxy/control-plane resources survive a guest reboot
/// the way real hardware would. A runaway boot loop (repeated reboots each under
/// [`MIN_HEALTHY_UPTIME`]) is capped at [`MAX_RAPID_REBOOTS`] so we don't spin forever.
pub fn run(
    spec: &WorkerSpec,
    control: Option<&crate::control::ControlPlane>,
    gateway: Option<&crate::gateway::Gateway>,
) -> Result<i32> {
    let mut guard = RebootGuard::new();
    loop {
        let started = Instant::now();
        let spawned = spawn_worker(spec, &[])?;
        // Clipboard for a stock guest: hand the fresh port to the broker. A relaunch
        // (reboot) replaces the previous one — the guest that owned it is gone.
        if let Some(cp) = control {
            if let Err(e) = cp.attach_vdagent(spawned.spice_host) {
                log::warn!("clipboard: no SPICE agent transport: {e:#}");
            }
            if let Err(e) = cp.attach_qga(spawned.qga_host) {
                log::warn!("qga: no guest-agent transport: {e:#}");
            }
        }
        let code = monitor(spawned.child, spec.shutdown_grace, control)?;

        // Relaunch only on a guest reboot (not power-off / error / stop / boot loop).
        if !guard.should_relaunch(code, started.elapsed()) {
            return Ok(code);
        }

        // Recycle the NAT gateway: gvproxy's vfkit socket is single-connection, so the fresh
        // worker can't reconnect to the old one. Restart it at the same path before re-spawning.
        if let Some(gw) = gateway {
            if let Err(e) = gw.restart() {
                log::error!("could not restart the NAT gateway for the reboot: {e:#}; stopping");
                return Ok(code);
            }
        }
        log::info!("guest rebooted (PSCI SYSTEM_RESET) → relaunching the VM worker");
    }
}

fn report_exit(status: ExitStatus) -> i32 {
    // The child is reaped: its pid may be recycled, so the panic hook must forget it.
    WORKER_PID.store(0, Ordering::Release);
    if let Some(code) = status.code() {
        if code == 0 {
            log::info!("VM powered off cleanly (worker exit 0)");
        } else if code == WORKER_EXIT_REBOOT {
            log::info!("guest rebooted (worker exit {WORKER_EXIT_REBOOT})");
        } else if code == WORKER_EXIT_SNAPSHOT {
            log::info!("guest snapshotted (worker exit {WORKER_EXIT_SNAPSHOT}); VM suspended");
        } else {
            log::warn!("VM stopped — worker exited with code {code}");
        }
        code
    } else {
        let sig = status.signal().unwrap_or(0);
        log::warn!("VM stopped — worker terminated by signal {sig}");
        128 + sig
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// The stop ladder's rungs must stay in order: agent, then power button, then the stock
    /// guest agent. Each of the first two needs room to work, so the guest-agent rung only
    /// comes up when the grace can still hold it *and* leave the button its own chance.
    #[test]
    fn the_guest_agent_rung_comes_up_after_the_power_button_has_had_its_turn() {
        let grace = Duration::from_secs(20);
        // Before the button has had its own grace: too early — the button is still the
        // guest's to answer.
        assert!(!qga_rung_due(Duration::from_secs(0), grace));
        assert!(!qga_rung_due(crate::control::AGENT_GRACE, grace));
        assert!(!qga_rung_due(
            crate::control::AGENT_GRACE + BUTTON_GRACE - Duration::from_millis(1),
            grace
        ));
        // Then it is due, with time left for `shutdown -P` to run before the supervisor
        // reports the guest as still running.
        assert!(qga_rung_due(
            crate::control::AGENT_GRACE + BUTTON_GRACE,
            grace
        ));
    }

    #[test]
    fn the_default_grace_leaves_the_rung_half_the_ladder() {
        // The default `--shutdown-grace-secs 20`: the rung must actually come up there, with
        // room for `shutdown -P` to get going. A constant sized past the grace would make this
        // whole rung dead code in production.
        let default_grace = Duration::from_secs(20);
        let due = crate::control::AGENT_GRACE + BUTTON_GRACE;
        assert!(qga_rung_due(due, default_grace));
        assert!(default_grace - due >= Duration::from_secs(10));
    }

    #[test]
    fn a_grace_too_short_to_hold_the_rung_skips_it_entirely() {
        // `limina stop` with a tight grace, or a test harness that wants a fast teardown:
        // asking an agent in the last moment of the grace is theatre, so the ladder behaves
        // exactly as it did before this rung existed.
        let tight = crate::control::AGENT_GRACE + BUTTON_GRACE;
        assert!(!qga_rung_due(tight, tight));
        assert!(!qga_rung_due(tight * 4, tight));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("limina-resume-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pending_resume_consumes_snapshot_and_record() {
        let dir = scratch("consume");
        let snap = dir.join("snapshot.bin");
        let state = dir.join("state.toml");
        std::fs::write(&snap, b"fake-snapshot").unwrap();
        crate::vmlib::state::set_suspended(
            &state,
            Some(crate::vmlib::state::Suspended {
                snapshot: snap.clone(),
            }),
        )
        .unwrap();

        let got = take_pending_resume(&snap, Some(&state)).expect("a pending resume");
        assert_eq!(got, dir.join("snapshot.bin.consumed"));
        assert!(!snap.exists(), "snapshot must leave its canonical name");
        assert!(got.exists(), "consumed snapshot must hold the payload");
        assert!(
            crate::vmlib::state::load(&state)
                .and_then(|s| s.suspended)
                .is_none(),
            "[suspended] record must be cleared at consume"
        );

        // The SAME check again (what a reboot relaunch does) must find nothing: one-shot.
        assert_eq!(take_pending_resume(&snap, Some(&state)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_snapshot_means_cold_boot_and_clears_stale_record() {
        let dir = scratch("stale");
        let snap = dir.join("snapshot.bin");
        let state = dir.join("state.toml");
        crate::vmlib::state::set_suspended(
            &state,
            Some(crate::vmlib::state::Suspended {
                snapshot: snap.clone(),
            }),
        )
        .unwrap();

        assert_eq!(take_pending_resume(&snap, Some(&state)), None);
        assert!(
            crate::vmlib::state::load(&state)
                .and_then(|s| s.suspended)
                .is_none(),
            "a record with no snapshot behind it must be reconciled away"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_explicit_vcpu_policy_always_beats_the_default() {
        // The default exists to make an idle guest punctual; a run that names a policy — including
        // an empty one, which is how the band gets turned off for an A/B — must get exactly that.
        assert_eq!(worker_vcpu_sched(None, None), Some("rt+dyn"));
        assert_eq!(worker_vcpu_sched(Some(OsStr::new("rt")), None), None);
        assert_eq!(worker_vcpu_sched(Some(OsStr::new("")), None), None);
        // The older spelling is explicit too: it means the static band, and silently upgrading it
        // to a dynamic one would change what a run measures without saying so.
        assert_eq!(worker_vcpu_sched(None, Some(OsStr::new("1"))), None);
    }
}
