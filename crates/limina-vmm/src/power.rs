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
//!   ([`crate::suspend::pulse`]) and drive [`crate::quiesce`] until every vCPU is parked,
//!   pause the vCPUs ourselves, then `IOAllowPowerChange`. A guest that will not get there
//!   is paused anyway and resumed with the interval hidden: a wrong wall clock (which the
//!   clock correctors fix) instead of a poisoned `CLOCK_MONOTONIC` (which nothing fixes).
//! - `kIOMessageSystemHasPoweredOn`: unpause, then wake the guest ([`crate::wake::guest`]) —
//!   but ONLY if we put it to sleep. Never wake a guest the user suspended, and never touch
//!   the *sleep* button here: a sleep-button pulse at an already-asleep guest is LATCHED and
//!   re-suspends it unwakeably on wake (the run-11 trap).
//! - A guest slower than the `willSleep` budget is paused wherever it got to and, once
//!   unpaused, carries on with the suspend we asked for — finishing it AFTER `didWake`. So a
//!   wake of our own suspend is not one decision at the instant of unpausing: a post-wake
//!   watch follows the guest (on libkrun's [`vmm::Vmm::power_watch`]) until the suspend
//!   completes (wake it), aborts (leave it), or, if it never started, a grace runs out.
//!
//! Where the guest is in a suspend is read off the devices we emulate ([`GuestSleep`]):
//! its drivers hand each virtio device back (reset it to `INIT`) on the way in, and the
//! last vCPU parks in PSCI `SYSTEM_SUSPEND` at the end.
//!
//! The decision logic lives in [`HostSleepState`], a pure state machine (unit-tested
//! below); this module's IOKit half can only be validated by a real host sleep.

use std::sync::{Arc, Mutex};

use vmm::Vmm;

use crate::quiesce::{QuiesceRequest, Quiesced};

/// How long `willSleep` holds the host's sleep ack. macOS allows ~30 s; we spend it in two
/// parts — waiting for the guest's devices to quiesce, then for its vCPUs to park — and
/// keep a margin so we always ack before the system stops asking.
const DEVICE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
const PARK_WAIT: std::time::Duration = std::time::Duration::from_secs(8);
/// "Every vCPU parked" must hold this long: one sample can catch the `s2idle_enter`
/// rendezvous mid-flight, with the last vCPU briefly in a WFx wait on its way elsewhere.
const PARK_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// How long after `didWake` a guest that has not yet started our suspend may still start it
/// and have it count as ours. It covers a pulse that landed but whose suspend was still in
/// userspace (the logind job, `systemd-sleep` hooks) when the host slept, so no device shows
/// it yet. A suspend that has visibly started needs no grace: it can only complete or abort.
const LATE_SUSPEND_GRACE: std::time::Duration = std::time::Duration::from_secs(60);
/// The post-wake watch re-reads the guest at least this often. Device and `SYSTEM_SUSPEND`
/// transitions wake it at once; this bounds only the one it cannot hear, s2idle vCPUs parking.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(250);
/// When a suspend under way has not finished after this long, say so once: a guest stuck
/// mid-suspend is worth a line in the log, even though the watch keeps waiting for it.
const SUSPENDING_TOO_LONG: std::time::Duration = std::time::Duration::from_secs(60);

/// Where the guest is in a suspend, read off the devices we emulate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestSleep {
    /// No suspend under way: devices are driver-held and none has been handed back, or the
    /// guest is taking them back (resuming).
    Awake,
    /// A suspend under way: the guest is handing devices back and has not finished — some
    /// still held, or all handed back with a vCPU still running.
    Suspending,
    /// Suspended: parked in PSCI `SYSTEM_SUSPEND`, or s2idle with every device handed back
    /// and every vCPU parked.
    Asleep,
}

/// What the VMM reports about the guest, for [`classify`].
#[derive(Debug, Clone, Copy)]
struct GuestReport {
    system_suspended: bool,
    /// Some virtio device is still driver-owned ([`vmm::Vmm::quiesce_holdouts`]).
    devices_held: bool,
    /// Some virtio device has been reset by its driver ([`vmm::Vmm::released_devices`]).
    devices_released: bool,
    all_vcpus_parked: bool,
    /// Which way the guest last moved a device ([`vmm::Vmm::last_driver_transition`]).
    last_transition: Option<vmm::DriverTransition>,
}

/// Where the guest is in a suspend. Released and held devices together are ambiguous — a
/// guest half-way into a suspend and one half-way out of it (drivers re-taking devices one by
/// one) look alike — and the direction of the newest device change settles it.
fn classify(r: GuestReport) -> GuestSleep {
    use vmm::DriverTransition::{Released, Taken};
    if r.system_suspended {
        return GuestSleep::Asleep;
    }
    if r.devices_held && !r.devices_released {
        return GuestSleep::Awake;
    }
    if !r.devices_held && r.all_vcpus_parked {
        return GuestSleep::Asleep;
    }
    match r.last_transition {
        Some(Taken) => GuestSleep::Awake,
        // Released, or no driver has touched a device yet (early boot): not a guest to pulse.
        Some(Released) | None => GuestSleep::Suspending,
    }
}

