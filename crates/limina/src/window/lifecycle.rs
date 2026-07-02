// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Worker-connection lifecycle: the swappable connection to the *current* worker
//! ([`WorkerConn`]) and the window's quit policy — distinguishing a real window close from a
//! minimize/app-hide, and the process-group kill fallback.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use arc_swap::ArcSwap;

/// One worker's pid plus the supervisor-side fds the window talks to it through (input sinks
/// and the shown-ack fd), **owned**. The fds close when the `WorkerIo` drops — i.e. when the
/// last `Arc<WorkerIo>` snapshot taken via [`WorkerConn::io`] goes away — so a reader holding
/// a snapshot can never observe a closed (or worse, reused) fd number, and every error path
/// cleans the fds up without manual `close` calls.
pub struct WorkerIo {
    pid: i32,
    kbd: OwnedFd,
    ptr: OwnedFd,
    rel_ptr: OwnedFd,
    ack: OwnedFd,
}

impl WorkerIo {
    pub fn new(pid: i32, kbd: OwnedFd, ptr: OwnedFd, rel_ptr: OwnedFd, ack: OwnedFd) -> Self {
        Self {
            pid,
            kbd,
            ptr,
            rel_ptr,
            ack,
        }
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }
    pub fn kbd_fd(&self) -> RawFd {
        self.kbd.as_raw_fd()
    }
    pub fn ptr_fd(&self) -> RawFd {
        self.ptr.as_raw_fd()
    }
    /// The relative-pointer (capture-mode mouse) sink fd.
    pub fn rel_ptr_fd(&self) -> RawFd {
        self.rel_ptr.as_raw_fd()
    }
    pub fn ack_fd(&self) -> RawFd {
        self.ack.as_raw_fd()
    }
}

/// The supervisor's live connection to the *current* worker: its pid (for shutdown signaling)
/// and the supervisor-side fds the window talks to it through (input sinks + the shown-ack
/// fd), as one atomically-swappable owned set ([`WorkerIo`]).
///
/// The whole set is swapped when the worker is relaunched after a guest reboot, so the window
/// keeps the same NSWindow/layer/event-monitor and just retargets whichever worker is current.
/// Readers (the AppKit main thread, the ack-sender thread, the capture tap) take a snapshot
/// via [`WorkerConn::io`] and hold the `Arc` across their sends: the relaunch path publishes
/// the new worker's set via [`WorkerConn::swap`], and the previous set's fds are closed only
/// when the last outstanding snapshot drops (`OwnedFd`), so a concurrent send can't hit a
/// closed — let alone reused — fd number.
pub struct WorkerConn {
    io: ArcSwap<WorkerIo>,
}

impl WorkerConn {
    pub fn new(io: WorkerIo) -> Arc<Self> {
        Arc::new(Self {
            io: ArcSwap::from_pointee(io),
        })
    }

    /// Snapshot the current worker's endpoints. Hold the returned `Arc` for the duration of
    /// any use of the fds — that's what keeps them open across a concurrent relaunch swap.
    pub fn io(&self) -> Arc<WorkerIo> {
        self.io.load_full()
    }

    /// The current worker's pid (for shutdown signaling). Always the most recently published
    /// worker's — [`WorkerConn::new`] and [`WorkerConn::swap`] both take a live set.
    pub fn pid(&self) -> i32 {
        self.io.load().pid
    }

    /// Publish a freshly-spawned worker's pid + supervisor-side fds (called on relaunch). The
    /// previous worker's fds close when the last outstanding [`WorkerConn::io`] snapshot of
    /// them drops.
    pub fn swap(&self, io: WorkerIo) {
        self.io.store(Arc::new(io));
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

/// SIGKILL the worker's process group — refusing a non-positive pid. `kill(0, sig)` signals
/// the CALLER'S own process group (`kill(-1, sig)` everything we may signal): tearing down
/// with a stray non-positive pid must be a no-op, not supervisor suicide. (With [`WorkerConn`]
/// now always holding a live worker's pid no caller passes 0 today; the guard stays as the
/// cheap backstop.) Returns whether a kill was actually issued.
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
        // kill(-0, SIGKILL) is kill(0, SIGKILL) — the supervisor's OWN process group — and
        // kill(-(-1)) is kill(1). A teardown handed a stray non-positive pid must be a no-op,
        // never a kill aimed at ourselves (or init).
        assert!(!kill_worker_group(0), "pid 0 must not issue any kill");
        assert!(
            !kill_worker_group(-1),
            "negative pids must not issue any kill"
        );
    }

