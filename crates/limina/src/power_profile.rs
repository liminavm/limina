// Copyright 2026 The limina Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The guest's power profile, and what the host does about it.
//!
//! A desktop power profile normally resolves to hardware — a lower frequency, a shallower turbo
//! budget, a fan curve. Our guest has none of that: no cpuidle driver, no real DVFS, and a
//! "frequency" the virtual cpufreq device invents and no host code acts on. Every joule is spent
//! on the host, so the profile is **user intent we relay**, and the backing is host policy.
//!
//! The guest end needs nothing from us. On a stock F44 guest `tuned-ppd` owns
//! `net.hadess.PowerProfiles` and offers all three profiles; `limina-agent` only reports which one
//! is active. See `docs/design/power-profiles.md`.
//!
//! This module is the mapping, kept pure and separate from the plumbing so the policy can be
//! tested without a VM.

// The plumbing that consumes this — the wire message, the agent's D-Bus watcher, and the
// application of each field — lands separately. `expect` rather than `allow` so that the first
// consumer makes this attribute itself an error, and it cannot outlive its reason.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the profile plumbing lands with the control-plane change"
    )
)]

use crate::vcpu_policy::CpuReclaim;

/// The three profiles `org.freedesktop.UPower.PowerProfiles` defines. Ordered from cheapest to
/// most eager, which is the order the D-Bus `Profiles` array uses.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum PowerProfile {
    /// The user asked to save power.
    PowerSaver,
    /// The default, and what a guest that never touches the toggle reports.
    #[default]
    Balanced,
    /// The user asked for responsiveness and does not want to be packed onto slow CPUs.
    Performance,
}

impl PowerProfile {
    /// Parse the D-Bus `ActiveProfile` string. Unknown values fall back to [`Balanced`] rather
    /// than erroring: the property is owned by the guest's daemon, whose vocabulary may grow, and
    /// an unrecognised profile must not leave the host holding a stale policy.
    ///
    /// [`Balanced`]: PowerProfile::Balanced
    pub fn parse(s: &str) -> PowerProfile {
        match s {
            "power-saver" => PowerProfile::PowerSaver,
            "performance" => PowerProfile::Performance,
            _ => PowerProfile::Balanced,
        }
    }

    /// The name this profile has on D-Bus.
    pub fn as_str(self) -> &'static str {
        match self {
            PowerProfile::PowerSaver => "power-saver",
            PowerProfile::Balanced => "balanced",
            PowerProfile::Performance => "performance",
        }
    }

    /// The wire encoding. Explicit rather than `as u8` so reordering the enum cannot silently
    /// change the protocol.
    pub fn to_wire(self) -> u8 {
        match self {
            PowerProfile::PowerSaver => 0,
            PowerProfile::Balanced => 1,
            PowerProfile::Performance => 2,
        }
    }

    /// Decode the wire encoding, defaulting to [`Balanced`] for anything unrecognised — a guest
    /// running a newer agent must degrade to the default, not desynchronise the host.
    ///
    /// [`Balanced`]: PowerProfile::Balanced
    pub fn from_wire(v: u8) -> PowerProfile {
        match v {
            0 => PowerProfile::PowerSaver,
            2 => PowerProfile::Performance,
            _ => PowerProfile::Balanced,
        }
    }
}

/// How the little vCPUs' host threads are backed.
///
/// The *number* of little vCPUs is fixed at boot — it comes from `capacity-dmips-mhz` in the
/// device tree, and `cpu_capacity` is `DEVICE_ATTR_RO` — so a profile can never add or remove
/// them. All it can change is whether they are actually slow.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LittleBacking {
    /// `QOS_CLASS_BACKGROUND`: confined to the efficiency cores, and throttled besides. Measured
    /// 3.75x slower than a big vCPU, which is what the advertised capacity describes.
    Confined,
    /// `QOS_CLASS_UTILITY`: no longer E-core-confined, and measured indistinguishable from a big
    /// vCPU.
    ///
    /// The guest still believes these CPUs are slow and will under-load them accordingly — misfit
    /// migration pulls big tasks off them and the load balancer weights them at their advertised
    /// capacity. That unused headroom is the accepted price of `performance`: the profile is
    /// latency-led, and the alternative (rebuilding the topology) is not available at runtime.
    Promoted,
}

