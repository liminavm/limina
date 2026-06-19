// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 command-mode test: drive the guest's interactive serial console as a tiny shell —
//! type a command, assert its output. This is the step up from the echo round-trip
//! (`l1_serial.rs`) and closes M2.5 Track A for the L1 guest: a typed command produces real
//! guest-state output over the PL011 serial tty (`ttyAMA0`). Stock Fedora gets the same
//! capability from a real `serial-getty@ttyAMA0` (see `boot.rs`).
//!
//! The L1 rootfs holds only `/init`, so the "shell" is a handful of built-ins the init
//! interprets in-process (`limina.console_shell`): `echo`, `cat <path>`, `uname`. Each
//! command's output is framed by a `LIMINA_SHELL_DONE` terminator so the harness
//! (`Guest::console_command`) can slice out exactly that command's output.
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn l1_serial_command_mode_runs_builtins_and_returns_output() {
    if !limina_test::require_hvf_or_skip("l1_serial_command_mode_runs_builtins_and_returns_output")
    {
        return;
    }

    // `with_serial_input` keeps `console=ttyAMA0`, so the PL011 is `/dev/console` — the
    // Track A serial device. `limina.console_shell` puts the init into command mode.
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_serial_input()
        .append_cmdline("limina.console_shell");
    eprintln!("booting L1 guest in serial command mode: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("LIMINA_SHELL_READY", Duration::from_secs(20))
        .expect("guest did not enter console command mode over ttyAMA0");

    // echo: the simplest round-trip — typed args come back verbatim.
    let out = guest
        .console_command("echo hello-m25", Duration::from_secs(10))
        .expect("echo command did not complete");
    assert!(out.contains("hello-m25"), "echo output: {out:?}");

    // cat: read real guest state — /proc/cmdline must carry the token that put us in shell mode.
    let out = guest
        .console_command("cat /proc/cmdline", Duration::from_secs(10))
        .expect("cat command did not complete");
    assert!(
        out.contains("limina.console_shell"),
        "cat /proc/cmdline output: {out:?}"
    );

    // uname: a syscall, not a file — proves the guest *acts* on the command, not just echoes.
    let out = guest
        .console_command("uname", Duration::from_secs(10))
        .expect("uname command did not complete");
    assert!(out.contains("Linux"), "uname output: {out:?}");

    // An unknown command reports an error but must keep the channel alive (next command works).
    let out = guest
        .console_command("nonsuch", Duration::from_secs(10))
        .expect("unknown command did not complete");
    assert!(
        out.contains("unknown command"),
        "unknown-command output: {out:?}"
    );
    let out = guest
        .console_command("echo still-alive", Duration::from_secs(10))
        .expect("channel dead after an unknown command");
    assert!(out.contains("still-alive"), "post-error output: {out:?}");

    // `exit` leaves the loop; the init then powers off cleanly (PSCI → worker exit 0).
    guest.console_send("exit").expect("sending exit");
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
