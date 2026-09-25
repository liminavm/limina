// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The virtual FIDO authenticator's request parser over any bytes a guest can send.
//!
//! The guest controls every byte of a CTAP2 request, and the authenticator is the one piece of
//! the host that holds a secret. The parser must answer every input with a request or a CTAP
//! status, and never panic. libFuzzer's memory cap stands for the second half of that: a length
//! the guest declares must not become an allocation the host makes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use limina_fuzz::ctap2_request::parse;

fuzz_target!(|data: &[u8]| {
    let _ = parse(data);
});