    /// A real, owned fd for a test `WorkerIo` (a fresh open of /dev/null). The tests must
    /// never wrap arbitrary integers in `OwnedFd` — its Drop closes, and would close an fd
    /// the test process doesn't own.
    fn devnull_fd() -> OwnedFd {
        std::fs::File::open("/dev/null")
            .expect("open /dev/null")
            .into()
    }

    fn test_io(pid: i32) -> (WorkerIo, [RawFd; 4]) {
        let (kbd, ptr, rel_ptr, ack) = (devnull_fd(), devnull_fd(), devnull_fd(), devnull_fd());
        let raws = [
            kbd.as_raw_fd(),
            ptr.as_raw_fd(),
            rel_ptr.as_raw_fd(),
            ack.as_raw_fd(),
        ];
        (WorkerIo::new(pid, kbd, ptr, rel_ptr, ack), raws)
    }

    #[test]
    fn worker_conn_swap_retargets_every_field() {
        // The windowed-reboot re-wiring contract: relaunching the worker must retarget the pid AND
        // all supervisor-side fds in one swap, or the still-open NSWindow keeps talking to the
        // dead worker (input written to a closed fd, shutdown signal to a stale pid). Guards
        // against a swap() that forgets a field.
        let (io1, raws1) = test_io(100);
        let conn = WorkerConn::new(io1);
        let snap = conn.io();
        assert_eq!(
            (
                conn.pid(),
                snap.kbd_fd(),
                snap.ptr_fd(),
                snap.rel_ptr_fd(),
                snap.ack_fd()
            ),
            (100, raws1[0], raws1[1], raws1[2], raws1[3])
        );
        drop(snap);

        let (io2, raws2) = test_io(200);
        conn.swap(io2);
        let snap = conn.io();
        assert_eq!(
            (
                conn.pid(),
                snap.kbd_fd(),
                snap.ptr_fd(),
                snap.rel_ptr_fd(),
                snap.ack_fd()
            ),
            (200, raws2[0], raws2[1], raws2[2], raws2[3]),
            "relaunch must retarget pid + all input/ack fds to the new worker"
        );
    }

    #[test]
    fn swap_keeps_fds_open_while_a_snapshot_is_held_then_closes_them() {
        use std::io::Read;

        // The reused-fd race fix: a sender holding a snapshot across a relaunch must keep the
        // OLD worker's fds open (so the OS can't reuse their numbers mid-send); once the last
        // snapshot drops, OwnedFd closes them — the relaunch path has no manual close loop.
        //
        // Oracle: a socketpair peer. Probing the raw fd number after close would be racy (a
        // parallel test could reuse the number); the peer end seeing EOF only when OUR end
        // actually closed is reuse-proof.
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        ours.set_nonblocking(true).expect("nonblocking");
        let io1 = WorkerIo::new(1, theirs.into(), devnull_fd(), devnull_fd(), devnull_fd());
        let conn = WorkerConn::new(io1);

        let held = conn.io();
        let (io2, _) = test_io(2);
        conn.swap(io2);

        let mut buf = [0u8; 1];
        match (&ours).read(&mut buf) {
            // Still open: no data yet, but no EOF either.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("old fd must stay open while a snapshot is held, got {other:?}"),
        }

        drop(held);
        match (&ours).read(&mut buf) {
            // Closed on the last snapshot drop: the peer reads EOF.
            Ok(0) => {}
            other => panic!("old fd must close once the last snapshot drops, got {other:?}"),
        }
    }
}
