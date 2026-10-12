// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The guest-controlled pointer's mouse: a mouse's own counts, read from HID and sent to the
//! guest's relative mouse while the pointer is captured, so the guest's acceleration is the only
//! one. macOS's pointer events carry its accelerated deltas; IOHIDManager hands over the device's
//! reports untouched, cursor disassociated or not (`spikes/hid-mouse/RESULTS.md`).
//!
//! The reader runs on a thread of its own, started the first time the mode is wanted (opening
//! it may make macOS ask for Input Monitoring), and keeps reading for the life of the process;
//! [`HidMouse::set_active`] decides whether what it reads goes anywhere. The built-in trackpad
//! is a HID mouse too, and is left out: its fingers reach the guest's touchpad.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use limina_input::InputEvent;
use limina_input::constants::{
    EV_REL, REL_HWHEEL, REL_HWHEEL_HI_RES, REL_WHEEL, REL_WHEEL_HI_RES, REL_X, REL_Y,
};
use objc2_core_foundation::CFString;

use super::input::{send_event, wire_now_us, wire_trace};
use super::{InputDev, WorkerConn};

/// How recently the reader must have sent motion (or a wheel step) for macOS's own event for
/// it to be a duplicate. Without one — no permission, a Universal Control or remote mouse, a
/// mouse the reader leaves out — macOS's deltas stand in ([`HidMouse::moving`]).
pub const HID_RECENT: Duration = Duration::from_millis(100);

const PAGE_GENERIC_DESKTOP: u32 = 0x01;
const PAGE_CONSUMER: u32 = 0x0C;
const USAGE_MOUSE: u32 = 0x02;
const USAGE_X: u32 = 0x30;
const USAGE_Y: u32 = 0x31;
const USAGE_WHEEL: u32 = 0x38;
const USAGE_AC_PAN: u32 = 0x238;
/// Apple's vendor page, on the built-in trackpad's usage pairs and on no plain mouse.
const PAGE_APPLE_VENDOR: u32 = 0xFF00;

/// Whether a device with these usage pairs is one the reader leaves out: Apple's multitouch
/// surfaces, which report pointer motion as a mouse too (measured: the built-in trackpad is
/// `1:2, 1:1, ff00:25`, and does not conform to Digitizer TouchPad).
pub fn leave_out(pairs: &[(u32, u32)]) -> bool {
    pairs.iter().any(|&(page, _)| page == PAGE_APPLE_VENDOR)
}

/// One report's worth of values, gathered into one frame: a report's X and Y must reach the
/// guest together, or its acceleration sees two motions a microsecond apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub dx: i32,
    pub dy: i32,
    pub wheel: i32,
    pub pan: i32,
}

impl Report {
    /// Fold one HID value in. `false` for a usage the reader does not forward.
    pub fn add(&mut self, page: u32, usage: u32, value: i32) -> bool {
        match (page, usage) {
            (PAGE_GENERIC_DESKTOP, USAGE_X) => self.dx += value,
            (PAGE_GENERIC_DESKTOP, USAGE_Y) => self.dy += value,
            (PAGE_GENERIC_DESKTOP, USAGE_WHEEL) => self.wheel += value,
            (PAGE_CONSUMER, USAGE_AC_PAN) => self.pan += value,
            _ => return false,
        }
        true
    }

    /// The frame for the guest's relative mouse, ending in SYN_REPORT; empty when nothing
    /// moved. A wheel step is a detent, sent in both its units.
    pub fn events(&self) -> Vec<InputEvent> {
        let mut events = Vec::new();
        for (code, value) in [
            (REL_X, self.dx),
            (REL_Y, self.dy),
            (REL_WHEEL, self.wheel),
            (REL_WHEEL_HI_RES, self.wheel * 120),
            (REL_HWHEEL, self.pan),
            (REL_HWHEEL_HI_RES, self.pan * 120),
        ] {
            if value != 0 {
                events.push(InputEvent::new(EV_REL, code, value));
            }
        }
        if !events.is_empty() {
            events.push(InputEvent::syn());
        }
        events
    }
}

