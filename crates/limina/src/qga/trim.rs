// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! When to ask the guest to hand its free blocks back. Pure: no clock, no socket, no
//! sysctls — the caller measures, this decides.
//!
//! A raw disk image only ever grows. The guest's `rm` frees blocks inside its filesystem,
//! but the host file keeps the allocation unless the guest also *discards* the range —
//! virtio-blk `VIRTIO_BLK_T_DISCARD`, which our imago fork turns into a punch-hole. So a
//! trim is the only way an image gives space back, and nobody but the guest knows which
//! blocks are free.
//!
//! Measured on a weeks-old F44 enhanced image (`spikes/qga-fstrim/RESULTS.md`): a trim
//! returned 958 MiB, ~6% of the image. But Fedora already mounts btrfs `discard=async` and
//! ships `fstrim.timer` enabled, and freshly-freed extents came back on their own within 30 s
//! — so this is **residue collection on a long cadence**, not a hot path. Everything below
//! follows from that: rare, cheap to skip, and never in anyone's way.
//!
//! The gate is the balloon's idle-scrub rule ([`crate::balloon_policy`]) in miniature, and for
//! the same reason: the work is worth doing only when nobody is waiting on the resource it
//! competes for. A trim floods the guest's block layer and the host's, so it wants a calm host
//! *and* a guest that is not itself doing I/O.

use std::time::{Duration, Instant};

/// How long between trims. Deliberately long: the guest's own `fstrim.timer` (weekly, where
/// it exists) is the floor this improves on, not a target to beat.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Never trim within this long of the port coming up. A guest is at its busiest while
/// booting, and a trim there competes with the thing the user is waiting for. Clamped to the
/// interval so a shortened cadence (tests) shortens the settle with it.
pub const SETTLE: Duration = Duration::from_secs(10 * 60);

/// Don't bother discarding runs shorter than this. Tiny extents cost a descriptor each and
/// punch holes too small for the host filesystem to release anyway.
pub const MIN_EXTENT: u64 = 1024 * 1024;

/// Guest PSI `full` IO pressure (hundredths of a percent, `avg10`) at/below which the guest
/// counts as not doing I/O. Only enhanced guests report it; see [`Gate::guest_io_full_avg10`].
pub const GUEST_IO_CALM: u32 = 500;

/// How long a guest's PSI reading stays meaningful. An enhanced guest reports every few
/// seconds; when the reports stop — its `limina-agent` died, or the guest is mid-upgrade —
/// the last reading must not keep speaking for it. If that reading happened to be "busy" it
/// would refuse every trim for the rest of the session, silently. Stale therefore reads the
/// same way absent does ([`Gate::guest_io_full_avg10`]): silence is not evidence of load.
pub const PSI_FRESH: Duration = Duration::from_secs(5 * 60);

/// The gate's view of the guest's IO pressure: the last reading, if it still speaks for now.
pub fn fresh_psi(now: Instant, last: Option<(Instant, u32)>) -> Option<u32> {
    last.filter(|(at, _)| now.duration_since(*at) < PSI_FRESH)
        .map(|(_, io)| io)
}

/// `LIMINA_QGA_TRIM_SECS` overrides the cadence; `0` disables trimming altogether.
pub fn interval_from_env() -> Option<Duration> {
    parse_interval(std::env::var("LIMINA_QGA_TRIM_SECS").ok().as_deref())
}

/// The pure half of [`interval_from_env`]. Unparseable text keeps the default rather than
/// disabling: a typo in a knob must not silently switch a feature off.
fn parse_interval(raw: Option<&str>) -> Option<Duration> {
    match raw {
        None => Some(DEFAULT_INTERVAL),
        Some("0") => None,
        Some(v) => Some(
            v.parse()
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_INTERVAL),
        ),
    }
}

/// What the caller measured about how busy things are right now.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    /// Host memory pressure is `Normal`.
    pub host_calm: bool,
    /// The guest's own PSI `full` IO `avg10`, from the last `MemPressure` report.
    ///
    /// `None` on a **stock** guest, which is the tier this whole feature exists for — it has
    /// no `limina-agent` to report PSI. Absence must therefore never block a trim; it only
    /// means we are trimming on the cadence and the host's calm alone.
    pub guest_io_full_avg10: Option<u32>,
}

