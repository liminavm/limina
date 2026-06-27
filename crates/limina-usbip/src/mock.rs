// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A hardware-free **CDC-ACM** mock device. It enumerates in the guest as `/dev/ttyACM0` (a
//! visible, functional artifact) using only upstream usbcore + cdc-acm — no custom guest driver.
//!
//! This is what makes the whole USB/IP pipeline testable with no physical USB device: the
//! [`MockBackend`] answers GET_DESCRIPTOR / SET_CONFIGURATION / the CDC class requests from the
//! canned descriptors below, and loops back bulk data (what you write to the guest's `/dev/ttyACM0`
//! comes back as a read) so a test can prove data actually flows end-to-end.

use crate::backend::{DeviceInfo, Dir, UsbBackend, UsbDevice};
use crate::proto;
use std::io;
use std::sync::{Arc, Mutex};

const VID: u16 = 0x1209; // pid.codes open VID — the mock is clearly synthetic
const PID: u16 = 0xACA0;
const BUSID: &str = "1-1";

// USB class codes.
const CLASS_CDC: u8 = 0x02;
const CLASS_CDC_DATA: u8 = 0x0a;
const SUBCLASS_ACM: u8 = 0x02;

// Standard requests (setup[1] when setup[0] is a standard request).
const GET_DESCRIPTOR: u8 = 0x06;
const SET_CONFIGURATION: u8 = 0x09;
const SET_INTERFACE: u8 = 0x0b;

// Descriptor types (high byte of wValue in GET_DESCRIPTOR).
const DT_DEVICE: u8 = 0x01;
const DT_CONFIG: u8 = 0x02;
const DT_STRING: u8 = 0x03;

/// The 18-byte device descriptor for the mock CDC-ACM device.
fn device_descriptor() -> Vec<u8> {
    vec![
        0x12, // bLength = 18
        DT_DEVICE,
        0x00,
        0x02,      // bcdUSB 2.00
        CLASS_CDC, // bDeviceClass = CDC
        0x00,      // bDeviceSubClass
        0x00,      // bDeviceProtocol
        0x40,      // bMaxPacketSize0 = 64
        (VID & 0xff) as u8,
        (VID >> 8) as u8, // idVendor (LE)
        (PID & 0xff) as u8,
        (PID >> 8) as u8, // idProduct (LE)
        0x00,
        0x01, // bcdDevice 1.00
        0x01, // iManufacturer
        0x02, // iProduct
        0x03, // iSerialNumber
        0x01, // bNumConfigurations
    ]
}

/// The full configuration block (config + 2 interfaces + CDC functional descriptors + 3 endpoints).
fn config_descriptor() -> Vec<u8> {
    let mut c = Vec::new();
    // --- configuration descriptor (9) ---
    let total_len: u16 = 67;
    c.extend_from_slice(&[
        0x09,
        DT_CONFIG,
        (total_len & 0xff) as u8,
        (total_len >> 8) as u8, // wTotalLength
        0x02,                   // bNumInterfaces
        0x01,                   // bConfigurationValue
        0x00,                   // iConfiguration
        0xc0,                   // bmAttributes (self-powered)
        0x00,                   // bMaxPower
    ]);
    // --- interface 0: CDC communication (1 interrupt EP) ---
    c.extend_from_slice(&[
        0x09,
        0x04,
        0x00,
        0x00,
        0x01,
        CLASS_CDC,
        SUBCLASS_ACM,
        0x01,
        0x00,
    ]);
    // CDC header functional descriptor
    c.extend_from_slice(&[0x05, 0x24, 0x00, 0x10, 0x01]);
    // CDC ACM functional descriptor (bmCapabilities=0x02 -> supports SET/GET_LINE_CODING)
    c.extend_from_slice(&[0x04, 0x24, 0x02, 0x02]);
    // CDC union functional descriptor (control if 0, data if 1)
    c.extend_from_slice(&[0x05, 0x24, 0x06, 0x00, 0x01]);
    // CDC call-management functional descriptor
    c.extend_from_slice(&[0x05, 0x24, 0x01, 0x00, 0x01]);
    // interrupt IN endpoint 0x81
    c.extend_from_slice(&[0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0xff]);
    // --- interface 1: CDC data (bulk IN + bulk OUT) ---
    c.extend_from_slice(&[
        0x09,
        0x04,
        0x01,
        0x00,
        0x02,
        CLASS_CDC_DATA,
        0x00,
        0x00,
        0x00,
    ]);
    // bulk OUT endpoint 0x02
    c.extend_from_slice(&[0x07, 0x05, 0x02, 0x02, 0x40, 0x00, 0x00]);
    // bulk IN endpoint 0x83
    c.extend_from_slice(&[0x07, 0x05, 0x83, 0x02, 0x40, 0x00, 0x00]);
    debug_assert_eq!(c.len(), total_len as usize);
    c
}

