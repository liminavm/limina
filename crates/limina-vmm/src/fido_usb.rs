// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Worker-side policy for the stock-tier FIDO USB gadget (M14 Stage C).
//!
//! The CTAP2 authenticator + Secure-Enclave signing + passkey store all live in the
//! **supervisor** (`crates/limina`) — it is the Apple-Development-signed binary the SEP
//! path works from, and it owns the per-VM `fido-credentials.json`. The worker only carries
//! the emulated USB bus, so here we present a plain HID **report pipe** gadget (libkrun
//! mechanism, [`devices::usb::HidReportPipe`]) with the FIDO identity/report descriptor and
//! shuttle its 64-byte CTAPHID frames verbatim to the supervisor over a UNIX socket. This
//! mirrors the agent/uhid transport exactly (which shuttles the same frames over guest
//! vsock) — one authenticator, one store, one keepalive engine, two transports.
//!
//! Wire protocol: raw fixed-size (64-byte) CTAPHID report frames, both directions, no
//! header. The worker **binds** the listener (path from `--fido-socket`, stable across a
//! reboot relaunch — the balloon/display-control-socket precedent) and the supervisor
//! connects (and reconnects across relaunches). Guest→host reports (interrupt-OUT /
//! SET_REPORT) go out the socket; host→guest reports (INIT/CBOR responses, KEEPALIVE) come
//! back in and complete a held interrupt-IN.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use devices::usb::{HidReportPipe, ReportSink, UsbDeviceModel};

/// Vendor-neutral FIDO identity, byte-identical to the uhid device so fido-id/udev/browsers
/// need nothing to discover it (`0x1d6b` = "Linux Foundation", `0x0f1d` our FIDO product).
const VID: u16 = 0x1d6b;
const PID: u16 = 0x0f1d;

/// CTAPHID reports are always 64 bytes (no report ids).
const REPORT_SIZE: usize = 64;

/// The FIDO HID report descriptor: vendor-defined usage page 0xF1D0 (the FIDO page —
/// systemd's fido-id keys off this) with a 64-byte input and 64-byte output report, no
/// report ids. Identical to the uhid/agent device's descriptor (policy — it *is* the FIDO
/// contract, hence here rather than in the generic libkrun pipe).
#[rustfmt::skip]
fn fido_report_descriptor() -> Vec<u8> {
    vec![
        0x06, 0xd0, 0xf1, // Usage Page (0xF1D0, FIDO)
        0x09, 0x01, //       Usage (0x01)
        0xa1, 0x01, //       Collection (Application)
        0x09, 0x20, //         Usage (0x20, data in)
        0x15, 0x00, //         Logical Minimum (0)
        0x26, 0xff, 0x00, //   Logical Maximum (255)
        0x75, 0x08, //         Report Size (8)
        0x95, REPORT_SIZE as u8, // Report Count (64)
        0x81, 0x02, //         Input (Data, Var, Abs)
        0x09, 0x21, //         Usage (0x21, data out)
        0x15, 0x00, //         Logical Minimum (0)
        0x26, 0xff, 0x00, //   Logical Maximum (255)
        0x75, 0x08, //         Report Size (8)
        0x95, REPORT_SIZE as u8, // Report Count (64)
        0x91, 0x02, //         Output (Data, Var, Abs)
        0xc0, //             End Collection
    ]
}

/// Build the FIDO USB gadget: a HID report pipe wired to `socket_path`, where the supervisor
/// serves CTAPHID. Binds the listener and spawns the pump thread; returns the gadget to
/// cold-plug onto the emulated controller (`vmr.usb_devices`).
pub fn build(socket_path: &Path) -> Result<Arc<dyn UsbDeviceModel>> {
    // Bind the listener the supervisor connects to (unlink a stale one first, like gvproxy).
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding FIDO USB socket {socket_path:?}"))?;

    // The current supervisor connection's write half, published on accept. The gadget's
    // out-sink writes guest→host frames here; None before the supervisor connects (a stray
    // early frame is dropped — the client re-sends CTAPHID_INIT).
    let conn: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));

    let sink_conn = conn.clone();
    let out_sink: ReportSink = Arc::new(move |mut frame: Vec<u8>| {
        frame.resize(REPORT_SIZE, 0);
        let mut guard = sink_conn.lock().unwrap();
        if let Some(stream) = guard.as_mut() {
            if stream.write_all(&frame[..REPORT_SIZE]).is_err() {
                *guard = None; // supervisor gone; wait for a reconnect
            }
        }
    });

    let pipe = HidReportPipe::new(
        VID,
        PID,
        fido_report_descriptor(),
        REPORT_SIZE,
        [
            "limina",
            "limina FIDO Authenticator",
            "LIMINA-FIDO-USB",
            "FIDO",
        ],
        out_sink,
    );

    let reader_pipe = pipe.clone();
    let reader_conn = conn.clone();
    std::thread::Builder::new()
        .name("limina-fido-usb".into())
        .spawn(move || pump(listener, reader_pipe, reader_conn))
        .context("spawning the FIDO USB pump thread")?;

    Ok(pipe)
}

/// Accept the supervisor's connection(s) and pump host→guest frames into the gadget. One
/// connection at a time; on EOF (supervisor relaunch/exit) loop back to accept the next.
fn pump(listener: UnixListener, pipe: Arc<HidReportPipe>, conn: Arc<Mutex<Option<UnixStream>>>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                log::warn!("fido-usb: accept failed: {e}");
                continue;
            }
        };
        // A fresh supervisor connection means a fresh authenticator (new channel ids);
        // clear any stale held IN / queued frames so the transports don't cross.
        pipe.reset();
        match stream.try_clone() {
            Ok(w) => *conn.lock().unwrap() = Some(w),
            Err(e) => {
                log::warn!("fido-usb: cloning the connection failed: {e}");
                continue;
            }
        }
        log::info!("fido-usb: supervisor connected");
        let mut reader = stream;
        let mut buf = [0u8; REPORT_SIZE];
        // Read host→guest frames until EOF/error (supervisor relaunch or exit).
        while reader.read_exact(&mut buf).is_ok() {
            pipe.push_in(buf.to_vec());
        }
        *conn.lock().unwrap() = None;
        pipe.reset();
        log::info!("fido-usb: supervisor disconnected");
    }
}
