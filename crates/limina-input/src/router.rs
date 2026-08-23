// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Which device carries a keystroke: the virtio keyboard, or the USB HID gadget that covers
//! the window in which the guest has no `virtio_input` driver.
//!
//! The guest is keyboard-less between `ExitBootServices` — where the firmware's
//! `VirtioKeyboardDxe` resets the virtio device — and the moment its own kernel binds
//! `virtio_input`, which no stock initramfs generator ships. USB HID *is* in every stock
//! initramfs (a bare-metal LUKS prompt requires it), so limina presents a USB keyboard gadget
//! and sends keys there for exactly that window. See `docs/design/usb-hid-keyboard.md`.
//!
//! The routing rule is one line: **while the virtio keyboard is not activated, keys go to the
//! USB gadget; otherwise they go to virtio.** Nothing in the guest has to cooperate — the
//! firmware performs the reset itself, so the window opens and closes on its own.
//!
//! [`Route`] is the whole policy and is pure; [`interpose`] is the plumbing that runs it on a
//! thread. That thread also makes the keyboard socket *always drained*: before it existed,
//! events written while the virtio device was inactive sat in the socket buffer and were
//! delivered in a burst the instant the guest bound the driver — a passphrase typed at a LUKS
//! prompt replaying into the session that prompt unlocked.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::constants::EV_KEY;
use crate::hidkbd::{KeyboardReport, REPORT_LEN};
use crate::{InputEvent, WIRE_LEN};

/// A sink for 8-byte HID keyboard input reports — the USB gadget's `push_in`.
pub type HidReportSink = Arc<dyn Fn([u8; REPORT_LEN]) + Send + Sync>;

/// What one input event turns into, given which device is currently carrying keys.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Step {
    /// HID reports to push into the USB gadget, in order.
    pub usb: Vec<[u8; REPORT_LEN]>,
    /// The event to hand the virtio keyboard, when that is the device carrying keys.
    pub virtio: Option<InputEvent>,
}

/// The routing state: which device carries keys, and the held-key state the USB reports are
/// diffed from. Pure — [`Route::step`] is the entire policy.
#[derive(Debug)]
pub struct Route {
    report: KeyboardReport,
    /// True while the USB gadget is the one carrying keys. Starts true: at VM start nothing
    /// has bound the virtio device yet.
    on_usb: bool,
}

impl Default for Route {
    fn default() -> Self {
        Self::new()
    }
}

impl Route {
    pub fn new() -> Self {
        Route {
            report: KeyboardReport::new(),
            on_usb: true,
        }
    }

    /// True while keys are going to the USB gadget.
    pub fn on_usb(&self) -> bool {
        self.on_usb
    }

    /// Route one event. `virtio_live` is whether the guest's driver currently holds the
    /// virtio keyboard (its device state is Activated).
    ///
    /// A change in `virtio_live` is noticed here, on the next event, rather than by watching
    /// for it — which is exactly right, because the event that matters is the first one after
    /// the change. On the handoff to virtio the gadget is told every key is up **before** that
    /// event is forwarded, so a key held across the moment the driver bound cannot stay stuck
    /// down in the guest. The reverse transition (a reboot) drops the held state instead: the
    /// gadget the guest will re-enumerate has no memory of it.
    pub fn step(&mut self, virtio_live: bool, ev: InputEvent) -> Step {
        let mut out = Step::default();

        if virtio_live && self.on_usb {
            if !self.report.is_idle() {
                out.usb.push(self.report.release_all());
            }
            self.on_usb = false;
        } else if !virtio_live && !self.on_usb {
            self.report = KeyboardReport::new();
            self.on_usb = true;
        }

        if self.on_usb {
            // HID reports are self-delimiting, so EV_SYN has nothing to carry, and no other
            // event type reaches a keyboard.
            if ev.type_ == EV_KEY {
                if let Some(report) = self.report.apply(ev.code, ev.value) {
                    out.usb.push(report);
                }
            }
        } else {
            out.virtio = Some(ev);
        }
        out
    }
}

/// Interpose the router between the supervisor's keyboard socket (`src`) and libkrun's
/// virtio-input events backend. Returns the fd that backend should read — the near end of a
/// fresh datagram pair the router forwards into — and the activation flag the backend sets
/// while the guest holds the virtio device.
///
/// The router owns `src` from here on; `src` must not be read anywhere else.
pub fn interpose(src: RawFd, sink: HidReportSink) -> std::io::Result<(RawFd, Arc<AtomicBool>)> {
    let (backend_end, router_end) = datagram_pair()?;
    let active = Arc::new(AtomicBool::new(false));
    let flag = active.clone();
    std::thread::Builder::new()
        .name("limina-key-router".into())
        .spawn(move || run(src, router_end, flag, sink))?;
    Ok((backend_end, active))
}

