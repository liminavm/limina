// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Driving the guest into a state where stopping it is safe — the one implementation.
//!
//! Two callers need this and had grown their own copies: the M9 snapshot bracket
//! (`krun/mod.rs`, 20 s budget, aborts on failure) and the host-sleep bracket
//! ([`crate::power`], 10 s budget, released the ack anyway). Same pulse, same oracle, same
//! poll interval, different budgets and opposite failure policies. The budgets and the
//! policy are genuinely per-caller; the sequence is not.
//!
//! **Two oracles, in order, and the second is the load-bearing one.** Device status
//! reaching `INIT` ([`Vmm::is_quiesced`]) happens in `dpm_suspend()`. The guest then still
//! owes `dpm_suspend_late`/`_noirq` and the `s2idle_enter` rendezvous, in which *every*
//! vCPU must be scheduled to reach `tick_freeze()`, before `timekeeping_suspend()` runs.
//! Stop the guest before that and the elapsed time is accounted as *running* time, landing
//! in `CLOCK_MONOTONIC` where nothing can reclaim it — sleeptime injection moves only
//! REALTIME and BOOTTIME, by construction. Measured on an idle host that leg is 0.28 ms;
//! it is unbounded when vCPU threads are not promptly scheduled, which is exactly the
//! condition at host sleep. [`Vmm::all_vcpus_parked`] is the signal that it finished.
//!
//! See `spikes/s2idle-monotonic/` for the measurements and the failure it causes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmm::Vmm;

/// How far the guest got. The caller decides what each outcome is worth to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quiesced {
    /// Devices at `INIT` *and* every vCPU parked: the guest is past `timekeeping_suspend`,
    /// so time spent stopped from here is classified as suspend.
    Parked,
    /// Devices at `INIT`, but a vCPU was still executing when the budget ran out. Stopping
    /// the guest now poisons `CLOCK_MONOTONIC`.
    DevicesOnly,
    /// Devices never reached `INIT` — the guest did not act on the suspend request.
    No,
}

/// Per-caller knobs. The sequence they drive is fixed.
pub struct QuiesceRequest {
    /// Pulse `KEY_SLEEP`. The snapshot bracket suppresses this when the caller already
    /// arranged the guest-side suspend: a second request lands after userspace freezes and
    /// replays on resume, re-suspending the guest unwakeably (the run-11 trap).
    pub pulse_button: bool,
    /// How long to wait for every virtio device to reach `INIT`.
    pub device_budget: Duration,
    /// How long to then wait for every vCPU to park.
    pub park_budget: Duration,
    /// How long "every vCPU parked" must hold continuously before we believe it — one
    /// sample can catch the rendezvous mid-flight, with the last vCPU briefly in a WFx wait
    /// on its way to somewhere else.
    pub park_settle: Duration,
}

const POLL: Duration = Duration::from_millis(50);

/// Pulse (optionally), wait for devices, then wait for the vCPUs to park.
///
/// Does not stop the guest: the caller decides whether to [`Vmm::pause`] and, crucially,
/// which resume flavour the outcome earns.
pub fn quiesce_guest(vmm: &Arc<Mutex<Vmm>>, req: &QuiesceRequest) -> Quiesced {
    if req.pulse_button {
        crate::suspend::pulse();
    }

    if !wait_until(req.device_budget, || vmm.lock().unwrap().is_quiesced()) {
        let holdouts = vmm.lock().unwrap().quiesce_holdouts();
        log::warn!(
            "quiesce: no device quiesce within {:?} (holdouts: {holdouts:?})",
            req.device_budget
        );
        return Quiesced::No;
    }

    // Settle: require the predicate to hold continuously, not just once.
    let deadline = Instant::now() + req.park_budget;
    let mut seen = 0u32;
    loop {
        if vmm.lock().unwrap().all_vcpus_parked() {
            let settled_at = Instant::now();
            let held = loop {
                if !vmm.lock().unwrap().all_vcpus_parked() {
                    break false;
                }
                if settled_at.elapsed() >= req.park_settle {
                    break true;
                }
                std::thread::sleep(POLL);
            };
            if held {
                log::info!("quiesce: devices at INIT and every vCPU parked");
                return Quiesced::Parked;
            }
        }
        if Instant::now() >= deadline {
            let holdouts = vmm.lock().unwrap().park_holdouts();
            log::warn!(
                "quiesce: devices reached INIT but vCPUs {holdouts:?} were still executing \
                 after {:?} (saw all-parked {seen} times) — stopping the guest now would \
                 land the stopped time in its CLOCK_MONOTONIC",
                req.park_budget
            );
            return Quiesced::DevicesOnly;
        }
        seen += u32::from(vmm.lock().unwrap().all_vcpus_parked());
        std::thread::sleep(POLL);
    }
}

fn wait_until(budget: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}
