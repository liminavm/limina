// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Is the stock SPICE agent carrying this user's clipboard?
//!
//! This is the arbitration probe: where `spice-vdagent` serves the session, it owns the
//! clipboard and we stay out — two selection owners in one session fight through mutter's
//! X11↔Wayland bridging, so the switch is thrown at capability negotiation (we simply
//! don't announce `clipboard`), never downstream.
//!
//! **Liveness is the probe, and it is a positive one** — nothing here infers anything
//! from the compositor's name. `spice-vdagent` *exits* when it cannot do the job
//! (vd_agent 0.22.1 `src/vdagent/vdagent.c:433`, `err_init`): it quits if it fails to
//! reach `spice-vdagentd` after its retry budget, and if `vdagent_display_create` fails —
//! which it does whenever `vdagent_x11_create`'s `XOpenDisplay(NULL)` finds no X server
//! ("could not connect to X-server"), i.e. on a Wayland session with no XWayland. So a
//! *live* process has already proven, by its own checks, that it reached the daemon and
//! built a display plus clipboards. That is exactly the fact we need.
//!
//! Scope: one instance **per user**, not per session — `spice-vdagent.service` and
//! `limina-agent-session.service` are both systemd *user* units, and gdm will not give
//! one user two graphical sessions. The residual gap (vdagentd serves only the
//! logind-active session, so a backgrounded user's sync pauses) is out of scope here.

use std::os::unix::fs::MetadataExt;

/// `/proc/<pid>/comm` of the session agent. Not `spice-vdagentd` (the root daemon):
/// the daemon runs regardless of whether any session is served, so it proves nothing.
const COMM: &str = "spice-vdagent";

/// Escape hatch: force the claim even where a vdagent is serving. For bring-up and
/// for anyone whose vdagent is alive but useless in a way its own checks don't catch.
fn forced() -> bool {
    std::env::var("LIMINA_CLIPBOARD_IGNORE_VDAGENT").is_ok_and(|v| v == "1")
}

/// Where a session agent could live. A guest with no binary can never grow one, so the
/// startup settle window is pointless there — and skipping it keeps the clipboard-less
/// gap at zero on every guest that doesn't ship SPICE (the L1 mock guest included).
const BINARIES: [&str; 2] = ["/usr/bin/spice-vdagent", "/usr/local/bin/spice-vdagent"];

/// Whether a session agent is installed at all — a cheap "could this ever happen?".
pub fn installed() -> bool {
    !forced() && BINARIES.iter().any(|p| std::path::Path::new(p).exists())
}

/// True when a live `spice-vdagent` owned by *us* is serving the clipboard.
pub fn serving() -> bool {
    if forced() {
        return false;
    }
    let uid = unsafe { libc::geteuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // Another user's agent serves another user's clipboard; only ours binds us.
        match entry.metadata() {
            Ok(m) if m.uid() == uid => {}
            _ => continue,
        }
        // A process can exit between readdir and read — a missing comm is just "gone".
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            if comm.trim_end() == COMM {
                return true;
            }
        }
    }
    false
}