/// What the window and the reader share.
pub struct HidMouse {
    conn: Arc<WorkerConn>,
    active: AtomicBool,
    epoch: Instant,
    /// When the reader last sent motion, and a wheel step, in ns since `epoch` (0: never).
    motion_at: AtomicU64,
    wheel_at: AtomicU64,
}

impl HidMouse {
    /// Start the reader. Its thread runs for the life of the process.
    pub fn start(conn: Arc<WorkerConn>) -> &'static HidMouse {
        let mouse: &'static HidMouse = Box::leak(Box::new(HidMouse {
            conn,
            active: AtomicBool::new(false),
            epoch: Instant::now(),
            motion_at: AtomicU64::new(0),
            wheel_at: AtomicU64::new(0),
        }));
        let spawned = std::thread::Builder::new()
            .name("limina-hidmouse".into())
            .spawn(move || run(mouse));
        if let Err(e) = spawned {
            log::warn!(
                "guest-controlled pointer: no HID mouse reader ({e}); macOS's deltas stand in"
            );
        }
        mouse
    }

    /// Whether what the reader reads reaches the guest: while the guest moves the captured
    /// pointer.
    pub fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::Relaxed);
    }

    /// Whether the reader sent motion within [`HID_RECENT`]: macOS's event for it is a
    /// duplicate.
    pub fn moving(&self) -> bool {
        self.recent(&self.motion_at)
    }

    /// As [`Self::moving`], for the wheel.
    pub fn wheeling(&self) -> bool {
        self.recent(&self.wheel_at)
    }

    fn recent(&self, at: &AtomicU64) -> bool {
        let at = at.load(Ordering::Relaxed);
        at != 0 && self.since_epoch().saturating_sub(at) <= HID_RECENT.as_nanos() as u64
    }

    fn since_epoch(&self) -> u64 {
        (self.epoch.elapsed().as_nanos() as u64).max(1)
    }

    fn send(&self, report: &Report) {
        let events = report.events();
        if events.is_empty() || !self.active.load(Ordering::Relaxed) {
            return;
        }
        let now = self.since_epoch();
        if report.dx != 0 || report.dy != 0 {
            self.motion_at.store(now, Ordering::Relaxed);
        }
        if report.wheel != 0 || report.pan != 0 {
            self.wheel_at.store(now, Ordering::Relaxed);
        }
        if wire_trace() {
            eprintln!(
                "[WIRE] t={} dev=rel hid dx={} dy={} wheel={} pan={}",
                wire_now_us(),
                report.dx,
                report.dy,
                report.wheel,
                report.pan
            );
        }
        // Snapshot rule as everywhere: hold the Arc so the fd stays open across the sends.
        let io = self.conn.io();
        let _frame = io.frame(InputDev::Rel);
        for ev in events {
            send_event(io.rel_ptr_fd(), ev);
        }
    }
}

// ---- IOKit -------------------------------------------------------------------------------------

type CFTypeRef = *const c_void;
type IOReturn = i32;
const IO_RETURN_SUCCESS: IOReturn = 0;
const IOHID_REQUEST_LISTEN_EVENT: u32 = 1;
const CF_NUMBER_SINT64: isize = 4;
const CF_RUN_LOOP_BEFORE_WAITING: usize = 1 << 5;

