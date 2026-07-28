// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Worker-side virtio-input backends (keyboard + absolute pointer).
//!
//! Each device is a `(config, events)` pair registered with libkrun via the safe Rust
//! `IntoInput*` wrappers (decision D2.1 — no hand-written `repr(C)` vtables). The *config*
//! backend advertises the device's name/ids/capabilities to the guest; the *events* backend
//! ([`FdEvents`]) hands the guest the [`InputEvent`](crate::InputEvent)s the supervisor
//! writes — as 8-byte datagrams — to an inherited socket fd. The notify fd the worker
//! epolls *is* that socket: it's readable exactly while events are queued.

use std::os::fd::{BorrowedFd, RawFd};

use krun_input::{
    write_bitmap, InputAbsInfo, InputBackendError, InputConfigBackend, InputDeviceIds,
    InputEvent as KrunInputEvent, InputEventProviderBackend, InputEventsImpl, InputQueryConfig,
    IntoInputConfig, IntoInputEvents, ObjectNew,
};

use crate::constants::*;
use crate::WIRE_LEN;

/// Userdata carrying the inherited read socket for a device's event stream. `RawFd` is
/// `Copy + Sync`; leaked to `'static` by the public constructors below.
pub struct FdConfig {
    fd: RawFd,
}

/// Events backend: drains evdev triples from the supervisor's socket. Created per-device in
/// the worker thread by libkrun (via `ObjectNew`).
pub struct FdEvents {
    fd: RawFd,
}

impl ObjectNew<FdConfig> for FdEvents {
    fn new(userdata: Option<&FdConfig>) -> Self {
        let fd = userdata.expect("FdEvents requires a socket fd").fd;
        // The worker epolls the fd and drains until empty, so reads must not block.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        Self { fd }
    }
}

impl InputEventsImpl for FdEvents {
    fn get_read_notify_fd(&self) -> Result<BorrowedFd<'_>, InputBackendError> {
        // SAFETY: `fd` is owned by the worker process for its lifetime (inherited at spawn).
        Ok(unsafe { BorrowedFd::borrow_raw(self.fd) })
    }

    fn next_event(&mut self) -> Result<Option<KrunInputEvent>, InputBackendError> {
        let mut buf = [0u8; WIRE_LEN];
        // One datagram = one event; recv won't straddle records (SOCK_DGRAM).
        let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, WIRE_LEN, 0) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            // EAGAIN == EWOULDBLOCK on macOS/Linux: no datagram queued right now.
            return match err.raw_os_error() {
                Some(libc::EAGAIN) => Ok(None),
                _ => Err(InputBackendError::InternalError),
            };
        }
        if n as usize != WIRE_LEN {
            // A short/empty datagram is malformed; skip it rather than desync.
            return Ok(None);
        }
        let ev = crate::InputEvent::from_bytes(&buf);
        Ok(Some(KrunInputEvent {
            type_: ev.type_,
            code: ev.code,
            value: ev.value as u32,
        }))
    }
}

/// Keyboard config: a plain `EV_KEY`/`EV_SYN` keyboard advertising every key limina can emit.
pub struct KeyboardConfig;

impl ObjectNew<()> for KeyboardConfig {
    fn new(_userdata: Option<&()>) -> Self {
        Self
    }
}

impl InputQueryConfig for KeyboardConfig {
    fn query_device_name(&self, name_buf: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(copy_name(KEYBOARD_DEVICE_NAME, name_buf))
    }

    fn query_serial_name(&self, name_buf: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(copy_name(KEYBOARD_SERIAL_NAME, name_buf))
    }

    fn query_device_ids(&self, ids: &mut InputDeviceIds) -> Result<(), InputBackendError> {
        *ids = InputDeviceIds {
            bustype: BUS_VIRTUAL,
            vendor: LIMINA_VENDOR_ID,
            product: KEYBOARD_PRODUCT_ID,
            version: 1,
        };
        Ok(())
    }

    fn query_event_capabilities(
        &self,
        event_type: u8,
        bitmap_buf: &mut [u8],
    ) -> Result<u8, InputBackendError> {
        Ok(match event_type as u16 {
            EV_KEY => write_bitmap(bitmap_buf, SUPPORTED_KEYBOARD_KEYS),
            _ => 0,
        })
    }

    fn query_abs_info(
        &self,
        _abs_axis: u8,
        _abs_info: &mut InputAbsInfo,
    ) -> Result<(), InputBackendError> {
        Ok(())
    }

    fn query_properties(&self, _bitmap: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(0)
    }
}

/// Absolute-pointer config: `ABS_X`/`ABS_Y` in `0..=ABS_MAX` with `INPUT_PROP_POINTER`
/// (cursor follows it, not a touchscreen), plus the three mouse buttons and the wheels.
pub struct PointerConfig;

impl ObjectNew<()> for PointerConfig {
    fn new(_userdata: Option<&()>) -> Self {
        Self
    }
}

impl InputQueryConfig for PointerConfig {
    fn query_device_name(&self, name_buf: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(copy_name(POINTER_DEVICE_NAME, name_buf))
    }

    fn query_serial_name(&self, name_buf: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(copy_name(POINTER_SERIAL_NAME, name_buf))
    }

