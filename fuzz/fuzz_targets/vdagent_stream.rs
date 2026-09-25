// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The SPICE vdagent reassembler over any bytes a guest agent can send, split any way.
//!
//! `Reassembler::push` promises that how the stream is split does not matter. So the same
//! bytes are pushed whole and in pieces, and the two must agree: the same messages when the
//! whole push succeeds, and a refusal somewhere in the pieces when it does not. Every message
//! that comes out is then decoded under both clipboard layouts, which must not panic.
//!
//! The first input byte seeds the split points; the rest is the stream.

#![no_main]

use libfuzzer_sys::fuzz_target;
use limina_fuzz::vdagent_codec::{Reassembler, decode};

fuzz_target!(|data: &[u8]| {
    let Some((&seed, stream)) = data.split_first() else {
        return;
    };

    let whole = Reassembler::new().push(stream);

    let mut pieces = Reassembler::new();
    let mut got = Vec::new();
    let mut refused = false;
    let mut rest = stream;
    let mut step = seed as usize % 7 + 1;
    while !rest.is_empty() {
        let n = step.min(rest.len());
        let (head, tail) = rest.split_at(n);
        match pieces.push(head) {
            Ok(msgs) => got.extend(msgs),
            Err(_) => {
                refused = true;
                break;
            }
        }
        rest = tail;
        step = step * 5 % 13 + 1;
    }

    match whole {
        Ok(msgs) => {
            assert!(!refused, "pieces refused a stream the whole push accepted");
            assert_eq!(got, msgs, "the split changed what came out");
            for (msg_type, body) in &msgs {
                let _ = decode(*msg_type, body, false);
                let _ = decode(*msg_type, body, true);
            }
        }
        Err(_) => assert!(refused, "pieces accepted a stream the whole push refused"),
    }
});
