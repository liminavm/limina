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
//! Activity comes from logind's user state file (`/run/systemd/users/<uid>`,
//! world-readable; `STATE=active` iff one of the uid's sessions is the seat's active
//! one — `online` is the fast-user-switched-away state). No logind, no file, or an
//! unreadable one **fails open**: a wrong layout beats no layout, and the single-session
//! floor must not regress where logind is absent.

use limina_proto::DisplayLayout;

/// The one holder of "what the compositor last reported" for the whole process: it
/// outlives the yield/claim alternation, so an arrangement learned in one phase is
/// still there to re-send when the other phase (re)connects.
pub struct LayoutGate {
    latest: Option<DisplayLayout>,
    was_active: bool,
}

impl LayoutGate {
    pub fn new() -> Self {
        Self {
            latest: None,
            // Start "inactive" so the first active poll re-offers whatever is stored —
            // harmless at real startup (nothing stored yet), correct after any race.
            was_active: false,
        }
    }

    /// Absorb newly observed layouts and the current activity state; returns the layout
    /// to send now, if any: on a change while active, or on the inactive→active edge
    /// (even unchanged — the host may hold another session's arrangement).
    pub fn poll(
        &mut self,
        new: impl IntoIterator<Item = DisplayLayout>,
        active: bool,
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
        if active && (changed || became_active) {
            if became_active && !changed && self.latest.is_some() {
                eprintln!(
                    "limina-agent-session: session became active; re-sending the arrangement"
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
        if active {
            self.latest.clone()
        } else {
            None
        }
    }
}

/// Whether this uid owns the seat's active session right now. Fails open (`true`) when
/// the answer cannot be read — see the module doc.
pub fn seat_active() -> bool {
    let uid = unsafe { libc::getuid() };
    match std::fs::read_to_string(format!("/run/systemd/users/{uid}")) {
        Ok(text) => parsed_state_is_active(&text).unwrap_or(true),
        Err(_) => true,
    }
}

/// `Some(state == "active")` from a logind user state file, `None` if no `STATE=` line
/// is present (the fail-open case).
fn parsed_state_is_active(text: &str) -> Option<bool> {
    text.lines()
        .find_map(|l| l.strip_prefix("STATE="))
        .map(|state| state == "active")
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
        assert_eq!(g.poll([layout(0)], true), Some(layout(0)));
    }

    #[test]
    fn an_unchanged_layout_is_not_resent() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(0)], true), Some(layout(0)));
        assert_eq!(g.poll([], true), None);
        assert_eq!(g.poll([layout(0)], true), None);
    }

    /// THE defect: an inactive session (fast-user-switched away, or the greeter behind
    /// a user session) must not clobber the active session's arrangement on the host.
    #[test]
    fn a_change_while_inactive_is_held_back() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(7)], false), None);
        assert_eq!(g.poll([], false), None);
    }

    /// …and the held layout goes out the moment the session becomes active, even if the
    /// compositor never repeats it.
    #[test]
    fn activation_resends_the_held_layout() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(7)], false), None);
        assert_eq!(g.poll([], true), Some(layout(7)));
        assert_eq!(g.poll([], true), None);
    }

    /// Re-activation re-sends even a layout that was already sent before the switch
    /// away: the other session wrote to the host in between.
    #[test]
    fn reactivation_resends_an_already_sent_layout() {
        let mut g = LayoutGate::new();
        assert_eq!(g.poll([layout(0)], true), Some(layout(0)));
        assert_eq!(g.poll([], false), None);
        assert_eq!(g.poll([], true), Some(layout(0)));
    }

    #[test]
    fn a_new_channel_is_seeded_only_while_active() {
        let mut g = LayoutGate::new();
        g.poll([layout(3)], false);
        assert_eq!(g.for_new_channel(false), None);
        assert_eq!(g.for_new_channel(true), Some(layout(3)));
    }

    #[test]
    fn state_file_parsing_and_fail_open() {
        let f = "# This is private data. Do not parse.\nNAME=kov\nSTATE=active\nGC_MODE=x\n";
        assert_eq!(parsed_state_is_active(f), Some(true));
        assert_eq!(
            parsed_state_is_active("NAME=gdm\nSTATE=online\n"),
            Some(false)
        );
        assert_eq!(
            parsed_state_is_active("NAME=gdm\nSTATE=lingering\n"),
            Some(false)
        );
        // No STATE line at all → None → the caller fails open.
        assert_eq!(parsed_state_is_active("NAME=gdm\n"), None);
    }
}
