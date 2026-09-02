//! Dynamic vCPU offlining **policy** (supervisor side) — the CPU sibling of the M6 autoballoon.
//!
//! Consumes guest [`CpuPressure`] reports from the control plane and answers with a [`CpuTarget`]:
//! how many vCPUs the host wants online. The *mechanism* is already shipped (libkrun models PSCI
//! `CPU_OFF`/`AFFINITY_INFO` and parks the vCPU thread; `tests/l2_vcpu_hotplug.rs` guards it) and
//! the sysfs write is the guest's — offlining a CPU is something only Linux can do to itself, so
//! this side never does more than ask.
//!
//! **Why it earns its keep.** A vCPU that is online but only lightly busy still costs the host: it
//! takes per-vCPU timer exits at roughly the guest's tick rate, each a full VM exit. Measured on a
//! 10-vCPU guest running eight threads waking at 1 kHz, with the *same* work (iteration counts
//! within 0.06%):
//!
//! | online | worker vCPU CPU-sec / 60 s | arch_timer/s | IPI1/s |
//! |--------|---------------------------|--------------|--------|
//! | 10     | 26.61, 27.06              | ~14,400      | 90–110 |
//! | 4      | 21.77                     | ~10,100      | ~1,180 |
//!
//! −19% of the worker's vCPU CPU time, about 8.5% of a core, for identical work. Note the
//! mechanism: **timer exits, not IPIs** — offlining *raised* IPI1 tenfold (function-call IPIs now
//! reach fewer CPUs but the sender still pays) while total CPU fell. That is why the policy gates
//! on how many tasks are runnable, not on IPI or wakeup counts.
//!
//! Two regimes it deliberately does NOT chase:
//! - A **truly idle** guest already costs ~1.8% of a core at 10 vCPUs under NO_HZ. There is
//!   nothing to reclaim there, so nobody should expect a saving from an untouched VM.
//! - A **saturated** guest needs every vCPU, and the grow path hands them all back at once.
//!
//! The win lives in between: the online-but-lightly-busy desktop, the VM sitting between builds.
//!
//! **Asymmetry is the whole safety story.** Shrinking is slow, one CPU at a time, behind a dwell
//! and a cooldown; growing is immediate and jumps straight to the maximum. Getting a vCPU back
//! late is a stall the user feels; giving one up late costs a few percent of a core nobody sees.
//! When in doubt this policy is wrong in the cheap direction.

use limina_proto::{CpuPressure, CpuTarget};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How hard the policy reclaims idle guest vCPUs. Mirrors
/// [`crate::balloon_policy::ReclaimMode`] in shape and in configuration (`--cpu-reclaim`,
/// `vm.toml [hardware] cpu_reclaim`); the modes differ only in the floor they squeeze toward,
/// because the dwell/cooldown that keep the thing from oscillating should not be a user knob.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CpuReclaim {
    /// Never offline a vCPU. The guest keeps every vCPU it booted with — today's behavior, and
    /// the default: a VM that was never told to do this must behave exactly as it did before.
    #[default]
    Disabled,
    /// Give back at most half the vCPUs, so a burst always finds a wide machine already there.
    Light,
    /// Squeeze toward two online vCPUs.
    Moderate,
    /// Squeeze toward a single online vCPU. cpu0 can never go offline, so this is the real floor
    /// of the mechanism.
    Aggressive,
}

impl CpuReclaim {
    /// The fewest vCPUs this mode will leave online, given the VM's boot (maximum) count.
    ///
    /// Never returns 0 and never exceeds `max`: a floor above the maximum would ask for a
    /// permanent grow, and a floor of 0 would ask the guest to offline cpu0, which it refuses.
    pub fn floor(self, max: u32) -> u32 {
        let want = match self {
            CpuReclaim::Disabled => max,
            CpuReclaim::Light => max.div_ceil(2),
            CpuReclaim::Moderate => 2,
            CpuReclaim::Aggressive => 1,
        };
        want.clamp(1, max.max(1))
    }
}

