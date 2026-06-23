// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Hand guest scanout IOSurfaces from the worker to the supervisor over a **Mach port**,
//! so the surfaces never need `kIOSurfaceIsGlobal` and can't be `IOSurfaceLookup`'d by a
//! stranger process (closing the local screen-read hole; see
//! `spikes/iosurface-machport/RESULTS.md`).
//!
//! Mach ports can't ride `SCM_RIGHTS` over the worker↔supervisor unix socketpair, so we set up
//! a tiny Mach channel by name:
//!
//! - The **supervisor** (long-lived parent, owns the window) calls [`SurfacePortReceiver::register`]:
//!   allocate a receive right, make a send right, register it under a name in the per-user
//!   bootstrap namespace.
//! - The **worker** calls [`SurfacePortSender::lookup`] with the same name (inherited
//!   `bootstrap_port`), getting a send right back to the supervisor.
//! - For each scanout the worker [`SurfacePortSender::send`]s the surface's Mach port (from
//!   `IOSurfaceCreateMachPort`) tagged with a small **ring index** (carried in `msgh_id`); the
//!   supervisor [`SurfacePortReceiver::recv`]s it and `IOSurfaceLookupFromMachPort`s it back to a
//!   live surface. The existing line protocol (`surface <idx> <w> <h>` / `frame <idx>`) still
//!   sequences presents; the index just keys into the Mach-delivered surfaces instead of a global id.
//!
//! `bootstrap_register` is deprecated but functional on current macOS (verified 26.5); it is the
//! simplest parent/child rendezvous that needs no launchd plist.
#![allow(deprecated)] // objc2-io-surface 0.3 renamed the Mach-port free fns; libc deprecates mach_task_self.

use std::ffi::{c_char, CString};
use std::io;

use objc2_core_foundation::CFRetained;
use objc2_io_surface::{IOSurfaceCreateMachPort, IOSurfaceLookupFromMachPort, IOSurfaceRef};

pub use libc::mach_port_t;

// --- Mach / bootstrap FFI not covered by `libc` -------------------------------------------------