/// What `willSleep` should do with the guest.
#[derive(Debug, PartialEq, Eq)]
enum SleepAction {
    /// The guest is already suspending or asleep — pulsing it again is the latched-button
    /// trap. Whether that suspend is ours stays whatever it already was.
    DontPulse,
    /// Guest is running — pulse the sleep button and wait for quiesce.
    PulseAndWait,
}

/// What `didWake` should do with the guest.
#[derive(Debug, PartialEq, Eq)]
enum WakeAction {
    /// We slept it and it is asleep — wake it.
    WakeGuest,
    /// Our suspend is not finished yet: watch the guest (watch `id`) until it is.
    Watch(u64),
    /// Not ours to wake (user-suspended guest, or never asked to sleep).
    LeaveAlone,
}

/// What the post-wake watch should do next.
#[derive(Debug, PartialEq, Eq)]
enum WatchAction {
    /// Keep watching.
    Keep,
    /// Our suspend completed — wake the guest, and stop.
    WakeGuest,
    /// Stop without waking: the suspend aborted, never started within the grace, or a new
    /// host sleep took over.
    Stop,
}

/// An armed post-wake watch.
#[derive(Clone)]
struct Watch {
    id: u64,
    /// Has the guest been seen part-way into the suspend? Once it has, coming back to
    /// `Awake` means the suspend aborted.
    seen_suspending: bool,
}

/// The wake-ownership state machine: the one invariant is that we only ever wake a guest
/// whose sleep WE requested. `ours` is set by our pulse and cleared only when that suspend
/// is resolved — woken, aborted, or given up on — so it survives a failed quiesce wait, a
/// wake that finds the guest still on its way down, and a host sleep in between.
#[derive(Default, Clone)]
struct HostSleepState {
    ours: bool,
    watch: Option<Watch>,
    next_watch_id: u64,
}

impl HostSleepState {
    fn on_will_sleep(&mut self, guest: GuestSleep) -> SleepAction {
        // The willSleep bracket takes over from any post-wake watch.
        self.watch = None;
        match guest {
            // A guest that looks awake while a pulse of ours is still unresolved has that
            // suspend pending in userspace (or swallowed, which the host cannot tell apart). A
            // second pulse is the latched-button trap: it replays after the first suspend and
            // re-suspends the guest with nobody left to wake it. The host sleeps with the guest
            // paused instead, and the suspend stays ours (found by `every_sequence`).
            GuestSleep::Awake if self.ours => SleepAction::DontPulse,
            GuestSleep::Awake => {
                self.ours = true;
                SleepAction::PulseAndWait
            }
            GuestSleep::Suspending | GuestSleep::Asleep => SleepAction::DontPulse,
        }
    }

    fn on_did_wake(&mut self, guest: GuestSleep) -> WakeAction {
        if !self.ours {
            return WakeAction::LeaveAlone;
        }
        if guest == GuestSleep::Asleep {
            self.ours = false;
            return WakeAction::WakeGuest;
        }
        let id = self.next_watch_id;
        self.next_watch_id += 1;
        self.watch = Some(Watch {
            id,
            seen_suspending: guest == GuestSleep::Suspending,
        });
        WakeAction::Watch(id)
    }

