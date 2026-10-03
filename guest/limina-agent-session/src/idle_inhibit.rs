// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Relay the session's idle inhibitors to the host, so it keeps its display awake while a
//! guest application asks the guest's own display to stay on.
//!
//! No single interface holds every inhibitor. An application inhibits with a Wayland
//! idle-inhibitor on its surface, or over D-Bus through `org.freedesktop.ScreenSaver`, the
//! portal or gnome-session, and each desktop sends those to a different place. So the helper
//! follows every source it can read and reports idle inhibited when **any** of them says so:
//!
//!   - **The compositor's own verdict**, through `ext-idle-notify-v1` version 2
//!     ([`crate::idle_notify`]). KWin, wlroots compositors (sway) and synoik offer it; mutter
//!     does not. It reflects what the compositor honours: on sway and synoik that is Wayland and
//!     D-Bus inhibitors alike, on KWin only Wayland ones.
//!   - **gnome-session**, for GNOME, where the portal and `org.freedesktop.ScreenSaver` land too:
//!     Firefox playing a video registers "Playing video" with the idle flag. gnome-session keeps
//!     that on the session bus as `org.gnome.SessionManager`'s `InhibitedActions` and does not
//!     forward it to logind, so this helper, already on the session bus, is the one component
//!     that can see it.
//!   - **PowerDevil**, for KDE's D-Bus inhibitors. KWin owns `org.freedesktop.ScreenSaver` and
//!     passes its inhibits to PowerDevil a few seconds later; PowerDevil's policy agent then
//!     answers `HasInhibition(ChangeScreenSettings)`.
//!
//! "Any" rather than "the best one available", because a source can be present without being the
//! authority: synoik runs inside a gnome-session but keeps the inhibits it receives, so
//! gnome-session's answer there is a confident "no" while a video plays. Without any source the
//! host falls back on its audio + decode heuristic (`crates/limina/src/window/wake_policy.rs`).
//!
//! Three rules shape the report:
//!   - **Advertised only when readable.** The `idleinhibit` capability means "my report is the
//!     truth about this session", so a helper on a desktop with no source does not claim it,
//!     and the host keeps its heuristic.
//!   - **Only the active seat session inhibits.** The host display is showing the active
//!     session; a video playing in a session switched away from is not being watched. Same rule
//!     as the arrangement report (`layout_gate`).
//!   - **Level-triggered.** The current value goes out on every new channel and on each change,
//!     like the root agent's power profile, so a reconnect resynchronises with no replay.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::idle_notify;

/// gnome-session's `GSM_INHIBITOR_FLAG_IDLE`: the session must not be marked idle.
const IDLE: u32 = 8;

/// PowerDevil's `PolicyAgent::ChangeScreenSettings`: the policy that dims and turns off the
/// screen.
const CHANGE_SCREEN_SETTINGS: u32 = 4;

/// Backoff between attempts to reach a source that is not there. A desktop without it retries
/// for the helper's whole life, so not short; but a source that comes up just after the helper's
/// startup probe should not wait long for its first report either.
const RETRY_EVERY: Duration = Duration::from_secs(10);

/// Whether an `InhibitedActions` value includes the idle flag.
pub fn inhibits_idle(actions: u32) -> bool {
    actions & IDLE != 0
}

/// One source's reading: `None` while it cannot be read.
type Cell = Arc<Mutex<Option<bool>>>;

/// What the session's inhibitors say, kept current by one background thread per source: `None`
/// while no source can be read.
pub struct Inhibitors {
    cells: Vec<Cell>,
}

impl Inhibitors {
    /// Read every source once now, then follow each on a thread for the life of the process.
    ///
    /// The first reads happen here, before the helper's first HELLO, so the capability it
    /// announces is right from the start instead of after a reconnect. Infallible by design: a
    /// source that fails stays `None`, and with none readable the helper behaves as if the
    /// desktop had nothing to report.
    pub fn watch() -> Inhibitors {
        let mut cells = Vec::new();
        let mut following = Vec::new();
        let mut missing = Vec::new();
        for kind in [Kind::Compositor, Kind::GnomeSession, Kind::PowerDevil] {
            let cell: Cell = Arc::new(Mutex::new(None));
            let first = kind.probe(&cell);
            match &first {
                Ok(_) => following.push(kind.name()),
                Err(e) => missing.push(format!("{}: {e}", kind.name())),
            }
            let shared = Arc::clone(&cell);
            let spawned = std::thread::Builder::new()
                .name("idle-inhibit".into())
                .spawn(move || watch_forever(kind, &shared, first.ok()));
            if let Err(e) = spawned {
                eprintln!("limina-agent-session: no {} watcher ({e})", kind.name());
            }
            cells.push(cell);
        }
        let inhibitors = Inhibitors { cells };
        if following.is_empty() {
            eprintln!(
                "limina-agent-session: no idle inhibitors to relay ({}); retrying quietly",
                missing.join("; ")
            );
        } else {
            eprintln!(
                "limina-agent-session: following the session's idle inhibitors through {} \
                 (idle {})",
                following.join(" and "),
                if inhibitors.current() == Some(true) {
                    "inhibited"
                } else {
                    "not inhibited"
                }
            );
        }
        inhibitors
    }

    /// Whether any application in this session inhibits idle, or `None` when no source can be
    /// read.
    pub fn current(&self) -> Option<bool> {
        combine(self.cells.iter().map(|c| current(c)))
    }
}

