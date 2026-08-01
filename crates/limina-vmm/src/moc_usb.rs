// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Worker-side identity for the impersonated MOC fingerprint reader (M14 wave 3).
//!
//! The elanmoc protocol state machine + per-VM template store + Touch ID verify all live in the
//! **supervisor** (`crates/limina/src/moc/`) — it is the Apple-Development-signed binary the
//! Secure-Enclave / `LAContext` path works from, and it owns the per-VM `moc-templates.json`. The
//! worker only carries the emulated USB bus, so here we present the **elanmoc USB identity** (a
//! generic libkrun [`BulkPipe`] with the byte-exact `04f3:0c7d` descriptors — 8 bulk endpoints, a
//! vendor-class interface) and shuttle raw bulk packets to the supervisor over a UNIX socket. This
//! mirrors the FIDO Stage-C proxy exactly (`fido_usb.rs`): mechanism (USB bus) in the worker,
//! policy (protocol + enclave) in the supervisor.
//!
//! **Wire protocol** (both directions): length-delimited, endpoint-tagged frames
//! `[ep: u8][kind: u8][len: u16 LE][payload]`. Guest→host: each bulk-OUT packet on EP `0x01` goes
//! out as `{ep: 0x01, kind: DATA}`. Host→guest: the supervisor sends `{ep: 0x83|0x84, kind: DATA}`
//! for a reply (completing that endpoint's held read) or `{ep, kind: STALL}` to fail it (an enroll
//! decline / protocol error). The worker **binds** the listener (`--moc-socket`, stable across a
//! reboot relaunch) and the supervisor connects (and reconnects across relaunches).

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use devices::usb::{
    BulkCancelSink, BulkPipe, BulkSink, DeviceDescriptors, UsbDeviceModel, UsbSpeed,
};

/// Elan match-on-chip identity libfprint's `elanmoc` binds (VID/PID matched exactly). `0x0c7d`
/// takes the default 9 enroll stages and is NOT in fwupd's `elanfp` quirk today (dodge PID).
const VID: u16 = 0x04f3;
const PID: u16 = 0x0c7d;

/// Frame kinds on the worker↔supervisor socket.
const KIND_DATA: u8 = 0;
const KIND_STALL: u8 = 1;
/// Host-ward only: the guest cancelled `ep`'s outstanding read (xHCI Stop Endpoint). No payload.
const KIND_CANCEL: u8 = 2;

/// Build the fingerprint reader gadget: a [`BulkPipe`] with the elanmoc descriptors wired to
/// `socket_path`, where the supervisor runs the protocol. Binds the listener and spawns the pump
/// thread; returns the gadget to cold-plug onto the controller (`vmr.usb_devices`).
pub fn build(socket_path: &Path) -> Result<Arc<dyn UsbDeviceModel>> {
    // Bind the listener the supervisor connects to (unlink a stale one first, like the FIDO gadget).
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding fingerprint USB socket {socket_path:?}"))?;

    // The current supervisor connection's write half, published on accept. The gadget's out-sink
    // writes guest→host command frames here; None before the supervisor connects (a stray early
    // frame is dropped — the guest's driver retries at open).
    let conn: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));

    let sink_conn = conn.clone();
    let out_sink: BulkSink = Arc::new(move |ep: u8, payload: Vec<u8>| {
        let mut guard = sink_conn.lock().unwrap();
        if let Some(stream) = guard.as_mut() {
            if write_frame(stream, ep, KIND_DATA, &payload).is_err() {
                *guard = None; // supervisor gone; wait for a reconnect
            }
        }
    });

    // The guest cancelling a finger-wait read (fprintd's verify being stopped because the user
    // dismissed the guest's own auth dialog) leaves nothing on the wire — the xHCI Stop Endpoint
    // is the only evidence. Forward it so the supervisor can take the Touch ID sheet down.
    let cancel_conn = conn.clone();
    let cancel_sink: BulkCancelSink = Arc::new(move |ep: u8| {
        let mut guard = cancel_conn.lock().unwrap();
        if let Some(stream) = guard.as_mut() {
            if write_frame(stream, ep, KIND_CANCEL, &[]).is_err() {
                *guard = None;
            }
        }
    });

    let pipe = BulkPipe::with_cancel_sink(
        elanmoc_descriptors(),
        UsbSpeed::Full,
        out_sink,
        Some(cancel_sink),
    );

    let reader_pipe = pipe.clone();
    let reader_conn = conn.clone();
    std::thread::Builder::new()
        .name("limina-moc-usb".into())
        .spawn(move || pump(listener, reader_pipe, reader_conn))
        .context("spawning the fingerprint USB pump thread")?;

    Ok(pipe)
}

