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
use std::sync::atomic::{AtomicU32, Ordering};
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
const DEFAULT_SHRINK_DWELL: Duration = Duration::from_secs(20);

/// The dwell in force, honouring `LIMINA_VCPU_DWELL_SECS`. The override exists for the L2 test,
/// which would otherwise spend minutes per shrink step waiting out a production dwell (and the
/// guest's boot loadavg on top of it); it is read once per policy, not per decision.
fn shrink_dwell() -> Duration {
    std::env::var("LIMINA_VCPU_DWELL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SHRINK_DWELL)
}

/// How much CPU the worker process has burned in total, and when we looked.
///
/// This is the **tier-independent** sensor: the host can see how hard its own vCPU threads are
/// working without asking the guest anything at all, so it responds on the sampling interval
/// rather than on whatever the guest is willing and able to report. It is what closes the stock
/// tier's grow latency — `guest-get-load` is a 1-minute average, so a burst took ~45s to show up
/// there, while this sees it on the next tick.
#[derive(Debug, Clone, Copy)]
struct HostCpu {
    at: Instant,
    cpu: Duration,
}

/// Total CPU (user + system) the process has consumed, via `proc_pid_rusage`.
///
/// Works on another process we own with no privileges and no entitlement — the same call
/// `limina-test` already uses to read the worker's `phys_footprint`. `None` when the process is
/// gone or the call fails, which the policy treats as "no host signal" rather than as idle.
fn process_cpu_time(pid: libc::pid_t) -> Option<Duration> {
    // SAFETY: zeroed POD; proc_pid_rusage fills it. The buffer arg is `rusage_info_t` (a void*
    // alias), so the concrete struct is cast to it, exactly as elsewhere in the tree.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            &mut info as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(Duration::from_nanos(
        info.ri_user_time.saturating_add(info.ri_system_time),
    ))
}

/// The shortest interval between two host CPU readings that yields a usable rate. Shorter than
/// this and the quantisation of the counters dominates.
const HOST_CPU_MIN_INTERVAL: Duration = Duration::from_millis(900);

/// How close to saturating its online vCPUs the guest must be before the host signal alone calls
/// for a grow, in hundredths of a core. A quarter of a core of slack left.
const GROW_WITHIN_X100: u32 = 25;

/// One decision's inputs and outcome, for the debug trace.
struct State {
    /// When the shrink condition started holding continuously. Cleared by any sample that
    /// fails it and by every issued target, so a grow re-arms the full dwell.
    calm_since: Option<Instant>,
    /// The previous host CPU reading, for the rate between ticks.
    last_host: Option<HostCpu>,
}

/// The supervisor-side policy object. Held by the control plane, which feeds it every guest
/// report and forwards whatever it returns.
pub struct VcpuPolicy {
    /// The VM's boot vCPU count: the maximum, and what a grow always jumps to.
    max: u32,
    /// The fewest vCPUs to leave online, derived from the current reclaim mode. Settable at
    /// runtime ([`VcpuPolicy::set_reclaim`]) because the guest's power profile chooses it:
    /// `power-saver` tightens the floor, and leaving it is what restores the whole machine.
    floor: AtomicU32,
    /// How long the shrink condition must hold before each step ([`shrink_dwell`]).
    dwell: Duration,
    /// The `limina-vmm` worker's pid, once it has been spawned. Set by the supervisor via
    /// [`VcpuPolicy::watch_worker`]; `None` before that (and in unit tests), which simply means
    /// the host signal is unavailable and the guest's own report decides alone.
    worker_pid: Mutex<Option<libc::pid_t>>,
    state: Mutex<State>,
}

impl VcpuPolicy {
    /// Build a policy for a VM that booted `max` vCPUs. `None` only for a 1-vCPU VM, which has
    /// nothing to give back at all — cpu0 can never go offline.
    ///
    /// A policy is created even for a mode that cannot currently act, including
    /// [`CpuReclaim::Disabled`] (whose floor is `max`, so the shrink guard never passes). That
    /// is deliberate: the control plane keys the `vcpu` capability off `Some`, the capability is
    /// negotiated once at WELCOME, and the guest's power profile can raise or lower the floor
    /// later. A VM that started with reclaim off must still be *able* to reclaim when its user
    /// selects `power-saver`, and it can only do that if the guest was asked to report from the
    /// start.
    pub fn new(max: u8, mode: CpuReclaim) -> Option<VcpuPolicy> {
        let max = u32::from(max);
        if max < 2 {
            return None;
        }
        let floor = mode.floor(max);
        let dwell = shrink_dwell();
        log::info!("dynamic vCPUs: {floor}..{max} online (reclaim {mode:?}, dwell {dwell:?})");
        Some(VcpuPolicy {
            max,
            floor: AtomicU32::new(floor),
            dwell,
            worker_pid: Mutex::new(None),
            state: Mutex::new(State {
                calm_since: None,
                last_host: None,
            }),
        })
    }