/// A USB string descriptor (UTF-16LE) for `s`; index 0 is the LANGID list (English-US).
fn string_descriptor(index: u8, s: &str) -> Vec<u8> {
    if index == 0 {
        return vec![0x04, DT_STRING, 0x09, 0x04]; // LANGID 0x0409
    }
    let utf16: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut d = vec![(2 + utf16.len()) as u8, DT_STRING];
    d.extend_from_slice(&utf16);
    d
}

/// The device summary advertised in DEVLIST/IMPORT.
fn summary() -> proto::UsbDevice {
    proto::UsbDevice {
        path: "/sys/devices/limina/usb1/1-1".into(),
        busid: BUSID.into(),
        busnum: 1,
        devnum: 2,
        speed: 2, // full speed
        id_vendor: VID,
        id_product: PID,
        bcd_device: 0x0100,
        b_device_class: CLASS_CDC,
        b_device_subclass: 0,
        b_device_protocol: 0,
        b_configuration_value: 1,
        b_num_configurations: 1,
        interfaces: vec![
            proto::UsbInterface {
                class: CLASS_CDC,
                subclass: SUBCLASS_ACM,
                protocol: 0x01,
            },
            proto::UsbInterface {
                class: CLASS_CDC_DATA,
                subclass: 0,
                protocol: 0,
            },
        ],
    }
}

/// The opened mock device. Holds a small loopback FIFO: bytes written to the bulk-OUT endpoint
/// come back on the next bulk-IN read, so a test can prove data round-trips through the guest tty.
pub struct MockDevice {
    loopback: Arc<Mutex<Vec<u8>>>,
}

impl UsbDevice for MockDevice {
    fn control(&mut self, setup: [u8; 8], _data: &[u8], length: u16) -> io::Result<Vec<u8>> {
        let bm_request_type = setup[0];
        let b_request = setup[1];
        let w_value = u16::from_le_bytes([setup[2], setup[3]]);
        let is_standard = (bm_request_type & 0x60) == 0x00;
        let is_class = (bm_request_type & 0x60) == 0x20;

        if is_standard && b_request == GET_DESCRIPTOR {
            let dt = (w_value >> 8) as u8;
            let idx = (w_value & 0xff) as u8;
            let full = match dt {
                DT_DEVICE => device_descriptor(),
                DT_CONFIG => config_descriptor(),
                DT_STRING => string_descriptor(idx, mock_string(idx)),
                _ => return Err(io::Error::from_raw_os_error(libc_epipe())),
            };
            let n = (length as usize).min(full.len());
            return Ok(full[..n].to_vec());
        }
        if is_standard && (b_request == SET_CONFIGURATION || b_request == SET_INTERFACE) {
            return Ok(Vec::new()); // ACK, no data stage
        }
        if is_class {
            // CDC SET/GET_LINE_CODING (0x20/0x21), SET_CONTROL_LINE_STATE (0x22): accept; for the
            // GET (IN) hand back a plausible 7-byte line coding (115200 8N1).
            if bm_request_type & 0x80 != 0 {
                return Ok(vec![0x00, 0xc2, 0x01, 0x00, 0x00, 0x00, 0x08]);
            }
            return Ok(Vec::new());
        }
        // Unhandled standard request: stall (EPIPE) rather than hang.
        Err(io::Error::from_raw_os_error(libc_epipe()))
    }