/// Drain `src` forever, routing each event. Ends only when the supervisor closes its end.
fn run(src: RawFd, virtio_fd: RawFd, active: Arc<AtomicBool>, sink: HidReportSink) {
    let mut route = Route::new();
    let mut buf = [0u8; WIRE_LEN];
    let mut announced = None;
    loop {
        // Blocking: this thread exists to keep the socket drained, so there is nothing else
        // for it to do. A short/partial datagram cannot happen on SOCK_DGRAM.
        let n = unsafe { libc::recv(src, buf.as_mut_ptr() as *mut libc::c_void, WIRE_LEN, 0) };
        if n == 0 {
            log::debug!("key router: supervisor closed the keyboard socket");
            return;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("key router: recv failed, stopping: {err}");
            return;
        }
        if n as usize != WIRE_LEN {
            continue; // malformed datagram; skip rather than desync
        }

        let step = route.step(active.load(Ordering::Acquire), InputEvent::from_bytes(&buf));
        if announced != Some(route.on_usb()) {
            announced = Some(route.on_usb());
            if route.on_usb() {
                log::info!("keyboard: the guest has no virtio-input driver; typing over USB HID");
            } else {
                log::info!("keyboard: the guest bound virtio-input; typing over virtio");
            }
        }
        for report in step.usb {
            sink(report);
        }
        if let Some(ev) = step.virtio {
            send_event(virtio_fd, ev);
        }
    }
}

/// Forward an event to the virtio-input events backend. Non-blocking: if that socket is full
/// the guest has stopped draining its event queue, and blocking here would wedge the router —
/// and with it every later keystroke, including the ones the USB path would have carried.
fn send_event(fd: RawFd, ev: InputEvent) {
    let bytes = ev.to_bytes();
    let n = unsafe { libc::send(fd, bytes.as_ptr() as *const libc::c_void, bytes.len(), 0) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) | Some(libc::ENOBUFS) => {
                log::warn!("key router: dropped {ev:?} — the guest is not draining its event queue")
            }
            _ => log::trace!("key router: send failed: {err}"),
        }
    }
}

