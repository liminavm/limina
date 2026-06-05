// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 PL011 serial test: prove the **PL011 serial tty** (`ttyAMA0`) works both ways
//! end-to-end — the M2.5 Track A goal (an interactive serial debug shell).
//!
//! This is the regression test for the halfword-MMIO bug. Exposing a PL011 *tty* needs
//! `arm,primecell` on the FDT serial node so the guest's AMBA layer binds `amba-pl011`
//! and creates `ttyAMA0`. The ARM PL011 driver then drives its registers with **16-bit**
//! (`writew`/`readw`) accesses — and libkrun's HVF MMIO handler used to `panic!` on
//! `len=2`, killing the vCPU thread mid-probe (which looked like a guest "deadlock"; it
//! was the worker dying). With the len=2 write case added, the probe completes and
//! `ttyAMA0` is a real bidirectional console.
//!
//! With `console=ttyAMA0` the guest's `/dev/console` is the PL011, so:
//!   - OUTPUT (guest→host): the init prints a ready marker; we read it from the captured
//!     console (`wait_for`).
//!   - INPUT (host→guest): we `console_send` lines; the init (in `limina.console_echo` mode)
//!     replies `ECHO:<line>`, which we read back — a full round trip over the serial port.
//!
//! `QUIT` ends the echo loop and the guest powers off cleanly. Companion to
//! `l1_console.rs`, which exercises the same round trip over virtio-console (`hvc0`).
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn l1_serial_roundtrips_input_and_output() {
    if !limina_test::require_hvf_or_skip("l1_serial_roundtrips_input_and_output") {
        return;
    }

    // `with_serial_input` keeps `console=ttyAMA0`, so the PL011 is `/dev/console`.
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_serial_input()
        .append_cmdline("limina.console_echo");
    eprintln!("booting L1 guest with PL011 serial echo: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // OUTPUT: the guest announces readiness over the PL011 serial console. Before the
    // len=2 fix the worker panicked during the amba-pl011 probe and this never appeared.
    guest
        .wait_for("LIMINA_CONSOLE_ECHO_READY", Duration::from_secs(20))
        .expect("guest did not announce console-echo readiness over ttyAMA0");

    // INPUT + OUTPUT round trip: type a line, expect it echoed back.
    guest.console_send("ping").expect("sending ping");
    guest
        .wait_for("ECHO:ping", Duration::from_secs(10))
        .expect("did not see echo of 'ping' (PL011 input path broken?)");

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