/// Accept the supervisor's connection(s) and pump host→guest frames into the gadget. One connection
/// at a time; on EOF (supervisor relaunch/exit) loop back to accept the next.
fn pump(listener: UnixListener, pipe: Arc<BulkPipe>, conn: Arc<Mutex<Option<UnixStream>>>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                log::warn!("moc-usb: accept failed: {e}");
                continue;
            }
        };
        // A fresh supervisor connection means a fresh protocol session; clear any stale held read
        // / queued events so the transports don't cross.
        pipe.reset();
        match stream.try_clone() {
            Ok(w) => *conn.lock().unwrap() = Some(w),
            Err(e) => {
                log::warn!("moc-usb: cloning the connection failed: {e}");
                continue;
            }
        }
        log::info!("moc-usb: supervisor connected");
        let mut reader = stream;
        // Read host→guest frames until EOF/error (supervisor relaunch or exit).
        while let Ok((ep, kind, payload)) = read_frame(&mut reader) {
            match kind {
                KIND_DATA => pipe.push_in(ep, payload),
                KIND_STALL => pipe.stall_in(ep),
                other => log::warn!("moc-usb: unknown frame kind {other}"),
            }
        }
        *conn.lock().unwrap() = None;
        pipe.reset();
        log::info!("moc-usb: supervisor disconnected");
    }
}

/// Write one `[ep][kind][len LE][payload]` frame.
fn write_frame(w: &mut impl Write, ep: u8, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u16;
    let mut hdr = [ep, kind, 0, 0];
    hdr[2..4].copy_from_slice(&len.to_le_bytes());
    w.write_all(&hdr)?;
    w.write_all(payload)
}

/// Read one `[ep][kind][len LE][payload]` frame.
fn read_frame(r: &mut impl Read) -> std::io::Result<(u8, u8, Vec<u8>)> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr)?;
    let len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((hdr[0], hdr[1], payload))
}

/// The byte-exact elanmoc descriptor set for `04f3:0c7d`, taken from libfprint's umockdev recording
/// (`tests/elanmoc/device`, a real `0c88` unit) with only `idProduct` changed. Full speed (required
/// for `bMaxPacketSize0 = 8`), vendor-class (0xFF) interface with 8 bulk endpoints.
fn elanmoc_descriptors() -> DeviceDescriptors {
    DeviceDescriptors {
        device: device_descriptor(),
        configs: vec![config_descriptor()],
        strings: vec![
            vec![0x04, 0x03, 0x09, 0x04], // index 0: LANGID 0x0409 (US English)
            string_descriptor("Elan Microelectronics"), // iManufacturer (1)
            string_descriptor("ELAN:Fingerprint"), // iProduct (2)
        ],
    }
}

/// The 18-byte device descriptor. `bMaxPacketSize0 = 8`, `bcdDevice 0x8004`, class defined at the
/// interface, `iSerialNumber = 0` (no serial string — keeps the fprintd device_id stable).
#[rustfmt::skip]
fn device_descriptor() -> Vec<u8> {
    vec![
        0x12,       // bLength = 18
        0x01,       // bDescriptorType = DEVICE
        0x00, 0x02, // bcdUSB 2.00
        0x00,       // bDeviceClass (per-interface)
        0x00,       // bDeviceSubClass
        0x00,       // bDeviceProtocol
        0x08,       // bMaxPacketSize0 = 8 (matches the real unit; legal only at full speed)
        (VID & 0xff) as u8, (VID >> 8) as u8,
        (PID & 0xff) as u8, (PID >> 8) as u8,
        0x04, 0x80, // bcdDevice 0x8004
        0x01,       // iManufacturer
        0x02,       // iProduct
        0x00,       // iSerialNumber (none)
        0x01,       // bNumConfigurations
    ]
}