type ValueCallback = extern "C" fn(*mut c_void, IOReturn, *mut c_void, *mut c_void);
type DeviceCallback = extern "C" fn(*mut c_void, IOReturn, *mut c_void, *mut c_void);
type ObserverCallback = extern "C" fn(*mut c_void, usize, *mut c_void);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(allocator: CFTypeRef, options: u32) -> *mut c_void;
    fn IOHIDManagerSetDeviceMatching(manager: *mut c_void, matching: CFTypeRef);
    fn IOHIDManagerRegisterDeviceMatchingCallback(
        manager: *mut c_void,
        callback: DeviceCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerRegisterDeviceRemovalCallback(
        manager: *mut c_void,
        callback: DeviceCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerRegisterInputValueCallback(
        manager: *mut c_void,
        callback: ValueCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(manager: *mut c_void, run_loop: CFTypeRef, mode: CFTypeRef);
    fn IOHIDManagerOpen(manager: *mut c_void, options: u32) -> IOReturn;
    fn IOHIDValueGetElement(value: *mut c_void) -> *mut c_void;
    fn IOHIDValueGetIntegerValue(value: *mut c_void) -> isize;
    fn IOHIDValueGetTimeStamp(value: *mut c_void) -> u64;
    fn IOHIDElementGetUsagePage(element: *mut c_void) -> u32;
    fn IOHIDElementGetUsage(element: *mut c_void) -> u32;
    fn IOHIDElementGetDevice(element: *mut c_void) -> *mut c_void;
    fn IOHIDDeviceGetProperty(device: *mut c_void, key: CFTypeRef) -> CFTypeRef;
    fn IOHIDDeviceGetService(device: *mut c_void) -> u32;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
    fn IOHIDCheckAccess(request: u32) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRunLoopGetCurrent() -> CFTypeRef;
    fn CFRunLoopRun();
    fn CFRunLoopObserverCreate(
        allocator: CFTypeRef,
        activities: usize,
        repeats: u8,
        order: isize,
        callout: ObserverCallback,
        context: *mut c_void,
    ) -> CFTypeRef;
    fn CFRunLoopAddObserver(run_loop: CFTypeRef, observer: CFTypeRef, mode: CFTypeRef);
    fn CFDictionaryCreateMutable(
        allocator: CFTypeRef,
        capacity: isize,
        keys: *const c_void,
        values: *const c_void,
    ) -> *mut c_void;
    fn CFDictionarySetValue(dict: *mut c_void, key: CFTypeRef, value: CFTypeRef);
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberCreate(allocator: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, kind: isize, value: *mut c_void) -> u8;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFRelease(cf: CFTypeRef);
}

/// The reader thread's own state: callbacks run on it, one at a time.
struct Reader {
    mouse: &'static HidMouse,
    /// The devices whose values are forwarded, by `IOHIDDeviceRef`, with their registry entry
    /// id: one device can surface as several objects.
    devices: Vec<(usize, u64)>,
    /// The report being gathered, and its timestamp.
    pending: Option<(u64, Report)>,
}

thread_local! {
    static READER: RefCell<Option<Reader>> = const { RefCell::new(None) };
}

fn with_reader(f: impl FnOnce(&mut Reader)) {
    READER.with(|r| {
        if let Some(reader) = r.borrow_mut().as_mut() {
            f(reader);
        }
    });
}

impl Reader {
    fn flush(&mut self) {
        if let Some((_, report)) = self.pending.take() {
            self.mouse.send(&report);
        }
    }
}

fn run(mouse: &'static HidMouse) {
    READER.with(|r| {
        *r.borrow_mut() = Some(Reader {
            mouse,
            devices: Vec::new(),
            pending: None,
        })
    });
    let access = match unsafe { IOHIDCheckAccess(IOHID_REQUEST_LISTEN_EVENT) } {
        0 => "granted",
        1 => "denied",
        _ => "not yet asked",
    };
    // SAFETY: plain CoreFoundation/IOKit calls on this thread; every object created here lives
    // for the thread's (the process's) life, so none is released.
    unsafe {
        let manager = IOHIDManagerCreate(std::ptr::null(), 0);
        if manager.is_null() {
            log::warn!("guest-controlled pointer: no HID manager; macOS's deltas stand in");
            return;
        }
        let matching = CFDictionaryCreateMutable(
            std::ptr::null(),
            0,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        );
        for (key, value) in [
            ("DeviceUsagePage", PAGE_GENERIC_DESKTOP),
            ("DeviceUsage", USAGE_MOUSE),
        ] {
            let key = CFString::from_str(key);
            let value = i64::from(value);
            let number = CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_SINT64,
                (&raw const value).cast(),
            );
            CFDictionarySetValue(matching, (&*key as *const CFString).cast(), number);
            CFRelease(number);
        }
        IOHIDManagerSetDeviceMatching(manager, matching);
        CFRelease(matching);
        IOHIDManagerRegisterDeviceMatchingCallback(manager, matched, std::ptr::null_mut());
        IOHIDManagerRegisterDeviceRemovalCallback(manager, removed, std::ptr::null_mut());
        IOHIDManagerRegisterInputValueCallback(manager, value_in, std::ptr::null_mut());
        let run_loop = CFRunLoopGetCurrent();
        IOHIDManagerScheduleWithRunLoop(manager, run_loop, kCFRunLoopDefaultMode);
        // The values of one report arrive in one pass of the run loop: send what was gathered
        // before it sleeps.
        let observer = CFRunLoopObserverCreate(
            std::ptr::null(),
            CF_RUN_LOOP_BEFORE_WAITING,
            1,
            0,
            before_waiting,
            std::ptr::null_mut(),
        );
        CFRunLoopAddObserver(run_loop, observer, kCFRunLoopDefaultMode);
        let ret = IOHIDManagerOpen(manager, 0);
        if ret == IO_RETURN_SUCCESS {
            log::info!("guest-controlled pointer: reading HID mice (Input Monitoring {access})");
        } else {
            log::warn!(
                "guest-controlled pointer: could not open HID mice (IOReturn {ret:#x}, Input \
                 Monitoring {access}); macOS's deltas stand in"
            );
        }
        CFRunLoopRun();
    }
}

/// A CFString property of a device, if it has one.
unsafe fn string_property(device: *mut c_void, key: &str) -> Option<String> {
    let key = CFString::from_str(key);
    let value = unsafe { IOHIDDeviceGetProperty(device, (&*key as *const CFString).cast()) };
    if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return None;
    }
    // SAFETY: a live CFString, borrowed from the device for the length of this call.
    Some(unsafe { &*(value as *const CFString) }.to_string())
}

