// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! When to step a stock guest's clock. Pure: no socket, no clock of its own — the caller
//! measures, this decides, so the rule is unit-testable and the trace of a real boot can be
//! replayed through it.
//!
//! The threshold is deliberately the same **1 s** that `limina-agent` uses
//! (`guest/limina-agent/src/main.rs`, `STEP_THRESHOLD_NS`): below it the guest's own NTP is
//! the better corrector, and the RTC we anchor to host wallclock is seconds-granular
//! anyway, so a tighter bound would only chase noise.

use std::time::Duration;

/// Step the guest clock only past this much disagreement.
pub const STEP_THRESHOLD: Duration = Duration::from_secs(1);

/// A round trip slower than this makes the midpoint estimate worse than the threshold it
/// feeds, so the sample is thrown away rather than acted on. A guest-serial round trip is
/// sub-millisecond in practice; this only trips when the guest is badly loaded or the agent
/// is busy.
pub const MAX_RTT: Duration = Duration::from_millis(250);

/// One measurement of the guest's clock against the host's, taken by bracketing
/// `guest-get-time` with two host reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSample {
    /// Host `CLOCK_REALTIME` at the midpoint of the round trip, in ns since the epoch.
    pub host_ns: i64,
    /// What the guest answered, in ns since the epoch.
    pub guest_ns: i64,
    /// How long the round trip took.
    pub rtt: Duration,
}

impl TimeSample {
    /// How far the guest is *behind* the host (negative = ahead).
    pub fn delta_ns(&self) -> i64 {
        self.host_ns.saturating_sub(self.guest_ns)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The guest's clock is close enough; leave it to the guest's own timekeeping.
    Nothing,
    /// The measurement is too noisy to act on — take another one later.
    Resample,
    /// Set the guest clock. Carries the measured delta purely so the caller can say how bad
    /// it was; the value actually sent is a fresh host reading at send time.
    Step { delta_ns: i64 },
}

/// Decide what to do about one sample.
pub fn decide(sample: &TimeSample) -> Action {
    if sample.rtt > MAX_RTT {
        return Action::Resample;
    }
    let delta = sample.delta_ns();
    if delta.unsigned_abs() < STEP_THRESHOLD.as_nanos() as u64 {
        return Action::Nothing;
    }
    Action::Step { delta_ns: delta }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000_000_000;

    fn sample(guest_ns: i64, rtt: Duration) -> TimeSample {
        TimeSample {
            host_ns: NOW,
            guest_ns,
            rtt,
        }
    }

    #[test]
    fn an_agreeing_clock_is_left_alone() {
        let s = sample(NOW - 400_000_000, Duration::from_millis(1));
        assert_eq!(decide(&s), Action::Nothing);
    }

    #[test]
    fn a_guest_that_slept_through_a_host_nap_is_stepped_forward() {
        // The dogfooded failure: the host napped six hours, the guest's counter did not.
        let behind = 6 * 3600 * 1_000_000_000i64;
        let s = sample(NOW - behind, Duration::from_millis(2));
        assert_eq!(decide(&s), Action::Step { delta_ns: behind });
    }

    #[test]
    fn a_guest_ahead_of_the_host_is_stepped_back() {
        let s = sample(NOW + 90 * 1_000_000_000, Duration::from_millis(2));
        assert_eq!(
            decide(&s),
            Action::Step {
                delta_ns: -90 * 1_000_000_000
            }
        );
    }

    #[test]
    fn the_threshold_is_exactly_one_second() {
        let just_under = sample(NOW - 999_999_999, Duration::from_millis(1));
        assert_eq!(decide(&just_under), Action::Nothing);
        let at = sample(NOW - 1_000_000_000, Duration::from_millis(1));
        assert!(matches!(decide(&at), Action::Step { .. }));
    }

    #[test]
    fn a_noisy_round_trip_is_measured_again_rather_than_acted_on() {
        // Big delta, but the measurement cannot be trusted to that precision.
        let s = sample(NOW - 30 * 1_000_000_000, MAX_RTT + Duration::from_millis(1));
        assert_eq!(decide(&s), Action::Resample);
    }

    #[test]
    fn a_nonsense_guest_clock_is_still_a_step() {
        // A guest whose clock never left the epoch (no RTC read, no NTP) must be corrected,
        // not treated as an arithmetic edge case.
        let s = sample(0, Duration::from_millis(1));
        assert_eq!(decide(&s), Action::Step { delta_ns: NOW });
    }
}
