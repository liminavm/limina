// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Host-sleep integration: s2idle the guest around host sleep (M9 follow-on).
//!
//! Design: `docs/design/host-sleep-s2idle.md` §4. When the HOST goes to sleep, a guest
//! left "running" keeps a RUNNING CNTVCT (measured: `spikes/s2idle-monotonic/`), and the
//! elapsed time lands in `CLOCK_MONOTONIC` — where nothing can ever reclaim it, because
//! sleeptime injection moves only REALTIME and BOOTTIME by construction. On a guest with
//! systemd service watchdogs (Debian arms 3 min on journald/udevd/logind; Fedora arms
//! none) that kills logind, which orphans the DRM and input leases and takes the seated
//! session with it. s2idle'ing the guest first puts the stop INSIDE the window the kernel
//! classifies as suspend, so the counter delta is injected as sleep instead.
//!
//! The injection comes from the arch counter, NOT the PL031: `timekeeping_resume()`
//! prefers a suspend-nonstop clocksource, and libkrun's timer node declares no
//! `arm,no-tick-in-suspend`, so the RTC rung is shadowed.
//!
//! Mechanism, all existing seams:
//! - `kIOMessageSystemWillSleep` (IOKit `IORegisterForSystemPower`, which HOLDS the sleep
//!   until we ack): if the guest is awake, pulse the sleep button
//!   ([`crate::suspend::pulse`]) and poll [`Vmm::is_quiesced`] up to [`QUIESCE_WAIT`],
//!   then `IOAllowPowerChange`. A guest that won't quiesce just gets today's
//!   frozen-counter behavior — fail-open, never worse.
//! - `kIOMessageSystemHasPoweredOn`: pulse the wake key ([`crate::wake::pulse`]) — but
//!   ONLY if we put the guest to sleep (or our pulse landed late and it slept while the
//!   host slept). Never wake a guest the user suspended, and never touch the *sleep*
//!   button here: a sleep-button pulse at an already-asleep guest is LATCHED and
//!   re-suspends it unwakeably on wake (the run-11 trap).
//!
//! The decision logic lives in [`HostSleepState`], a pure state machine (unit-tested
//! below); this module's IOKit half can only be validated by a real host sleep.

use std::sync::{Arc, Mutex};

use vmm::Vmm;

/// How long `willSleep` holds the host's sleep ack while the guest quiesces. The system
/// allows ~30 s; a stock guest s2idles in 1–4 s. On expiry we release the ack anyway.
const QUIESCE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
const QUIESCE_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// What `willSleep` should do with the guest.
#[derive(Debug, PartialEq, Eq)]
enum SleepAction {
    /// Guest is already in s2idle (user-suspended or a previous pulse) — leave it be,
    /// and remember it was NOT ours to wake.
    LeaveAsleep,
    /// Guest is running — pulse the sleep button and wait for quiesce.
    PulseAndWait,
}

/// What `didWake` should do with the guest.
#[derive(Debug, PartialEq, Eq)]
enum WakeAction {
    /// We slept it (or our pulse landed late and it is asleep now) — wake it.
    WakeGuest,
    /// Not ours to wake (user-suspended guest, or it never went to sleep).
    LeaveAlone,
}

/// The wake-ownership state machine: the one invariant is that we only ever wake a guest
/// whose sleep WE requested. `pulsed` survives a failed quiesce wait on purpose — if our
/// pulse lands late and the guest suspends during host sleep, `did_wake` still recovers
/// it (quiesced-now + pulsed = ours).
#[derive(Default)]
struct HostSleepState {
    pulsed: bool,
}

impl HostSleepState {
    fn on_will_sleep(&mut self, guest_quiesced: bool) -> SleepAction {
        if guest_quiesced {
            self.pulsed = false;
            SleepAction::LeaveAsleep
        } else {
            self.pulsed = true;
            SleepAction::PulseAndWait
        }
    }

