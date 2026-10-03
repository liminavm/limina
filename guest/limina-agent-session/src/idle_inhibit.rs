// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Relay the session's idle inhibitors to the host, so it keeps its display awake while a
//! guest application asks the guest's own display to stay on.
//!
//! On GNOME an application inhibits idle through gnome-session (directly, or through the portal
//! and `org.freedesktop.ScreenSaver`, which land in the same place): Firefox playing a video
//! registers "Playing video" with the idle flag. gnome-session keeps that on the session bus as
//! `org.gnome.SessionManager`'s `InhibitedActions` and does not forward it to logind, so this
//! helper -- already on the session bus -- is the one component that can see it. Without it the
//! host falls back on its audio + decode heuristic (`crates/limina/src/window/wake_policy.rs`).
//!
//! Three rules shape the report:
//!   - **Advertised only when readable.** The `idleinhibit` capability means "my report is the
//!     truth about this session", so a helper on a desktop without gnome-session's interface
//!     does not claim it, and the host keeps its heuristic.
//!   - **Only the active seat session inhibits.** The host display is showing the active
//!     session; a video playing in a session switched away from is not being watched. Same rule
//!     as the arrangement report (`layout_gate`).
//!   - **Level-triggered.** The current value goes out on every new channel and on each change,
//!     like the root agent's power profile, so a reconnect resynchronises with no replay.

use std::sync::{Arc, Mutex};
use std::time::Duration;

const BUS: &str = "org.gnome.SessionManager";
const PATH: &str = "/org/gnome/SessionManager";
const IFACE: &str = "org.gnome.SessionManager";
const PROPERTY: &str = "InhibitedActions";

/// gnome-session's `GSM_INHIBITOR_FLAG_IDLE`: the session must not be marked idle.
const IDLE: u32 = 8;

/// Backoff between attempts to reach gnome-session. A desktop with no gnome-session retries for
/// the helper's whole life, so not short; but a session bus whose gnome-session registers just
/// after the helper's startup probe should not wait long for its first report either.
const RETRY_EVERY: Duration = Duration::from_secs(10);

/// Whether an `InhibitedActions` value includes the idle flag.
pub fn inhibits_idle(actions: u32) -> bool {
    actions & IDLE != 0
}

/// What the session's inhibitors say, kept current by a background thread: `None` while
/// gnome-session's interface cannot be read.
pub struct Inhibitors {
    cell: Arc<Mutex<Option<bool>>>,
}

impl Inhibitors {
    /// Read the inhibitors once now, then follow them on a thread for the life of the process.
    ///
    /// The first read happens here, before the helper's first HELLO, so the capability it
    /// announces is right from the start instead of after a reconnect. Infallible by design: on
    /// any failure the value stays `None` and the helper behaves as if the desktop had nothing
    /// to report.
    pub fn watch() -> Inhibitors {
        let cell = Arc::new(Mutex::new(None));
        let first = probe(&cell);
        match &first {
            Ok(_) => eprintln!(
                "limina-agent-session: following the session's idle inhibitors (idle {})",
                if current(&cell) == Some(true) {
                    "inhibited"
                } else {
                    "not inhibited"
                }
            ),
            Err(e) => eprintln!(
                "limina-agent-session: no idle inhibitors to relay ({e}); retrying quietly"
            ),
        }
        let shared = Arc::clone(&cell);
        let spawned = std::thread::Builder::new()
            .name("idle-inhibit".into())
            .spawn(move || watch_forever(&shared, first.ok()));
        if let Err(e) = spawned {
            eprintln!("limina-agent-session: no idle-inhibit watcher ({e})");
        }
        Inhibitors { cell }
    }

    /// Whether any application in this session inhibits idle, or `None` when that cannot be read.
    pub fn current(&self) -> Option<bool> {
        current(&self.cell)
    }
}

fn current(cell: &Mutex<Option<bool>>) -> Option<bool> {
    *cell.lock().unwrap_or_else(|e| e.into_inner())
}

