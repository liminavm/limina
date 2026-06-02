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

use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use std::os::unix::process::{CommandExt, ExitStatusExt};

/// Set by the SIGINT/SIGTERM handler; observed by the monitor loop.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
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
pub fn monitor(mut child: std::process::Child, grace: Duration) -> Result<i32> {
    let pid = child.id() as libc::pid_t;
    let mut shutdown_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait().context("polling worker")? {
            return Ok(report_exit(status));
        }

        if STOP.load(Ordering::SeqCst) && shutdown_at.is_none() {
            log::info!("shutdown requested → asking guest to power off");
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            shutdown_at = Some(Instant::now());
        }

        if let Some(t) = shutdown_at {
            if t.elapsed() >= grace {
                log::warn!("guest did not power off within {grace:?}; forcing (SIGKILL)");
                let _ = child.kill();
                let status = child.wait().context("waiting on worker after SIGKILL")?;
                return Ok(report_exit(status));
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Spawn and supervise the worker until it exits (headless/non-windowed path).
pub fn run(spec: &WorkerSpec) -> Result<i32> {
    let child = spawn_worker(spec, &[])?;
    monitor(child, spec.shutdown_grace)
}

fn report_exit(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        if code == 0 {
            log::info!("VM powered off cleanly (worker exit 0)");
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