    fn query_device_ids(&self, ids: &mut InputDeviceIds) -> Result<(), InputBackendError> {
        *ids = InputDeviceIds {
            bustype: BUS_VIRTUAL,
            vendor: LIMINA_VENDOR_ID,
            product: POINTER_PRODUCT_ID,
            version: 1,
        };
        Ok(())
    }

    fn query_event_capabilities(
        &self,
        event_type: u8,
        bitmap_buf: &mut [u8],
    ) -> Result<u8, InputBackendError> {
        Ok(match event_type as u16 {
            EV_KEY => write_bitmap(bitmap_buf, &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE]),
            EV_ABS => write_bitmap(bitmap_buf, &[ABS_X, ABS_Y]),
            EV_REL => write_bitmap(
                bitmap_buf,
                &[REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES],
            ),
            _ => 0,
        })
    }

    fn query_abs_info(
        &self,
        abs_axis: u8,
        abs_info: &mut InputAbsInfo,
    ) -> Result<(), InputBackendError> {
        if abs_axis as u16 == ABS_X || abs_axis as u16 == ABS_Y {
            *abs_info = InputAbsInfo {
                min: 0,
                max: ABS_MAX,
                fuzz: 0,
                flat: 0,
                res: 0,
            };
        }
        Ok(())
    }

    fn query_properties(&self, bitmap: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(write_bitmap(bitmap, &[INPUT_PROP_POINTER]))
    }
}

/// Relative-pointer config: a plain mouse — `REL_X`/`REL_Y` motion + wheels + the three
/// buttons, **no** `ABS`/`INPUT_PROP_POINTER`. A SEPARATE device from the absolute pointer
/// (matching the QEMU virtio-tablet + virtio-mouse split) so adding relative capture never
/// reclassifies the shipped absolute device in the guest's libinput. Used only in pointer
/// *capture* mode (mouselook / guest-warped cursors); the supervisor routes events here while
/// captured and to the absolute device otherwise.
pub struct RelPointerConfig;

impl ObjectNew<()> for RelPointerConfig {
    fn new(_userdata: Option<&()>) -> Self {
        Self
    }
}

impl InputQueryConfig for RelPointerConfig {
    fn query_device_name(&self, name_buf: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(copy_name(REL_POINTER_DEVICE_NAME, name_buf))
    }

    fn query_serial_name(&self, name_buf: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(copy_name(REL_POINTER_SERIAL_NAME, name_buf))
    }

    fn query_device_ids(&self, ids: &mut InputDeviceIds) -> Result<(), InputBackendError> {
        *ids = InputDeviceIds {
            bustype: BUS_VIRTUAL,
            vendor: LIMINA_VENDOR_ID,
            product: REL_POINTER_PRODUCT_ID,
            version: 1,
        };
        Ok(())
    }

    fn query_event_capabilities(
        &self,
        event_type: u8,
        bitmap_buf: &mut [u8],
    ) -> Result<u8, InputBackendError> {
        Ok(match event_type as u16 {
            EV_KEY => write_bitmap(bitmap_buf, &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE]),
            EV_REL => write_bitmap(
                bitmap_buf,
                &[
                    REL_X,
                    REL_Y,
                    REL_WHEEL,
                    REL_HWHEEL,
                    REL_WHEEL_HI_RES,
                    REL_HWHEEL_HI_RES,
                ],
            ),
            _ => 0,
        })
    }

    fn query_abs_info(
        &self,
        _abs_axis: u8,
        _abs_info: &mut InputAbsInfo,
    ) -> Result<(), InputBackendError> {
        Ok(())
    }

    fn query_properties(&self, _bitmap: &mut [u8]) -> Result<u8, InputBackendError> {
        Ok(0) // a plain relative mouse has no INPUT_PROP_*
    }
}

fn copy_name(name: &[u8], buf: &mut [u8]) -> u8 {
    let n = name.len().min(buf.len());
    buf[..n].copy_from_slice(&name[..n]);
    n as u8
}

/// Build the keyboard backend pair. `fd` is the worker's read end of the keyboard event
/// socket (inherited from the supervisor).
pub fn keyboard_backends(
    fd: RawFd,
) -> (
    InputConfigBackend<'static>,
    InputEventProviderBackend<'static>,
) {
    let cfg: &'static FdConfig = Box::leak(Box::new(FdConfig { fd }));
    (
        KeyboardConfig::into_input_config(None),
        FdEvents::into_input_events(Some(cfg)),
    )
}

/// Build the absolute-pointer backend pair. `fd` is the worker's read end of the pointer
/// event socket (inherited from the supervisor).
pub fn pointer_backends(
    fd: RawFd,
) -> (
    InputConfigBackend<'static>,
    InputEventProviderBackend<'static>,
) {
    let cfg: &'static FdConfig = Box::leak(Box::new(FdConfig { fd }));
    (
        PointerConfig::into_input_config(None),
        FdEvents::into_input_events(Some(cfg)),
    )
}

/// Build the relative-pointer (mouse) backend pair for pointer-capture mode. `fd` is the
/// worker's read end of the relative-pointer event socket (inherited from the supervisor).
pub fn rel_pointer_backends(
    fd: RawFd,
) -> (
    InputConfigBackend<'static>,
    InputEventProviderBackend<'static>,
) {
    let cfg: &'static FdConfig = Box::leak(Box::new(FdConfig { fd }));
    (
        RelPointerConfig::into_input_config(None),
        FdEvents::into_input_events(Some(cfg)),
    )
}