    fn on_did_wake(&mut self, guest_quiesced_now: bool) -> WakeAction {
        let pulsed = std::mem::take(&mut self.pulsed);
        if pulsed && guest_quiesced_now {
            WakeAction::WakeGuest
        } else {
            WakeAction::LeaveAlone
        }
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::c_void;

    pub type IoConnect = u32; // mach_port_t
    pub type IoObject = u32;
    pub type IoService = u32;
    pub type IoNotificationPortRef = *mut c_void;
    pub type CfRunLoopSourceRef = *mut c_void;
    pub type CfRunLoopRef = *mut c_void;
    pub type CfStringRef = *const c_void;

    // iokit_common_msg(...) values from IOKit/IOMessage.h.
    pub const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
    pub const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
    pub const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;

    pub type IoServiceInterestCallback = extern "C" fn(
        refcon: *mut c_void,
        service: IoService,
        message_type: u32,
        message_argument: *mut c_void,
    );

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        pub fn IORegisterForSystemPower(
            refcon: *mut c_void,
            the_port_ref: *mut IoNotificationPortRef,
            callback: IoServiceInterestCallback,
            notifier: *mut IoObject,
        ) -> IoConnect;
        pub fn IOAllowPowerChange(kernel_port: IoConnect, notification_id: isize) -> i32;
        pub fn IONotificationPortGetRunLoopSource(
            notify: IoNotificationPortRef,
        ) -> CfRunLoopSourceRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub static kCFRunLoopDefaultMode: CfStringRef;
        pub fn CFRunLoopGetCurrent() -> CfRunLoopRef;
        pub fn CFRunLoopAddSource(rl: CfRunLoopRef, source: CfRunLoopSourceRef, mode: CfStringRef);
        pub fn CFRunLoopRun();
    }
}

#[cfg(target_os = "macos")]
struct PowerCtx {
    vmm: Arc<Mutex<Vmm>>,
    state: HostSleepState,
    /// The root power domain port, filled in right after registration (before the run
    /// loop starts, so before any callback can fire).
    root_port: ffi::IoConnect,
}

#[cfg(target_os = "macos")]
impl PowerCtx {
    fn guest_quiesced(&self) -> bool {
        self.vmm.lock().unwrap().is_quiesced()
    }

