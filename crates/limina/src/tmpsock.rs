// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Temp-directory control sockets this supervisor allocated, removed when the run ends.
//!
//! **The allocator owns the path.** These paths are named with the supervisor's pid and handed to
//! the worker to bind, and the *worker* cannot clean them up: libkrun ends that process with
//! `libc::_exit` on the guest's PSCI SYSTEM_OFF (`vmm/src/lib.rs`, `Vmm::stop`), which runs no
//! destructor and no atexit hook. The supervisor outlives it, so cleanup belongs here.
//!
//! A process-global rather than an RAII guard for the same reason `gateway`/`control` use one:
//! **the supervisor also leaves via `process::exit`** — the headless path at the end of `run_vm`,
//! the windowed path from inside the AppKit timer — and that skips destructors. A `Drop` guard
//! here would look right, pass a unit test, and never run.
//!
//! Only *auto-allocated* paths are registered. A path the caller named on the command line is the
//! caller's to manage.
//!
//! This cannot cover the supervisor being SIGKILLed — nothing can — so a stray socket is still
//! possible. It is harmless on the next run: every binder unlinks the path before `bind()`.
//! **Do not reap other runs' leftovers by testing whether the pid in the name is alive.** Pids are
//! recycled: sweeping this host's 6450 accumulated sockets that way left five "live" ones that had
//! all been reassigned to unrelated system daemons.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

static OWNED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Register a socket path this run allocated.
pub fn own(path: &Path) {
    OWNED.lock().unwrap().push(path.to_path_buf());
}

/// Remove every registered socket. Idempotent and safe from any exit path.
pub fn cleanup() {
    for path in OWNED.lock().unwrap().drain(..) {
        // Already gone is the normal case for a path the run never got to bind.
        match std::fs::remove_file(&path) {
            Ok(()) => log::debug!("removed socket {path:?}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("could not remove socket {path:?}: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every VM run used to leave its control sockets behind: 6450 had accumulated on the dev
    /// Mac, dominated by the moc/fido gadget sockets at one pair per run.
    #[test]
    fn cleanup_removes_owned_paths_and_tolerates_missing_ones() {
        let dir = std::env::temp_dir().join(format!("limina-socktest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ours = dir.join("auto.sock");
        let theirs = dir.join("explicit.sock");
        std::fs::write(&ours, b"").unwrap();
        std::fs::write(&theirs, b"").unwrap();

        own(&ours);
        // `theirs` is deliberately never registered — a caller-named path is the caller's.
        own(&dir.join("never-created.sock"));
        assert!(ours.exists(), "nothing is removed before cleanup");

        cleanup();

        assert!(!ours.exists(), "an owned socket must be removed");
        assert!(theirs.exists(), "an unowned socket must be left alone");

        // Idempotent: a second exit path calling it again must not panic.
        cleanup();

        std::fs::remove_dir_all(&dir).ok();
    }
}
