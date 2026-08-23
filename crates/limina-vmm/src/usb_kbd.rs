// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Worker-side policy for the USB HID keyboard gadget: the keyboard the guest has during the
//! window in which it has no `virtio_input` driver. Design: `docs/design/usb-hid-keyboard.md`.
//!
//! The gadget is an input-device HID pipe (libkrun mechanism,
//! [`devices::usb::HidReportPipe::new_in_only`]) carrying the standard 8-byte keyboard report;
//! the policy here is its identity and its report descriptor, and the translation from evdev
//! lives in [`limina_input::hidkbd`]. It is **always plugged** when the USB controller is on,
//! rather than hot-plugged around the window: presenting an unplug races a keystroke in flight,
//! and the cost of leaving it is one idle keyboard in the guest's device list.
//!
//! Which device a key actually goes to is [`limina_input::router`]'s decision, not this
//! module's — the gadget just carries whatever reaches its sink.

use std::sync::Arc;

use devices::usb::{HidReportPipe, ReportSink, UsbDeviceModel};
use limina_input::hidkbd;
use limina_input::router::HidReportSink;

/// The same vendor-neutral "Linux Foundation" id the FIDO gadget uses, with its own product id.
const VID: u16 = 0x1d6b;
const PID: u16 = 0x0f1e;

/// Build the gadget. Returns the model to cold-plug onto the emulated controller
/// (`vmr.usb_devices`) and the sink the key router pushes HID reports into.
pub fn build() -> (Arc<dyn UsbDeviceModel>, HidReportSink) {
    // The guest's host→device report is the 1-byte LED bitmap (caps/num/scroll), delivered as
    // a SET_REPORT control since the gadget has no interrupt-OUT endpoint. limina has no LEDs
    // to light, so it is observed and dropped — declaring the Output item still matters: a
    // keyboard descriptor without one is nonstandard, and some HID stacks probe for it.
    let leds: ReportSink = Arc::new(|frame: Vec<u8>| {
        log::trace!("usb-kbd: guest set the keyboard LEDs to {frame:02x?}");
    });

    let pipe = HidReportPipe::new_in_only(
        VID,
        PID,
        hidkbd::report_descriptor(),
        hidkbd::REPORT_LEN,
        ["limina", "limina Keyboard", "LIMINA-KBD-USB", "Keyboard"],
        leds,
    );

    let reports = pipe.clone();
    let sink: HidReportSink = Arc::new(move |report: [u8; hidkbd::REPORT_LEN]| {
        reports.push_in(report.to_vec());
    });
    (pipe, sink)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gadget must present as a report-only HID keyboard on one interrupt-IN endpoint.
    /// Boot protocol (subclass/protocol 1/1) is the trap this pins: EFI ConIn aggregates every
    /// keyboard it finds, so a boot-protocol gadget alongside the firmware's VirtioKeyboardDxe
    /// would type every keystroke at the GRUB menu twice.
    #[test]
    fn the_gadget_is_a_report_only_keyboard_with_one_in_endpoint() {
        let (gadget, _sink) = build();
        let d = gadget.descriptors();
        assert_eq!(u16::from_le_bytes([d.device[8], d.device[9]]), VID);
        assert_eq!(u16::from_le_bytes([d.device[10], d.device[11]]), PID);
        let c = &d.configs[0];
        assert_eq!(c[9 + 4], 0x01, "one endpoint");
        assert_eq!(c[9 + 5], 0x03, "HID class");
        assert_eq!(c[9 + 6], 0x00, "no boot subclass");
        assert_eq!(c[9 + 7], 0x00, "no boot protocol");
    }

    /// The report descriptor must be the one the guest parses into a keyboard; a truncated
    /// or unclosed collection yields no input device at all, which looks exactly like the gap
    /// this gadget exists to close.
    #[test]
    fn the_report_descriptor_is_advertised_at_its_real_length() {
        let (gadget, _sink) = build();
        let c = &gadget.descriptors().configs[0];
        // The HID descriptor's wDescriptorLength (last two bytes of the 9-byte HID block,
        // which follows the 9-byte config and 9-byte interface descriptors).
        let declared = u16::from_le_bytes([c[9 + 9 + 7], c[9 + 9 + 8]]) as usize;
        assert_eq!(declared, hidkbd::report_descriptor().len());
    }
}
