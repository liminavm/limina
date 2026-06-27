// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The transport-agnostic USB/IP server loop. Given any `Read + Write` stream (a TCP socket for
//! the Option-C prototype, a vsock stream for the shipping path) and a [`UsbBackend`], it speaks
//! the protocol a stock guest `usbip` client expects:
//!
//!  1. **op_ phase** — answer OP_REQ_DEVLIST, then OP_REQ_IMPORT (claim the device).
//!  2. **URB phase** — after a successful import, loop forever translating CMD_SUBMIT → a backend
//!     transfer → RET_SUBMIT, and CMD_UNLINK → RET_UNLINK.
//!
//! One connection serves one imported device (the stock client model). Transfers run inline on
//! the connection thread; for v1 that's correct and simplest (the guest pipelines by seqnum, and
//! UNLINK of an already-completed URB just returns status 0).

use crate::backend::{Dir, UsbBackend, UsbDevice};
use crate::proto;
use std::io::{self, Read, Write};

/// Serve a single USB/IP client connection to completion (client hangs up or errors).
pub fn serve<S: Read + Write>(mut stream: S, backend: &dyn UsbBackend) -> io::Result<()> {
    // --- op_ phase: at least one request precedes any URB traffic. ---
    let mut device: Option<Box<dyn UsbDevice>> = None;
    while device.is_none() {
        let mut hdr = [0u8; proto::OP_HEADER_LEN];
        if read_full_or_eof(&mut stream, &mut hdr)? == 0 {
            return Ok(()); // clean disconnect before importing
        }
        let (code, _status) = proto::decode_op_header(&hdr)?;
        match code {
            c if c == proto::op::REQ_DEVLIST => {
                let devs: Vec<proto::UsbDevice> =
                    backend.list()?.into_iter().map(|d| d.summary).collect();
                stream.write_all(&proto::encode_devlist(&devs))?;
                stream.flush()?;
                // The stock client opens a fresh connection for IMPORT, so we may now see EOF.
            }
            c if c == proto::op::REQ_IMPORT => {
                let mut busid_buf = [0u8; 32];
                read_full(&mut stream, &mut busid_buf)?;
                let busid = proto::decode_import_busid(&busid_buf)?;
                match backend.import(&busid)? {
                    Some((summary, dev)) => {
                        log::info!("usbip: imported device busid={busid}");
                        stream.write_all(&proto::encode_import_reply(Some(&summary)))?;
                        stream.flush()?;
                        device = Some(dev);
                    }
                    None => {
                        log::warn!("usbip: import of unknown busid={busid} refused");
                        stream.write_all(&proto::encode_import_reply(None))?;
                        stream.flush()?;
                        return Ok(());
                    }
                }
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("usbip: unexpected op code 0x{other:04x}"),
                ));
            }
        }
    }

    // --- URB phase ---
    let mut dev = device.unwrap();
    serve_urbs(&mut stream, dev.as_mut())
}

fn serve_urbs<S: Read + Write>(stream: &mut S, dev: &mut dyn UsbDevice) -> io::Result<()> {
    loop {
        let mut hdr = [0u8; proto::URB_HEADER_LEN];
        if read_full_or_eof(stream, &mut hdr)? == 0 {
            return Ok(()); // guest detached
        }
        match proto::urb_command(&hdr)? {
            c if c == proto::urb::CMD_SUBMIT => {
                // For OUT transfers the data payload follows the header; read it first so the
                // stream stays framed even if the transfer itself errors.
                let tbl = u32::from_be_bytes(hdr[24..28].try_into().unwrap());
                let direction = u32::from_be_bytes(hdr[12..16].try_into().unwrap());
                let mut data = Vec::new();
                if direction == proto::DIR_OUT && tbl > 0 {
                    data = vec![0u8; tbl as usize];
                    read_full(stream, &mut data)?;
                }
                let cmd = proto::SubmitCmd::decode(&hdr, data)?;
                let reply = service_submit(dev, &cmd);
                stream.write_all(&reply.encode())?;
                stream.flush()?;
            }
            c if c == proto::urb::CMD_UNLINK => {
                let u = proto::UnlinkCmd::decode(&hdr)?;
                // Inline transfers complete before we read the next PDU, so any URB the guest
                // tries to unlink has already returned: reply status 0 (too-late-to-cancel).
                stream.write_all(&proto::encode_ret_unlink(u.header.seqnum, 0))?;
                stream.flush()?;
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("usbip: unexpected urb command 0x{other:08x}"),
                ));
            }
        }
    }
}

/// Run one CMD_SUBMIT against the backend and build its RET_SUBMIT (success or negative errno).
fn service_submit(dev: &mut dyn UsbDevice, cmd: &proto::SubmitCmd) -> proto::SubmitRet {
    let result = if cmd.is_control() {
        let length = u16::from_le_bytes([cmd.setup[6], cmd.setup[7]]);
        dev.control(cmd.setup, &cmd.data, length)
    } else {
        let ep = (cmd.header.ep & 0x7f) as u8;
        let dir = if cmd.dir_in() { Dir::In } else { Dir::Out };
        dev.transfer(ep, dir, &cmd.data, cmd.transfer_buffer_length)
    };
    match result {
        Ok(data) => proto::SubmitRet::ok(cmd, data),
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(libc_eproto());
            proto::SubmitRet::err(cmd, -errno)
        }
    }
}

