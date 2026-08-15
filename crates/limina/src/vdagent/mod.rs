// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The SPICE vdagent transport (M12, task #37).
//!
//! Stock Fedora already ships `spice-vdagent`, and its udev rule starts it the moment a
//! virtio-serial port named `com.redhat.spice.0` appears — so a **stock guest** gets
//! clipboard sharing with nothing installed into it. That is the whole point: this is the
//! transport that satisfies the two-tier guarantee's baseline, while `limina-agent` keeps
//! the enhanced-tier capabilities (richer formats, per-session identity) on the control
//! plane. One pasteboard owner ([`crate::clipboard::Clipboard`]), two transports.
//!
//! [`codec`] is the wire format, verified against upstream source. The broker that drives
//! it lands next; until then the port is still the `LIMINA_SPICE_PORT=1` probe in
//! `limina-vmm`.

// The codec lands before its consumer so the wire format can be reviewed and tested on its
// own (13 unit tests, no VM). Nothing calls it until the broker wiring commit, which drops
// this allow — if it is still here once `broker` exists, something went unwired.
#![allow(dead_code)]

pub mod codec;