/// Is a trim due on the clock alone? Separate from [`gate_ok`] because it is the cheap half:
/// the caller runs this every tick and only pays for measuring [`Gate`] when it passes.
pub fn due(
    now: Instant,
    attached_at: Instant,
    last_trim: Option<Instant>,
    interval: Duration,
) -> bool {
    let settle = SETTLE.min(interval);
    if now.duration_since(attached_at) < settle {
        return false;
    }
    match last_trim {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

/// Is now a good moment? `Err` carries why not, for the trace.
pub fn gate_ok(gate: Gate) -> Result<(), &'static str> {
    if !gate.host_calm {
        return Err("the host is under memory pressure");
    }
    match gate.guest_io_full_avg10 {
        Some(io) if io > GUEST_IO_CALM => Err("the guest is busy with its own IO"),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn nothing_is_trimmed_while_the_guest_is_still_settling() {
        let a = t0();
        assert!(!due(a + Duration::from_secs(60), a, None, DEFAULT_INTERVAL));
        assert!(due(a + SETTLE, a, None, DEFAULT_INTERVAL));
    }

    /// A shortened cadence has to shorten the settle with it, or a test that sets
    /// LIMINA_QGA_TRIM_SECS=30 would still wait ten minutes for the first trim.
    #[test]
    fn a_short_cadence_shortens_the_settle_too() {
        let a = t0();
        let interval = Duration::from_secs(30);
        assert!(!due(a + Duration::from_secs(20), a, None, interval));
        assert!(due(a + Duration::from_secs(30), a, None, interval));
    }

    #[test]
    fn the_cadence_holds_between_trims() {
        let a = t0();
        let last = a + SETTLE;
        assert!(!due(
            last + Duration::from_secs(60),
            a,
            Some(last),
            DEFAULT_INTERVAL
        ));
        assert!(due(
            last + DEFAULT_INTERVAL,
            a,
            Some(last),
            DEFAULT_INTERVAL
        ));
    }

    #[test]
    fn a_busy_host_or_a_busy_guest_defers_the_trim() {
        assert!(gate_ok(Gate {
            host_calm: false,
            guest_io_full_avg10: Some(0),
        })
        .is_err());
        assert!(gate_ok(Gate {
            host_calm: true,
            guest_io_full_avg10: Some(GUEST_IO_CALM + 1),
        })
        .is_err());
        assert!(gate_ok(Gate {
            host_calm: true,
            guest_io_full_avg10: Some(GUEST_IO_CALM),
        })
        .is_ok());
    }

    /// The stock tier reports no PSI at all, and it is the tier this feature is for. A
    /// missing reading must read as "no reason to wait", never as "assume the worst".
    #[test]
    fn a_stock_guest_reporting_no_psi_is_not_treated_as_busy() {
        assert!(gate_ok(Gate {
            host_calm: true,
            guest_io_full_avg10: None,
        })
        .is_ok());
    }

    /// The mirror of the rule above: a guest that *stops* reporting must stop being believed.
    /// A busy reading left frozen in place would refuse every trim for the rest of the session.
    #[test]
    fn a_psi_reading_stops_speaking_for_a_guest_that_went_quiet() {
        let a = t0();
        let busy = Some((a, GUEST_IO_CALM + 1));
        assert_eq!(
            fresh_psi(a + Duration::from_secs(30), busy),
            Some(GUEST_IO_CALM + 1)
        );
        assert_eq!(fresh_psi(a + PSI_FRESH, busy), None);
        assert_eq!(fresh_psi(a, None), None);
        assert!(gate_ok(Gate {
            host_calm: true,
            guest_io_full_avg10: fresh_psi(a + PSI_FRESH, busy),
        })
        .is_ok());
    }

    #[test]
    fn the_cadence_knob_can_disable_trimming_and_survives_a_typo() {
        assert_eq!(parse_interval(None), Some(DEFAULT_INTERVAL));
        assert_eq!(parse_interval(Some("0")), None);
        assert_eq!(parse_interval(Some("30")), Some(Duration::from_secs(30)));
        assert_eq!(parse_interval(Some("half an hour")), Some(DEFAULT_INTERVAL));
    }
}