/// Everything the host changes in response to a profile. One struct so the whole response is
/// visible in one place and testable as a unit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PowerPolicy {
    /// How the little vCPUs are backed on the host.
    pub little: LittleBacking,
    /// Whether vCPU threads may take the real-time scheduling band.
    ///
    /// Off under `power-saver`: the band exists to make an idle guest's frame clock punctual, and
    /// a reservation is exactly what a user asking to save power is not asking for.
    pub rt_band: bool,
    /// How eagerly idle vCPUs are offlined.
    pub cpu_reclaim: CpuReclaim,
    /// Whether the balloon gives memory back to the host more eagerly than its default.
    pub eager_giveback: bool,
}

impl PowerProfile {
    /// The host's response to this profile.
    ///
    /// `Balanced` is exactly today's defaults, so a guest whose user never touches the toggle
    /// behaves as it always has — which is what makes it safe to relay the profile at all.
    pub fn policy(self) -> PowerPolicy {
        match self {
            PowerProfile::PowerSaver => PowerPolicy {
                little: LittleBacking::Confined,
                rt_band: false,
                cpu_reclaim: CpuReclaim::Moderate,
                eager_giveback: true,
            },
            PowerProfile::Balanced => PowerPolicy {
                little: LittleBacking::Confined,
                rt_band: true,
                cpu_reclaim: CpuReclaim::Disabled,
                eager_giveback: false,
            },
            PowerProfile::Performance => PowerPolicy {
                little: LittleBacking::Promoted,
                rt_band: true,
                cpu_reclaim: CpuReclaim::Disabled,
                eager_giveback: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole safety argument for relaying the profile at all: a guest that never touches the
    /// toggle reports `balanced`, and `balanced` must leave the VM exactly as it was.
    #[test]
    fn balanced_is_todays_defaults() {
        let p = PowerProfile::Balanced.policy();
        assert_eq!(p.cpu_reclaim, CpuReclaim::Disabled);
        assert!(p.rt_band);
        assert!(!p.eager_giveback);
    }

    /// A profile arriving from the guest is a string owned by someone else's daemon. An
    /// unrecognised one must land on the default, never leave the host on a stale policy.
    #[test]
    fn an_unknown_profile_is_balanced() {
        for s in ["", "quiet", "cool", "POWER-SAVER", "low-power", "🔋"] {
            assert_eq!(PowerProfile::parse(s), PowerProfile::Balanced, "{s:?}");
        }
        assert_eq!(PowerProfile::parse("power-saver"), PowerProfile::PowerSaver);
        assert_eq!(
            PowerProfile::parse("performance"),
            PowerProfile::Performance
        );
    }

    /// The wire encoding is explicit, so reordering the enum cannot silently change it, and an
    /// unknown value from a newer agent degrades rather than desynchronising.
    #[test]
    fn the_wire_encoding_round_trips_and_tolerates_the_unknown() {
        for p in [
            PowerProfile::PowerSaver,
            PowerProfile::Balanced,
            PowerProfile::Performance,
        ] {
            assert_eq!(PowerProfile::from_wire(p.to_wire()), p);
            assert_eq!(PowerProfile::parse(p.as_str()), p);
        }
        for v in [3, 4, 200, 255] {
            assert_eq!(PowerProfile::from_wire(v), PowerProfile::Balanced);
        }
    }

    /// Only `performance` promotes the little vCPUs. The number of them cannot change at
    /// runtime — capacity comes from the device tree and `cpu_capacity` is read-only — so this
    /// is the only lever a profile has over them.
    #[test]
    fn only_performance_promotes_the_little_vcpus() {
        assert_eq!(
            PowerProfile::Performance.policy().little,
            LittleBacking::Promoted
        );
        for p in [PowerProfile::PowerSaver, PowerProfile::Balanced] {
            assert_eq!(p.policy().little, LittleBacking::Confined, "{p:?}");
        }
    }

    /// Power-saver is the only profile that gives anything up: it is also the only one that
    /// drops the real-time band and reclaims vCPUs.
    #[test]
    fn power_saver_is_the_only_profile_that_concedes() {
        let ps = PowerProfile::PowerSaver.policy();
        assert!(!ps.rt_band);
        assert!(ps.eager_giveback);
        assert_ne!(ps.cpu_reclaim, CpuReclaim::Disabled);

        for p in [PowerProfile::Balanced, PowerProfile::Performance] {
            let q = p.policy();
            assert!(q.rt_band, "{p:?}");
            assert!(!q.eager_giveback, "{p:?}");
            assert_eq!(q.cpu_reclaim, CpuReclaim::Disabled, "{p:?}");
        }
    }
}