    /// The fewest vCPUs this policy will currently leave online.
    pub fn floor(&self) -> u32 {
        self.floor.load(Ordering::Relaxed)
    }

    /// Change the reclaim mode at runtime, as the guest's power profile selects one.
    ///
    /// Returns `true` when the floor *rose*, which is the caller's cue to grow now rather than
    /// wait: a VM shrunk under `power-saver` and then switched back to `balanced` must get its
    /// vCPUs back immediately, not on the next pressure report.
    pub fn set_reclaim(&self, mode: CpuReclaim) -> bool {
        let want = mode.floor(self.max);
        let previous = self.floor.swap(want, Ordering::Relaxed);
        if want != previous {
            log::info!(
                "dynamic vCPUs: reclaim {mode:?}, floor {previous} -> {want} (max {})",
                self.max
            );
        }
        want > previous
    }

    /// Start reading the worker's CPU time as a second, tier-independent signal.
    ///
    /// Called once the worker exists (the supervisor knows the pid; the control plane is built
    /// before the spawn). Until this lands the policy simply has no host signal and decides on the
    /// guest's report alone — which is also what unit tests do.
    pub fn watch_worker(&self, pid: libc::pid_t) {
        *self.worker_pid.lock().unwrap() = Some(pid);
        log::debug!("dynamic vCPUs: watching worker {pid} CPU time as a host-side load signal");
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
        let now = Instant::now();
        let host = self.sample_host(now);
        self.decide_with(p, host, now)
    }

    /// Read the worker's CPU time and turn two readings into a rate: hundredths of a core burned
    /// since the last sample. `None` until there are two readings far enough apart to divide.
    fn sample_host(&self, now: Instant) -> Option<u32> {
        let pid = (*self.worker_pid.lock().unwrap())?;
        let cpu = process_cpu_time(pid)?;
        let mut st = self.state.lock().unwrap();
        let prev = st.last_host;
        // A counter that went backwards means a different process wears the pid now; start over.
        if prev.is_none_or(|p| cpu < p.cpu) {
            st.last_host = Some(HostCpu { at: now, cpu });
            return None;
        }
        let prev = prev?;
        let wall = now.duration_since(prev.at);
        if wall < HOST_CPU_MIN_INTERVAL {
            return None;
        }
        st.last_host = Some(HostCpu { at: now, cpu });
        let busy = cpu.saturating_sub(prev.cpu);
        Some((busy.as_secs_f64() / wall.as_secs_f64() * 100.0).round() as u32)
    }

    /// The pure decision, with the clock injected.
    ///
    /// Everything is derived from the guest's *reported* online count rather than from what we
    /// last asked for, so the policy has no belief to get out of date: a failed sysfs write, a
    /// guest that onlined a CPU itself, or a restore that diverged the two all correct themselves
    /// on the next sample.
    /// The guest-only decision, for tests and for callers with no host reading.
    #[cfg(test)]
    pub fn decide(&self, p: &CpuPressure, now: Instant) -> Option<CpuTarget> {
        self.decide_with(p, None, now)
    }