/// The shrink condition must hold continuously for this long before the first CPU goes, and
/// again between steps. Long enough that a lull between two compiler invocations does not start
/// dismantling the machine.
const SHRINK_DWELL: Duration = Duration::from_secs(20);

/// One decision's inputs and outcome, for the debug trace.
struct State {
    /// When the shrink condition started holding continuously. Cleared by any sample that
    /// fails it and by every issued target, so a grow re-arms the full dwell.
    calm_since: Option<Instant>,
}

/// The supervisor-side policy object. Held by the control plane, which feeds it every guest
/// report and forwards whatever it returns.
pub struct VcpuPolicy {
    /// The VM's boot vCPU count: the maximum, and what a grow always jumps to.
    max: u32,
    /// The fewest vCPUs to leave online (derived from the reclaim mode at construction).
    floor: u32,
    state: Mutex<State>,
}

impl VcpuPolicy {
    /// Build a policy for a VM that booted `max` vCPUs. Returns `None` for
    /// [`CpuReclaim::Disabled`] and for any VM whose floor cannot be below its maximum — a
    /// 1-vCPU VM, or a mode whose floor rounds up to the whole machine. Those have nothing to
    /// give back, and a policy that can never act should not exist (the control plane keys the
    /// `vcpu` capability off `Some`, so the guest is never even asked to report).
    pub fn new(max: u8, mode: CpuReclaim) -> Option<VcpuPolicy> {
        let max = u32::from(max);
        if mode == CpuReclaim::Disabled || max < 2 {
            return None;
        }
        let floor = mode.floor(max);
        if floor >= max {
            return None;
        }
        log::info!("dynamic vCPUs: {floor}..{max} online (reclaim {mode:?})");
        Some(VcpuPolicy {
            max,
            floor,
            state: Mutex::new(State { calm_since: None }),
        })
    }

    /// The target that undoes all shrinking. Used by the suspend bracket: task #41 leaves the
    /// guest-visible online state out of the M9 snapshot, so a snapshot taken while a vCPU is
    /// offline restores it as online and the two sides' bookkeeping diverges. Rather than change
    /// the snapshot format, we make sure a snapshot never contains an offline vCPU.
    pub fn max_target(&self) -> CpuTarget {
        CpuTarget { online: self.max }
    }

    /// Feed one guest report; returns the target to send, or `None` to leave the guest alone.
    pub fn on_pressure(&self, p: &CpuPressure) -> Option<CpuTarget> {
        self.decide(p, Instant::now())
    }