/// Inhibited when any readable source says so; unreadable when none can be read.
fn combine(readings: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    readings
        .into_iter()
        .flatten()
        .fold(None, |acc, r| Some(acc.unwrap_or(false) || r))
}

fn current(cell: &Mutex<Option<bool>>) -> Option<bool> {
    *cell.lock().unwrap_or_else(|e| e.into_inner())
}

fn set(cell: &Mutex<Option<bool>>, value: Option<bool>) {
    *cell.lock().unwrap_or_else(|e| e.into_inner()) = value;
}

#[derive(Clone, Copy)]
enum Kind {
    Compositor,
    GnomeSession,
    PowerDevil,
}

/// A source being followed.
enum Live {
    Compositor(Box<idle_notify::Compositor>),
    /// A D-Bus object: `read` answers the question, and a change to the `changes` property is
    /// the cue to ask again.
    Bus {
        proxy: zbus::blocking::Proxy<'static>,
        changes: &'static str,
        read: fn(&zbus::blocking::Proxy<'static>) -> zbus::Result<bool>,
    },
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Compositor => "the compositor (ext-idle-notify-v1)",
            Kind::GnomeSession => "gnome-session",
            Kind::PowerDevil => "PowerDevil",
        }
    }

    /// Reach the source and read it once into `cell`, or say why it cannot be read.
    fn probe(self, cell: &Mutex<Option<bool>>) -> Result<Live, String> {
        let live = match self {
            Kind::Compositor => Live::Compositor(Box::new(idle_notify::probe()?)),
            Kind::GnomeSession => bus(
                "org.gnome.SessionManager",
                "/org/gnome/SessionManager",
                "org.gnome.SessionManager",
                "InhibitedActions",
                |p| Ok(inhibits_idle(p.get_property("InhibitedActions")?)),
            )?,
            Kind::PowerDevil => bus(
                "org.kde.Solid.PowerManagement",
                "/org/kde/Solid/PowerManagement/PolicyAgent",
                "org.kde.Solid.PowerManagement.PolicyAgent",
                "ActiveInhibitions",
                |p| p.call("HasInhibition", &(CHANGE_SCREEN_SETTINGS,)),
            )?,
        };
        set(cell, Some(live.read()));
        Ok(live)
    }
}

fn bus(
    name: &'static str,
    path: &'static str,
    iface: &'static str,
    changes: &'static str,
    read: fn(&zbus::blocking::Proxy<'static>) -> zbus::Result<bool>,
) -> Result<Live, String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&conn, name, path, iface).map_err(|e| e.to_string())?;
    // The first read doubles as the liveness probe: it fails fast when nothing owns the name.
    read(&proxy).map_err(|e| e.to_string())?;
    Ok(Live::Bus {
        proxy,
        changes,
        read,
    })
}

impl Live {
    fn read(&self) -> bool {
        match self {
            Live::Compositor(compositor) => compositor.inhibited(),
            Live::Bus { proxy, read, .. } => read(proxy).unwrap_or(false),
        }
    }

    /// Keep `cell` current until the source fails, and say why it did.
    fn follow(self, cell: &Mutex<Option<bool>>) -> String {
        match self {
            Live::Compositor(compositor) => {
                compositor.follow(|inhibited| set(cell, Some(inhibited)))
            }
            Live::Bus {
                proxy,
                changes,
                read,
            } => {
                // Same caveat as the root agent's power-profile watcher: in zbus 5.16 the stream
                // parks rather than ends when the bus dies, and a dead session bus ends this
                // helper anyway. An ending stream is handled as a re-watch.
                for _ in proxy.receive_property_changed::<zbus::zvariant::OwnedValue>(changes) {
                    match read(&proxy) {
                        Ok(inhibited) => set(cell, Some(inhibited)),
                        Err(e) => return format!("reading it failed: {e}"),
                    }
                }
                "the property stream ended".into()
            }
        }
    }
}

fn watch_forever(kind: Kind, cell: &Mutex<Option<bool>>, mut first: Option<Live>) {
    // The startup outcome was already said; after that one line per change of fortune.
    let mut reachable = first.is_some();
    loop {
        match first.take().map_or_else(|| kind.probe(cell), Ok) {
            Ok(live) => {
                if !reachable {
                    eprintln!(
                        "limina-agent-session: following idle inhibitors through {}",
                        kind.name()
                    );
                }
                reachable = true;
                let why = live.follow(cell);
                set(cell, None);
                eprintln!(
                    "limina-agent-session: lost {} ({why}); re-watching",
                    kind.name()
                );
            }
            Err(_) => reachable = false,
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
    fn any_readable_source_inhibits() {
        assert_eq!(combine([None, None, None]), None, "nothing to read");
        assert_eq!(combine([Some(false), None, None]), Some(false));
        assert_eq!(
            combine([Some(true), Some(false), None]),
            Some(true),
            "synoik: the compositor says yes, gnome-session no"
        );
        assert_eq!(
            combine([Some(false), None, Some(true)]),
            Some(true),
            "KDE: a D-Bus inhibitor only PowerDevil sees"
        );
    }

    #[test]
    fn a_session_switched_away_from_does_not_inhibit() {
        assert_eq!(effective(Some(true), false), Some(false));
        assert_eq!(effective(Some(true), true), Some(true));
        assert_eq!(effective(None, true), None);
    }
}
