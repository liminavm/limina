// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The guest's power profile, watched on D-Bus and relayed to the host.
//!
//! A desktop power profile normally resolves to hardware; our guest has none, so every joule is
//! spent on the host and the profile is **user intent we relay** — the host backs it with policy
//! (vCPU QoS, the RT band, CPU reclaim). See `docs/design/power-profiles.md`.
//!
//! The provider is whichever daemon owns `net.hadess.PowerProfiles` (`tuned-ppd` on Fedora,
//! `power-profiles-daemon` elsewhere) — only the D-Bus interface is involved, so both work.
//! A guest with no such daemon (no GNOME, or the L1 test guest with no bus at all) is normal,
//! not an error: the watcher stays at [`UNKNOWN`], nothing is ever sent, and the host holds its
//! default.
//!
//! Shape: one background thread blocks on the `ActiveProfile` property stream and publishes the
//! wire value into an atomic cell; the serve loop reads the cell on its existing idle tick. The
//! alternative — polling the property from the tick — would put a D-Bus round trip on every idle
//! second forever, in an agent whose surrounding workstream is about *removing* idle work.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use limina_proto::PowerProfileMsg;

/// Cell value meaning "no profile known" (no daemon, or none seen yet). Never sent on the wire:
/// the wire vocabulary is `PowerProfileMsg`'s 0/1/2.
pub const UNKNOWN: u8 = 0xff;

/// Backoff between attempts to reach the profile daemon. Long, because the common failure is a
/// guest that simply has no daemon (or no system bus), and that guest retries forever.
const RETRY_EVERY: Duration = Duration::from_secs(30);

/// Decides when a profile report is due. Level-triggered: constructed fresh for every host
/// connection, so the current profile is (re)sent on connect — a reconnect, a host restart and a
/// snapshot restore all resynchronise for free — and after that only a change is sent.
pub struct ProfileReporter {
    last: u8,
}

impl ProfileReporter {
    pub fn new() -> ProfileReporter {
        ProfileReporter { last: UNKNOWN }
    }

    /// The wire value to send now, if any: the current profile when it differs from what this
    /// connection last sent, `None` while it is unknown or unchanged.
    pub fn due(&mut self, current: u8) -> Option<u8> {
        if current == UNKNOWN || current == self.last {
            return None;
        }
        self.last = current;
        Some(current)
    }
}

/// The background watcher. Owns nothing but the shared cell; the thread runs for the life of the
/// process.
pub struct ProfileWatcher {
    cell: Arc<AtomicU8>,
}

impl ProfileWatcher {
    /// Spawn the watcher thread. Infallible by design: on any failure (no bus, no daemon, no
    /// thread) the cell simply stays [`UNKNOWN`] and the agent behaves as if the guest had no
    /// profile to report.
    pub fn start() -> ProfileWatcher {
        let cell = Arc::new(AtomicU8::new(UNKNOWN));
        let shared = Arc::clone(&cell);
        let spawned = std::thread::Builder::new()
            .name("power-profile".into())
            .spawn(move || watch_forever(&shared));
        if let Err(e) = spawned {
            eprintln!("limina-agent: no power-profile watcher ({e})");
        }
        ProfileWatcher { cell }
    }

    /// The current wire value, or [`UNKNOWN`].
    pub fn current(&self) -> u8 {
        self.cell.load(Ordering::Relaxed)
    }
}

fn watch_forever(cell: &AtomicU8) {
    // One line for the first failure, then quiet: a daemon-less guest hits this every retry
    // forever, and the reason will not change.
    let mut logged = false;
    loop {
        match watch_once(cell) {
            Ok(()) => {
                // The property stream ended: the bus connection died (a dbus-broker restart).
                // The daemon may come back with a different profile, so re-establish.
                eprintln!("limina-agent: power-profile stream ended; re-watching");
                logged = false;
            }
            Err(e) if !logged => {
                eprintln!("limina-agent: power profile unavailable ({e}); retrying quietly");
                logged = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(RETRY_EVERY);
    }
}

/// Connect, read the initial profile, then block on changes until the stream ends.
fn watch_once(cell: &AtomicU8) -> zbus::Result<()> {
    let conn = zbus::blocking::Connection::system()?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "net.hadess.PowerProfiles",
        "/net/hadess/PowerProfiles",
        "net.hadess.PowerProfiles",
    )?;
    // The initial Get doubles as the liveness probe: it fails fast when nothing owns the name.
    let active: String = proxy.get_property("ActiveProfile")?;
    eprintln!("limina-agent: watching power profile (active: {active})");
    cell.store(PowerProfileMsg::wire_from_dbus(&active), Ordering::Relaxed);

    for change in proxy.receive_property_changed::<String>("ActiveProfile") {
        let Ok(active) = change.get() else {
            // An unreadable change (the daemon went away mid-signal): keep the last value —
            // it is still what the daemon most recently applied — and wait for the next.
            continue;
        };
        cell.store(PowerProfileMsg::wire_from_dbus(&active), Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The level-trigger contract: a fresh reporter sends the current profile once, repeats
    /// nothing, and reports every change exactly once.
    #[test]
    fn a_connection_gets_the_profile_once_and_then_only_changes() {
        let mut r = ProfileReporter::new();
        assert_eq!(
            r.due(PowerProfileMsg::BALANCED),
            Some(PowerProfileMsg::BALANCED)
        );
        assert_eq!(r.due(PowerProfileMsg::BALANCED), None);
        assert_eq!(
            r.due(PowerProfileMsg::POWER_SAVER),
            Some(PowerProfileMsg::POWER_SAVER)
        );
        assert_eq!(r.due(PowerProfileMsg::POWER_SAVER), None);
    }

    /// A guest with no profile daemon reports nothing, ever — the host holding its default IS
    /// the correct outcome, so there is no message for this state.
    #[test]
    fn unknown_is_never_reported() {
        let mut r = ProfileReporter::new();
        assert_eq!(r.due(UNKNOWN), None);
        // ...including after a real value: a daemon that went away does not unsend its profile.
        assert_eq!(
            r.due(PowerProfileMsg::PERFORMANCE),
            Some(PowerProfileMsg::PERFORMANCE)
        );
        assert_eq!(r.due(UNKNOWN), None);
    }

    /// A reconnect constructs a fresh reporter; the unchanged profile must be sent again, since
    /// the other end may be a freshly restarted host that knows nothing.
    #[test]
    fn a_fresh_reporter_resends_for_a_fresh_host() {
        let mut a = ProfileReporter::new();
        assert_eq!(
            a.due(PowerProfileMsg::POWER_SAVER),
            Some(PowerProfileMsg::POWER_SAVER)
        );
        let mut b = ProfileReporter::new();
        assert_eq!(
            b.due(PowerProfileMsg::POWER_SAVER),
            Some(PowerProfileMsg::POWER_SAVER)
        );
    }
}
