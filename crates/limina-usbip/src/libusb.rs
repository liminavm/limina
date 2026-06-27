// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The real host backend: enumerate, open, claim, and drive host USB devices via libusb (rusb).
//!
//! Maps each USB/IP operation onto the matching rusb call (per design doc): DEVLIST → list +
//! read descriptors, IMPORT → `open()` + `claim_interface()`, CMD_SUBMIT control/bulk/interrupt →
//! `read_control`/`write_control`/`read_bulk`/`write_bulk`/`read_interrupt`/`write_interrupt`.
//! Transfers run synchronously on the connection thread (v1; thread-per-endpoint is the future
//! refinement once async is needed — see the design's async-strategy note).
//!
//! ## macOS claiming gate
//! On macOS, libusb can claim devices with **no matching Apple driver** freely (FTDI/CP210x serial
//! without the Apple CDC match, security keys like the SoloKeys Solo 2, custom hardware). Devices
//! Apple binds (standard HID, mass storage, audio) need the Apple-managed, restricted
//! `com.apple.vm.device-access` entitlement (or a DriverKit dext). v1 targets the free-to-claim
//! set; `spikes/usb-probe` classifies a given device empirically.

use crate::backend::{DeviceInfo, Dir, UsbBackend, UsbDevice};
use crate::proto;
use rusb::{Context, Device, DeviceHandle, Direction, TransferType, UsbContext};
use std::io;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_millis(2000);

/// Map a rusb error to an io::Error carrying a USB-ish errno (negated onto the wire by the server).
fn rusb_io(e: rusb::Error) -> io::Error {
    let errno = match e {
        rusb::Error::Timeout => 110, // ETIMEDOUT
        rusb::Error::Pipe => 32,     // EPIPE (stall)
        rusb::Error::NoDevice => 19, // ENODEV
        rusb::Error::Access => 13,   // EACCES (the macOS claim gate)
        rusb::Error::NotFound => 2,  // ENOENT
        rusb::Error::Busy => 16,     // EBUSY
        rusb::Error::Overflow => 75, // EOVERFLOW
        _ => 71,                     // EPROTO
    };
    io::Error::from_raw_os_error(errno)
}

/// busid we present for a device: `"<bus>-<addr>"` (mirrors Linux's `usbip` busid shape closely
/// enough for selection; the host bus/addr are stable for the connection's lifetime).
fn busid_of(dev: &Device<Context>) -> String {
    format!("{}-{}", dev.bus_number(), dev.address())
}

/// Build the USB/IP device summary from a device's descriptors.
fn summary_of(dev: &Device<Context>) -> io::Result<proto::UsbDevice> {
    let dd = dev.device_descriptor().map_err(rusb_io)?;
    let busid = busid_of(dev);
    let speed = match dev.speed() {
        rusb::Speed::Low => 1,
        rusb::Speed::Full => 2,
        rusb::Speed::High => 3,
        rusb::Speed::Super => 5,
        _ => 0,
    };
    let mut interfaces = Vec::new();
    let (mut config_value, mut num_ifaces) = (0u8, 0u8);
    if let Ok(cfg) = dev.active_config_descriptor() {
        config_value = cfg.number();
        num_ifaces = cfg.num_interfaces();
        for iface in cfg.interfaces() {
            if let Some(d) = iface.descriptors().next() {
                interfaces.push(proto::UsbInterface {
                    class: d.class_code(),
                    subclass: d.sub_class_code(),
                    protocol: d.protocol_code(),
                });
            }
        }
    }
    let _ = num_ifaces;
    Ok(proto::UsbDevice {
        path: format!("/sys/devices/limina/usb{}/{}", dev.bus_number(), busid),
        busid,
        busnum: dev.bus_number() as u32,
        devnum: dev.address() as u32,
        speed,
        id_vendor: dd.vendor_id(),
        id_product: dd.product_id(),
        bcd_device: {
            // rusb splits bcdDevice into (major, minor, sub_minor) nibbles; rebuild the BCD u16.
            let v = dd.device_version();
            ((v.major() as u16) << 8) | ((v.minor() as u16) << 4) | (v.sub_minor() as u16)
        },
        b_device_class: dd.class_code(),
        b_device_subclass: dd.sub_class_code(),
        b_device_protocol: dd.protocol_code(),
        b_configuration_value: config_value,
        b_num_configurations: dd.num_configurations(),
        interfaces,
    })
}

/// libusb-backed backend.
pub struct LibusbBackend {
    ctx: Context,
}