/// config + vendor-class interface + HID class descriptor + 8 bulk endpoints.
/// wTotalLength = 9 + 9 + 9 + 8*7 = 83 (0x0053).
#[rustfmt::skip]
fn config_descriptor() -> Vec<u8> {
    let mut c = vec![
        // Configuration descriptor.
        0x09, 0x02,
        0x53, 0x00, // wTotalLength = 83
        0x01,       // bNumInterfaces
        0x01,       // bConfigurationValue
        0x00,       // iConfiguration
        0xa0,       // bmAttributes (bus-powered + remote-wakeup)
        0x32,       // bMaxPower = 100 mA
        // Interface 0: vendor-specific (0xFF), 8 endpoints.
        0x09, 0x04,
        0x00,       // bInterfaceNumber
        0x00,       // bAlternateSetting
        0x08,       // bNumEndpoints
        0xff,       // bInterfaceClass = vendor-specific
        0x00,       // bInterfaceSubClass
        0x00,       // bInterfaceProtocol
        0x00,       // iInterface
        // HID class descriptor riding on the vendor interface (present on the real unit; no HID
        // driver binds a 0xFF interface, so the report descriptor is never requested).
        0x09, 0x21,
        0x10, 0x01, // bcdHID 1.10
        0x00,       // bCountryCode
        0x01,       // bNumDescriptors
        0x22,       // bDescriptorType (report)
        0x15, 0x00, // wDescriptorLength = 21
    ];
    // 8 bulk endpoints (IN/OUT interleaved as on the real unit), all wMaxPacketSize 64, bInterval 1.
    // Serviced: OUT 0x01 (commands), IN 0x83 (immediate replies), IN 0x84 (finger-wait replies).
    for addr in [0x81u8, 0x01, 0x82, 0x02, 0x83, 0x03, 0x84, 0x04] {
        c.extend_from_slice(&[0x07, 0x05, addr, 0x02, 0x40, 0x00, 0x01]);
    }
    debug_assert_eq!(c.len(), 83);
    c
}

/// A UTF-16LE string descriptor.
fn string_descriptor(s: &str) -> Vec<u8> {
    let utf16: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut d = vec![(2 + utf16.len()) as u8, 0x03];
    d.extend_from_slice(&utf16);
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_are_the_byte_exact_elanmoc_identity() {
        let d = elanmoc_descriptors();
        // Device: VID/PID, EP0 max packet 8.
        assert_eq!(d.device.len(), 18);
        assert_eq!(d.device[7], 0x08, "bMaxPacketSize0 = 8");
        assert_eq!(u16::from_le_bytes([d.device[8], d.device[9]]), VID);
        assert_eq!(u16::from_le_bytes([d.device[10], d.device[11]]), PID);
        assert_eq!(d.device[16], 0x00, "iSerialNumber = 0 (stable device_id)");
        // Config: 83-byte total, vendor class, 8 endpoints.
        let c = &d.configs[0];
        assert_eq!(u16::from_le_bytes([c[2], c[3]]) as usize, c.len());
        assert_eq!(c.len(), 83);
        assert_eq!(c[9 + 4], 0x08, "8 endpoints");
        assert_eq!(c[9 + 5], 0xff, "vendor-specific interface class");
    }

    #[test]
    fn frame_roundtrips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x84, KIND_DATA, &[0x40, 0x00]).unwrap();
        write_frame(&mut buf, 0x83, KIND_STALL, &[]).unwrap();
        let mut r = &buf[..];
        assert_eq!(
            read_frame(&mut r).unwrap(),
            (0x84, KIND_DATA, vec![0x40, 0x00])
        );
        assert_eq!(read_frame(&mut r).unwrap(), (0x83, KIND_STALL, vec![]));
    }
}