    /// The pure decision, with the clock injected.
    ///
    /// Everything is derived from the guest's *reported* online count rather than from what we
    /// last asked for, so the policy has no belief to get out of date: a failed sysfs write, a
    /// guest that onlined a CPU itself, or a restore that diverged the two all correct themselves
    /// on the next sample.
    pub fn decide(&self, p: &CpuPressure, now: Instant) -> Option<CpuTarget> {
        let mut st = self.state.lock().unwrap();
        // A guest that reported no online count told us nothing (unreadable sysfs, an agent
        // predating the field). Never act on an absent reading.
        if p.online == 0 {
            st.calm_since = None;
            return None;
        }
        let online = p.online.clamp(1, self.max);

        // GROW — immediate, coarse, no dwell and no cooldown. More runnable tasks than CPUs, or
        // a 1-minute load that already fills the machine, means the guest is short *now*; hand
        // back everything rather than climbing one step per dwell while it waits.
        if p.nr_running > online || p.loadavg1_x100 >= online * 100 {
            st.calm_since = None;
            if online < self.max {
                log::info!(
                    "dynamic vCPUs: {online} online but {} runnable (load1 {:.2}) → asking for {}",
                    p.nr_running,
                    f64::from(p.loadavg1_x100) / 100.0,
                    self.max
                );
                return Some(CpuTarget { online: self.max });
            }
            return None;
        }

        // SHRINK — one step, and only after the condition has held for the whole dwell.
        if online <= self.floor {
            st.calm_since = None;
            return None;
        }
        let smaller = online - 1;
        // Demand must fit in the smaller set on BOTH the instantaneous signal and the smoothed
        // one, and the smoothed bound is the strict one: the 1-minute load has to fit with a
        // whole CPU to spare. That asymmetry is deliberate. `nr_running` is a single sample of a
        // spiky quantity, so holding it to `<= smaller` is as much as it can honestly say
        // (it counts the agent doing the reading, so an idle guest reports 1); loadavg is the
        // one that remembers, so it carries the headroom requirement. It also makes the last
        // step self-guarding: going down to a single CPU needs a 1-minute load of 0.00, which
        // only a guest that has been doing nothing for a minute can show.
        let fits = p.nr_running <= smaller && p.loadavg1_x100 <= smaller.saturating_sub(1) * 100;
        if !fits {
            st.calm_since = None;
            return None;
        }
        let since = *st.calm_since.get_or_insert(now);
        if now.duration_since(since) < SHRINK_DWELL {
            return None;
        }
        // Re-arm the dwell so the next step costs another full one.
        st.calm_since = None;
        log::info!(
            "dynamic vCPUs: {online} online, {} runnable (load1 {:.2}) for {SHRINK_DWELL:?} → \
             asking for {smaller}",
            p.nr_running,
            f64::from(p.loadavg1_x100) / 100.0,
        );
        Some(CpuTarget { online: smaller })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle(online: u32) -> CpuPressure {
        CpuPressure {
            nr_running: 1,
            loadavg1_x100: 0,
            loadavg5_x100: 0,
            some_avg10: 0,
            some_avg60: 0,
            online,
            present: 10,
        }
    }

    fn busy(online: u32, nr_running: u32) -> CpuPressure {
        CpuPressure {
            nr_running,
            loadavg1_x100: nr_running * 100,
            ..idle(online)
        }
    }

    /// The dwell is the anti-thrash guarantee: no sample, however calm, may shrink on its own.
    #[test]
    fn one_calm_sample_is_never_enough_to_shrink() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let t0 = Instant::now();
        assert_eq!(p.decide(&idle(10), t0), None);
        assert_eq!(p.decide(&idle(10), t0 + Duration::from_secs(19)), None);
    }

