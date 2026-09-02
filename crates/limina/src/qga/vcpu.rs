// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The stock tier's half of dynamic vCPU offlining: planning `guest-set-vcpus` calls.
//!
//! [`crate::vcpu_policy`] decides *how many* vCPUs should be online; this decides *which* ones,
//! for a guest we reach through the stock `qemu-guest-agent` rather than through `limina-agent`.
//! It is the same job [`limina_proto::plan_cpu_transitions`] does for the enhanced tier, but from
//! better information: `guest-get-vcpus` reports `can-offline` per processor, so the guest kernel
//! tells us which CPUs are candidates instead of us inferring it from "never cpu0".
//!
//! Why this rung exists at all: the roadmap deferred `guest-get/set-vcpus` and `guest-get-load`
//! "until they have a consumer" (`docs/roadmap.md`, M12.5 §What is left). The policy is that
//! consumer, and routing it through QGA is what lets a **stock** guest — no limina-agent, no
//! custom kernel, nothing installed — take part in dynamic vCPU offlining. That is the two-tier
//! guarantee working the way it is supposed to: the enhanced agent makes the feature *better*
//! (a richer signal, pushed rather than polled), never a precondition for having it at all.

use super::client::Vcpu;

/// Which `(logical_id, online)` changes take the guest from its current state to `target` CPUs
/// online, in the order to apply them. Empty when nothing needs to change.
///
/// Rules, mirroring the enhanced-tier planner so the two tiers cannot drift apart in behaviour:
/// - Only processors the guest marked `can_offline` are ever taken away. On Linux cpu0 reports
///   `can-offline: false`, so this subsumes the "never cpu0" rule rather than restating it — and
///   it also respects a guest that has pinned some other CPU for its own reasons.
/// - Offline the **highest**-numbered first and re-online the **lowest**-numbered first, so the
///   online set stays a low, dense prefix instead of drifting into holes.
/// - An empty reading yields no plan: not knowing the current state is never a reason to write.
pub fn plan(vcpus: &[Vcpu], target: u32) -> Vec<(u32, bool)> {
    if vcpus.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&Vcpu> = vcpus.iter().collect();
    sorted.sort_by_key(|v| v.logical_id);
    let online_now = sorted.iter().filter(|v| v.online).count();
    let target = target.max(1) as usize;
    let mut plan = Vec::new();
    if online_now > target {
        for v in sorted.iter().rev() {
            if online_now - plan.len() <= target {
                break;
            }
            if v.online && v.can_offline {
                plan.push((v.logical_id, false));
            }
        }
    } else if online_now < target {
        for v in sorted.iter() {
            if online_now + plan.len() >= target {
                break;
            }
            if !v.online {
                plan.push((v.logical_id, true));
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpus(n: u32, online: u32) -> Vec<Vcpu> {
        (0..n)
            .map(|id| Vcpu {
                logical_id: id,
                online: id < online,
                // Linux: cpu0 cannot be offlined, every other CPU can.
                can_offline: id != 0,
            })
            .collect()
    }

    #[test]
    fn shrinking_takes_the_highest_offlinable_cpus() {
        assert_eq!(
            plan(&cpus(10, 10), 4),
            vec![
                (9, false),
                (8, false),
                (7, false),
                (6, false),
                (5, false),
                (4, false)
            ]
        );
    }

    /// The guest's own `can_offline` is authoritative — a CPU it will not give up is simply not a
    /// candidate, and the plan stops short rather than asking and being refused.
    #[test]
    fn a_cpu_the_guest_pinned_is_never_asked_for() {
        let mut v = cpus(4, 4);
        v[3].can_offline = false;
        assert_eq!(
            plan(&v, 2),
            vec![(2, false), (1, false)],
            "cpu3 is pinned, so the plan must reach past it rather than ask"
        );
        // And cpu0 is never a candidate at all, however low the target.
        assert_eq!(plan(&cpus(2, 2), 0), vec![(1, false)]);
        let only_cpu0 = vec![Vcpu {
            logical_id: 0,
            online: true,
            can_offline: false,
        }];
        assert!(plan(&only_cpu0, 1).is_empty());
    }

    #[test]
    fn growing_refills_the_lowest_offline_cpus() {
        assert_eq!(plan(&cpus(10, 2), 4), vec![(2, true), (3, true)]);
        // Capped by what the guest actually has.
        assert_eq!(plan(&cpus(2, 1), 8), vec![(1, true)]);
    }

    #[test]
    fn a_plan_that_changes_nothing_is_empty() {
        assert!(plan(&cpus(10, 4), 4).is_empty());
        assert!(plan(&[], 2).is_empty(), "no reading, no write");
    }

    /// The guest is free to report its processors in any order; the plan must not depend on it.
    #[test]
    fn the_reported_order_does_not_matter() {
        let mut shuffled = cpus(4, 4);
        shuffled.reverse();
        assert_eq!(plan(&shuffled, 2), vec![(3, false), (2, false)]);
    }
}