fn libc_eproto() -> i32 {
    71 // EPROTO
}

/// Read exactly `buf.len()` bytes; error on early EOF.
fn read_full<S: Read>(stream: &mut S, buf: &mut [u8]) -> io::Result<()> {
    stream.read_exact(buf)
}

/// Read exactly `buf.len()` bytes, but treat an EOF *before any byte* as a clean disconnect
/// (returns `Ok(0)`); a partial frame is still an error. Returns the number of bytes read.
fn read_full_or_eof<S: Read>(stream: &mut S, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match stream.read(&mut buf[got..]) {
            Ok(0) => {
                if got == 0 {
                    return Ok(0);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "usbip: short frame",
                ));
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockBackend;
    use std::io::Cursor;

    /// A fake bidirectional stream: reads drain `inbound`, writes append to `outbound`.
    struct Pipe {
        inbound: Cursor<Vec<u8>>,
        outbound: Vec<u8>,
    }
    impl Read for Pipe {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.inbound.read(b)
        }
    }
    impl Write for Pipe {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.outbound.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn op_devlist() -> Vec<u8> {
        proto::encode_op_header(proto::op::REQ_DEVLIST, 0).to_vec()
    }
    fn op_import(busid: &str) -> Vec<u8> {
        let mut v = proto::encode_op_header(proto::op::REQ_IMPORT, 0).to_vec();
        let mut b = [0u8; 32];
        b[..busid.len()].copy_from_slice(busid.as_bytes());
        v.extend_from_slice(&b);
        v
    }
    fn submit_control(seqnum: u32, setup: [u8; 8], length: u32) -> Vec<u8> {
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&proto::urb::CMD_SUBMIT.to_be_bytes());
        hdr.extend_from_slice(&seqnum.to_be_bytes());
        hdr.extend_from_slice(&0x0001_0002u32.to_be_bytes()); // devid
        hdr.extend_from_slice(&proto::DIR_IN.to_be_bytes());
        hdr.extend_from_slice(&0u32.to_be_bytes()); // ep 0
        hdr.extend_from_slice(&0u32.to_be_bytes()); // transfer_flags
        hdr.extend_from_slice(&length.to_be_bytes()); // transfer_buffer_length
        hdr.extend_from_slice(&0i32.to_be_bytes());
        hdr.extend_from_slice(&0i32.to_be_bytes());
        hdr.extend_from_slice(&0i32.to_be_bytes());
        hdr.extend_from_slice(&setup);
        hdr
    }

    #[test]
    fn devlist_then_eof_lists_the_mock() {
        let mut p = Pipe {
            inbound: Cursor::new(op_devlist()),
            outbound: Vec::new(),
        };
        serve(&mut p, &MockBackend::new()).unwrap();
        // reply = op header + ndev(1) + one device record.
        let (code, _) = proto::decode_op_header(&p.outbound).unwrap();
        assert_eq!(code, proto::op::REP_DEVLIST);
        let ndev = u32::from_be_bytes(p.outbound[8..12].try_into().unwrap());
        assert_eq!(ndev, 1);
        let (dev, _) = proto::UsbDevice::decode(&p.outbound, 12, true).unwrap();
        assert_eq!(dev.busid, MockBackend::busid());
    }

    #[test]
    fn import_then_get_device_descriptor_round_trips() {
        let mut input = op_import(MockBackend::busid());
        // After import, fetch the 18-byte device descriptor over EP0.
        let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00];
        input.extend_from_slice(&submit_control(1, setup, 18));
        let mut p = Pipe {
            inbound: Cursor::new(input),
            outbound: Vec::new(),
        };
        serve(&mut p, &MockBackend::new()).unwrap();

        // outbound = OP_REP_IMPORT (8 + 0x138) then RET_SUBMIT (48 + 18 data).
        let import_len = proto::OP_HEADER_LEN + proto::USB_DEVICE_LEN;
        let (code, st) = proto::decode_op_header(&p.outbound).unwrap();
        assert_eq!(code, proto::op::REP_IMPORT);
        assert_eq!(st, proto::status::OK);

        let ret = &p.outbound[import_len..];
        assert_eq!(proto::urb_command(ret).unwrap(), proto::urb::RET_SUBMIT);
        assert_eq!(&ret[4..8], &1u32.to_be_bytes()); // seqnum echoed
        let actual_len = u32::from_be_bytes(ret[24..28].try_into().unwrap());
        assert_eq!(actual_len, 18);
        let data = &ret[proto::URB_HEADER_LEN..];
        assert_eq!(data[0], 18); // bLength
        assert_eq!(data[1], 0x01); // DEVICE descriptor type
    }

    #[test]
    fn import_unknown_busid_replies_error_and_stops() {
        let mut p = Pipe {
            inbound: Cursor::new(op_import("9-9")),
            outbound: Vec::new(),
        };
        serve(&mut p, &MockBackend::new()).unwrap();
        assert_eq!(p.outbound.len(), proto::OP_HEADER_LEN);
        let (_, st) = proto::decode_op_header(&p.outbound).unwrap();
        assert_eq!(st, proto::status::ERROR);
    }
}