/// An integer in a CFDictionary, if it holds a CFNumber under `key`.
unsafe fn dict_number(dict: CFTypeRef, key: &str) -> Option<u32> {
    let key = CFString::from_str(key);
    let value = unsafe { CFDictionaryGetValue(dict, (&*key as *const CFString).cast()) };
    if value.is_null() || unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut n = 0i64;
    let ok = unsafe { CFNumberGetValue(value, CF_NUMBER_SINT64, (&raw mut n).cast()) };
    (ok != 0).then_some(n as u32)
}

/// A device's usage pairs (page, usage).
unsafe fn usage_pairs(device: *mut c_void) -> Vec<(u32, u32)> {
    let key = CFString::from_str("DeviceUsagePairs");
    let pairs = unsafe { IOHIDDeviceGetProperty(device, (&*key as *const CFString).cast()) };
    if pairs.is_null() || unsafe { CFGetTypeID(pairs) != CFArrayGetTypeID() } {
        return Vec::new();
    }
    (0..unsafe { CFArrayGetCount(pairs) })
        .filter_map(|i| {
            let pair = unsafe { CFArrayGetValueAtIndex(pairs, i) };
            Some((unsafe { dict_number(pair, "DeviceUsagePage") }?, unsafe {
                dict_number(pair, "DeviceUsage")
            }?))
        })
        .collect()
}