fn set(cell: &Mutex<Option<bool>>, value: Option<bool>) {
    *cell.lock().unwrap_or_else(|e| e.into_inner()) = value;
}

/// Connect and read the property once, leaving the proxy ready to follow it.
fn probe(cell: &Mutex<Option<bool>>) -> zbus::Result<zbus::blocking::Proxy<'static>> {
    let conn = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&conn, BUS, PATH, IFACE)?;
    // The initial Get doubles as the liveness probe: it fails fast when nothing owns the name.
    let actions: u32 = proxy.get_property(PROPERTY)?;
    set(cell, Some(inhibits_idle(actions)));
    Ok(proxy)
}

fn watch_forever(cell: &Mutex<Option<bool>>, mut first: Option<zbus::blocking::Proxy<'static>>) {
    // The startup failure, if any, was already said; after that one line per change of fortune.
    let mut logged = first.is_none();
    loop {
        match first.take().map_or_else(|| probe(cell), Ok) {
            Ok(proxy) => {
                if logged {
                    eprintln!("limina-agent-session: following the session's idle inhibitors");
                }
                logged = false;
                // Same caveat as the root agent's power-profile watcher: in zbus 5.16 the stream
                // parks rather than ends when the bus dies, and a dead session bus ends this
                // helper anyway. An ending stream is handled as a re-watch.
                for change in proxy.receive_property_changed::<u32>(PROPERTY) {
                    if let Ok(actions) = change.get() {
                        set(cell, Some(inhibits_idle(actions)));
                    }
                }
                set(cell, None);
                eprintln!("limina-agent-session: idle inhibitors stream ended; re-watching");
            }
            Err(e) if !logged => {
                eprintln!(
                    "limina-agent-session: idle inhibitors unavailable ({e}); retrying quietly"
                );
                logged = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(RETRY_EVERY);
    }
}

/// What one host channel should be told, and when. Constructed fresh for every channel, so the
/// current value is (re)sent on connect and after that only a change is.
pub struct Reporter {
    sent: Option<bool>,
}

impl Reporter {
    pub fn new() -> Reporter {
        Reporter { sent: None }
    }

    /// The value to send now, if any. `now` is `None` while the inhibitors cannot be read, which
    /// sends nothing: a channel that could not read them never announced the capability.
    pub fn due(&mut self, now: Option<bool>) -> Option<bool> {
        let now = now?;
        (self.sent != Some(now)).then(|| {
            self.sent = Some(now);
            now
        })
    }
}

/// What this session reports: its inhibitors, but only while it is the active seat session.
pub fn effective(inhibitors: Option<bool>, seat_active: bool) -> Option<bool> {
    inhibitors.map(|inhibited| inhibited && seat_active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_idle_flag_inhibits_idle() {
        assert!(!inhibits_idle(0));
        assert!(inhibits_idle(8), "a video player");
        assert!(
            !inhibits_idle(1 | 2 | 4 | 16),
            "logout, switch-user, suspend, automount"
        );
        assert!(inhibits_idle(4 | 8));
    }

    #[test]
    fn a_channel_hears_the_value_once_and_then_only_changes() {
        let mut r = Reporter::new();
        assert_eq!(
            r.due(Some(false)),
            Some(false),
            "the seed, even when nothing inhibits"
        );
        assert_eq!(r.due(Some(false)), None);
        assert_eq!(r.due(Some(true)), Some(true));
        assert_eq!(r.due(Some(true)), None);
        assert_eq!(r.due(Some(false)), Some(false));
    }

    #[test]
    fn nothing_is_sent_while_the_inhibitors_cannot_be_read() {
        let mut r = Reporter::new();
        assert_eq!(r.due(None), None);
        assert_eq!(
            r.due(Some(true)),
            Some(true),
            "and the first readable value goes out"
        );
    }

    #[test]
    fn a_session_switched_away_from_does_not_inhibit() {
        assert_eq!(effective(Some(true), false), Some(false));
        assert_eq!(effective(Some(true), true), Some(true));
        assert_eq!(effective(None, true), None);
    }
}
