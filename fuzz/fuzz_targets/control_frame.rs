// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The control plane's reader over any bytes a guest agent can send.
//!
//! Beyond not panicking: a frame that decodes must encode back to a frame that decodes to the
//! same message on the same channel. `Unknown` included -- it carries its payload so the
//! receiver can answer it, and a newer peer's message must survive being passed along.

#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use limina_proto::{read_message, write_message};

fuzz_target!(|data: &[u8]| {
    let mut r = Cursor::new(data);
    // A stream carries frames back to back; keep reading until one is refused.
    while let Ok((channel, msg)) = read_message(&mut r) {
        let mut wire = Vec::new();
        write_message(&mut wire, channel, &msg).expect("a decoded message encodes");
        let (again_channel, again) =
            read_message(&mut Cursor::new(&wire)).expect("an encoded message decodes");
        assert_eq!(
            again_channel, channel,
            "the channel changed across a round trip"
        );
        assert_eq!(again, msg, "the message changed across a round trip");
    }
});
