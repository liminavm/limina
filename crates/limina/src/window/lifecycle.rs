// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Worker-connection lifecycle: the swappable connection to the *current* worker
//! ([`WorkerConn`]) and the window's quit policy — distinguishing a real window close from a
//! minimize/app-hide, and the process-group kill fallback.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

/// The supervisor's live connection to the *current* worker: its pid (for shutdown signaling)
/// and the supervisor-side fds the window talks to it through (input sinks + the shown-ack fd).
///
/// All are swapped atomically when the worker is relaunched after a guest reboot, so the window
/// keeps the same NSWindow/layer/event-monitor and just retargets whichever worker is current.
/// Readers (the AppKit main thread) load each field fresh; the relaunch path publishes the new
/// worker's values via [`WorkerConn::swap`] *before* closing the old fds, so a concurrent input
/// `send` can't hit a reused fd number. A `pid` of 0 / fd of -1 means "no current worker".
pub struct WorkerConn {
    pid: AtomicI32,
    kbd_fd: AtomicI32,
    ptr_fd: AtomicI32,
    rel_ptr_fd: AtomicI32,
    ack_fd: AtomicI32,
}

impl WorkerConn {
    pub fn new(
        pid: i32,
        kbd_fd: RawFd,
        ptr_fd: RawFd,
        rel_ptr_fd: RawFd,
        ack_fd: RawFd,
    ) -> Arc<Self> {
        Arc::new(Self {
            pid: AtomicI32::new(pid),
            kbd_fd: AtomicI32::new(kbd_fd),
            ptr_fd: AtomicI32::new(ptr_fd),
            rel_ptr_fd: AtomicI32::new(rel_ptr_fd),
            ack_fd: AtomicI32::new(ack_fd),
        })
    }

    pub fn pid(&self) -> i32 {
        self.pid.load(Ordering::Acquire)
    }
    pub fn kbd_fd(&self) -> RawFd {
        self.kbd_fd.load(Ordering::Acquire)
    }
    pub fn ptr_fd(&self) -> RawFd {
        self.ptr_fd.load(Ordering::Acquire)
    }
    /// The relative-pointer (capture-mode mouse) sink fd.
    pub fn rel_ptr_fd(&self) -> RawFd {
        self.rel_ptr_fd.load(Ordering::Acquire)
    }
    pub fn ack_fd(&self) -> RawFd {
        self.ack_fd.load(Ordering::Acquire)
    }

    /// Publish a freshly-spawned worker's pid + supervisor-side fds (called on relaunch).
    pub fn swap(&self, pid: i32, kbd_fd: RawFd, ptr_fd: RawFd, rel_ptr_fd: RawFd, ack_fd: RawFd) {
        self.kbd_fd.store(kbd_fd, Ordering::Release);
        self.ptr_fd.store(ptr_fd, Ordering::Release);
        self.rel_ptr_fd.store(rel_ptr_fd, Ordering::Release);
        self.ack_fd.store(ack_fd, Ordering::Release);
        self.pid.store(pid, Ordering::Release);
    }
}

/// Should the supervisor tear the VM down (orderly guest power-off, then kill the worker)?
///
/// True only when the user asked to stop (Ctrl-C) or actually **closed** the window. It is NOT
/// true when the window is merely *miniaturized* (minimize-to-Dock) or the whole app is *hidden*
/// (Cmd-H): both of those also make `NSWindow.isVisible()` return `false`, but the user wants the
/// VM to keep running in the background, not be powered off. The old check was a bare
/// `stop_requested || !visible`, which powered the guest off the instant the window was minimized
/// or the app was hidden (reproduced live: minimizing the enhanced-tier window triggered
/// "window closed → asked the guest agent to power off" ~1s later). A closed window is the only
/// not-visible state that is neither miniaturized nor app-hidden.
pub(crate) fn should_initiate_quit(
    stop_requested: bool,
    visible: bool,
    miniaturized: bool,
    app_hidden: bool,
) -> bool {
    // A closed window is the only not-visible state that is neither miniaturized nor app-hidden,
    // so guarding those two turns "is it closed?" back into a reliable signal while letting a
    // minimized/hidden VM keep running.
    stop_requested || (!visible && !miniaturized && !app_hidden)
}

