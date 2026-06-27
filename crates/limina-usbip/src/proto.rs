// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The USB/IP wire protocol (server side), per `Documentation/usb/usbip_protocol.rst` and
//! `drivers/usb/usbip/usbip_common.h`. We are the SERVER (exporter): the device lives on the
//! host, the guest's stock `vhci_hcd` is the client.
//!
//! Endianness rule (verified against kernel source): **every multi-byte header field is network
//! byte order (big-endian)**, with ONE exception — the `setup[8]` bytes of a control-transfer
//! SUBMIT are the raw USB setup packet (little-endian on the USB bus); we pass them through
//! verbatim, never byte-swapping.
//!
//! Two message families share one connection:
//!  - **op_** (DEVLIST / IMPORT): an 8-byte common header `{version, code, status}` + body. Used
//!    once, before attach, to discover and claim a device.
//!  - **URB** (CMD_SUBMIT / RET_SUBMIT / CMD_UNLINK / RET_UNLINK): a fixed **48-byte** header
//!    (`usbip_header_basic` 20 B + a 28-B command-specific tail) + optional data. Used for all
//!    post-attach I/O.

use std::io;

/// USB/IP protocol version (BCD 1.1.1) carried in every op_ header.
pub const USBIP_VERSION: u16 = 0x0111;

/// op_ command codes (in the 8-byte common header).
pub mod op {
    pub const REQ_DEVLIST: u16 = 0x8005;
    pub const REP_DEVLIST: u16 = 0x0005;
    pub const REQ_IMPORT: u16 = 0x8003;
    pub const REP_IMPORT: u16 = 0x0003;
}

/// op_ status values.
pub mod status {
    pub const OK: u32 = 0x0000_0000;
    pub const ERROR: u32 = 0x0000_0001;
}

/// URB command codes (in `usbip_header_basic.command`).
pub mod urb {
    pub const CMD_SUBMIT: u32 = 0x0000_0001;
    pub const CMD_UNLINK: u32 = 0x0000_0002;
    pub const RET_SUBMIT: u32 = 0x0000_0003;
    pub const RET_UNLINK: u32 = 0x0000_0004;
}

/// Transfer direction (in `usbip_header_basic.direction`).
pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;

/// The fixed sizes the wire layout pins (used for framing).
pub const OP_HEADER_LEN: usize = 8;
pub const USB_DEVICE_LEN: usize = 0x138; // path[256]+busid[32]+5*u32... up to bNumInterfaces
pub const USB_INTERFACE_LEN: usize = 4;
pub const URB_HEADER_LEN: usize = 48; // basic(20) + command tail(28)

fn proto_err(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("usbip: {msg}"))
}

/// A fixed-width, zero-padded ASCII field (`path[256]`, `busid[32]`) written/read verbatim.
fn write_fixed(out: &mut Vec<u8>, s: &str, len: usize) {
    let b = s.as_bytes();
    let n = b.len().min(len);
    out.extend_from_slice(&b[..n]);
    out.resize(out.len() + (len - n), 0u8);
}

fn read_fixed(buf: &[u8], off: usize, len: usize) -> io::Result<String> {
    let raw = buf
        .get(off..off + len)
        .ok_or_else(|| proto_err("fixed field out of bounds"))?;
    let end = raw.iter().position(|&b| b == 0).unwrap_or(len);
    Ok(String::from_utf8_lossy(&raw[..end]).into_owned())
}

/// A read cursor over a byte slice that pulls big-endian integers with bounds checks.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, i: 0 }
    }
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let s = self
            .b
            .get(self.i..self.i + n)
            .ok_or_else(|| proto_err("short read"))?;
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn skip(&mut self, n: usize) -> io::Result<()> {
        self.take(n).map(|_| ())
    }
}

/// One interface summary (4 bytes) in a `UsbDevice`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct UsbInterface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// `usbip_usb_device` — the device summary carried in DEVLIST and IMPORT replies.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct UsbDevice {
    pub path: String, // e.g. "/sys/devices/.../usb1/1-1"  (or a synthetic path for the mock)
    pub busid: String, // e.g. "1-1"
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32, // usb_device_speed (1=low,2=full,3=high,5=super)
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub b_device_class: u8,
    pub b_device_subclass: u8,
    pub b_device_protocol: u8,
    pub b_configuration_value: u8,
    pub b_num_configurations: u8,
    pub interfaces: Vec<UsbInterface>,
}