impl LibusbBackend {
    pub fn new() -> io::Result<Self> {
        Ok(LibusbBackend {
            ctx: Context::new().map_err(rusb_io)?,
        })
    }

    fn find(&self, busid: &str) -> io::Result<Option<Device<Context>>> {
        for dev in self.ctx.devices().map_err(rusb_io)?.iter() {
            if busid_of(&dev) == busid {
                return Ok(Some(dev));
            }
        }
        Ok(None)
    }
}

impl UsbBackend for LibusbBackend {
    fn list(&self) -> io::Result<Vec<DeviceInfo>> {
        let mut out = Vec::new();
        for dev in self.ctx.devices().map_err(rusb_io)?.iter() {
            match summary_of(&dev) {
                Ok(summary) => out.push(DeviceInfo { summary }),
                Err(e) => log::debug!("usbip: skipping {}: {e}", busid_of(&dev)),
            }
        }
        Ok(out)
    }

    fn import(&self, busid: &str) -> io::Result<Option<(proto::UsbDevice, Box<dyn UsbDevice>)>> {
        let Some(dev) = self.find(busid)? else {
            return Ok(None);
        };
        let summary = summary_of(&dev)?;
        let handle = dev.open().map_err(rusb_io)?;
        // Claim every interface of the active configuration so the guest can drive them all.
        if let Ok(cfg) = dev.active_config_descriptor() {
            for iface in cfg.interfaces() {
                let n = iface.number();
                // On macOS detach is a no-op; on Linux it frees an Apple-bound kernel driver.
                let _ = handle.set_auto_detach_kernel_driver(true);
                if let Err(e) = handle.claim_interface(n) {
                    log::warn!("usbip: claim interface {n} failed: {e}");
                }
            }
        }
        let opened = LibusbDevice {
            handle,
            device: dev,
        };
        Ok(Some((summary, Box::new(opened))))
    }
}

struct LibusbDevice {
    handle: DeviceHandle<Context>,
    device: Device<Context>,
}

impl LibusbDevice {
    /// Look up the transfer type of `ep` from the active config (to choose bulk vs interrupt).
    fn endpoint_type(&self, ep: u8, dir: Dir) -> TransferType {
        let want_dir = match dir {
            Dir::In => Direction::In,
            Dir::Out => Direction::Out,
        };
        if let Ok(cfg) = self.device.active_config_descriptor() {
            for iface in cfg.interfaces() {
                for d in iface.descriptors() {
                    for epd in d.endpoint_descriptors() {
                        if epd.number() == ep && epd.direction() == want_dir {
                            return epd.transfer_type();
                        }
                    }
                }
            }
        }
        TransferType::Bulk
    }
}

impl UsbDevice for LibusbDevice {
    fn control(&mut self, setup: [u8; 8], data: &[u8], length: u16) -> io::Result<Vec<u8>> {
        let bm_request_type = setup[0];
        let b_request = setup[1];
        let w_value = u16::from_le_bytes([setup[2], setup[3]]);
        let w_index = u16::from_le_bytes([setup[4], setup[5]]);
        if bm_request_type & 0x80 != 0 {
            let mut buf = vec![0u8; length as usize];
            let n = self
                .handle
                .read_control(
                    bm_request_type,
                    b_request,
                    w_value,
                    w_index,
                    &mut buf,
                    TIMEOUT,
                )
                .map_err(rusb_io)?;
            buf.truncate(n);
            Ok(buf)
        } else {
            self.handle
                .write_control(bm_request_type, b_request, w_value, w_index, data, TIMEOUT)
                .map_err(rusb_io)?;
            Ok(Vec::new())
        }
    }

    fn transfer(&mut self, ep: u8, dir: Dir, data: &[u8], length: u32) -> io::Result<Vec<u8>> {
        let ttype = self.endpoint_type(ep, dir);
        match dir {
            Dir::In => {
                let addr = ep | 0x80;
                let mut buf = vec![0u8; length as usize];
                let n = match ttype {
                    TransferType::Interrupt => self.handle.read_interrupt(addr, &mut buf, TIMEOUT),
                    _ => self.handle.read_bulk(addr, &mut buf, TIMEOUT),
                }
                .map_err(rusb_io)?;
                buf.truncate(n);
                Ok(buf)
            }
            Dir::Out => {
                let addr = ep & 0x7f;
                let n = match ttype {
                    TransferType::Interrupt => self.handle.write_interrupt(addr, data, TIMEOUT),
                    _ => self.handle.write_bulk(addr, data, TIMEOUT),
                }
                .map_err(rusb_io)?;
                let _ = n;
                Ok(Vec::new())
            }
        }
    }
}
