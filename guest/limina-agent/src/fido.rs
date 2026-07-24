// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guest half of the virtual FIDO authenticator (M14, Spike B tier): a `/dev/uhid`
//! HID device with the standard FIDO report descriptor. Applications (browsers,
//! libfido2, pam_u2f) see an ordinary hidraw FIDO key — systemd's fido-id udev rules
//! detect it by usage page (0xF1D0), vendor-neutral — while every CTAP HID report is
//! relayed verbatim over the control plane to the host authenticator and back.
//!
//! uhid is a stable kernel ABI (no module, no kernel patch — CONFIG_UHID is on in
//! Fedora and in our kernels). The device lives as long as the fd: dropping the
//! bridge (agent restart, host disconnect) auto-destroys it, so a dead bridge can
//! never leave a zombie authenticator visible to the guest.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

/// CTAP HID reports are 64 bytes both directions (no report ids).
pub const REPORT_SIZE: usize = 64;

// linux/uhid.h event types (the ones we speak).
const UHID_DESTROY: u32 = 1;
const UHID_START: u32 = 2;
const UHID_STOP: u32 = 3;
const UHID_OPEN: u32 = 4;
const UHID_CLOSE: u32 = 5;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;

// struct uhid_event layout (packed): u32 type + the largest union member
// (uhid_create2_req = 4372 bytes) = 4376. All offsets below are into the
// serialized event buffer.
const EVENT_SIZE: usize = 4376;
const CREATE2_NAME: usize = 4; // u8[128]
const CREATE2_PHYS: usize = 132; // u8[64]
const CREATE2_UNIQ: usize = 196; // u8[64]
const CREATE2_RD_SIZE: usize = 260; // u16
const CREATE2_BUS: usize = 262; // u16
const CREATE2_VENDOR: usize = 264; // u32
const CREATE2_PRODUCT: usize = 268; // u32
const CREATE2_VERSION: usize = 272; // u32
const CREATE2_RD_DATA: usize = 280; // u8[4096]
const OUTPUT_DATA: usize = 4; // u8[4096]
const OUTPUT_SIZE: usize = 4100; // u16
const INPUT2_SIZE: usize = 4; // u16
const INPUT2_DATA: usize = 6; // u8[4096]
const GET_REPORT_ID: usize = 4; // u32 (request id we must echo in the reply)

const BUS_USB: u16 = 0x03;

/// The standard FIDO/CTAP HID report descriptor: usage page 0xF1D0, usage CTAPHID,
/// one 64-byte IN report + one 64-byte OUT report.
const FIDO_REPORT_DESCRIPTOR: &[u8] = &[
    0x06, 0xD0, 0xF1, // Usage Page (FIDO alliance)
    0x09, 0x01, // Usage (CTAPHID)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x20, //   Usage (Input report data)
    0x15, 0x00, //   Logical minimum (0)
    0x26, 0xFF, 0x00, //   Logical maximum (255)
    0x75, 0x08, //   Report size (8)
    0x95, 0x40, //   Report count (64)
    0x81, 0x02, //   Input (Data, Var, Abs)
    0x09, 0x21, //   Usage (Output report data)
    0x15, 0x00, //   Logical minimum (0)
    0x26, 0xFF, 0x00, //   Logical maximum (255)
    0x75, 0x08, //   Report size (8)
    0x95, 0x40, //   Report count (64)
    0x91, 0x02, //   Output (Data, Var, Abs)
    0xC0, // End collection
];

/// The uhid-backed virtual FIDO device. Created when the host advertises the `fido`
/// capability; dropped (device destroyed) with the connection.
pub struct FidoBridge {
    uhid: File,
}

impl FidoBridge {
    /// Create the uhid device. Errors are non-fatal to the agent (no /dev/uhid on
    /// ancient kernels, or uhid off) — the caller logs and runs without FIDO.
    pub fn create() -> std::io::Result<FidoBridge> {
        let mut uhid = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uhid")?;

        let mut ev = [0u8; EVENT_SIZE];
        ev[..4].copy_from_slice(&UHID_CREATE2.to_le_bytes());
        let name = b"limina Touch ID FIDO";
        ev[CREATE2_NAME..CREATE2_NAME + name.len()].copy_from_slice(name);
        let phys = b"limina/vsock";
        ev[CREATE2_PHYS..CREATE2_PHYS + phys.len()].copy_from_slice(phys);
        let uniq = b"limina-fido-0";
        ev[CREATE2_UNIQ..CREATE2_UNIQ + uniq.len()].copy_from_slice(uniq);
        let rd = FIDO_REPORT_DESCRIPTOR;
        ev[CREATE2_RD_SIZE..CREATE2_RD_SIZE + 2].copy_from_slice(&(rd.len() as u16).to_le_bytes());
        ev[CREATE2_BUS..CREATE2_BUS + 2].copy_from_slice(&BUS_USB.to_le_bytes());
        // Vendor-neutral identity: FIDO detection is by HID usage page, not VID/PID.
        ev[CREATE2_VENDOR..CREATE2_VENDOR + 4].copy_from_slice(&0x1d6b_u32.to_le_bytes());
        ev[CREATE2_PRODUCT..CREATE2_PRODUCT + 4].copy_from_slice(&0x0f1d_u32.to_le_bytes());
        ev[CREATE2_VERSION..CREATE2_VERSION + 4].copy_from_slice(&1_u32.to_le_bytes());
        ev[CREATE2_RD_DATA..CREATE2_RD_DATA + rd.len()].copy_from_slice(rd);
        uhid.write_all(&ev)?;
        Ok(FidoBridge { uhid })
    }

