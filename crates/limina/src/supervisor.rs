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
//! - if the guest doesn't power off within the grace period, escalate to SIGKILL.
//! - map the worker's exit to a VM-stopped outcome and report it.
//!
//! Reboot: libkrun is single-shot — a guest reboot (PSCI `SYSTEM_RESET`) tears the worker
//! process down just like a power-off. Our libkrun patch makes it exit with a *distinct* code
//! ([`WORKER_EXIT_REBOOT`]) so [`run`] can tell the two apart and **relaunch the worker** (a
//! fresh boot) on reboot, while the supervisor — and the resources it owns (gvproxy, the
//! control plane) — survive. A boot-loop guard stops endless relaunches.

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
    /// How long to wait for an orderly guest power-off before SIGKILL.
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

/// A spawned worker, plus the host ends of the channels this function created for it.
pub struct Spawned {
    pub child: std::process::Child,
    /// Host end of the guest's `com.redhat.spice.0` port — hand it to
    /// [`crate::control::ControlPlane::attach_vdagent`] to get a clipboard.
    pub spice_host: OwnedFd,
}

/// Spawn the worker in its own process group. `inherit_fds` are extra file descriptors
/// the child should keep open across exec (the windowed control channel) — Rust sets
/// `O_CLOEXEC` on fds it doesn't know about, so we clear it via `pre_exec`.
///
/// The SPICE agent port is created **here**, for every spawn, rather than at each call
/// site. Two reasons: the guest's device topology must not depend on how the VM was
/// started (a headless run and a windowed run that differ in device count would restore
/// each other's snapshots wrong), and "one port per spawn" is what makes the broker's
/// announce-once-per-open rule fall out for free on reboot and resume.
pub fn spawn_worker(spec: &WorkerSpec, inherit_fds: &[i32]) -> Result<Spawned> {
    install_signal_handlers()?;
    install_panic_kill_hook();
    let mut cmd = Command::new(&spec.vmm_bin);
    cmd.args(&spec.args).process_group(0);

    let (spice_host, spice_worker) = socketpair(libc::SOCK_STREAM)?;
    cmd.arg("--spice-fd")
        .arg(spice_worker.as_raw_fd().to_string());
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
    {
        let mut fds = inherit_fds.to_vec();
        fds.push(spice_worker.as_raw_fd());
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
    WORKER_PID.store(child.id() as i32, Ordering::Release);
    Ok(Spawned { child, spice_host })
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

/// Monitor an already-spawned worker until it exits, honoring the stop signal and grace
/// period. Returns the process exit code (or `128 + signal`).
///
/// Shutdown ladder on SIGINT/SIGTERM: ask the guest **agent** over the control plane
/// (orderly, the guest runs its own shutdown path) → after [`crate::control::AGENT_GRACE`]
/// fall back to SIGTERM → worker → shutdown eventfd → GPIO power button (stock guests with
/// no agent — most ignore it too) → after `grace`, SIGKILL.
pub fn monitor(
    mut child: std::process::Child,
    grace: Duration,
    control: Option<&crate::control::ControlPlane>,
) -> Result<i32> {
    let pid = child.id() as libc::pid_t;
    let mut shutdown_at: Option<Instant> = None;
    let mut sigterm_sent = false;
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
            if t.elapsed() >= grace || force_stop_requested() {
                if force_stop_requested() {
                    log::warn!(
                        "force stop requested (second signal); skipping the grace (SIGKILL)"
                    );
                } else {
                    log::warn!("guest did not power off within {grace:?}; forcing (SIGKILL)");
                }
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
}