const MACH_PORT_NULL: mach_port_t = 0;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const MACH_MSG_TYPE_MOVE_SEND: u32 = 17;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_SEND_MSG: i32 = 0x0000_0001;
const MACH_RCV_MSG: i32 = 0x0000_0002;
const MACH_RCV_TIMEOUT: i32 = 0x0000_0100;
const MACH_MSG_TIMEOUT_NONE: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MachMsgHeader {
    msgh_bits: u32,
    msgh_size: u32,
    msgh_remote_port: mach_port_t,
    msgh_local_port: mach_port_t,
    msgh_voucher_port: mach_port_t,
    msgh_id: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MachMsgBody {
    descriptor_count: u32,
}

// Matches the C `mach_msg_port_descriptor_t` bitfield layout on little-endian: pad2 (low 16),
// disposition (next 8), type (top 8).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MachMsgPortDescriptor {
    name: mach_port_t,
    pad1: u32,
    pad2: u16,
    disposition: u8,
    descriptor_type: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SendMsg {
    header: MachMsgHeader,
    body: MachMsgBody,
    port: MachMsgPortDescriptor,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RecvMsg {
    header: MachMsgHeader,
    body: MachMsgBody,
    port: MachMsgPortDescriptor,
    // Generous room for the receive trailer (mach appends a `mach_msg_max_trailer_t`).
    trailer: [u8; 72],
}

extern "C" {
    static bootstrap_port: mach_port_t;
    fn bootstrap_register(bp: mach_port_t, name: *const c_char, sp: mach_port_t) -> i32;
    fn bootstrap_look_up(bp: mach_port_t, name: *const c_char, sp: *mut mach_port_t) -> i32;
    fn mach_port_allocate(task: mach_port_t, right: u32, name: *mut mach_port_t) -> i32;
    fn mach_port_insert_right(
        task: mach_port_t,
        name: mach_port_t,
        poly: mach_port_t,
        poly_poly: u32,
    ) -> i32;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> i32;
    fn mach_port_mod_refs(task: mach_port_t, name: mach_port_t, right: u32, delta: i32) -> i32;
    fn mach_msg(
        msg: *mut MachMsgHeader,
        option: i32,
        send_size: u32,
        rcv_size: u32,
        rcv_name: mach_port_t,
        timeout: u32,
        notify: mach_port_t,
    ) -> i32;
}

fn task() -> mach_port_t {
    // SAFETY: `mach_task_self()` returns this task's self port; always valid.
    unsafe { libc::mach_task_self() }
}

fn kr_err(what: &str, kr: i32) -> io::Error {
    io::Error::other(format!("{what}: mach/bootstrap error {kr} (0x{kr:x})"))
}

/// Supervisor side: owns a receive right registered under a bootstrap name; [`recv`] blocks for the
/// next surface the worker hands over.
///
/// [`recv`]: SurfacePortReceiver::recv
pub struct SurfacePortReceiver {
    recv_port: mach_port_t,
    name: CString,
}

impl SurfacePortReceiver {
    /// Allocate a receive port + send right and register it under `name` in the per-user bootstrap
    /// namespace. The worker looks the same name up. Call once; the port outlives worker relaunches.
    pub fn register(name: &str) -> io::Result<Self> {
        let cname = CString::new(name).map_err(io::Error::other)?;
        let t = task();
        let mut recv_port: mach_port_t = MACH_PORT_NULL;
        // SAFETY: plain Mach calls on our own task; `recv_port` is a valid out-pointer.
        unsafe {
            let kr = mach_port_allocate(t, MACH_PORT_RIGHT_RECEIVE, &mut recv_port);
            if kr != 0 {
                return Err(kr_err("mach_port_allocate(RECEIVE)", kr));
            }
            // Make a send right on the same name so we can register it for the worker to find.
            let kr = mach_port_insert_right(t, recv_port, recv_port, MACH_MSG_TYPE_MAKE_SEND);
            if kr != 0 {
                mach_port_mod_refs(t, recv_port, MACH_PORT_RIGHT_RECEIVE, -1);
                return Err(kr_err("mach_port_insert_right(MAKE_SEND)", kr));
            }
            let kr = bootstrap_register(bootstrap_port, cname.as_ptr(), recv_port);
            if kr != 0 {
                mach_port_mod_refs(t, recv_port, MACH_PORT_RIGHT_RECEIVE, -1);
                return Err(kr_err("bootstrap_register", kr));
            }
        }
        Ok(Self {
            recv_port,
            name: cname,
        })
    }

    /// The registered bootstrap name (hand this to the worker so it can look up the port).
    pub fn name(&self) -> &str {
        self.name.to_str().unwrap_or_default()
    }

    /// Block (up to `timeout`, or forever if `None`) for the next surface. Returns the worker's
    /// ring index for it and a retained handle to the surface.
    pub fn recv(
        &self,
        timeout: Option<std::time::Duration>,
    ) -> io::Result<(u32, CFRetained<IOSurfaceRef>)> {
        let mut msg: RecvMsg = unsafe { std::mem::zeroed() };
        let (option, ms) = match timeout {
            Some(d) => (
                MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                d.as_millis().min(u32::MAX as u128) as u32,
            ),
            None => (MACH_RCV_MSG, MACH_MSG_TIMEOUT_NONE),
        };
        // SAFETY: `msg` is a correctly-sized, repr(C) receive buffer; `recv_port` is our receive right.
        let kr = unsafe {
            mach_msg(
                &mut msg.header,
                option,
                0,
                std::mem::size_of::<RecvMsg>() as u32,
                self.recv_port,
                ms,
                MACH_PORT_NULL,
            )
        };
        if kr != 0 {
            return Err(kr_err("mach_msg(RCV)", kr));
        }
        if msg.body.descriptor_count != 1 {
            return Err(io::Error::other(format!(
                "surfaceport: expected 1 descriptor, got {}",
                msg.body.descriptor_count
            )));
        }
        let port = msg.port.name;
        let ring_idx = msg.header.msgh_id as u32;
        // `port` is the send right the worker MOVE'd to us, naming a live IOSurface.
        let surface = IOSurfaceLookupFromMachPort(port);
        // The lookup retains the surface; we own the moved send right and drop it now.
        unsafe {
            mach_port_deallocate(task(), port);
        }
        match surface {
            Some(s) => Ok((ring_idx, s)),
            None => Err(io::Error::other(
                "surfaceport: IOSurfaceLookupFromMachPort returned null",
            )),
        }
    }
}

impl Drop for SurfacePortReceiver {
    fn drop(&mut self) {
        // SAFETY: releasing our own receive right.
        unsafe {
            mach_port_mod_refs(task(), self.recv_port, MACH_PORT_RIGHT_RECEIVE, -1);
        }
    }
}

/// Worker side: a send right to the supervisor's receive port, obtained by bootstrap name.
pub struct SurfacePortSender {
    remote: mach_port_t,
}

impl SurfacePortSender {
    /// Look up the supervisor's port by the `name` it registered. Retries briefly: the worker may
    /// reach this before the supervisor has registered (spawn race).
    pub fn lookup(name: &str) -> io::Result<Self> {
        let cname = CString::new(name).map_err(io::Error::other)?;
        let mut last = 0;
        for _ in 0..200 {
            let mut remote: mach_port_t = MACH_PORT_NULL;
            // SAFETY: bootstrap look-up on the inherited bootstrap port; `remote` is a valid out-ptr.
            let kr = unsafe { bootstrap_look_up(bootstrap_port, cname.as_ptr(), &mut remote) };
            if kr == 0 && remote != MACH_PORT_NULL {
                return Ok(Self { remote });
            }
            last = kr;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(kr_err("bootstrap_look_up", last))
    }

    /// Hand `surface` to the supervisor, tagged with `ring_idx`. The surface's Mach port is created
    /// fresh and MOVE'd into the message (no leak).
    pub fn send(&self, ring_idx: u32, surface: &IOSurfaceRef) -> io::Result<()> {
        // Returns a send right we own (+1); MOVE'd into the message below.
        let port = IOSurfaceCreateMachPort(surface);
        if port == MACH_PORT_NULL {
            return Err(io::Error::other("IOSurfaceCreateMachPort returned null"));
        }
        let mut msg = SendMsg {
            header: MachMsgHeader {
                // remote = COPY_SEND (keep our right to the supervisor for the next surface);
                // the message is complex (carries a port descriptor).
                msgh_bits: MACH_MSG_TYPE_COPY_SEND | MACH_MSGH_BITS_COMPLEX,
                msgh_size: std::mem::size_of::<SendMsg>() as u32,
                msgh_remote_port: self.remote,
                msgh_local_port: MACH_PORT_NULL,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: ring_idx as i32,
            },
            body: MachMsgBody {
                descriptor_count: 1,
            },
            port: MachMsgPortDescriptor {
                name: port,
                pad1: 0,
                pad2: 0,
                // MOVE the surface's send right to the supervisor (consume our ref → no leak).
                disposition: MACH_MSG_TYPE_MOVE_SEND as u8,
                descriptor_type: MACH_MSG_PORT_DESCRIPTOR,
            },
        };
        // SAFETY: `msg` is a correctly-laid-out complex send message; `remote` is a live send right.
        let kr = unsafe {
            mach_msg(
                &mut msg.header,
                MACH_SEND_MSG,
                std::mem::size_of::<SendMsg>() as u32,
                0,
                MACH_PORT_NULL,
                MACH_MSG_TIMEOUT_NONE,
                MACH_PORT_NULL,
            )
        };
        if kr != 0 {
            // The MOVE didn't happen on failure; reclaim the port so we don't leak it.
            unsafe {
                mach_port_deallocate(task(), port);
            }
            return Err(kr_err("mach_msg(SEND)", kr));
        }
        Ok(())
    }
}

impl Drop for SurfacePortSender {
    fn drop(&mut self) {
        // SAFETY: releasing our send right to the supervisor.
        unsafe {
            mach_port_deallocate(task(), self.remote);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    use objc2_core_foundation::{
        kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber,
        CFRetained, CFString,
    };
    use objc2_io_surface::{
        kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
        kIOSurfaceWidth, IOSurfaceCreate, IOSurfaceGetBaseAddress, IOSurfaceGetID, IOSurfaceLock,
        IOSurfaceLockOptions, IOSurfaceUnlock,
    };

    fn cfnum(v: i32) -> CFRetained<CFNumber> {
        unsafe {
            CFNumber::new(
                None,
                objc2_core_foundation::CFNumberType::SInt32Type,
                &v as *const i32 as *const c_void,
            )
        }
        .unwrap()
    }

    // Minimal NON-global 8x4 BGRA surface with a known first byte; mirrors limina-display's
    // create_global_iosurface but without kIOSurfaceIsGlobal (the whole point).
    fn make_surface(tag: u8) -> CFRetained<IOSurfaceRef> {
        unsafe {
            let vw = cfnum(8);
            let vh = cfnum(4);
            let vbpe = cfnum(4);
            let vbpr = cfnum(32);
            let vpf = cfnum(i32::from_be_bytes(*b"BGRA"));
            let mut keys: [*const c_void; 5] = [
                (kIOSurfaceWidth as *const CFString).cast(),
                (kIOSurfaceHeight as *const CFString).cast(),
                (kIOSurfaceBytesPerElement as *const CFString).cast(),
                (kIOSurfaceBytesPerRow as *const CFString).cast(),
                (kIOSurfacePixelFormat as *const CFString).cast(),
            ];
            let mut values: [*const c_void; 5] = [
                (&*vw as *const CFNumber).cast(),
                (&*vh as *const CFNumber).cast(),
                (&*vbpe as *const CFNumber).cast(),
                (&*vbpr as *const CFNumber).cast(),
                (&*vpf as *const CFNumber).cast(),
            ];
            let dict = CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .unwrap();
            let s = IOSurfaceCreate(&dict).expect("create surface");
            IOSurfaceLock(&s, IOSurfaceLockOptions(0), std::ptr::null_mut());
            (IOSurfaceGetBaseAddress(&s).as_ptr() as *mut u8).write(tag);
            IOSurfaceUnlock(&s, IOSurfaceLockOptions(0), std::ptr::null_mut());
            s
        }
    }

    #[test]
    fn round_trips_a_surface_in_process() {
        // Register a receiver, look it up as a sender, hand a surface across, verify identity + pixel.
        let name = format!("eti.noronha.limina.test.{}", std::process::id());
        let rx = SurfacePortReceiver::register(&name).expect("register");
        let tx = SurfacePortSender::lookup(&name).expect("lookup");

        let surf = make_surface(0xAB);
        let want_id = IOSurfaceGetID(&surf);
        tx.send(7, &surf).expect("send");

        let (idx, got) = rx
            .recv(Some(std::time::Duration::from_secs(2)))
            .expect("recv");
        assert_eq!(idx, 7, "ring index round-trips via msgh_id");
        assert_eq!(IOSurfaceGetID(&got), want_id, "same underlying surface");
        unsafe {
            IOSurfaceLock(&got, IOSurfaceLockOptions(0), std::ptr::null_mut());
            let base = IOSurfaceGetBaseAddress(&got).as_ptr() as *const u8;
            assert_eq!(base.read(), 0xAB, "the shared pixel came across");
            IOSurfaceUnlock(&got, IOSurfaceLockOptions(0), std::ptr::null_mut());
        }
    }
}
