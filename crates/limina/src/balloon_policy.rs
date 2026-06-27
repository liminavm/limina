// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M6 PSI autoballoon **policy** (supervisor side). Consumes guest [`MemPressure`] reports from the
//! control plane and drives the balloon target between `0` and `max-min` pages with hysteresis + a
//! dwell, by writing `target <bytes>` to the worker's balloon control socket.
//!
//! Mechanism vs policy: the balloon device, the target/`actual` loop, and the control socket are in
//! libkrun / limina-vmm; *this* is the policy that decides when and how much. The rule is simple and
//! conservative: release fast under pressure (always safe), reclaim gradually when the guest is idle
//! with memory to spare. The thresholds are starting points, not a tuned policy.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use limina_proto::MemPressure;

/// `some` pressure (hundredths of a percent) at/above which we deflate fast (give memory back).
const PRESSURE_HIGH: u32 = 1000; // 10.00%
/// `some` pressure at/below which the guest counts as idle (eligible to inflate).
const PRESSURE_LOW: u32 = 200; // 2.00%
/// Plus: inflate only while MemAvailable is at least this fraction of MemTotal (percent).
const IDLE_FREE_PERCENT: u64 = 30;
/// Minimum time between *inflation* steps (releasing under pressure ignores this — it's urgent).
const DWELL: Duration = Duration::from_millis(800);

/// 4 KiB balloon pages per MiB.
pub const PAGES_PER_MIB: u32 = 256;

/// The supervisor-side autoballoon policy. Cheap to construct; thread-safe (`on_pressure` locks).
pub struct BalloonPolicy {
    /// Floor: effective guest RAM never shrinks below this (in 4 KiB pages).
    min_pages: u32,
    /// Ceiling: total guest RAM libkrun allocated (in 4 KiB pages).
    max_pages: u32,
    /// The worker's balloon control socket (`target <bytes>` / `stats`).
    socket: PathBuf,
    state: Mutex<State>,
}

struct State {
    /// A kept-open connection to the balloon socket (reconnected on error).
    conn: Option<UnixStream>,
    /// The balloon size we've currently commanded (4 KiB pages).
    target_pages: u32,
    /// When we last changed the target (for the inflation dwell).
    last_change: Option<Instant>,
}

impl BalloonPolicy {
    pub fn new(min_pages: u32, max_pages: u32, socket: PathBuf) -> Self {
        Self {
            min_pages,
            max_pages,
            socket,
            state: Mutex::new(State {
                conn: None,
                target_pages: 0,
                last_change: None,
            }),
        }
    }

    /// The most the balloon may inflate to, leaving the guest with `min`.
    fn room(&self) -> u32 {
        self.max_pages.saturating_sub(self.min_pages)
    }

    /// Decide the next target from a pressure report and drive it. Pure decision in
    /// [`Self::decide`]; this wrapper adds the I/O (connect + write) and dwell bookkeeping.
    pub fn on_pressure(&self, p: &MemPressure) {
        let room = self.room();
        if room == 0 {
            return;
        }
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        let Some(new_target) = decide(p, st.target_pages, room, st.last_change, now) else {
            return;
        };
        if self.send_target(&mut st, new_target) {
            st.target_pages = new_target;
            st.last_change = Some(now);
            log::debug!(
                "autoballoon: target -> {new_target} pages (some_avg10={}, avail/total={}/{})",
                p.some_avg10,
                p.mem_available_kib,
                p.mem_total_kib
            );
        }
    }

    /// Write `target <bytes>` to the balloon socket, reconnecting once on failure. Returns whether
    /// the command went out.
    fn send_target(&self, st: &mut State, pages: u32) -> bool {
        let bytes = (pages as u64) << 12;
        for attempt in 0..2 {
            if st.conn.is_none() {
                match UnixStream::connect(&self.socket) {
                    Ok(c) => st.conn = Some(c),
                    Err(e) => {
                        if attempt == 1 {
                            log::warn!("autoballoon: connect {:?}: {e}", self.socket);
                        }
                        continue;
                    }
                }
            }
            let conn = st.conn.as_mut().unwrap();
            if writeln!(conn, "target {bytes}")
                .and_then(|()| conn.flush())
                .is_ok()
            {
                return true;
            }
            st.conn = None; // broken pipe — drop and retry once
        }
        false
    }
}

/// The pure policy decision (unit-tested): given a pressure report and the current target, return
/// the next target in pages, or `None` to hold. Releases to 0 immediately under high pressure;
/// inflates one step (¼ of room) toward full when idle with memory to spare, gated by `dwell`.
fn decide(
    p: &MemPressure,
    current: u32,
    room: u32,
    last_change: Option<Instant>,
    now: Instant,
) -> Option<u32> {
    let idle_free = p.mem_total_kib > 0
        && p.mem_available_kib.saturating_mul(100)
            >= p.mem_total_kib.saturating_mul(IDLE_FREE_PERCENT);

    let desired = if p.some_avg10 >= PRESSURE_HIGH {
        0 // under pressure: hand memory back to the guest, now
    } else if p.some_avg10 <= PRESSURE_LOW && idle_free {
        room // idle with headroom: reclaim toward the floor
    } else {
        return None; // neutral band: hold (hysteresis)
    };

    if desired == current {
        return None;
    }
    // Inflation is gradual and rate-limited; release is immediate.
    let next = if desired == 0 {
        0
    } else {
        if let Some(t) = last_change {
            if now.duration_since(t) < DWELL {
                return None;
            }
        }
        let step = (room / 4).max(1);
        (current + step).min(desired)
    };
    (next != current).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pressure(some_avg10: u32, avail_kib: u64, total_kib: u64) -> MemPressure {
        MemPressure {
            some_avg10,
            some_avg60: 0,
            full_avg10: 0,
            full_avg60: 0,
            mem_available_kib: avail_kib,
            mem_total_kib: total_kib,
        }
    }

    #[test]
    fn high_pressure_releases_to_zero_immediately() {
        let now = Instant::now();
        // Even with a recent change (dwell), release ignores the dwell.
        let next = decide(&pressure(5000, 100, 1000), 800, 1000, Some(now), now);
        assert_eq!(next, Some(0));
    }

    #[test]
    fn idle_with_headroom_inflates_one_step() {
        let now = Instant::now();
        // room=1000, step=250; from 0 -> 250 when idle (some<=2%) and >=30% free.
        let next = decide(&pressure(0, 700, 1000), 0, 1000, None, now);
        assert_eq!(next, Some(250));
    }

    #[test]
    fn idle_inflation_respects_dwell() {
        let now = Instant::now();
        // A change just happened -> hold until the dwell elapses.
        let next = decide(&pressure(0, 700, 1000), 250, 1000, Some(now), now);
        assert_eq!(next, None);
    }

    #[test]
    fn idle_but_low_free_holds() {
        let now = Instant::now();
        // Idle pressure but little available memory (<30%): don't reclaim.
        let next = decide(&pressure(0, 100, 1000), 0, 1000, None, now);
        assert_eq!(next, None);
    }

    #[test]
    fn neutral_band_holds() {
        let now = Instant::now();
        // some=5% is between LOW(2%) and HIGH(10%) -> hold.
        let next = decide(&pressure(500, 700, 1000), 250, 1000, None, now);
        assert_eq!(next, None);
    }
}