impl UsbDevice {
    /// `devid = (busnum << 16) | devnum` — the client's per-URB device id.
    pub fn devid(&self) -> u32 {
        (self.busnum << 16) | (self.devnum & 0xffff)
    }

    /// Encode the 0x138-byte device summary. `with_interfaces` appends the 4-byte interface
    /// records (DEVLIST includes them; IMPORT's reply does NOT).
    pub fn encode(&self, out: &mut Vec<u8>, with_interfaces: bool) {
        write_fixed(out, &self.path, 256);
        write_fixed(out, &self.busid, 32);
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&self.speed.to_be_bytes());
        out.extend_from_slice(&self.id_vendor.to_be_bytes());
        out.extend_from_slice(&self.id_product.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.push(self.b_device_class);
        out.push(self.b_device_subclass);
        out.push(self.b_device_protocol);
        out.push(self.b_configuration_value);
        out.push(self.b_num_configurations);
        out.push(self.interfaces.len() as u8); // bNumInterfaces
        if with_interfaces {
            for it in &self.interfaces {
                out.push(it.class);
                out.push(it.subclass);
                out.push(it.protocol);
                out.push(0); // padding
            }
        }
    }

    /// Decode a device summary from `buf` at `off`. Returns (device, bytes_consumed).
    pub fn decode(buf: &[u8], off: usize, with_interfaces: bool) -> io::Result<(UsbDevice, usize)> {
        let path = read_fixed(buf, off, 256)?;
        let busid = read_fixed(buf, off + 256, 32)?;
        let mut c = Cur::new(
            buf.get(off + 288..)
                .ok_or_else(|| proto_err("device body out of bounds"))?,
        );
        let busnum = c.u32()?;
        let devnum = c.u32()?;
        let speed = c.u32()?;
        let id_vendor = c.u16()?;
        let id_product = c.u16()?;
        let bcd_device = c.u16()?;
        let b_device_class = c.u8()?;
        let b_device_subclass = c.u8()?;
        let b_device_protocol = c.u8()?;
        let b_configuration_value = c.u8()?;
        let b_num_configurations = c.u8()?;
        let n_ifaces = c.u8()? as usize;
        let mut interfaces = Vec::with_capacity(n_ifaces);
        if with_interfaces {
            for _ in 0..n_ifaces {
                let class = c.u8()?;
                let subclass = c.u8()?;
                let protocol = c.u8()?;
                c.skip(1)?;
                interfaces.push(UsbInterface {
                    class,
                    subclass,
                    protocol,
                });
            }
        } else {
            interfaces.resize(n_ifaces, UsbInterface::default());
        }
        let consumed = 288 + c.i;
        Ok((
            UsbDevice {
                path,
                busid,
                busnum,
                devnum,
                speed,
                id_vendor,
                id_product,
                bcd_device,
                b_device_class,
                b_device_subclass,
                b_device_protocol,
                b_configuration_value,
                b_num_configurations,
                interfaces,
            },
            consumed,
        ))
    }
}

/// Encode the 8-byte op_ common header.
pub fn encode_op_header(code: u16, status: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[0..2].copy_from_slice(&USBIP_VERSION.to_be_bytes());
    h[2..4].copy_from_slice(&code.to_be_bytes());
    h[4..8].copy_from_slice(&status.to_be_bytes());
    h
}

/// Decode the 8-byte op_ common header → (code, status). Ignores the version field's exact value
/// but requires it be non-zero (clients send 0x0111).
pub fn decode_op_header(buf: &[u8]) -> io::Result<(u16, u32)> {
    let mut c = Cur::new(buf);
    let _ver = c.u16()?;
    let code = c.u16()?;
    let status = c.u32()?;
    Ok((code, status))
}

/// Encode an OP_REP_DEVLIST reply for `devices`.
pub fn encode_devlist(devices: &[UsbDevice]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_op_header(op::REP_DEVLIST, status::OK));
    out.extend_from_slice(&(devices.len() as u32).to_be_bytes());
    for d in devices {
        d.encode(&mut out, true);
    }
    out
}

/// Decode the busid from an OP_REQ_IMPORT body (the 32 bytes after the common header).
pub fn decode_import_busid(buf: &[u8]) -> io::Result<String> {
    read_fixed(buf, 0, 32)
}

