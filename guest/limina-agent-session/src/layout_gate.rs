//! Multi-session arbitration for the arrangement report.
//!
//! Every graphical session runs a helper (the unit is enabled `--global`, so the gdm
//! greeter's user instance runs one too), and the host takes `DisplayLayout`
//! last-writer-wins. The rule that makes last-writer-wins correct is **only the active
//! seat session writes**: the arrangement that governs the host's pointer mapping is the
//! one used by the compositor currently scanned out. A helper whose uid does not own the
//! active session stores what its compositor reports and stays quiet; on becoming active
//! it re-sends unconditionally, because the host may be holding the other session's
//! arrangement.
//!
//! Activity is the seat's answer, not the user's: `ACTIVE_UID=` in logind's seat state
//! file (`/run/systemd/seats/seat0`, world-readable). The user's own `STATE=` will not do —
//! logind counts every seatless session as active, so a `sudo -u`, `su`, ssh or cron job as
//! a user with a background graphical session reads as that session taking the seat while
//! seat0 never moves. No logind, no file, or one without an owner **fails open**: a wrong
//! layout beats no layout, and the single-session floor must not regress where logind is
//! absent.
//!
//! Polling alone cannot see the seat leave and come back between two polls — every poll
//! reads "active", no edge, while the other session's helper may have written. A
//! [`SeatWatch`] (inotify on logind's seat directory) records that the seat changed at all,
//! and a change seen while active re-sends too.

use limina_proto::DisplayLayout;

/// logind's state file for the one seat a guest desktop runs on.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SEAT_DIR: &str = "/run/systemd/seats";
const SEAT_FILE: &str = "/run/systemd/seats/seat0";

/// The one holder of "what the compositor last reported" for the whole process: it
/// outlives the yield/claim alternation, so an arrangement learned in one phase is
/// still there to re-send when the other phase (re)connects.
pub struct LayoutGate {
    latest: Option<DisplayLayout>,
    was_active: bool,
    seat: SeatWatch,
}

impl LayoutGate {
    pub fn new() -> Self {
        Self {
            latest: None,
            // Start "inactive" so the first active poll re-offers whatever is stored —
            // harmless at real startup (nothing stored yet), correct after any race.
            was_active: false,
            seat: SeatWatch::new(),
        }
    }

    /// [`Self::poll`] against the live seat: who holds it now, and whether it changed
    /// since the last call.
    pub fn poll_seat(
        &mut self,
        new: impl IntoIterator<Item = DisplayLayout>,
    ) -> Option<DisplayLayout> {
        let seat_changed = self.seat.changed();
        self.poll(new, seat_active(), seat_changed)
    }

    /// Absorb newly observed layouts and the current activity state; returns the layout
    /// to send now, if any: on a change while active, on the inactive→active edge, or on
    /// any change to the seat while active (even unchanged — the host may hold another
    /// session's arrangement).
    pub fn poll(
        &mut self,
        new: impl IntoIterator<Item = DisplayLayout>,
        active: bool,
        seat_changed: bool,
    ) -> Option<DisplayLayout> {
        let mut changed = false;
        for layout in new {
            if self.latest.as_ref() != Some(&layout) {
                self.latest = Some(layout);
                changed = true;
            }
        }
        let became_active = active && !self.was_active;
        self.was_active = active;
        if changed && !active {
            eprintln!(
                "limina-agent-session: holding the arrangement (session inactive; \
                 the active session's helper owns the report)"
            );
        }
        if active && (changed || became_active || seat_changed) {
            if (became_active || seat_changed) && !changed && self.latest.is_some() {
                eprintln!(
                    "limina-agent-session: {}; re-sending the arrangement",
                    if became_active {
                        "session became active"
                    } else {
                        "the seat changed"
                    }
                );
            }
            self.latest.clone()
        } else {
            None
        }
    }

    /// What a freshly (re)connected control channel should be seeded with: the host's
    /// copy died with the old channel and the compositor will not repeat itself. Quiet
    /// while inactive — the active session's helper owns the wire.
    pub fn for_new_channel(&self, active: bool) -> Option<DisplayLayout> {
        if active { self.latest.clone() } else { None }
    }
}

/// Whether this uid owns seat0's active session right now. Fails open (`true`) when the
/// answer cannot be read — see the module doc.
pub fn seat_active() -> bool {
    let uid = unsafe { libc::getuid() };
    match std::fs::read_to_string(SEAT_FILE) {
        Ok(text) => seat_owner_is(&text, uid).unwrap_or(true),
        Err(_) => true,
    }
}

/// `Some(ACTIVE_UID == uid)` from a logind seat state file, `None` if the seat names no
/// owner (the fail-open case).
fn seat_owner_is(text: &str, uid: u32) -> Option<bool> {
    text.lines()
        .find_map(|l| l.strip_prefix("ACTIVE_UID="))
        .and_then(|owner| owner.trim().parse::<u32>().ok())
        .map(|owner| owner == uid)
}