    fn handle(&mut self, message_type: u32, message_argument: *mut std::ffi::c_void) {
        match message_type {
            // Idle-sleep query: never veto (a veto would keep the user's Mac awake).
            ffi::K_IO_MESSAGE_CAN_SYSTEM_SLEEP => {
                unsafe { ffi::IOAllowPowerChange(self.root_port, message_argument as isize) };
            }
            ffi::K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
                match self.state.on_will_sleep(self.guest_quiesced()) {
                    SleepAction::LeaveAsleep => {
                        log::info!("host sleep: guest is already suspended; leaving it be");
                    }
                    SleepAction::PulseAndWait => {
                        hold_ack_until_safe_to_stop(&self.vmm);
                    }
                }
                unsafe { ffi::IOAllowPowerChange(self.root_port, message_argument as isize) };
            }
            ffi::K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => {
                match self.state.on_did_wake(self.guest_quiesced()) {
                    WakeAction::WakeGuest => {
                        log::info!("host wake: waking the guest (KEY_WAKEUP)");
                        crate::wake::pulse();
                    }
                    WakeAction::LeaveAlone => {
                        log::info!(
                            "host wake: guest not ours to wake (user-suspended or never slept)"
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

/// The `willSleep` release decision: pulse the guest's sleep button and hold the host's
/// sleep ack until it is safe for the host to stop our vCPUs. Returns whether the guest
/// reached that state before [`QUIESCE_WAIT`].
///
/// "Safe" means the guest is past `timekeeping_suspend`, so that whatever time the host
/// spends asleep is classified as suspend rather than absorbed by `CLOCK_MONOTONIC`.
/// [`Vmm::is_quiesced`] does NOT establish that: it observes virtio device status, which
/// the guest clears in its `.suspend` callbacks during `dpm_suspend()`, while timekeeping
/// freezes later — after `dpm_suspend_late`/`_noirq` and after the `s2idle_enter`
/// rendezvous in which every vCPU must be scheduled to reach `tick_freeze()`. Measured on
/// an idle host that leg is 0.28 ms; it is unbounded when vCPU threads are not promptly
/// scheduled, which is the condition at host sleep. See `spikes/s2idle-monotonic/`.
#[cfg(target_os = "macos")]
fn hold_ack_until_safe_to_stop(vmm: &Arc<Mutex<Vmm>>) -> bool {
    log::info!(
        "host sleep: pulsing the guest sleep button and holding the sleep ack for \
         quiesce (<={QUIESCE_WAIT:?})"
    );
    crate::suspend::pulse();
    let deadline = std::time::Instant::now() + QUIESCE_WAIT;
    let quiesced = loop {
        if vmm.lock().unwrap().is_quiesced() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(QUIESCE_POLL);
    };
    if quiesced {
        log::info!("host sleep: guest quiesced; releasing the sleep ack");
    } else {
        log::warn!(
            "host sleep: guest did not quiesce within {QUIESCE_WAIT:?}; releasing the \
             sleep ack anyway — the host will stop the vCPUs mid-suspend and the guest \
             will absorb the sleep into CLOCK_MONOTONIC"
        );
    }
    quiesced
}

/// Test seam for the release-point decision (`LIMINA_HOST_SLEEP_SEAM=1`, `SIGURG`).
///
/// IOKit's half cannot run in CI — sleeping the host kills the session driving the test —
/// so this stands in for macOS: run the real [`hold_ack_until_safe_to_stop`], then stop
/// this process at exactly the moment we release the ack. Stopping *at* the release point
/// rather than some microseconds later is deliberate: it is the worst case macOS is
/// entitled to, and it makes the race deterministic, because the rendezvous the guest
/// still owes cannot complete while its vCPUs are frozen. The driving test sends
/// `SIGCONT` after the gap it wants to simulate, and this thread then pulses the wake key
/// exactly as `didWake` would.
///
/// `SIGSTOP` (not [`Vmm::pause`]) is what models a host sleep: `Vmm::pause`/`resume` hide
/// the elapsed time by advancing the vtimer offset, which is exactly what macOS does NOT
/// do to us.
#[cfg(target_os = "macos")]
pub fn install_test_seam(vmm: Arc<Mutex<Vmm>>) {
    if std::env::var("LIMINA_HOST_SLEEP_SEAM").as_deref() != Ok("1") {
        return;
    }
    static FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    extern "C" fn handle_sigurg(_sig: libc::c_int) {
        FIRED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigurg as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGURG, &sa, std::ptr::null_mut()) != 0 {
            log::warn!("host sleep seam: installing the SIGURG handler failed");
            return;
        }
    }
    std::thread::Builder::new()
        .name("host-sleep-seam".into())
        .spawn(move || {
            loop {
                if FIRED.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    log::warn!("host sleep seam: simulating willSleep");
                    hold_ack_until_safe_to_stop(&vmm);
                    log::warn!("host sleep seam: ack released — stopping the worker");
                    unsafe { libc::raise(libc::SIGSTOP) };
                    // Let the guest finish the suspend it was stopped partway through
                    // before the wake key lands, or the pulse arrives at a guest that is
                    // not yet in s2idle and is simply ignored.
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    log::warn!("host sleep seam: continued — pulsing the guest wake key");
                    crate::wake::pulse();
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
        .expect("spawning the host-sleep seam thread");
}

#[cfg(not(target_os = "macos"))]
pub fn install_test_seam(_vmm: Arc<Mutex<Vmm>>) {}

#[cfg(target_os = "macos")]
extern "C" fn power_callback(
    refcon: *mut std::ffi::c_void,
    _service: ffi::IoService,
    message_type: u32,
    message_argument: *mut std::ffi::c_void,
) {
    // Safety: refcon is the Box<PowerCtx> leaked by `start`, alive for the process.
    let ctx = unsafe { &mut *(refcon as *mut PowerCtx) };
    ctx.handle(message_type, message_argument);
}

/// Register for host sleep/wake notifications and run their delivery loop on a dedicated
/// thread, for the life of the worker. Call once, only when `on_host_sleep = s2idle`.
#[cfg(target_os = "macos")]
pub fn start(vmm: Arc<Mutex<Vmm>>) {
    std::thread::Builder::new()
        .name("host-sleep".into())
        .spawn(move || {
            let ctx = Box::leak(Box::new(PowerCtx {
                vmm,
                state: HostSleepState::default(),
                root_port: 0,
            }));
            let mut port: ffi::IoNotificationPortRef = std::ptr::null_mut();
            let mut notifier: ffi::IoObject = 0;
            let root_port = unsafe {
                ffi::IORegisterForSystemPower(
                    ctx as *mut PowerCtx as *mut std::ffi::c_void,
                    &mut port,
                    power_callback,
                    &mut notifier,
                )
            };
            if root_port == 0 {
                log::warn!(
                    "host sleep: IORegisterForSystemPower failed; guest will not be \
                     suspended around host sleep"
                );
                return;
            }
            // Callbacks fire on THIS thread's run loop, which hasn't started yet — the
            // port is set before any delivery.
            ctx.root_port = root_port;
            unsafe {
                ffi::CFRunLoopAddSource(
                    ffi::CFRunLoopGetCurrent(),
                    ffi::IONotificationPortGetRunLoopSource(port),
                    ffi::kCFRunLoopDefaultMode,
                );
            }
            log::info!("host sleep: registered (guest will s2idle around host sleep)");
            unsafe { ffi::CFRunLoopRun() };
            log::warn!("host sleep: notification run loop exited");
        })
        .expect("spawning the host-sleep thread");
}

#[cfg(not(target_os = "macos"))]
pub fn start(_vmm: Arc<Mutex<Vmm>>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_guest_is_slept_and_woken() {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(false), SleepAction::PulseAndWait);
        assert_eq!(s.on_did_wake(true), WakeAction::WakeGuest);
    }

    #[test]
    fn user_suspended_guest_is_left_alone() {
        let mut s = HostSleepState::default();
        // Asleep before the host sleeps → not ours; must NOT be woken on host wake.
        assert_eq!(s.on_will_sleep(true), SleepAction::LeaveAsleep);
        assert_eq!(s.on_did_wake(true), WakeAction::LeaveAlone);
    }

    #[test]
    fn guest_that_ignored_the_pulse_is_not_woken() {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(false), SleepAction::PulseAndWait);
        // Never quiesced (e.g. a session swallowed the sleep key) — nothing to wake.
        assert_eq!(s.on_did_wake(false), WakeAction::LeaveAlone);
    }

    #[test]
    fn late_suspend_is_recovered_on_wake() {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(false), SleepAction::PulseAndWait);
        // The quiesce wait timed out, the host slept anyway, and our pulse landed late —
        // the guest is asleep at wake time and it was OUR pulse: wake it.
        assert_eq!(s.on_did_wake(true), WakeAction::WakeGuest);
    }

    #[test]
    fn wake_ownership_does_not_leak_across_cycles() {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(false), SleepAction::PulseAndWait);
        assert_eq!(s.on_did_wake(true), WakeAction::WakeGuest);
        // Next cycle: the user suspended the guest themselves.
        assert_eq!(s.on_will_sleep(true), SleepAction::LeaveAsleep);
        assert_eq!(s.on_did_wake(true), WakeAction::LeaveAlone);
    }
}