    pub fn fd(&self) -> i32 {
        self.uhid.as_raw_fd()
    }

    /// Drain one uhid event. Returns a 64-byte CTAP report if the event was an
    /// application OUTPUT report (the only event that crosses to the host); other
    /// events (START/STOP/OPEN/CLOSE, GET/SET_REPORT queries) are handled here.
    pub fn read_event(&mut self) -> std::io::Result<Option<[u8; REPORT_SIZE]>> {
        let mut ev = [0u8; EVENT_SIZE];
        let n = self.uhid.read(&mut ev)?;
        if n < 4 {
            return Ok(None);
        }
        let ev_type = u32::from_le_bytes([ev[0], ev[1], ev[2], ev[3]]);
        match ev_type {
            UHID_OUTPUT => {
                let size = u16::from_le_bytes([ev[OUTPUT_SIZE], ev[OUTPUT_SIZE + 1]]) as usize;
                let data = &ev[OUTPUT_DATA..OUTPUT_DATA + size.min(4096)];
                // hidraw requires writers to prepend a report number; for unnumbered
                // devices the kernel usually strips the leading 0, but be tolerant of
                // both shapes (65 bytes led by 0x00, or the bare 64).
                let payload = if data.len() == REPORT_SIZE + 1 && data[0] == 0 {
                    &data[1..]
                } else {
                    data
                };
                let mut report = [0u8; REPORT_SIZE];
                let take = payload.len().min(REPORT_SIZE);
                report[..take].copy_from_slice(&payload[..take]);
                Ok(Some(report))
            }
            // FIDO consumers never use GET/SET_REPORT (CTAP rides interrupt-style
            // reports), but the kernel can forward hidraw ioctls: answer "no" rather
            // than leaving the caller to time out.
            UHID_GET_REPORT => {
                let mut reply = [0u8; 16];
                reply[..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_le_bytes());
                reply[4..8].copy_from_slice(&ev[GET_REPORT_ID..GET_REPORT_ID + 4]);
                reply[8..10].copy_from_slice(&(libc::EIO as u16).to_le_bytes());
                self.uhid.write_all(&reply)?;
                Ok(None)
            }
            UHID_SET_REPORT => {
                let mut reply = [0u8; 12];
                reply[..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_le_bytes());
                reply[4..8].copy_from_slice(&ev[GET_REPORT_ID..GET_REPORT_ID + 4]);
                reply[8..10].copy_from_slice(&(libc::EIO as u16).to_le_bytes());
                self.uhid.write_all(&reply)?;
                Ok(None)
            }
            UHID_START | UHID_STOP | UHID_OPEN | UHID_CLOSE => Ok(None),
            _ => Ok(None),
        }
    }

    /// Deliver a host authenticator report to the guest as an INPUT report.
    pub fn deliver(&mut self, report: &[u8]) -> std::io::Result<()> {
        let mut ev = [0u8; INPUT2_DATA + REPORT_SIZE];
        ev[..4].copy_from_slice(&UHID_INPUT2.to_le_bytes());
        ev[INPUT2_SIZE..INPUT2_SIZE + 2].copy_from_slice(&(REPORT_SIZE as u16).to_le_bytes());
        let take = report.len().min(REPORT_SIZE);
        ev[INPUT2_DATA..INPUT2_DATA + take].copy_from_slice(&report[..take]);
        self.uhid.write_all(&ev)?;
        Ok(())
    }
}

impl Drop for FidoBridge {
    fn drop(&mut self) {
        // Explicit DESTROY is polite; closing the fd would also tear the device down.
        let mut ev = [0u8; 4];
        ev.copy_from_slice(&UHID_DESTROY.to_le_bytes());
        let _ = self.uhid.write_all(&ev);
    }
}