/// Remembers that logind rewrote a seat's state file since the last look. logind writes it
/// to a temporary and renames it into place, so a rename into the directory is a change.
/// Events queue in the kernel between polls, which is the point: a flip away and back is
/// still a change. Unavailable (no inotify, no directory) it reports nothing, which is the
/// polling-only behaviour.
pub struct SeatWatch {
    #[cfg(target_os = "linux")]
    fd: Option<std::os::fd::OwnedFd>,
}

impl SeatWatch {
    #[cfg(target_os = "linux")]
    fn new() -> Self {
        use std::os::fd::{FromRawFd, OwnedFd};
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Self { fd: None };
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let dir = std::ffi::CString::new(SEAT_DIR).expect("no NUL in a constant path");
        let wd = unsafe {
            use std::os::fd::AsRawFd;
            libc::inotify_add_watch(
                fd.as_raw_fd(),
                dir.as_ptr(),
                libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE,
            )
        };
        Self {
            fd: (wd >= 0).then_some(fd),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn new() -> Self {
        Self {}
    }

    /// Drain the queued events; whether any was there.
    #[cfg(target_os = "linux")]
    fn changed(&mut self) -> bool {
        use std::os::fd::AsRawFd;
        let Some(fd) = &self.fd else {
            return false;
        };
        let mut buf = [0u8; 4096];
        let mut any = false;
        loop {
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return any;
            }
            any = true;
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn changed(&mut self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use limina_proto::{DisplayLayout, GuestMonitor};

    fn layout(x: i32) -> DisplayLayout {
        DisplayLayout {
            monitors: vec![GuestMonitor {
                connector: "Virtual-1".into(),
                x,
                y: 0,
                width: 2048,
                height: 1152,
            }],
        }
    }

    #[test]
    fn a_change_while_active_is_sent() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(0)], true, false), Some(layout(0)));
    }

    #[test]
    fn an_unchanged_layout_is_not_resent() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(0)], true, false), Some(layout(0)));
        assert_eq!(g.poll([], true, false), None);
        assert_eq!(g.poll([layout(0)], true, false), None);
    }

    /// THE defect: an inactive session (fast-user-switched away, or the greeter behind
    /// a user session) must not clobber the active session's arrangement on the host.
    #[test]
    fn a_change_while_inactive_is_held_back() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(7)], false, false), None);
        assert_eq!(g.poll([], false, false), None);
    }

    /// …and the held layout goes out the moment the session becomes active, even if the
    /// compositor never repeats it.
    #[test]
    fn activation_resends_the_held_layout() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(7)], false, false), None);
        assert_eq!(g.poll([], true, false), Some(layout(7)));
        assert_eq!(g.poll([], true, false), None);
    }

    /// Re-activation re-sends even a layout that was already sent before the switch
    /// away: the other session wrote to the host in between.
    #[test]
    fn reactivation_resends_an_already_sent_layout() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(0)], true, false), Some(layout(0)));
        assert_eq!(g.poll([], false, false), None);
        assert_eq!(g.poll([], true, false), Some(layout(0)));
    }

    #[test]
    fn a_new_channel_is_seeded_only_while_active() {
        let mut g = LayoutGate::new();
        g.poll([layout(3)], false, false);
        assert_eq!(g.for_new_channel(false), None);
        assert_eq!(g.for_new_channel(true), Some(layout(3)));
    }

    /// A user's seatless sessions (`sudo -u`, `su`, ssh, cron) are active in logind's eyes,
    /// which flips the user's `STATE=` to `active` while seat0 never moves. The seat's own
    /// `ACTIVE_UID=` is the question that matters: the dogfood state that let a background
    /// session's helper overwrite the fronted session's arrangement on the host.
    #[test]
    fn a_seatless_session_does_not_take_the_seat() {
        let seat0 = "# This is private data. Do not parse.\nIS_SEAT0=1\nCAN_MULTI_SESSION=1\n\
                     ACTIVE=4\nACTIVE_UID=1000\nSESSIONS=9 4 c1\nUIDS=1002 1000 957\n";
        assert_eq!(seat_owner_is(seat0, 1000), Some(true));
        assert_eq!(seat_owner_is(seat0, 1002), Some(false));
        assert_eq!(seat_owner_is(seat0, 957), Some(false));
    }

    /// A seat with no `ACTIVE_UID=` line says nothing either way → the caller fails open.
    #[test]
    fn a_seat_file_without_an_owner_fails_open() {
        assert_eq!(seat_owner_is("IS_SEAT0=1\nSESSIONS=4\n", 1000), None);
    }

    /// The seat can go away and come back between two polls (a brief front of another
    /// session); every poll then reads "active" and sees no edge, while the other helper may
    /// have written in between. A change to the seat seen while active re-sends.
    #[test]
    fn a_seat_change_between_polls_resends() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(0)], true, false), Some(layout(0)));
        assert_eq!(g.poll([], true, false), None);
        assert_eq!(g.poll([], true, true), Some(layout(0)));
        assert_eq!(g.poll([], true, false), None);
    }

    /// …but never from a session that does not hold the seat.
    #[test]
    fn a_seat_change_while_inactive_stays_quiet() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(5)], false, true), None);
        assert_eq!(g.poll([], false, true), None);
    }
}