/// A datagram socket pair sized and flagged like the supervisor's input channels: deep enough
/// (~32k events) that a momentary lag never drops a key, non-blocking on the send side, and
/// SIGPIPE-free so a dead peer is an error rather than a killed process.
fn datagram_pair() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (read_end, write_end) = (fds[0], fds[1]);
    for fd in [read_end, write_end] {
        set_opt(fd, libc::SO_RCVBUF, 256 * 1024);
        set_opt(fd, libc::SO_SNDBUF, 256 * 1024);
        set_opt(fd, libc::SO_NOSIGPIPE, 1);
    }
    unsafe {
        let flags = libc::fcntl(write_end, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(write_end, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    Ok((read_end, write_end))
}

fn set_opt(fd: RawFd, name: libc::c_int, value: libc::c_int) {
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::*;

    fn key(code: u16, value: i32) -> InputEvent {
        InputEvent::new(EV_KEY, code, value)
    }

    /// The window this whole gadget exists for: no virtio driver, so the passphrase types
    /// over USB and the virtio device is handed nothing.
    #[test]
    fn keys_go_to_usb_while_the_guest_has_no_virtio_driver() {
        let mut r = Route::new();
        let down = r.step(false, key(KEY_A, 1));
        assert_eq!(down.usb.len(), 1);
        assert_eq!(down.usb[0][2], 0x04);
        assert_eq!(down.virtio, None);
        assert!(
            r.step(false, InputEvent::syn()).usb.is_empty(),
            "EV_SYN has nothing to carry"
        );
    }

    /// Once the guest binds virtio-input, keys go there — verbatim, and exactly once. The
    /// double-typing this pins is what a boot-protocol USB gadget would have caused, and what
    /// forwarding to both devices would cause now.
    #[test]
    fn keys_go_to_virtio_once_the_guest_binds_it() {
        let mut r = Route::new();
        let ev = key(KEY_A, 1);
        let step = r.step(true, ev);
        assert_eq!(step.virtio, Some(ev));
        assert!(step.usb.is_empty(), "not delivered twice");
    }

    /// The handoff. A key held when the driver binds would otherwise stay down in the guest
    /// forever: the USB gadget never sends its release (routing has moved on) and the virtio
    /// device never saw its press. One all-released report closes it, and it must go out
    /// *before* the event that triggered the handoff is forwarded.
    #[test]
    fn a_key_held_across_the_handoff_is_released_on_the_usb_side_first() {
        let mut r = Route::new();
        r.step(false, key(KEY_LEFTSHIFT, 1));
        assert!(r.on_usb());

        let release = key(KEY_LEFTSHIFT, 0);
        let step = r.step(true, release);
        assert_eq!(
            step.usb,
            vec![[0u8; REPORT_LEN]],
            "everything released on USB"
        );
        assert_eq!(step.virtio, Some(release));
        assert!(!r.on_usb());
    }

    /// With nothing held there is no stuck key to clear, so the handoff is silent — the guest
    /// should not see a report it has no use for right as its driver comes up.
    #[test]
    fn an_idle_handoff_sends_no_report() {
        let mut r = Route::new();
        r.step(false, key(KEY_A, 1));
        r.step(false, key(KEY_A, 0));
        let step = r.step(true, key(KEY_B, 1));
        assert!(step.usb.is_empty());
        assert_eq!(step.virtio, Some(key(KEY_B, 1)));
    }

    /// A reboot resets the virtio device, and the window reopens: keys go back to USB. The
    /// held state does not survive — the guest re-enumerates a gadget with no memory of it.
    #[test]
    fn a_reboot_reopens_the_window_and_forgets_what_was_held() {
        let mut r = Route::new();
        r.step(true, key(KEY_A, 1)); // running guest
        assert!(!r.on_usb());
        let step = r.step(false, key(KEY_B, 1)); // virtio reset by the firmware
        assert!(r.on_usb());
        assert_eq!(step.virtio, None);
        assert_eq!(step.usb.len(), 1);
        assert_eq!(
            step.usb[0][2..],
            [0x05, 0, 0, 0, 0, 0],
            "only B, no stale A"
        );
    }

    /// Autorepeat crosses to virtio verbatim (the guest's own driver decides what to do with
    /// it) but produces no USB report — HID hosts run their own typematic.
    #[test]
    fn autorepeat_crosses_virtio_but_not_usb() {
        let mut r = Route::new();
        r.step(false, key(KEY_A, 1));
        assert!(r.step(false, key(KEY_A, 2)).usb.is_empty());

        let mut r = Route::new();
        let repeat = key(KEY_A, 2);
        assert_eq!(r.step(true, repeat).virtio, Some(repeat));
    }

    /// The interposer is transparent once virtio is live: what the supervisor writes is what
    /// the events backend reads, byte for byte.
    #[test]
    fn the_interposed_socket_carries_events_through() {
        let (sup, worker) = super::datagram_pair().unwrap();
        let sink: HidReportSink = Arc::new(|_| {});
        let (backend_fd, active) = interpose(worker, sink).unwrap();
        active.store(true, Ordering::Release);

        let ev = key(KEY_ENTER, 1);
        let bytes = ev.to_bytes();
        let n = unsafe { libc::send(sup, bytes.as_ptr() as *const libc::c_void, bytes.len(), 0) };
        assert_eq!(n, WIRE_LEN as isize);

        let mut got = [0u8; WIRE_LEN];
        let n = unsafe {
            libc::recv(
                backend_fd,
                got.as_mut_ptr() as *mut libc::c_void,
                WIRE_LEN,
                0,
            )
        };
        assert_eq!(n, WIRE_LEN as isize);
        assert_eq!(InputEvent::from_bytes(&got), ev);
    }

    /// And the other half of the same thread: while virtio is inactive the socket is still
    /// drained — the events become HID reports instead of piling up to be replayed later.
    #[test]
    fn the_router_drains_the_socket_into_hid_reports_while_virtio_is_inactive() {
        let (sup, worker) = super::datagram_pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: HidReportSink = Arc::new(move |r| tx.send(r).unwrap());
        let (_backend_fd, _active) = interpose(worker, sink).unwrap();

        for ev in [key(KEY_H, 1), key(KEY_H, 0)] {
            let bytes = ev.to_bytes();
            unsafe {
                libc::send(sup, bytes.as_ptr() as *const libc::c_void, bytes.len(), 0);
            }
        }
        let down = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(down[2], 0x0b, "H pressed");
        let up = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(up, [0u8; REPORT_LEN], "H released");
    }
}
