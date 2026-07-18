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

use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
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
const SUSPEND_BRACKET_TIMEOUT: Duration = Duration::from_secs(60);

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

fn install_signal_handlers() -> Result<()> {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as usize;
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
        sus.sa_sigaction = on_suspend as usize;
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
    /// Arguments forwarded to the worker.
    pub args: Vec<String>,
    /// How long to wait for an orderly guest power-off before SIGKILL.
    pub shutdown_grace: Duration,
}

/// Spawn the worker in its own process group. `inherit_fds` are extra file descriptors
/// the child should keep open across exec (the windowed control channel) — Rust sets
/// `O_CLOEXEC` on fds it doesn't know about, so we clear it via `pre_exec`.
pub fn spawn_worker(spec: &WorkerSpec, inherit_fds: &[i32]) -> Result<std::process::Child> {
    install_signal_handlers()?;
    let mut cmd = Command::new(&spec.vmm_bin);
    cmd.args(&spec.args).process_group(0);

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
    if !inherit_fds.is_empty() {
        let fds = inherit_fds.to_vec();
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
    Ok(child)
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
        let child = spawn_worker(spec, &[])?;
        let code = monitor(child, spec.shutdown_grace, control)?;

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