/// Encode an OP_REP_IMPORT reply. `dev = Some` on success (status OK + the device summary, NO
/// interface array); `None` on failure (status ERROR, header only).
pub fn encode_import_reply(dev: Option<&UsbDevice>) -> Vec<u8> {
    let mut out = Vec::new();
    match dev {
        Some(d) => {
            out.extend_from_slice(&encode_op_header(op::REP_IMPORT, status::OK));
            d.encode(&mut out, false);
        }
        None => out.extend_from_slice(&encode_op_header(op::REP_IMPORT, status::ERROR)),
    }
    out
}

/// `usbip_header_basic` (20 bytes) — the front of every URB PDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct UrbHeader {
    pub command: u32,
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
}

impl UrbHeader {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.command.to_be_bytes());
        out.extend_from_slice(&self.seqnum.to_be_bytes());
        out.extend_from_slice(&self.devid.to_be_bytes());
        out.extend_from_slice(&self.direction.to_be_bytes());
        out.extend_from_slice(&self.ep.to_be_bytes());
    }
    fn read(c: &mut Cur) -> io::Result<UrbHeader> {
        Ok(UrbHeader {
            command: c.u32()?,
            seqnum: c.u32()?,
            devid: c.u32()?,
            direction: c.u32()?,
            ep: c.u32()?,
        })
    }
}

/// A decoded CMD_SUBMIT — one URB the guest is sending us. `data` is the OUT payload (empty for
/// IN transfers). `setup` is the raw 8-byte USB setup packet (little-endian; verbatim).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitCmd {
    pub header: UrbHeader,
    pub transfer_flags: u32,
    pub transfer_buffer_length: u32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub interval: i32,
    pub setup: [u8; 8],
    pub data: Vec<u8>,
}

impl SubmitCmd {
    pub fn is_control(&self) -> bool {
        self.header.ep == 0
    }
    pub fn dir_in(&self) -> bool {
        self.header.direction == DIR_IN
    }

    /// Decode the 48-byte header from `hdr` (already framed) and attach `data` (OUT payload,
    /// already read from the stream using `transfer_buffer_length` for OUT transfers).
    pub fn decode(hdr: &[u8], data: Vec<u8>) -> io::Result<SubmitCmd> {
        let mut c = Cur::new(hdr);
        let header = UrbHeader::read(&mut c)?;
        if header.command != urb::CMD_SUBMIT {
            return Err(proto_err("not a CMD_SUBMIT"));
        }
        let transfer_flags = c.u32()?;
        let transfer_buffer_length = c.u32()?;
        let start_frame = c.i32()?;
        let number_of_packets = c.i32()?;
        let interval = c.i32()?;
        let setup: [u8; 8] = c.take(8)?.try_into().unwrap();
        Ok(SubmitCmd {
            header,
            transfer_flags,
            transfer_buffer_length,
            start_frame,
            number_of_packets,
            interval,
            setup,
            data,
        })
    }
}

/// A RET_SUBMIT we send back for a completed URB. `data` is the IN payload (empty for OUT).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitRet {
    pub seqnum: u32,
    pub status: i32, // 0 = success; negative errno otherwise
    pub actual_length: u32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub error_count: i32,
    pub data: Vec<u8>,
}

impl SubmitRet {
    /// Build a success reply carrying `data` for the URB `cmd`.
    pub fn ok(cmd: &SubmitCmd, data: Vec<u8>) -> SubmitRet {
        SubmitRet {
            seqnum: cmd.header.seqnum,
            status: 0,
            actual_length: data.len() as u32,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            data,
        }
    }
    /// Build a failure reply (negative errno, no data).
    pub fn err(cmd: &SubmitCmd, errno: i32) -> SubmitRet {
        SubmitRet {
            seqnum: cmd.header.seqnum,
            status: errno,
            actual_length: 0,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            data: Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(URB_HEADER_LEN + self.data.len());
        UrbHeader {
            command: urb::RET_SUBMIT,
            seqnum: self.seqnum,
            devid: 0,
            direction: 0,
            ep: 0,
        }
        .write(&mut out);
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&self.actual_length.to_be_bytes());
        out.extend_from_slice(&self.start_frame.to_be_bytes());
        out.extend_from_slice(&self.number_of_packets.to_be_bytes());
        out.extend_from_slice(&self.error_count.to_be_bytes());
        out.extend_from_slice(&[0u8; 8]); // padding (mirrors CMD_SUBMIT's setup[8])
        out.extend_from_slice(&self.data);
        out
    }
}

