// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A snapshot whose vCPU save fails must leave the guest running.
//!
//! Every vCPU parks when it answers a snapshot, whether its save worked or not. If one fails, the
//! snapshot is abandoned and the abort path resumes the parked vCPUs — which only works if the VM
//! knows they are parked. It did not: the failure returned before the VM was marked paused, so
//! the resume was a no-op, every vCPU stayed parked and the abort still logged "guest resumed".
//! Seen on a macOS 27 host, where HVF refuses to read a vCPU's pending interrupt lines and so
//! every vCPU's save failed: a guest suspend left the VM wedged for good.
//!
//! `LIMINA_TEST_FAIL_VCPU_SAVE=<id>` makes that one vCPU's save fail. Oracle: the guest's own
//! counter heartbeat keeps advancing after the abort.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

fn last_mono_ms(console: &str) -> Option<u64> {
    console.lines().rev().find_map(|line| {
        let rest = &line[line.find("mono_ms=")? + "mono_ms=".len()..];
        rest.split_whitespace().next()?.parse().ok()
    })
}

#[test]
fn a_failed_vcpu_save_leaves_the_guest_running() {
    if !limina_test::require_hvf_or_skip("a_failed_vcpu_save_leaves_the_guest_running") {
        return;
    }
    let mut config = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .append_cmdline("limina.counter")
        .with_supervisor_log()
        .with_snapshot()
        .with_env("LIMINA_RAW_SNAPSHOT_SEAM", "1")
        .with_env("LIMINA_TEST_FAIL_VCPU_SAVE", "1");
    config.cpus = 2;
    config.ram_mib = 512;
    let mut guest = Guest::boot(&config).expect("spawning the supervisor");
    guest
        .wait_for("LIMINA_COUNTER_READY", Duration::from_secs(20))
        .expect("counter guest did not reach userspace");
    guest
        .wait_for("mono_ms=", Duration::from_secs(5))
        .expect("no counter heartbeat");

    guest.snapshot().expect("triggering the snapshot");
    guest
        .wait_for_supervisor_log("vCPU 1 snapshot save failed", Duration::from_secs(30))
        .expect("the injected vCPU save failure never happened");
    guest
        .wait_for_supervisor_log(
            "snapshot: aborted and guest resumed",
            Duration::from_secs(30),
        )
        .expect("the abort path never finished");

    let after_abort = last_mono_ms(&guest.console()).expect("a heartbeat before the abort");
    let deadline = Instant::now() + Duration::from_secs(20);
    let ran = loop {
        if last_mono_ms(&guest.console()).is_some_and(|m| m > after_abort + 2000) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let _ = guest.shutdown(Duration::from_secs(10));
    assert!(
        ran,
        "the guest stopped running after the failed snapshot: its vCPUs stayed parked"
    );
}
