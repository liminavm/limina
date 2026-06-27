// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina-usbip — a host-side USB/IP server (exporter) that hands host USB devices to the guest.
//!
//! The guest side is 100% upstream (`vhci_hcd` + `usbip attach`); this crate is the host half:
//! the [`proto`] wire protocol, a transport-agnostic [`server`] loop, and a [`backend`]
//! abstraction with a hardware-free [`mock`] device (for end-to-end tests with no USB hardware)
//! and a libusb-backed real device (feature `libusb`). See `docs/design/m7-usb-passthrough.md`.

pub mod backend;
pub mod mock;
pub mod proto;
pub mod server;

#[cfg(feature = "libusb")]
pub mod libusb;

pub use backend::{DeviceInfo, Dir, UsbBackend, UsbDevice};
pub use mock::MockBackend;
pub use server::serve;
