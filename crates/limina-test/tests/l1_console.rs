// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 console I/O test: prove the guest console works **both ways** end-to-end.
//!
//! Where `l1_vsock.rs` uses a vsock side-channel, this exercises the real console through
//! the actual `limina` → `limina-vmm` → virtio-console (`hvc0`) path. (We use hvc0, not the
//! PL011 serial: the guest kernel exposes hvc0 as a readable tty out of the box, whereas a
//! PL011 tty needs an FDT change that currently deadlocks the guest amba probe — see the
//! `limina-pl011-tty-deadlock` note. PL011 stays the firmware/early-boot console.) With
//! `console=hvc0` (set by `with_console_input`) hvc0 is the guest's `/dev/console`:
//!   - OUTPUT (guest→host): the init prints a ready marker; we read it from the captured
//!     console (`wait_for`).
//!   - INPUT (host→guest): we `console_send` lines; the init (in `limina.console_echo` mode)
//!     replies `ECHO:<line>`, which we then read back — a full round trip.
//!
//! `QUIT` ends the echo loop and the guest powers off cleanly.
//!
//! This is the seed of typing commands at the guest and asserting their output. Build the
//! guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn l1_console_roundtrips_input_and_output() {
    if !limina_test::require_hvf_or_skip("l1_console_roundtrips_input_and_output") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_console_input()
        .append_cmdline("limina.console_echo");
    eprintln!("booting L1 guest with console echo: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // OUTPUT: the guest announces readiness over the serial console.
    guest
        .wait_for("LIMINA_CONSOLE_ECHO_READY", Duration::from_secs(20))
        .expect("guest did not announce console-echo readiness");

    // INPUT + OUTPUT round trip: type a line, expect it echoed back.
    guest.console_send("ping").expect("sending ping");
    guest
        .wait_for("ECHO:ping", Duration::from_secs(10))
        .expect("did not see echo of 'ping' (console input path broken?)");

    // A second, distinct line — guards against matching a stale marker.
    guest.console_send("limina-42").expect("sending limina-42");
    guest
        .wait_for("ECHO:limina-42", Duration::from_secs(10))
        .expect("did not see echo of 'limina-42'");

    // QUIT ends the echo loop; the init then powers off cleanly.
    guest.console_send("QUIT").expect("sending QUIT");
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(!outcome.forced, "harness had to force teardown");
    assert_eq!(
        outcome.code,
        Some(0),
        "expected clean power-off, got {outcome:?}"
    );
}