    /// The decision. `host_busy_x100` is hundredths of a core the worker burned since the last
    /// sample, or `None` when there is no host reading yet.
    ///
    /// The host signal is used **asymmetrically**, like everything else here. It can trigger a
    /// grow on its own, because it is the fast one — it sees a burst on the next tick where the
    /// stock tier's loadavg needs ~45s. It can only *veto* a shrink, never cause one, because it
    /// measures the whole worker process rather than the vCPU threads alone: device threads
    /// (GPU, IO) are counted in, so a busy reading may not be the guest wanting CPU at all. That
    /// asymmetry makes the imprecision harmless — it can keep vCPUs online that were not needed,
    /// which costs a little CPU, and it can never take away vCPUs that were.
    fn decide_with(
        &self,
        p: &CpuPressure,
        host_busy_x100: Option<u32>,
        now: Instant,
    ) -> Option<CpuTarget> {
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
        // The host term: the worker is burning nearly as much CPU as the guest has vCPUs, so the
        // guest is saturating what it has whatever its own report says (or has not said yet).
        let host_saturated =
            host_busy_x100.is_some_and(|busy| busy + GROW_WITHIN_X100 >= online * 100);
        if p.nr_running > online || p.loadavg1_x100 >= online * 100 || host_saturated {
            st.calm_since = None;
            if online < self.max {
                log::info!(
                    "dynamic vCPUs: {online} online but {} runnable (load1 {:.2}, host {:.2} \
                     cores) → asking for {}",
                    p.nr_running,
                    f64::from(p.loadavg1_x100) / 100.0,
                    host_busy_x100.map_or(f64::NAN, |b| f64::from(b) / 100.0),
                    self.max
                );
                return Some(CpuTarget { online: self.max });
            }
            return None;
        }

        // SHRINK — one step, and only after the condition has held for the whole dwell.
        if online <= self.floor() {
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
        // The host veto: never shrink while the worker is burning more CPU than the smaller set
        // would have. Only ever blocks a shrink (see `decide_with`).
        let host_allows = host_busy_x100.is_none_or(|busy| busy + 100 <= smaller * 100);
        if !fits || !host_allows {
            st.calm_since = None;
            return None;
        }
        let since = *st.calm_since.get_or_insert(now);
        if now.duration_since(since) < self.dwell {
            return None;
        }
        // Re-arm the dwell so the next step costs another full one.
        st.calm_since = None;
        log::info!(
            "dynamic vCPUs: {online} online, {} runnable (load1 {:.2}) for {:?} → \
             asking for {smaller}",
            p.nr_running,
            f64::from(p.loadavg1_x100) / 100.0,
            self.dwell,
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
            p.decide(&idle(10), t0 + DEFAULT_SHRINK_DWELL),
            Some(CpuTarget { online: 9 })
        );
        // The guest complies; the next step costs another full dwell, not one more sample.
        let t1 = t0 + DEFAULT_SHRINK_DWELL;
        assert_eq!(p.decide(&idle(9), t1), None);
        assert_eq!(
            p.decide(&idle(9), t1 + DEFAULT_SHRINK_DWELL),
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
                now += DEFAULT_SHRINK_DWELL;
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
        let rearmed = t0 + DEFAULT_SHRINK_DWELL;
        assert_eq!(p.decide(&idle(10), rearmed), None);
        assert_eq!(
            p.decide(
                &idle(10),
                rearmed + DEFAULT_SHRINK_DWELL - Duration::from_millis(1)
            ),
            None
        );
        assert_eq!(
            p.decide(&idle(10), rearmed + DEFAULT_SHRINK_DWELL),
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
            p.decide(&idle(10), t0 + DEFAULT_SHRINK_DWELL),
            Some(CpuTarget { online: 9 })
        );
        // ...then the guest reports 10 again (restore, or a manual online). One more dwell of
        // calm and we simply shrink from 10 again, rather than insisting on the stale 9.
        let t1 = t0 + DEFAULT_SHRINK_DWELL;
        assert_eq!(p.decide(&idle(10), t1), None);
        assert_eq!(
            p.decide(&idle(10), t1 + DEFAULT_SHRINK_DWELL),
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
        assert_eq!(p.decide(&idle(0), t0 + DEFAULT_SHRINK_DWELL * 10), None);
    }

    /// Only a 1-vCPU VM has nothing to give back. Every other VM gets a policy even when its
    /// mode cannot currently act, because the `vcpu` capability is negotiated once at WELCOME
    /// and the guest's power profile may ask for reclaim later.
    #[test]
    fn every_vm_that_could_ever_act_gets_a_policy() {
        assert!(VcpuPolicy::new(1, CpuReclaim::Aggressive).is_none());
        assert!(VcpuPolicy::new(10, CpuReclaim::Disabled).is_some());
        assert!(VcpuPolicy::new(2, CpuReclaim::Moderate).is_some());
    }

    /// A disabled policy exists but never acts: its floor is the whole machine, so the shrink
    /// guard never passes however long the guest stays idle.
    #[test]
    fn a_disabled_policy_never_shrinks() {
        let p = VcpuPolicy::new(10, CpuReclaim::Disabled).expect("policy");
        assert_eq!(p.floor(), 10);
        let t0 = Instant::now();
        for step in 0..40 {
            let at = t0 + DEFAULT_SHRINK_DWELL * step;
            assert_eq!(p.decide(&idle(10), at), None, "shrank at step {step}");
        }
    }

    /// The power profile moves the floor, and says when the caller must grow at once: a VM
    /// shrunk under power-saver and switched back must not wait for the next report.
    #[test]
    fn set_reclaim_moves_the_floor_and_reports_a_rise() {
        let p = VcpuPolicy::new(10, CpuReclaim::Disabled).expect("policy");
        assert_eq!(p.floor(), 10);

        // Tightening never asks the caller to grow.
        assert!(!p.set_reclaim(CpuReclaim::Moderate));
        assert_eq!(p.floor(), 2);
        assert!(!p.set_reclaim(CpuReclaim::Aggressive));
        assert_eq!(p.floor(), 1);

        // Loosening does, and returning to Disabled restores the whole machine.
        assert!(p.set_reclaim(CpuReclaim::Light));
        assert_eq!(p.floor(), 5);
        assert!(p.set_reclaim(CpuReclaim::Disabled));
        assert_eq!(p.floor(), 10);

        // Setting the same mode twice is not a rise, so it cannot cause a spurious grow.
        assert!(!p.set_reclaim(CpuReclaim::Disabled));
    }

    /// The host signal exists to be FAST: it grows on the reading alone, without waiting for the
    /// guest to notice. This is what closes the stock tier's ~45s grow latency, where loadavg —
    /// a 1-minute average — is the only thing the guest can offer.
    #[test]
    fn a_saturated_worker_grows_before_the_guest_has_noticed() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        // The guest still reports itself calm: loadavg has not caught up yet.
        let quiet = idle(2);
        assert_eq!(
            p.decide_with(&quiet, None, Instant::now()),
            None,
            "with no host reading this sample says nothing"
        );
        // 1.8 cores burned against 2 online: saturated.
        assert_eq!(
            p.decide_with(&quiet, Some(180), Instant::now()),
            Some(CpuTarget { online: 10 })
        );
    }

    /// ...but it may only ever VETO a shrink, never cause one, because it measures the whole
    /// worker process — device threads included — so a busy reading is not proof the guest wants
    /// CPU. Wrong in the cheap direction: it can keep vCPUs that were not needed, never take away
    /// vCPUs that were.
    #[test]
    fn a_busy_worker_blocks_a_shrink_the_guest_report_would_allow() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let t0 = Instant::now();
        // The guest looks idle, but the worker is burning 9.5 cores (say, heavy GPU work) — more
        // than the 9 a shrink would leave, so there is no headroom to give away.
        assert_eq!(p.decide_with(&idle(10), Some(950), t0), None);
        assert_eq!(
            p.decide_with(&idle(10), Some(950), t0 + DEFAULT_SHRINK_DWELL),
            None,
            "a busy worker must veto the shrink however long the guest has looked calm"
        );
        // The host quietens; only now does the dwell start, and it must run in full.
        let t1 = t0 + DEFAULT_SHRINK_DWELL;
        assert_eq!(p.decide_with(&idle(10), Some(10), t1), None);
        assert_eq!(
            p.decide_with(&idle(10), Some(10), t1 + DEFAULT_SHRINK_DWELL),
            Some(CpuTarget { online: 9 })
        );
    }

    /// The veto is about HEADROOM, not about the worker being busy at all. A worker burning 8
    /// cores on a 10-vCPU guest still leaves room to drop to 9, so that shrink proceeds — being
    /// stricter would mean a GPU-heavy VM could never give a single vCPU back.
    #[test]
    fn the_host_veto_only_bites_when_the_smaller_set_would_not_fit() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let t0 = Instant::now();
        assert_eq!(p.decide_with(&idle(10), Some(800), t0), None);
        assert_eq!(
            p.decide_with(&idle(10), Some(800), t0 + DEFAULT_SHRINK_DWELL),
            Some(CpuTarget { online: 9 }),
            "8 cores of work fits in 9 CPUs with room to spare"
        );
    }

    /// No host reading must leave behaviour exactly as it was — a VM whose worker we cannot read
    /// is not a VM that should start behaving differently.
    #[test]
    fn absent_host_readings_change_nothing() {
        let p = VcpuPolicy::new(10, CpuReclaim::Moderate).unwrap();
        let t0 = Instant::now();
        assert_eq!(p.decide_with(&idle(10), None, t0), None);
        assert_eq!(
            p.decide_with(&idle(10), None, t0 + DEFAULT_SHRINK_DWELL),
            Some(CpuTarget { online: 9 })
        );
    }

    /// The suspend bracket's escape hatch (see [`VcpuPolicy::max_target`]).
    #[test]
    fn max_target_asks_for_the_whole_machine() {
        let p = VcpuPolicy::new(10, CpuReclaim::Aggressive).unwrap();
        assert_eq!(p.max_target(), CpuTarget { online: 10 });
    }
}