    #[test]
    fn a_sustained_lull_gives_back_exactly_one_cpu_per_dwell() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let t0 = Instant::now();
        assert_eq!(p.decide(&idle(10), t0), None);
        assert_eq!(
            p.decide(&idle(10), t0 + SHRINK_DWELL),
            Some(CpuTarget { online: 9 })
        );
        // The guest complies; the next step costs another full dwell, not one more sample.
        let t1 = t0 + SHRINK_DWELL;
        assert_eq!(p.decide(&idle(9), t1), None);
        assert_eq!(
            p.decide(&idle(9), t1 + SHRINK_DWELL),
            Some(CpuTarget { online: 8 })
        );
    }

    /// Shrinking stops at the mode's floor and never asks for cpu0's head.
    #[test]
    fn the_floor_is_never_crossed() {
        for (mode, floor) in [
            (CpuReclaim::Light, 5u32),
            (CpuReclaim::Moderate, 2),
            (CpuReclaim::Aggressive, 1),
        ] {
            let p = VcpuPolicy::new(10, mode).unwrap();
            let mut now = Instant::now();
            let mut online = 10;
            // Run far more dwells than there are CPUs to give away.
            for _ in 0..40 {
                if let Some(t) = p.decide(&idle(online), now) {
                    assert!(t.online >= floor, "{mode:?} asked for {} CPUs", t.online);
                    online = t.online;
                }
                now += SHRINK_DWELL;
            }
            assert_eq!(online, floor, "{mode:?} settled at the wrong floor");
        }
    }

    /// A load spike gets the WHOLE machine back on the very next sample — no dwell, no
    /// one-step-per-tick climb. Being slow here is a stall the user feels.
    #[test]
    fn a_spike_restores_every_cpu_immediately() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let now = Instant::now();
        assert_eq!(
            p.decide(&busy(2, 8), now),
            Some(CpuTarget { online: 10 }),
            "a burst on a shrunk guest must jump straight to max"
        );
    }

    /// The dwell restarts from zero after a spike: a lull that follows a burst must be proven
    /// again before anything is taken away.
    #[test]
    fn a_spike_rearms_the_full_dwell() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let t0 = Instant::now();
        assert_eq!(p.decide(&idle(10), t0), None);
        // 19s of calm, then one busy sample, then calm again.
        assert_eq!(p.decide(&busy(10, 10), t0 + Duration::from_secs(19)), None);
        // The dwell restarts at the first calm sample AFTER the spike, not at t0. Had it
        // survived the spike, this would shrink here.
        let rearmed = t0 + SHRINK_DWELL;
        assert_eq!(p.decide(&idle(10), rearmed), None);
        assert_eq!(
            p.decide(&idle(10), rearmed + SHRINK_DWELL - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            p.decide(&idle(10), rearmed + SHRINK_DWELL),
            Some(CpuTarget { online: 9 })
        );
    }

    /// Load alone — no runnable tasks at the instant we looked — is enough to grow. `nr_running`
    /// is a single sample of a bursty quantity; loadavg is the one that remembers.
    #[test]
    fn a_full_loadavg_grows_even_with_nothing_runnable_right_now() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let sample = CpuPressure {
            nr_running: 0,
            loadavg1_x100: 210,
            ..idle(2)
        };
        assert_eq!(
            p.decide(&sample, Instant::now()),
            Some(CpuTarget { online: 10 })
        );
    }

    /// The policy believes the guest, not itself. A guest that came back from a restore with
    /// more CPUs online than we last asked for is simply the new truth to reason from — no
    /// stored target to disagree with it.
    #[test]
    fn the_guests_reported_count_is_the_only_state_that_matters() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let t0 = Instant::now();
        // Shrink to 9...
        p.decide(&idle(10), t0);
        assert_eq!(
            p.decide(&idle(10), t0 + SHRINK_DWELL),
            Some(CpuTarget { online: 9 })
        );
        // ...then the guest reports 10 again (restore, or a manual online). One more dwell of
        // calm and we simply shrink from 10 again, rather than insisting on the stale 9.
        let t1 = t0 + SHRINK_DWELL;
        assert_eq!(p.decide(&idle(10), t1), None);
        assert_eq!(
            p.decide(&idle(10), t1 + SHRINK_DWELL),
            Some(CpuTarget { online: 9 })
        );
    }

    /// An absent reading (`online == 0`) is not a real zero — acting on it would ask a guest
    /// with an unreadable sysfs to offline every CPU it has.
    #[test]
    fn a_missing_online_count_is_never_acted_on() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let t0 = Instant::now();
        assert_eq!(p.decide(&idle(0), t0), None);
        assert_eq!(p.decide(&idle(0), t0 + SHRINK_DWELL * 10), None);
    }

    /// Nothing to give back means no policy at all, so the `vcpu` capability is never offered
    /// and the guest never spends a byte reporting.
    #[test]
    fn a_policy_that_could_never_act_is_not_created() {
        assert!(VcpuPolicy::new(10, CpuReclaim::Disabled).is_none());
        assert!(VcpuPolicy::new(1, CpuReclaim::Aggressive).is_none());
        // Light on a 2-vCPU VM floors at 1, which is below max — that one CAN act.
        assert!(VcpuPolicy::new(2, CpuReclaim::Light).is_some());
        // ...but Moderate on the same VM floors at 2, which is the whole machine.
        assert!(VcpuPolicy::new(2, CpuReclaim::Moderate).is_none());
    }

    /// The suspend bracket's escape hatch (see [`VcpuPolicy::max_target`]).
    #[test]
    fn max_target_asks_for_the_whole_machine() {
        let p = VcpuPolicy::new(10, CpuReclaim::Aggressive).unwrap();
        assert_eq!(p.max_target(), CpuTarget { online: 10 });
    }
}
