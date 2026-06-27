// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host USB backend abstraction.
//!
//! The USB/IP server is written against [`UsbBackend`] / [`UsbDevice`] so the protocol + server
//! loop are fully testable with **no physical device**: [`crate::mock::MockBackend`] answers
//! enumeration and control/bulk/interrupt transfers from canned data, while
//! [`crate::libusb::LibusbBackend`] (feature `libusb`) drives real hardware via rusb. Both look
//! identical to `server.rs`.

use crate::proto;
use std::io;

/// Transfer direction for a data-stage transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

/// What a single device exposes for enumeration: its USB/IP summary plus the raw descriptor
/// blob the guest will fetch with GET_DESCRIPTOR(config). The backend owns descriptor bytes so
/// the mock and libusb paths produce identical wire behaviour.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// The USB/IP `usbip_usb_device` summary (busid, ids, class, interface list, …).
    pub summary: proto::UsbDevice,
}

/// An opened, claimed device that can service URBs. One per imported device.
pub trait UsbDevice: Send {
    /// Service a **control** transfer (endpoint 0). `setup` is the raw 8-byte USB setup packet
    /// (little-endian). For IN, return the data read (≤ `length`); for OUT, `data` carries the
    /// payload and the return is the byte count accepted. Errors map to a negative errno on the
    /// wire.
    fn control(&mut self, setup: [u8; 8], data: &[u8], length: u16) -> io::Result<Vec<u8>>;

    /// Service a **bulk/interrupt** transfer on `ep` (endpoint number, 1..=15). For IN, return up
    /// to `length` bytes; for OUT, submit `data` and return how many bytes went.
    fn transfer(&mut self, ep: u8, dir: Dir, data: &[u8], length: u32) -> io::Result<Vec<u8>>;
}

/// A source of exportable USB devices.
pub trait UsbBackend: Send {
    /// Enumerate the devices this backend can export (for OP_REQ_DEVLIST).
    fn list(&self) -> io::Result<Vec<DeviceInfo>>;

    /// Open + claim the device named by `busid` (for OP_REQ_IMPORT). Returns the opened device
    /// and its summary; `Ok(None)` if no such device.
    fn import(&self, busid: &str) -> io::Result<Option<(proto::UsbDevice, Box<dyn UsbDevice>)>>;
}