/// SIGKILL the worker's process group — refusing a non-positive pid. `WorkerConn.pid()`
/// is 0 while there is "no current worker" (mid-relaunch), and `kill(0, sig)` signals the
/// CALLER'S own process group (`kill(-1, sig)` everything we may signal): tearing down
/// during that window must be a no-op, not supervisor suicide. Returns whether a kill
/// was actually issued.
pub(crate) fn kill_worker_group(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimize_and_hide_do_not_power_off_the_vm() {
        // Regression for the live-reproduced bug: minimizing the window (or hiding the app via
        // Cmd-H) makes NSWindow.isVisible() return false, which the render-timer quit-check used
        // to treat as a window close → orderly guest power-off. Minimize/hide must KEEP the VM
        // running; only an actual close (not-visible AND not-miniaturized AND app-not-hidden) or
        // an explicit stop (Ctrl-C) tears it down.
        //
        // args: (stop_requested, visible, miniaturized, app_hidden)
        // Steady state — window on screen: never quit.
        assert!(!should_initiate_quit(false, true, false, false));
        // Minimized to Dock (isVisible()==false, isMiniaturized()==true): keep the VM running.
        assert!(
            !should_initiate_quit(false, false, true, false),
            "minimizing the window must NOT power off the guest"
        );
        // App hidden with Cmd-H (isVisible()==false, NSApp.isHidden()==true): keep the VM running.
        assert!(
            !should_initiate_quit(false, false, false, true),
            "hiding the app must NOT power off the guest"
        );
        // Window closed (not visible, not miniaturized, app not hidden): the one not-visible state
        // that IS a close — tear the VM down.
        assert!(
            should_initiate_quit(false, false, false, false),
            "closing the window must power off the guest"
        );
        // Ctrl-C (stop_requested) always tears down, regardless of window state.
        assert!(should_initiate_quit(true, true, false, false));
    }

    #[test]
    fn kill_worker_group_refuses_non_positive_pids() {
        // WorkerConn.pid() == 0 means "no current worker" (mid-relaunch). kill(-0, SIGKILL)
        // is kill(0, SIGKILL) — the supervisor's OWN process group — and kill(-(-1)) is
        // kill(1). A teardown racing the relaunch window must be a no-op, never a kill
        // aimed at ourselves (or init).
        assert!(!kill_worker_group(0), "pid 0 must not issue any kill");
        assert!(
            !kill_worker_group(-1),
            "negative pids must not issue any kill"
        );
    }

    #[test]
    fn worker_conn_swap_retargets_every_field() {
        // The windowed-reboot re-wiring contract: relaunching the worker must retarget the pid AND
        // all three supervisor-side fds in one swap, or the still-open NSWindow keeps talking to the
        // dead worker (input written to a closed fd, shutdown signal to a stale pid). Guards against
        // a swap() that forgets a field. (The fds here are plain ints — WorkerConn never touches
        // them, it only publishes the numbers for the main thread to load.)
        let conn = WorkerConn::new(100, 3, 4, 5, 6);
        assert_eq!(
            (
                conn.pid(),
                conn.kbd_fd(),
                conn.ptr_fd(),
                conn.rel_ptr_fd(),
                conn.ack_fd()
            ),
            (100, 3, 4, 5, 6)
        );

        conn.swap(200, 7, 8, 9, 10);
        assert_eq!(
            (
                conn.pid(),
                conn.kbd_fd(),
                conn.ptr_fd(),
                conn.rel_ptr_fd(),
                conn.ack_fd()
            ),
            (200, 7, 8, 9, 10),
            "relaunch must retarget pid + all input/ack fds to the new worker"
        );
    }
}
