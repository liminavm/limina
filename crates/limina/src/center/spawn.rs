// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Child-process control for the center: start/stop/reset/delete a managed VM.
//!
//! Pure Rust (no AppKit) and deliberately handle-less: a started VM is a detached
//! `limina start <bundle>` child that survives the center, and stop/reset work off
//! the bundle's flock+pidfile — so a center relaunch (or a VM started from a
//! terminal) is controlled identically.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;

use crate::vmlib::{bundle::VmBundle, runtime};

/// Spawn `limina start <bundle>` as a detached child. Its own process group so a
/// terminal Ctrl-C on the center never reaches it; stdout/stderr to the bundle's
/// `logs/supervisor.log` (truncated per run); a reaper thread waits it (children
/// are NEVER killed on drop — VMs outlive the center by design).
pub fn start_vm(bundle: &VmBundle) -> Result<()> {
    let exe = std::env::current_exe().context("locating the limina binary")?;
    std::fs::create_dir_all(bundle.logs_dir())
        .with_context(|| format!("creating {}", bundle.logs_dir().display()))?;
    let log_path = bundle.logs_dir().join("supervisor.log");
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let child = Command::new(exe)
        .arg("start")
        .arg(&bundle.path)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(log.try_clone().context("duplicating the log fd")?)
        .stderr(log)
        .spawn()
        .context("spawning limina start")?;
    log::info!(
        "control center: started {} (supervisor pid {})",
        bundle.dir_name(),
        child.id()
    );
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// Ask a running VM to stop (the graceful ladder); `force` skips the grace.
pub fn stop_vm(bundle: &VmBundle, force: bool) -> Result<()> {
    match runtime::status(bundle) {
        runtime::VmStatus::Running { pid } => runtime::signal_stop(pid, force),
        runtime::VmStatus::Stopped => Ok(()), // already what the user wanted
    }
}

/// Reset = force stop, wait for the flock to release, start fresh. Blocking (up to
/// ~30 s if the supervisor is wedged) — call from a background thread, not the
/// AppKit main thread.
pub fn reset_vm(bundle: &VmBundle) -> Result<()> {
    stop_vm(bundle, true)?;
    anyhow::ensure!(
        runtime::wait_stopped(bundle, Duration::from_secs(30)),
        "{} did not stop within 30s; not restarting",
        bundle.dir_name()
    );
    start_vm(bundle)
}
