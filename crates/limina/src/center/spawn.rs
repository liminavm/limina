// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Child-process control for the center: start/stop/reset/delete a managed VM.
//!
//! Pure Rust (no AppKit) and deliberately handle-less: a started VM is a detached
//! `limina start <bundle>` child that survives the center, and stop/reset work off
//! the bundle's flock+pidfile — so a center relaunch (or a VM started from a
//! terminal) is controlled identically.
//!
//! Starting is two-layered (`docs/design/vm-start-preflight.md`). [`vmlib::preflight`]
//! refuses a start that cannot work *before* anything is spawned, so the failure is specific
//! and reaches the caller as an ordinary `Err`. Whatever pre-flight did not anticipate is
//! caught behind it by the reaper, which reports a supervisor that dies in its first seconds
//! instead of discarding its exit status. Neither layer is sufficient alone: without the
//! reaper the first unenumerated failure is silent again, and without pre-flight the user
//! clicks a button that was never going to work.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;

use crate::vmlib::{
    bundle::VmBundle,
    logrot,
    preflight::{self, Depth},
    runtime,
};

/// Errors raised by background work, drained into an alert on the center's next refresh.
pub type ErrorSink = Arc<Mutex<Vec<String>>>;

/// A supervisor that exits non-zero within this window failed to *start*. Later than this and
/// it is a guest that ran and then died, which is not this reporter's story to tell.
const STARTUP_GRACE: Duration = Duration::from_secs(5);

/// How much of the supervisor log to quote when a start dies early.
const LOG_TAIL_LINES: usize = 12;

/// Spawn `limina start <bundle>` as a detached child. Its own process group so a
/// terminal Ctrl-C on the center never reaches it; stdout/stderr to the bundle's
/// `logs/supervisor.log` (rotated per run, `logrot::GENERATIONS` deep — the boot worth
/// reading is the one that just died); a reaper thread waits it (children
/// are NEVER killed on drop — VMs outlive the center by design).
///
/// Refuses before spawning when pre-flight finds a blocker, so "the disk is missing" is an
/// error the caller can show rather than a line in a log nobody opens.
pub fn start_vm(bundle: &VmBundle, errors: &ErrorSink) -> Result<()> {
    let cfg = bundle
        .load()
        .with_context(|| format!("reading {}'s definition", bundle.dir_name()))?;
    preflight::check(bundle, &cfg, Depth::Full)
        .ensure_startable()
        .with_context(|| format!("{} cannot start", bundle.dir_name()))?;

    let exe = std::env::current_exe().context("locating the limina binary")?;
    std::fs::create_dir_all(bundle.logs_dir())
        .with_context(|| format!("creating {}", bundle.logs_dir().display()))?;
    let (log_path, log) = open_run_log(bundle)?;
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
    let name = bundle.dir_name();
    let errors = errors.clone();
    std::thread::spawn(move || {
        let mut child = child;
        let started = Instant::now();
        let Ok(status) = child.wait() else { return };
        if status.success() || started.elapsed() > STARTUP_GRACE {
            return;
        }
        let detail = explain_early_exit(&log_path);
        log::warn!("control center: {name} failed to start: {detail}");
        if let Ok(mut q) = errors.lock() {
            q.push(format!("{name} failed to start.\n\n{detail}"));
        }
    });
    Ok(())
}

/// Open `logs/supervisor.log` for the run about to start, keeping the previous boots.
///
/// The boot worth reading is almost always the one that just died, and this used to truncate
/// it: on 2026-08-31 a dogfood SIGSEGV was diagnosable only because the user saved a copy by
/// hand before restarting. Called after pre-flight, so a refused start rotates nothing.
fn open_run_log(bundle: &VmBundle) -> Result<(PathBuf, std::fs::File)> {
    let path = bundle.logs_dir().join("supervisor.log");
    logrot::rotate(&path, logrot::GENERATIONS);
    let file =
        std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    Ok((path, file))
}

/// What to tell the user about a supervisor that died in its first seconds: the tail of its
/// log, plus a translation for the failures whose raw text explains nothing.
fn explain_early_exit(log_path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(log_path).unwrap_or_default();
    let tail: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(LOG_TAIL_LINES)
        .collect();
    let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    if let Some(hint) = known_cause(&text) {
        return format!("{hint}\n\n{tail}");
    }
    if tail.is_empty() {
        return "the supervisor exited immediately and logged nothing.".into();
    }
    tail
}