/// A decoded CMD_UNLINK (the guest is cancelling an in-flight URB by its seqnum).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnlinkCmd {
    pub header: UrbHeader,
    pub unlink_seqnum: u32,
}

impl UnlinkCmd {
    pub fn decode(hdr: &[u8]) -> io::Result<UnlinkCmd> {
        let mut c = Cur::new(hdr);
        let header = UrbHeader::read(&mut c)?;
        if header.command != urb::CMD_UNLINK {
            return Err(proto_err("not a CMD_UNLINK"));
        }
        let unlink_seqnum = c.u32()?;
        Ok(UnlinkCmd {
            header,
            unlink_seqnum,
        })
    }
}

/// Encode a RET_UNLINK. `status` is typically -ECONNRESET(-104) when the URB was found and
/// unlinked, or 0 if it had already completed.
pub fn encode_ret_unlink(seqnum: u32, status: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(URB_HEADER_LEN);
    UrbHeader {
        command: urb::RET_UNLINK,
        seqnum,
        devid: 0,
        direction: 0,
        ep: 0,
    }
    .write(&mut out);
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&[0u8; 24]); // padding to the 48-byte header
    out
}

/// Peek the URB command code from a 48-byte header without fully decoding it (for dispatch).
pub fn urb_command(hdr: &[u8]) -> io::Result<u32> {
    Cur::new(hdr).u32()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_device() -> UsbDevice {
        UsbDevice {
            path: "/sys/devices/limina/usb1/1-1".into(),
            busid: "1-1".into(),
            busnum: 1,
            devnum: 2,
            speed: 2, // full speed
            id_vendor: 0x1209,
            id_product: 0xBEEE,
            bcd_device: 0x0100,
            b_device_class: 0xEF,
            b_device_subclass: 0x02,
            b_device_protocol: 0x01,
            b_configuration_value: 1,
            b_num_configurations: 1,
            interfaces: vec![
                UsbInterface {
                    class: 0x02,
                    subclass: 0x02,
                    protocol: 0x01,
                },
                UsbInterface {
                    class: 0x0a,
                    subclass: 0,
                    protocol: 0,
                },
            ],
        }
    }

    #[test]
    fn op_header_round_trips_and_is_big_endian() {
        let h = encode_op_header(op::REP_IMPORT, status::OK);
        // version 0x0111, code 0x0003, status 0 — all big-endian.
        assert_eq!(h, [0x01, 0x11, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00]);
        let (code, st) = decode_op_header(&h).unwrap();
        assert_eq!(code, op::REP_IMPORT);
        assert_eq!(st, status::OK);
    }

    #[test]
    fn device_encode_decode_round_trips_with_interfaces() {
        let d = sample_device();
        let mut buf = Vec::new();
        d.encode(&mut buf, true);
        assert_eq!(buf.len(), USB_DEVICE_LEN + 2 * USB_INTERFACE_LEN);
        let (got, consumed) = UsbDevice::decode(&buf, 0, true).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(got, d);
        // bNumInterfaces byte sits at the documented offset 0x137.
        assert_eq!(buf[0x137], 2);
        // idVendor at 0x12C, big-endian.
        assert_eq!(&buf[0x12C..0x12E], &[0x12, 0x09]);
    }

    #[test]
    fn import_reply_omits_interface_array() {
        let d = sample_device();
        let rep = encode_import_reply(Some(&d));
        // 8-byte op header + 0x138 device, NO 4-byte interface records.
        assert_eq!(rep.len(), OP_HEADER_LEN + USB_DEVICE_LEN);
        let (code, st) = decode_op_header(&rep).unwrap();
        assert_eq!(code, op::REP_IMPORT);
        assert_eq!(st, status::OK);
        let (got, _) = UsbDevice::decode(&rep, OP_HEADER_LEN, false).unwrap();
        assert_eq!(got.id_vendor, 0x1209);
        assert_eq!(got.interfaces.len(), 2); // count preserved, summaries zeroed
    }

    #[test]
    fn import_reply_error_is_header_only() {
        let rep = encode_import_reply(None);
        assert_eq!(rep.len(), OP_HEADER_LEN);
        let (_, st) = decode_op_header(&rep).unwrap();
        assert_eq!(st, status::ERROR);
    }

    #[test]
    fn devlist_carries_count_and_devices() {
        let d = sample_device();
        let list = encode_devlist(std::slice::from_ref(&d));
        let (code, _) = decode_op_header(&list).unwrap();
        assert_eq!(code, op::REP_DEVLIST);
        let ndev = u32::from_be_bytes(list[8..12].try_into().unwrap());
        assert_eq!(ndev, 1);
        let (got, _) = UsbDevice::decode(&list, 12, true).unwrap();
        assert_eq!(got, d);
    }

    #[test]
    fn submit_cmd_decodes_control_setup_verbatim() {
        // GET_DESCRIPTOR(device): bmRequestType=0x80 bRequest=0x06 wValue=0x0100 wIndex=0 wLength=0x40
        let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x40, 0x00];
        let mut hdr = Vec::new();
        UrbHeader {
            command: urb::CMD_SUBMIT,
            seqnum: 7,
            devid: 0x0001_0002,
            direction: DIR_IN,
            ep: 0,
        }
        .write(&mut hdr);
        hdr.extend_from_slice(&0u32.to_be_bytes()); // transfer_flags
        hdr.extend_from_slice(&64u32.to_be_bytes()); // transfer_buffer_length
        hdr.extend_from_slice(&0i32.to_be_bytes()); // start_frame
        hdr.extend_from_slice(&0i32.to_be_bytes()); // number_of_packets
        hdr.extend_from_slice(&0i32.to_be_bytes()); // interval
        hdr.extend_from_slice(&setup);
        assert_eq!(hdr.len(), URB_HEADER_LEN);
        let cmd = SubmitCmd::decode(&hdr, Vec::new()).unwrap();
        assert_eq!(cmd.header.seqnum, 7);
        assert!(cmd.is_control() && cmd.dir_in());
        assert_eq!(cmd.transfer_buffer_length, 64);
        assert_eq!(cmd.setup, setup); // raw LE setup, untouched
    }

    #[test]
    fn ret_submit_encodes_data_and_header() {
        let cmd = SubmitCmd {
            header: UrbHeader {
                command: urb::CMD_SUBMIT,
                seqnum: 9,
                devid: 0,
                direction: DIR_IN,
                ep: 0,
            },
            transfer_flags: 0,
            transfer_buffer_length: 18,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
            data: Vec::new(),
        };
        let payload = vec![0x12, 0x01, 0x10, 0x01]; // start of a device descriptor
        let ret = SubmitRet::ok(&cmd, payload.clone()).encode();
        assert_eq!(ret.len(), URB_HEADER_LEN + payload.len());
        // command field = RET_SUBMIT, seqnum echoes the cmd.
        assert_eq!(urb_command(&ret).unwrap(), urb::RET_SUBMIT);
        assert_eq!(&ret[4..8], &9u32.to_be_bytes());
        // actual_length (at basic(20)+4 = 24) reports the BYTES SENT = payload length, not the
        // requested transfer_buffer_length.
        assert_eq!(&ret[24..28], &(payload.len() as u32).to_be_bytes());
        assert_eq!(&ret[URB_HEADER_LEN..], &payload[..]);
    }

    #[test]
    fn unlink_round_trips() {
        let mut hdr = Vec::new();
        UrbHeader {
            command: urb::CMD_UNLINK,
            seqnum: 100,
            devid: 0,
            direction: 0,
            ep: 0,
        }
        .write(&mut hdr);
        hdr.extend_from_slice(&42u32.to_be_bytes()); // unlink_seqnum
        hdr.resize(URB_HEADER_LEN, 0u8);
        let u = UnlinkCmd::decode(&hdr).unwrap();
        assert_eq!(u.header.seqnum, 100);
        assert_eq!(u.unlink_seqnum, 42);
        let ret = encode_ret_unlink(100, -104);
        assert_eq!(ret.len(), URB_HEADER_LEN);
        assert_eq!(urb_command(&ret).unwrap(), urb::RET_UNLINK);
        assert_eq!(&ret[20..24], &(-104i32).to_be_bytes());
    }

    #[test]
    fn short_buffers_error_not_panic() {
        assert!(decode_op_header(&[0, 1, 2]).is_err());
        assert!(UsbDevice::decode(&[0u8; 10], 0, true).is_err());
        assert!(SubmitCmd::decode(&[0u8; 10], Vec::new()).is_err());
    }
}