    /// One look at the guest by watch `id`. `grace_over`: [`LATE_SUSPEND_GRACE`] has passed
    /// since the watch was armed.
    fn on_watch(&mut self, id: u64, guest: GuestSleep, grace_over: bool) -> WatchAction {
        let Some(watch) = self.watch.as_mut().filter(|w| w.id == id) else {
            return WatchAction::Stop;
        };
        let action = match guest {
            GuestSleep::Asleep => WatchAction::WakeGuest,
            GuestSleep::Suspending => {
                watch.seen_suspending = true;
                WatchAction::Keep
            }
            GuestSleep::Awake if watch.seen_suspending || grace_over => WatchAction::Stop,
            GuestSleep::Awake => WatchAction::Keep,
        };
        if action != WatchAction::Keep {
            self.watch = None;
            self.ours = false;
        }
        action
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
    unsafe extern "C" {
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
    unsafe extern "C" {
        pub static kCFRunLoopDefaultMode: CfStringRef;
        pub fn CFRunLoopGetCurrent() -> CfRunLoopRef;
        pub fn CFRunLoopAddSource(rl: CfRunLoopRef, source: CfRunLoopSourceRef, mode: CfStringRef);
        pub fn CFRunLoopRun();
    }
}

/// The host-sleep bracket: the ownership state machine plus what the last `willSleep`
/// achieved, shared by the IOKit notification thread and the L2 test seam so that both drive
/// exactly the same sleep and wake decisions.
#[cfg(target_os = "macos")]
struct Bracket {
    vmm: Arc<Mutex<Vmm>>,
    inner: Mutex<BracketInner>,
}

#[cfg(target_os = "macos")]
struct BracketInner {
    state: HostSleepState,
    /// How far the guest got quiescing on the last `willSleep` — it decides which resume
    /// flavour `didWake` owes it.
    quiesced: Quiesced,
}

#[cfg(target_os = "macos")]
impl Bracket {
    fn new(vmm: Arc<Mutex<Vmm>>) -> Arc<Self> {
        Arc::new(Self {
            vmm,
            inner: Mutex::new(BracketInner {
                state: HostSleepState::default(),
                quiesced: Quiesced::No,
            }),
        })
    }

    /// Where the guest is in a suspend, right now.
    fn guest_sleep(&self) -> GuestSleep {
        let vmm = self.vmm.lock().unwrap();
        classify(GuestReport {
            system_suspended: vmm.system_suspended(),
            devices_held: !vmm.quiesce_holdouts().is_empty(),
            devices_released: !vmm.released_devices().is_empty(),
            all_vcpus_parked: vmm.all_vcpus_parked(),
            last_transition: vmm.last_driver_transition(),
        })
    }

    /// `willSleep`, up to (not including) releasing the host's sleep ack.
    fn before_host_sleep(&self) {
        let mut inner = self.inner.lock().unwrap();
        // A guest already suspending or asleep still needs its vCPUs confirmed parked and
        // paused — it is only the sleep-button pulse it must not get again (a latched pulse
        // re-suspends it unwakeably on wake).
        let guest = self.guest_sleep();
        let pulse = match inner.state.on_will_sleep(guest) {
            SleepAction::DontPulse if guest == GuestSleep::Awake => {
                log::info!(
                    "host sleep: our last sleep request has not shown in the guest yet; not \
                     pulsing it again"
                );
                false
            }
            SleepAction::DontPulse => {
                log::info!("host sleep: guest is already {guest:?}; not pulsing");
                false
            }
            SleepAction::PulseAndWait => true,
        };
        inner.quiesced = hold_ack_until_safe_to_stop(&self.vmm, pulse);
    }

    /// `didWake`: unpause, then wake the guest if its sleep was ours — now, or once the
    /// suspend it is still finishing completes.
    fn after_host_wake(self: &Arc<Self>) {
        let mut inner = self.inner.lock().unwrap();
        // Unpause first: a paused guest cannot answer a wake key.
        resume_after_host_sleep(&self.vmm, inner.quiesced);
        let guest = self.guest_sleep();
        match inner.state.on_did_wake(guest) {
            WakeAction::WakeGuest => {
                log::info!("host wake: waking the guest");
                crate::wake::guest(&self.vmm);
            }
            WakeAction::Watch(id) => {
                log::info!(
                    "host wake: our suspend is not finished (guest {guest:?}); watching for it"
                );
                let bracket = self.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("host-wake-watch".into())
                    .spawn(move || bracket.watch_late_suspend(id))
                {
                    log::warn!("host wake: spawning the post-wake watch failed ({e}); waking now");
                    // Resolve the watch as a wake, so ownership does not outlive it.
                    inner.state.on_watch(id, GuestSleep::Asleep, true);
                    crate::wake::guest(&self.vmm);
                }
            }
            WakeAction::LeaveAlone => {
                log::info!("host wake: guest not ours to wake (user-suspended or never slept)");
            }
        }
    }

    /// Follow a guest that is finishing our suspend after the host woke, and wake it when it
    /// gets there. Runs on its own thread: the IOKit run loop must stay free for the next
    /// `willSleep`, which cancels this watch.
    fn watch_late_suspend(&self, id: u64) {
        let watch = self.vmm.lock().unwrap().power_watch();
        let armed = std::time::Instant::now();
        let mut warned = false;
        // s2idle has no discrete event: "every vCPU parked" must hold for PARK_SETTLE, as in
        // the willSleep bracket, or a KEY_WAKEUP can land before the guest is in s2idle.
        let mut asleep_since: Option<std::time::Instant> = None;
        loop {
            // Read the generation before the state, so a transition in between wakes the wait.
            let seen = watch.generation();
            // Hold the bracket from reading the guest through to waking it, as `didWake` does. A
            // `willSleep` in between would otherwise find our suspend finished and no longer
            // ours, and pause the guest just before the wake reached it — the lost wake again.
            let mut inner = self.inner.lock().unwrap();
            let raw = self.guest_sleep();
            let guest = if raw == GuestSleep::Asleep && !self.vmm.lock().unwrap().system_suspended()
            {
                let since = *asleep_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= PARK_SETTLE {
                    GuestSleep::Asleep
                } else {
                    GuestSleep::Suspending
                }
            } else {
                asleep_since = None;
                raw
            };
            let grace_over = armed.elapsed() >= LATE_SUSPEND_GRACE;
            match inner.state.on_watch(id, guest, grace_over) {
                WatchAction::WakeGuest => {
                    log::info!(
                        "host wake: the guest finished our suspend {:.1?} after the host woke; \
                         waking it",
                        armed.elapsed()
                    );
                    crate::wake::guest(&self.vmm);
                    return;
                }
                WatchAction::Stop => {
                    log::info!(
                        "host wake: stopped watching after {:.1?} (guest {guest:?}): the suspend \
                         aborted, never started, or a new host sleep took over",
                        armed.elapsed()
                    );
                    return;
                }
                WatchAction::Keep => {}
            }
            drop(inner);
            if guest == GuestSleep::Suspending && !warned && armed.elapsed() >= SUSPENDING_TOO_LONG
            {
                warned = true;
                let (held, released) = {
                    let vmm = self.vmm.lock().unwrap();
                    (vmm.quiesce_holdouts(), vmm.released_devices())
                };
                log::warn!(
                    "host wake: the guest has been part-way into our suspend for {:.0?} — still \
                     waiting to wake it when it finishes (held: {held:?}; released: {released:?})",
                    armed.elapsed()
                );
            }
            watch.wait_past(seen, WATCH_POLL);
        }
    }
}

#[cfg(target_os = "macos")]
struct PowerCtx {
    bracket: Arc<Bracket>,
    /// The root power domain port, filled in right after registration (before the run
    /// loop starts, so before any callback can fire).
    root_port: ffi::IoConnect,
}

#[cfg(target_os = "macos")]
impl PowerCtx {
    fn handle(&mut self, message_type: u32, message_argument: *mut std::ffi::c_void) {
        match message_type {
            // Idle-sleep query: never veto (a veto would keep the user's Mac awake).
            ffi::K_IO_MESSAGE_CAN_SYSTEM_SLEEP => {
                unsafe { ffi::IOAllowPowerChange(self.root_port, message_argument as isize) };
            }
            ffi::K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
                self.bracket.before_host_sleep();
                unsafe { ffi::IOAllowPowerChange(self.root_port, message_argument as isize) };
            }
            ffi::K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => self.bracket.after_host_wake(),
            _ => {}
        }
    }
}

/// The `willSleep` release decision: pulse the guest's sleep button and hold the host's
/// sleep ack until it is safe for the host to stop our vCPUs.
///
/// "Safe" means the guest is past `timekeeping_suspend`, so the host's sleep is classified
/// as suspend rather than absorbed by `CLOCK_MONOTONIC` — see [`crate::quiesce`] for why
/// device quiesce alone does not establish that.
///
/// Ends by pausing the vCPUs ourselves. On the happy path that is a ribbon: the guest is
/// already parked, and pausing only means the stop happens at a boundary we picked instead
/// of wherever macOS would have cut us. When the guest did NOT get there it is the
/// backstop, and it is what keeps a lost race survivable — see [`resume_after_host_sleep`].
#[cfg(target_os = "macos")]
fn hold_ack_until_safe_to_stop(vmm: &Arc<Mutex<Vmm>>, pulse_button: bool) -> Quiesced {
    log::info!("host sleep: quiescing the guest and holding the sleep ack");
    let outcome = crate::quiesce::quiesce_guest(
        vmm,
        &QuiesceRequest {
            pulse_button,
            device_budget: DEVICE_WAIT,
            park_budget: PARK_WAIT,
            park_settle: PARK_SETTLE,
        },
    );
    match outcome {
        Quiesced::Parked => {
            log::info!("host sleep: guest parked; releasing the sleep ack")
        }
        Quiesced::SystemSuspended => {
            log::info!("host sleep: guest suspended to RAM; releasing the sleep ack")
        }
        other => log::warn!(
            "host sleep: guest reached only {other:?} within the budget; pausing it \
             ourselves and releasing the sleep ack — the guest's wall clock will need \
             correcting on wake, but its CLOCK_MONOTONIC is protected"
        ),
    }
    // Stop the guest at a boundary we chose, rather than leaving it to macOS.
    if let Err(e) = vmm.lock().unwrap().pause() {
        log::warn!("host sleep: pausing the vCPUs failed ({e}); the host will stop them itself");
    }
    outcome
}

/// The `didWake` counterpart: unpause, choosing the flavour the sleep-side outcome earned.
///
/// [`Quiesced::Parked`] resumes **keeping the counter**: the guest is in s2idle past
/// `timekeeping_suspend`, and `timekeeping_resume()` derives the sleep it injects into
/// REALTIME/BOOTTIME from exactly that counter delta — hiding the interval would leave the
/// wall clock behind by the length of the host's sleep. Anything else resumes with the
/// interval hidden: that guest still had timekeeping live, so letting it see the elapsed
/// time would put the whole host sleep into `CLOCK_MONOTONIC`. Its wall clock is then
/// behind, which chrony, the agent's TimeSync, or the qga `guest-set-time` rung correct —
/// a degraded clock instead of a killed session.
#[cfg(target_os = "macos")]
fn resume_after_host_sleep(vmm: &Arc<Mutex<Vmm>>, outcome: Quiesced) {
    let mut guard = vmm.lock().unwrap();
    let r = if outcome.survives_a_host_stop() {
        guard.resume_keeping_counter()
    } else {
        guard.resume()
    };
    if let Err(e) = r {
        log::warn!("host wake: resuming the vCPUs failed: {e}");
    }
}

/// Test seam for the release-point decision (`LIMINA_HOST_SLEEP_SEAM=1`, `SIGURG`).
///
/// IOKit's half cannot run in CI — sleeping the host kills the session driving the test —
/// so this stands in for macOS: run the real [`hold_ack_until_safe_to_stop`], then stop
/// this process at exactly the moment we release the ack. Stopping *at* the release point
/// rather than some microseconds later is deliberate: it is the worst case macOS is
/// entitled to, and it makes the race deterministic, because the rendezvous the guest
/// still owes cannot complete while its vCPUs are frozen. The driving test sends
/// `SIGCONT` after the gap it wants to simulate, and this thread then runs the same `didWake`
/// path the IOKit handler does — nothing the seam adds of its own, so a wake decision that
/// strands the guest in production strands it here too.
///
/// `SIGSTOP` (not [`Vmm::pause`]) is what models a host sleep: `Vmm::pause`/`resume` hide
/// the elapsed time by advancing the vtimer offset, which is exactly what macOS does NOT
/// do to us.
#[cfg(target_os = "macos")]
fn install_test_seam(bracket: Arc<Bracket>) {
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
                    bracket.before_host_sleep();
                    log::warn!("host sleep seam: ack released — stopping the worker");
                    unsafe { libc::raise(libc::SIGSTOP) };
                    log::warn!("host sleep seam: continued — simulating didWake");
                    bracket.after_host_wake();
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
        .expect("spawning the host-sleep seam thread");
}

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
///
/// Also installs the L2 test seam, which stands in for macOS and is a no-op unless
/// `LIMINA_HOST_SLEEP_SEAM=1`.
#[cfg(target_os = "macos")]
pub fn start(vmm: Arc<Mutex<Vmm>>) {
    let bracket = Bracket::new(vmm);
    install_test_seam(bracket.clone());
    std::thread::Builder::new()
        .name("host-sleep".into())
        .spawn(move || {
            let ctx = Box::leak(Box::new(PowerCtx {
                bracket,
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
    use super::GuestSleep::{Asleep, Awake, Suspending};
    use super::*;

    fn report() -> GuestReport {
        GuestReport {
            system_suspended: false,
            devices_held: true,
            devices_released: false,
            all_vcpus_parked: false,
            last_transition: Some(vmm::DriverTransition::Taken),
        }
    }

    #[test]
    fn a_running_guest_is_awake() {
        assert_eq!(classify(report()), Awake);
    }

    #[test]
    fn a_guest_handing_devices_back_is_suspending() {
        let r = GuestReport {
            devices_released: true,
            last_transition: Some(vmm::DriverTransition::Released),
            ..report()
        };
        assert_eq!(classify(r), Suspending);
    }

    /// Half-way out of a suspend the devices look exactly as they do half-way in. Read as
    /// Suspending, a host sleep in that window would skip the pulse and stop a running guest.
    #[test]
    fn a_guest_taking_devices_back_is_awake() {
        let r = GuestReport {
            devices_released: true,
            last_transition: Some(vmm::DriverTransition::Taken),
            ..report()
        };
        assert_eq!(classify(r), Awake);
    }

    #[test]
    fn every_device_released_with_a_vcpu_running_is_suspending() {
        let r = GuestReport {
            devices_held: false,
            devices_released: true,
            last_transition: Some(vmm::DriverTransition::Released),
            ..report()
        };
        assert_eq!(classify(r), Suspending);
    }

    #[test]
    fn s2idle_and_system_suspend_are_asleep() {
        let s2idle = GuestReport {
            devices_held: false,
            devices_released: true,
            all_vcpus_parked: true,
            last_transition: Some(vmm::DriverTransition::Released),
            ..report()
        };
        assert_eq!(classify(s2idle), Asleep);
        let deep = GuestReport {
            system_suspended: true,
            ..s2idle
        };
        assert_eq!(classify(deep), Asleep);
    }

    /// Before any driver has touched a device the guest is still booting: not one to pulse.
    #[test]
    fn a_guest_no_driver_has_touched_yet_is_not_pulsed() {
        let r = GuestReport {
            devices_held: false,
            devices_released: true,
            last_transition: None,
            ..report()
        };
        assert_eq!(classify(r), Suspending);
    }

    /// Arm a watch the way a slow guest does: pulsed, host slept before it got there.
    fn watching(guest_at_wake: GuestSleep) -> (HostSleepState, u64) {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(Awake), SleepAction::PulseAndWait);
        match s.on_did_wake(guest_at_wake) {
            WakeAction::Watch(id) => (s, id),
            other => panic!("expected a watch, got {other:?}"),
        }
    }

    #[test]
    fn running_guest_is_slept_and_woken() {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(Awake), SleepAction::PulseAndWait);
        assert_eq!(s.on_did_wake(Asleep), WakeAction::WakeGuest);
    }

    #[test]
    fn user_suspended_guest_is_left_alone() {
        let mut s = HostSleepState::default();
        // Asleep before the host sleeps → not ours; must NOT be woken on host wake.
        assert_eq!(s.on_will_sleep(Asleep), SleepAction::DontPulse);
        assert_eq!(s.on_did_wake(Asleep), WakeAction::LeaveAlone);
    }

    /// The tester's wedge: paused part-way into our suspend, unpaused on wake, and finishing
    /// it only then. The wake must follow it down, not judge it once at the unpause.
    #[test]
    fn a_suspend_finished_after_the_wake_is_woken() {
        let (mut s, id) = watching(Suspending);
        assert_eq!(s.on_watch(id, Suspending, false), WatchAction::Keep);
        assert_eq!(s.on_watch(id, Asleep, false), WatchAction::WakeGuest);
    }

    /// A suspend that has visibly started can only complete or abort, so it is waited for
    /// however long it takes — the grace is for suspends not yet visible.
    #[test]
    fn a_started_suspend_outlives_the_grace() {
        let (mut s, id) = watching(Suspending);
        assert_eq!(s.on_watch(id, Suspending, true), WatchAction::Keep);
        assert_eq!(s.on_watch(id, Asleep, true), WatchAction::WakeGuest);
    }

    /// Our pulse landed but its suspend was still in userspace when the host slept: no
    /// device shows it at the wake, then it starts and completes.
    #[test]
    fn a_suspend_started_after_the_wake_is_woken() {
        let (mut s, id) = watching(Awake);
        assert_eq!(s.on_watch(id, Awake, false), WatchAction::Keep);
        assert_eq!(s.on_watch(id, Suspending, false), WatchAction::Keep);
        assert_eq!(s.on_watch(id, Asleep, false), WatchAction::WakeGuest);
    }

    /// Devices coming back before `SYSTEM_SUSPEND` means the suspend aborted: stop, and do
    /// not claim a later suspend the user starts.
    #[test]
    fn an_aborted_suspend_releases_ownership() {
        let (mut s, id) = watching(Suspending);
        assert_eq!(s.on_watch(id, Awake, false), WatchAction::Stop);
        assert_eq!(s.on_watch(id, Asleep, false), WatchAction::Stop);
        assert_eq!(s.on_will_sleep(Asleep), SleepAction::DontPulse);
        assert_eq!(s.on_did_wake(Asleep), WakeAction::LeaveAlone);
    }

    /// Never quiesced (e.g. a session swallowed the sleep key) — nothing to wake once the
    /// grace runs out.
    #[test]
    fn guest_that_ignored_the_pulse_is_not_woken() {
        let (mut s, id) = watching(Awake);
        assert_eq!(s.on_watch(id, Awake, true), WatchAction::Stop);
        assert_eq!(s.on_will_sleep(Asleep), SleepAction::DontPulse);
        assert_eq!(s.on_did_wake(Asleep), WakeAction::LeaveAlone);
    }

    #[test]
    fn late_suspend_is_recovered_on_wake() {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(Awake), SleepAction::PulseAndWait);
        // The quiesce wait timed out, the host slept anyway, and our pulse landed late —
        // the guest is asleep at wake time and it was OUR pulse: wake it.
        assert_eq!(s.on_did_wake(Asleep), WakeAction::WakeGuest);
    }

    /// A host sleep while our suspend is still under way: no second pulse (the latched-button
    /// trap), and the suspend stays ours across it.
    #[test]
    fn a_host_sleep_mid_suspend_does_not_pulse_and_keeps_ownership() {
        let (mut s, id) = watching(Suspending);
        assert_eq!(s.on_will_sleep(Suspending), SleepAction::DontPulse);
        // The new bracket cancelled the old watch...
        assert_eq!(s.on_watch(id, Asleep, false), WatchAction::Stop);
        // ...but not the ownership: the guest finished while the host slept.
        assert_eq!(s.on_did_wake(Asleep), WakeAction::WakeGuest);
    }

    /// A suspend the guest started on its own is not ours, even though it is under way when
    /// the host sleeps and asleep when it wakes.
    #[test]
    fn a_guest_suspending_on_its_own_is_not_pulsed_or_woken() {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(Suspending), SleepAction::DontPulse);
        assert_eq!(s.on_did_wake(Asleep), WakeAction::LeaveAlone);
    }

    /// A watch left over from an earlier wake must not act on a later one's behalf.
    #[test]
    fn a_stale_watch_stops() {
        let (mut s, old) = watching(Suspending);
        assert_eq!(s.on_will_sleep(Suspending), SleepAction::DontPulse);
        let new = match s.on_did_wake(Suspending) {
            WakeAction::Watch(id) => id,
            other => panic!("expected a watch, got {other:?}"),
        };
        assert_ne!(old, new);
        assert_eq!(s.on_watch(old, Asleep, false), WatchAction::Stop);
        assert_eq!(s.on_watch(new, Asleep, false), WatchAction::WakeGuest);
    }

    /// Our pulse was still in userspace at the wake, and the host sleeps again before the guest
    /// shows it. A second pulse would replay after the first suspend and re-suspend the guest;
    /// the suspend stays ours instead, and is woken once it completes.
    #[test]
    fn a_host_sleep_before_our_suspend_shows_does_not_pulse_again() {
        let (mut s, _) = watching(Awake);
        assert_eq!(s.on_will_sleep(Awake), SleepAction::DontPulse);
        assert_eq!(s.on_did_wake(Asleep), WakeAction::WakeGuest);
    }

    #[test]
    fn wake_ownership_does_not_leak_across_cycles() {
        let mut s = HostSleepState::default();
        assert_eq!(s.on_will_sleep(Awake), SleepAction::PulseAndWait);
        assert_eq!(s.on_did_wake(Asleep), WakeAction::WakeGuest);
        // Next cycle: the user suspended the guest themselves.
        assert_eq!(s.on_will_sleep(Asleep), SleepAction::DontPulse);
        assert_eq!(s.on_did_wake(Asleep), WakeAction::LeaveAlone);
    }
}

/// Every sequence of host sleeps, guest suspends and watch ticks, to a fixed depth, against a
/// model guest that knows whose request each suspend answers.
///
/// [`HostSleepState`] is driven exactly as [`Bracket`] drives it: `willSleep` classifies the guest
/// and pulses it if told to, then the guest may move while the bracket waits for it to quiesce,
/// then the host pauses it; `didWake` classifies it and wakes it or arms a watch; a watch thread
/// ticks with its own id, a stale one included, before or after the grace, and may see an s2idle
/// guest as still suspending until it has settled. The guest suspends on our pulse or on its user's
/// request, starts or swallows a pulse, and finishes or aborts a suspend.
///
/// After every step: no wake lands on a suspend the user started; no pulse lands on a guest that
/// already has one of ours outstanding (the latched-button trap: the second request replays after
/// the first suspend and re-suspends the guest with nobody left to wake it); and, with the host
/// awake, a suspend of ours is always still ours and watched, so its wake cannot be lost.
#[cfg(test)]
mod every_sequence {
    use super::{GuestSleep, HostSleepState, SleepAction, WakeAction, WatchAction};

    /// Who asked for the suspend a guest is in, or has pending.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Owner {
        Us,
        /// Ours, but the watch gave up on it: the documented cost of the grace.
        Abandoned,
        User,
        /// The user's, started while a request of ours was outstanding: from the host it looks
        /// exactly like ours, and nothing the host can see tells them apart.
        UserWhileOurs,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Guest {
        Awake,
        /// Our pulse landed and its suspend is still in userspace: no device shows it yet.
        Requested(Owner),
        Suspending(Owner),
        Asleep(Owner),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Host {
        Awake,
        /// `willSleep` is holding the ack while the guest quiesces.
        Quiescing,
        Asleep,
    }

    #[derive(Clone, Copy, Debug)]
    enum Op {
        UserSuspends,
        PulseStarts,
        PulseSwallowed,
        Finishes,
        Aborts,
        WillSleep,
        Pause,
        DidWake,
        /// A tick of the newest (`true`) or oldest live watch thread, after the grace or not.
        Tick {
            newest: bool,
            grace_over: bool,
        },
        /// The newest watch sees an s2idle guest before it has settled.
        TickUnsettled,
    }

    const OPS: [Op; 13] = [
        Op::UserSuspends,
        Op::PulseStarts,
        Op::PulseSwallowed,
        Op::Finishes,
        Op::Aborts,
        Op::WillSleep,
        Op::Pause,
        Op::DidWake,
        Op::Tick {
            newest: true,
            grace_over: false,
        },
        Op::Tick {
            newest: true,
            grace_over: true,
        },
        Op::Tick {
            newest: false,
            grace_over: false,
        },
        Op::Tick {
            newest: false,
            grace_over: true,
        },
        Op::TickUnsettled,
    ];

    #[derive(Clone)]
    struct World {
        state: HostSleepState,
        guest: Guest,
        host: Host,
        /// Watch threads still running, by id, oldest first.
        watches: Vec<u64>,
        /// A pulse went to a guest whose earlier pulse the watch had given up on.
        repulsed_after_grace: bool,
        /// A wake went to a user's suspend the host could not tell from ours.
        woke_ambiguous: bool,
    }

    fn classify(g: Guest) -> GuestSleep {
        match g {
            Guest::Awake | Guest::Requested(_) => GuestSleep::Awake,
            Guest::Suspending(_) => GuestSleep::Suspending,
            Guest::Asleep(_) => GuestSleep::Asleep,
        }
    }

    impl World {
        fn new() -> World {
            World {
                state: HostSleepState::default(),
                guest: Guest::Awake,
                host: Host::Awake,
                watches: Vec::new(),
                repulsed_after_grace: false,
                woke_ambiguous: false,
            }
        }

        fn wake(&mut self) -> Result<(), String> {
            if let Guest::Asleep(owner) = self.guest {
                match owner {
                    Owner::User => return Err("woke a guest its user suspended".into()),
                    Owner::UserWhileOurs => self.woke_ambiguous = true,
                    Owner::Us | Owner::Abandoned => {}
                }
                self.guest = Guest::Awake;
            }
            Ok(())
        }

        fn tick(&mut self, id: u64, seen: GuestSleep, grace_over: bool) -> Result<(), String> {
            match self.state.on_watch(id, seen, grace_over) {
                WatchAction::Keep => {}
                WatchAction::WakeGuest => {
                    self.watches.retain(|w| *w != id);
                    self.wake()?;
                }
                WatchAction::Stop => {
                    self.watches.retain(|w| *w != id);
                    // A pulse still pending when the bracket lets go of it is the grace's cost.
                    if self.guest == Guest::Requested(Owner::Us) && !self.state.ours {
                        self.guest = Guest::Requested(Owner::Abandoned);
                    }
                }
            }
            Ok(())
        }

        /// Apply `op`; `None` when it cannot happen here.
        fn step(&mut self, op: Op) -> Option<Result<(), String>> {
            let guest_may_move = self.host != Host::Asleep;
            let watch_may_run = self.host == Host::Awake;
            match op {
                Op::UserSuspends if guest_may_move && self.guest == Guest::Awake => {
                    self.guest = Guest::Suspending(if self.state.ours {
                        Owner::UserWhileOurs
                    } else {
                        Owner::User
                    });
                }
                Op::PulseStarts if guest_may_move => match self.guest {
                    Guest::Requested(o) => self.guest = Guest::Suspending(o),
                    _ => return None,
                },
                Op::PulseSwallowed if guest_may_move => match self.guest {
                    Guest::Requested(_) => self.guest = Guest::Awake,
                    _ => return None,
                },
                Op::Finishes if guest_may_move => match self.guest {
                    Guest::Suspending(o) => self.guest = Guest::Asleep(o),
                    _ => return None,
                },
                Op::Aborts if guest_may_move => match self.guest {
                    Guest::Suspending(_) => self.guest = Guest::Awake,
                    _ => return None,
                },
                Op::WillSleep if self.host == Host::Awake => {
                    self.host = Host::Quiescing;
                    if self.state.on_will_sleep(classify(self.guest)) == SleepAction::PulseAndWait {
                        match self.guest {
                            Guest::Awake => self.guest = Guest::Requested(Owner::Us),
                            // The grace's cost: a pulse the watch gave up on as swallowed may in
                            // fact still be pending. Counted, so the walk shows it is reachable.
                            Guest::Requested(Owner::Abandoned) => {
                                self.guest = Guest::Requested(Owner::Us);
                                self.repulsed_after_grace = true;
                            }
                            Guest::Requested(_) => {
                                return Some(Err(
                                    "pulsed a guest that already had a request of ours pending"
                                        .into(),
                                ));
                            }
                            other => return Some(Err(format!("pulsed a guest {other:?}"))),
                        }
                    }
                }
                Op::Pause if self.host == Host::Quiescing => self.host = Host::Asleep,
                Op::DidWake if self.host == Host::Asleep => {
                    self.host = Host::Awake;
                    match self.state.on_did_wake(classify(self.guest)) {
                        WakeAction::WakeGuest => return Some(self.wake()),
                        WakeAction::Watch(id) => self.watches.push(id),
                        WakeAction::LeaveAlone => {}
                    }
                }
                Op::Tick { newest, grace_over } if watch_may_run => {
                    let id = if newest {
                        *self.watches.last()?
                    } else {
                        *self.watches.first()?
                    };
                    return Some(self.tick(id, classify(self.guest), grace_over));
                }
                Op::TickUnsettled if watch_may_run && matches!(self.guest, Guest::Asleep(_)) => {
                    let id = *self.watches.last()?;
                    return Some(self.tick(id, GuestSleep::Suspending, false));
                }
                _ => return None,
            }
            Some(Ok(()))
        }

        /// With the host awake, a suspend of ours must still be ours and watched, or nothing will
        /// ever wake it.
        fn check(&self) -> Result<(), String> {
            let ours_in_flight = matches!(
                self.guest,
                Guest::Requested(Owner::Us)
                    | Guest::Suspending(Owner::Us)
                    | Guest::Asleep(Owner::Us)
            );
            if self.host == Host::Awake && ours_in_flight && self.state.watch.is_none() {
                return Err(format!(
                    "our suspend ({:?}) is no longer watched: its wake is lost",
                    self.guest
                ));
            }
            Ok(())
        }
    }

    struct Walk {
        sequences: u64,
        wakes: u64,
        pulses_pending_at_sleep: u64,
        repulsed_after_grace: u64,
        woke_ambiguous: u64,
    }

    fn walk(world: &World, path: &mut Vec<Op>, depth: usize, w: &mut Walk) {
        w.sequences += 1;
        if let Err(e) = world.check() {
            panic!("{e}, after {path:?}");
        }
        if matches!(world.guest, Guest::Requested(Owner::Us)) && world.host == Host::Asleep {
            w.pulses_pending_at_sleep += 1;
        }
        if world.repulsed_after_grace {
            w.repulsed_after_grace += 1;
        }
        if world.woke_ambiguous {
            w.woke_ambiguous += 1;
        }
        if depth == 0 {
            return;
        }
        for op in OPS {
            let mut next = world.clone();
            let before = next.guest;
            match next.step(op) {
                None => continue,
                Some(Err(e)) => {
                    path.push(op);
                    panic!("{e}, after {path:?}");
                }
                Some(Ok(())) => {}
            }
            if matches!(before, Guest::Asleep(_)) && next.guest == Guest::Awake {
                w.wakes += 1;
            }
            path.push(op);
            walk(&next, path, depth - 1, w);
            path.pop();
        }
    }

    #[test]
    fn every_host_sleep_sequence_wakes_exactly_our_suspends() {
        let mut w = Walk {
            sequences: 0,
            wakes: 0,
            pulses_pending_at_sleep: 0,
            repulsed_after_grace: 0,
            woke_ambiguous: 0,
        };
        walk(&World::new(), &mut Vec::new(), 12, &mut w);
        eprintln!(
            "{} sequences, {} wakes, {} with our pulse pending across a host sleep, {} pulsed \
             again after the grace gave up, {} waking a user's suspend begun while ours was out",
            w.sequences,
            w.wakes,
            w.pulses_pending_at_sleep,
            w.repulsed_after_grace,
            w.woke_ambiguous
        );
        assert!(w.wakes > 0, "the walk never woke a guest");
        assert!(
            w.pulses_pending_at_sleep > 0,
            "the walk never slept the host with our pulse still in userspace"
        );
        // The two costs of not being able to see into the guest, which the walk must reach to
        // say so: a pulse re-sent after the grace gave up, and a user's suspend woken because
        // it began while ours was outstanding.
        assert!(w.repulsed_after_grace > 0);
        assert!(w.woke_ambiguous > 0);
    }
}
