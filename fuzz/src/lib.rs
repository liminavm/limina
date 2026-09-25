// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Modules of the `limina` binary crate that the fuzz targets reach.
//!
//! `limina` has no library target, so a module with no dependency on the rest of the
//! supervisor is compiled in here from its own source file. Only such modules belong here:
//! one that names `crate::` anything will not build, and that is the signal to give it a
//! seam first rather than to widen this list.

#![allow(dead_code)]

#[path = "../../crates/limina/src/vdagent/codec.rs"]
pub mod vdagent_codec;

#[path = "../../crates/limina/src/fido/request.rs"]
pub mod ctap2_request;
