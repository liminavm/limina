// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The `no-throttle` lever's assertion: keep App Nap off the supervisor while a harness measures.
//!
//! App Nap moves every thread of an app that is neither frontmost nor visible to the background
//! band after about 40 s, and the main thread is the one every frame goes on glass from: guest
//! flips keep coming at full rate while the window shows fewer of them, late
//! (`docs/graphics.md` §8, "Host state and timing honesty"). An `NSProcessInfo` activity with
//! `UserInitiatedAllowingIdleSystemSleep` exempts the app from App Nap; `LatencyCritical` is
//! added because the work it protects is the present path's timing. It leaves both sleeps
//! alone: display sleep is `display_awake`'s, and a harness lever must not keep the Mac awake.
//!
//! Only the supervisor holds it. The worker is a launchd job with `ProcessType=Interactive`, not
//! an app, and was measured unthrottled in every window state (priorities, guest CPU, timer
//! latency), so nothing is carried over its debug link.
//!
//! It does nothing against Game Mode, which clamps an app's whole process tree through its
//! thread group whatever the app asserts (`spikes/game-mode-throttle/RESULTS.md`, result 1).
//!
//! Held while the lever is on: [`reconcile`] takes or releases it to match, from the lever's
//! request handler and from every window tick (which covers a lever seeded from its variable).

use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

/// The activity token. `beginActivity`/`endActivity` are thread-safe; objc2 leaves the token
/// `!Send` conservatively.
struct Token(Retained<ProtocolObject<dyn NSObjectProtocol>>);
// SAFETY: see above — the token is only handed back to `endActivity`, from whichever thread.
unsafe impl Send for Token {}

static HELD: Mutex<Option<Token>> = Mutex::new(None);

/// The options taken: no App Nap, latency-critical, idle system and display sleep allowed.
pub(crate) fn options() -> NSActivityOptions {
    NSActivityOptions::UserInitiatedAllowingIdleSystemSleep | NSActivityOptions::LatencyCritical
}

/// Take or release the activity so it matches the `no-throttle` lever.
pub(crate) fn reconcile() {
    let want = crate::debug_ctl::NO_THROTTLE.on();
    let mut held = HELD.lock().unwrap_or_else(|p| p.into_inner());
    match (want, held.is_some()) {
        (true, false) => {
            let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
                options(),
                &NSString::from_str("limina no-throttle: a harness is measuring guest timing"),
            );
            *held = Some(Token(token));
            log::warn!("no-throttle: holding a user-initiated, latency-critical activity");
        }
        (false, true) => {
            if let Some(Token(token)) = held.take() {
                // SAFETY: the token `beginActivityWithOptions` returned, ended once.
                unsafe { NSProcessInfo::processInfo().endActivity(&token) };
                log::warn!("no-throttle: activity released");
            }
        }
        _ => {}
    }
}

/// Whether the activity is held now.
pub(crate) fn held() -> bool {
    HELD.lock().unwrap_or_else(|p| p.into_inner()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_options_prevent_app_nap_and_leave_both_sleeps_alone() {
        let o = options();
        assert!(o.contains(NSActivityOptions::UserInitiatedAllowingIdleSystemSleep));
        assert!(o.contains(NSActivityOptions::LatencyCritical));
        assert!(!o.contains(NSActivityOptions::IdleSystemSleepDisabled));
        assert!(!o.contains(NSActivityOptions::IdleDisplaySleepDisabled));
    }

    #[test]
    fn the_activity_follows_the_lever() {
        let _serial = crate::debug_ctl::ACCESS_LEVER_TESTS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let lever = &crate::debug_ctl::NO_THROTTLE;
        assert!(!lever.on(), "off by default");
        reconcile();
        assert!(!held());
        lever.set(true);
        reconcile();
        assert!(held());
        // Idempotent: a second reconcile takes nothing more.
        reconcile();
        assert!(held());
        lever.set(false);
        reconcile();
        assert!(!held());
    }
}