    fn transfer(&mut self, ep: u8, dir: Dir, data: &[u8], length: u32) -> io::Result<Vec<u8>> {
        match (ep, dir) {
            (2, Dir::Out) => {
                // bulk OUT: stash for loopback.
                self.loopback.lock().unwrap().extend_from_slice(data);
                Ok(Vec::new())
            }
            (3, Dir::In) => {
                // bulk IN: drain up to `length` from the loopback FIFO.
                let mut q = self.loopback.lock().unwrap();
                let n = (length as usize).min(q.len());
                Ok(q.drain(..n).collect())
            }
            (1, Dir::In) => Ok(Vec::new()), // interrupt IN (CDC notifications): nothing pending
            _ => Err(io::Error::from_raw_os_error(libc_epipe())),
        }
    }
}

fn mock_string(idx: u8) -> &'static str {
    match idx {
        1 => "limina",
        2 => "Mock CDC-ACM",
        3 => "LIMINA-MOCK-0001",
        _ => "",
    }
}

fn libc_epipe() -> i32 {
    32 // EPIPE — the conventional "endpoint stalled" errno
}

/// A backend exporting exactly one mock CDC-ACM device.
#[derive(Default)]
pub struct MockBackend;

impl MockBackend {
    pub fn new() -> Self {
        MockBackend
    }
    /// The busid of the single mock device (for tests/CLI: `usbip attach -b <this>`).
    pub fn busid() -> &'static str {
        BUSID
    }
}

impl UsbBackend for MockBackend {
    fn list(&self) -> io::Result<Vec<DeviceInfo>> {
        Ok(vec![DeviceInfo { summary: summary() }])
    }

    fn import(&self, busid: &str) -> io::Result<Option<(proto::UsbDevice, Box<dyn UsbDevice>)>> {
        if busid != BUSID {
            return Ok(None);
        }
        let dev = MockDevice {
            loopback: Arc::new(Mutex::new(Vec::new())),
        };
        Ok(Some((summary(), Box::new(dev))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_descriptor_is_well_formed() {
        let d = device_descriptor();
        assert_eq!(d.len(), 18);
        assert_eq!(d[0], 18); // bLength
        assert_eq!(d[1], DT_DEVICE);
        assert_eq!(u16::from_le_bytes([d[8], d[9]]), VID);
        assert_eq!(d[17], 1); // bNumConfigurations
    }

    #[test]
    fn config_block_total_length_matches_header() {
        let c = config_descriptor();
        let wtotal = u16::from_le_bytes([c[2], c[3]]) as usize;
        assert_eq!(wtotal, c.len());
        assert_eq!(c[4], 2); // bNumInterfaces
    }

    #[test]
    fn get_descriptor_respects_wlength() {
        let mut dev = MockDevice {
            loopback: Arc::new(Mutex::new(Vec::new())),
        };
        // GET_DESCRIPTOR(device), wLength = 8 -> truncated to 8 bytes (the enumerate probe).
        let setup = [
            0x80,
            GET_DESCRIPTOR,
            0x00,
            DT_DEVICE,
            0x00,
            0x00,
            0x08,
            0x00,
        ];
        let r = dev.control(setup, &[], 8).unwrap();
        assert_eq!(r.len(), 8);
        assert_eq!(r[0], 18); // bLength still reports the true length
    }

    #[test]
    fn bulk_loopback_round_trips() {
        let mut dev = MockDevice {
            loopback: Arc::new(Mutex::new(Vec::new())),
        };
        let sent = b"hello tty";
        dev.transfer(2, Dir::Out, sent, sent.len() as u32).unwrap();
        let got = dev.transfer(3, Dir::In, &[], 64).unwrap();
        assert_eq!(&got, sent);
    }

    #[test]
    fn import_unknown_busid_is_none() {
        let b = MockBackend::new();
        assert!(b.import("9-9").unwrap().is_none());
        assert!(b.import(BUSID).unwrap().is_some());
    }
}