extern "C" fn matched(_: *mut c_void, _: IOReturn, _: *mut c_void, device: *mut c_void) {
    // SAFETY: IOKit hands a live device for the length of the callback.
    let (product, pairs, entry) = unsafe {
        let mut entry = 0u64;
        IORegistryEntryGetRegistryEntryID(IOHIDDeviceGetService(device), &mut entry);
        (
            string_property(device, "Product").unwrap_or_else(|| "?".into()),
            usage_pairs(device),
            entry,
        )
    };
    if leave_out(&pairs) {
        log::info!("guest-controlled pointer: leaving out \"{product}\" (a multitouch surface)");
        return;
    }
    with_reader(|r| {
        if r.devices.iter().any(|&(_, e)| e == entry) {
            return;
        }
        log::info!("guest-controlled pointer: reading \"{product}\"");
        r.devices.push((device as usize, entry));
    });
}

extern "C" fn removed(_: *mut c_void, _: IOReturn, _: *mut c_void, device: *mut c_void) {
    with_reader(|r| r.devices.retain(|&(d, _)| d != device as usize));
}

extern "C" fn value_in(_: *mut c_void, _: IOReturn, _: *mut c_void, value: *mut c_void) {
    // SAFETY: IOKit hands a live value, and its element and device, for the callback.
    let (device, page, usage, v, at) = unsafe {
        let element = IOHIDValueGetElement(value);
        (
            IOHIDElementGetDevice(element) as usize,
            IOHIDElementGetUsagePage(element),
            IOHIDElementGetUsage(element),
            IOHIDValueGetIntegerValue(value) as i32,
            IOHIDValueGetTimeStamp(value),
        )
    };
    if v == 0 {
        return;
    }
    with_reader(|r| {
        if !r.devices.iter().any(|&(d, _)| d == device) {
            return;
        }
        // A new report: the last one is whole.
        if r.pending.is_some_and(|(t, _)| t != at) {
            r.flush();
        }
        let mut report = r.pending.map_or_else(Report::default, |(_, report)| report);
        if report.add(page, usage, v) {
            r.pending = Some((at, report));
        }
    });
}

extern "C" fn before_waiting(_: *mut c_void, _: usize, _: *mut c_void) {
    with_reader(Reader::flush);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_trackpad_is_left_out_and_a_mouse_is_read() {
        // Measured (`spikes/hid-mouse/RESULTS.md`).
        assert!(leave_out(&[(1, 2), (1, 1), (0xff00, 0x25)]));
        assert!(!leave_out(&[
            (1, 2),
            (1, 1),
            (1, 0x38),
            (1, 0x238),
            (0xc, 1)
        ]));
    }

    #[test]
    fn a_reports_motion_is_one_frame() {
        let mut r = Report::default();
        assert!(r.add(PAGE_GENERIC_DESKTOP, USAGE_X, -15));
        assert!(r.add(PAGE_GENERIC_DESKTOP, USAGE_Y, 3));
        assert!(!r.add(0x09, 1, 1), "buttons come from macOS's events");
        assert_eq!(
            r.events(),
            vec![
                InputEvent::new(EV_REL, REL_X, -15),
                InputEvent::new(EV_REL, REL_Y, 3),
                InputEvent::syn(),
            ]
        );
    }

    #[test]
    fn a_wheel_step_is_a_detent_in_both_units() {
        let mut r = Report::default();
        r.add(PAGE_GENERIC_DESKTOP, USAGE_WHEEL, -1);
        r.add(PAGE_CONSUMER, USAGE_AC_PAN, 2);
        assert_eq!(
            r.events(),
            vec![
                InputEvent::new(EV_REL, REL_WHEEL, -1),
                InputEvent::new(EV_REL, REL_WHEEL_HI_RES, -120),
                InputEvent::new(EV_REL, REL_HWHEEL, 2),
                InputEvent::new(EV_REL, REL_HWHEEL_HI_RES, 240),
                InputEvent::syn(),
            ]
        );
        assert!(Report::default().events().is_empty());
    }
}
