#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Live USB survival probe for the emulated xHCI controller (docs/design/usb-xhci-snapshot/).

Given a VID:PID, report the device's *identity across a suspend* and prove its **data path still
works** — the two things a suspend/resume regression breaks independently:

- `devnum` (and `busnum`) come from sysfs. They are a sharp re-enumeration oracle: if the device
  silently disconnected and came back, the kernel assigns it a NEW device number. `lsusb` alone
  cannot tell the difference, because a re-enumerated device is still listed.
- the descriptor read is a REAL control transfer, issued through usbfs with `USBDEVFS_CONTROL`
  (GET_DESCRIPTOR(device), the same 8-byte SETUP a host controller sends at enumeration). It walks
  the full path a restored controller has to have rebuilt: doorbell -> transfer ring -> Setup/Data/
  Status TDs -> gadget -> event ring -> interrupt. A device whose identity survived but whose rings
  did not fails here, not above.

Pure python3 + ctypes: nothing to install in the guest. Needs root for the usbfs open.

Usage:  sudo python3 usbprobe.py 04f3:0c7d
Prints: `USBPROBE: ok vid=04f3 pid=0c7d bus=1 devnum=2` on success,
        `USBPROBE: fail <reason>` otherwise (always exit 0 — the caller greps the marker).
"""

import ctypes
import fcntl
import os
import sys

# include/uapi/linux/usbdevice_fs.h
#   struct usbdevfs_ctrltransfer {
#       __u8 bRequestType; __u8 bRequest; __u16 wValue; __u16 wIndex; __u16 wLength;
#       __u32 timeout; void __user *data;
#   }
class CtrlTransfer(ctypes.Structure):
    _fields_ = [
        ("bRequestType", ctypes.c_uint8),
        ("bRequest", ctypes.c_uint8),
        ("wValue", ctypes.c_uint16),
        ("wIndex", ctypes.c_uint16),
        ("wLength", ctypes.c_uint16),
        ("timeout", ctypes.c_uint32),
        ("data", ctypes.c_void_p),
    ]


# USBDEVFS_CONTROL = _IOWR('U', 0, struct usbdevfs_ctrltransfer). The struct size is part of the
# ioctl number and is ABI-dependent (the pointer's alignment pads it to 24 bytes on LP64), so
# derive it from the struct rather than hardcoding a constant for one architecture.
def _iowr(type_ch, nr, size):
    return (3 << 30) | (size << 16) | (ord(type_ch) << 8) | nr


USBDEVFS_CONTROL = _iowr("U", 0, ctypes.sizeof(CtrlTransfer))


SYSFS = "/sys/bus/usb/devices"


def read_attr(path, name):
    try:
        with open(os.path.join(path, name)) as f:
            return f.read().strip()
    except OSError:
        return ""


def find(vid, pid):
    """Return (sysfs_path, busnum, devnum) for the first device matching vid:pid."""
    for entry in sorted(os.listdir(SYSFS)):
        path = os.path.join(SYSFS, entry)
        if read_attr(path, "idVendor").lower() != vid or read_attr(path, "idProduct").lower() != pid:
            continue
        bus, dev = read_attr(path, "busnum"), read_attr(path, "devnum")
        if bus and dev:
            return path, int(bus), int(dev)
    return None, None, None


def get_device_descriptor(bus, dev):
    """Issue a real GET_DESCRIPTOR(device) control transfer; return the 18 bytes."""
    node = f"/dev/bus/usb/{bus:03d}/{dev:03d}"
    buf = ctypes.create_string_buffer(18)
    xfer = CtrlTransfer(
        bRequestType=0x80,  # device-to-host, standard, device
        bRequest=0x06,  # GET_DESCRIPTOR
        wValue=0x0100,  # type 1 (DEVICE), index 0
        wIndex=0,
        wLength=18,
        timeout=5000,
        data=ctypes.cast(buf, ctypes.c_void_p),
    )
    fd = os.open(node, os.O_RDWR)
    try:
        n = fcntl.ioctl(fd, USBDEVFS_CONTROL, xfer)
    finally:
        os.close(fd)
    if n != 18:
        raise OSError(f"short control transfer: {n} of 18 bytes")
    return buf.raw


def main():
    if len(sys.argv) != 2 or ":" not in sys.argv[1]:
        print("USBPROBE: fail usage (expected <vid>:<pid>)")
        return
    vid, pid = (x.lower() for x in sys.argv[1].split(":", 1))

    path, bus, dev = find(vid, pid)
    if path is None:
        print(f"USBPROBE: fail {vid}:{pid} not present in {SYSFS}")
        return
    try:
        d = get_device_descriptor(bus, dev)
    except OSError as e:
        print(f"USBPROBE: fail control transfer to {vid}:{pid} (bus {bus} dev {dev}): {e}")
        return

    # Bytes 8..12 of the device descriptor are idVendor/idProduct, little-endian. Compare against
    # what we were asked for: this is the device answering for itself, not sysfs's cached copy.
    got_vid = int.from_bytes(d[8:10], "little")
    got_pid = int.from_bytes(d[10:12], "little")
    if (got_vid, got_pid) != (int(vid, 16), int(pid, 16)):
        print(f"USBPROBE: fail descriptor says {got_vid:04x}:{got_pid:04x}, expected {vid}:{pid}")
        return
    print(f"USBPROBE: ok vid={vid} pid={pid} bus={bus} devnum={dev}")


main()