/// Failures we cannot predict from outside the process but can recognise once they happen.
/// Pre-flight deliberately does not guess at these (`vmlib::preflight`'s "not checked" list);
/// recognising them here is the other half of that bargain.
fn known_cause(log: &str) -> Option<&'static str> {
    if log.contains("VmCreate") {
        return Some(
            "The VM worker could not create a hypervisor VM. It is most likely not codesigned \
             with com.apple.security.hypervisor — run limina from a signed app bundle.",
        );
    }
    None
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
pub fn reset_vm(bundle: &VmBundle, errors: &ErrorSink) -> Result<()> {
    stop_vm(bundle, true)?;
    anyhow::ensure!(
        runtime::wait_stopped(bundle, Duration::from_secs(30)),
        "{} did not stop within 30s; not restarting",
        bundle.dir_name()
    );
    start_vm(bundle, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmlib::bundle::tests::{basic_opts, env_lock, scratch_library};
    use crate::vmlib::import::create;

    /// The originating bug, at the layer that had it: a bundle whose disk is gone must fail
    /// *before* anything is spawned, so the caller has an error to show instead of a child
    /// that dies into a truncated log.
    #[test]
    fn a_blocked_vm_is_refused_without_spawning() {
        let _g = env_lock();
        let lib = scratch_library("spawn-blocked");
        std::env::set_var("LIMINA_VM_LIBRARY", &lib);
        let src = lib.join("seed.raw");
        std::fs::write(&src, vec![0u8; 1024]).unwrap();
        let mut opts = basic_opts("Blocked");
        opts.disk = Some(src);
        let bundle = create(&opts, &lib).unwrap();
        let cfg = bundle.load().unwrap();
        std::fs::remove_file(bundle.resolve_path(&cfg.disks[0].path)).unwrap();
        // A previous run's log, so "did we spawn?" is answered by whether it was truncated.
        std::fs::create_dir_all(bundle.logs_dir()).unwrap();
        let log = bundle.logs_dir().join("supervisor.log");
        std::fs::write(&log, "from an earlier run\n").unwrap();

        let errors: ErrorSink = Arc::new(Mutex::new(Vec::new()));
        let err = start_vm(&bundle, &errors).expect_err("a missing disk must refuse the start");
        let msg = format!("{err:#}");
        assert!(msg.contains("cannot start"), "{msg}");
        assert!(msg.contains("not found"), "{msg}");

        // Nothing ran: the log is untouched and no run lock was taken.
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "from an earlier run\n"
        );
        assert!(!runtime::status(&bundle).is_running());

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    /// The crashed boot's log is the one worth reading, and opening the next run's used to
    /// truncate it — on 2026-08-31 a dogfood SIGSEGV was diagnosable only because the user
    /// saved a copy by hand before restarting.
    #[test]
    fn opening_a_run_log_keeps_the_previous_boot() {
        let _g = env_lock();
        let lib = scratch_library("spawn-rotate");
        std::env::set_var("LIMINA_VM_LIBRARY", &lib);
        let bundle = create(&basic_opts("Rotate"), &lib).unwrap();
        std::fs::create_dir_all(bundle.logs_dir()).unwrap();
        std::fs::write(
            bundle.logs_dir().join("supervisor.log"),
            "the boot that crashed\n",
        )
        .unwrap();

        let (path, _log) = open_run_log(&bundle).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "the new run starts empty"
        );
        assert_eq!(
            std::fs::read_to_string(bundle.logs_dir().join("supervisor.1.log")).unwrap(),
            "the boot that crashed\n",
            "the previous run must survive the start that follows it"
        );

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn an_early_exit_is_explained_from_the_log_tail() {
        let dir = std::env::temp_dir().join(format!("limina-reaper-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("supervisor.log");

        std::fs::write(&log, "noise\n\nError: something specific went wrong\n").unwrap();
        let out = explain_early_exit(&log);
        assert!(out.contains("something specific went wrong"), "{out}");

        // A supervisor that died before writing anything still gets a sentence.
        std::fs::write(&log, "").unwrap();
        assert!(explain_early_exit(&log).contains("logged nothing"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The entitlement failure cannot be predicted from outside the process, so pre-flight
    /// deliberately does not try; recognising it here is the other half of that bargain.
    #[test]
    fn a_hypervisor_entitlement_failure_is_translated() {
        let hint = known_cause("worker: Error: VmCreate\n").expect("VmCreate must be recognised");
        assert!(hint.contains("com.apple.security.hypervisor"), "{hint}");
        assert!(known_cause("some unrelated failure").is_none());
    }
}
