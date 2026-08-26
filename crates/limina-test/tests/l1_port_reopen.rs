// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 regression test for the virtio-console **port-reopen** crash (found by the M12 SPICE
//! spike, `spikes/m12-spice-port/RESULTS.md`).
//!
//! The console device moved a port's rx/tx queues *out of itself* when the guest opened the
//! port (`VIRTIO_CONSOLE_PORT_OPEN`) and never returned them when the guest closed it. The
//! **second** open of the same port then unwrapped a `None`:
//!
//! ```text
//! panicked at .../virtio/console/device.rs: port rx queue should exist
//! ```
//!
//! …which aborts the worker and kills the whole VM. It is guest-triggerable by anything
//! that reopens a port — `systemctl restart spice-vdagentd`, a package update, or a plain
//! `dd` on `/dev/vportNpM` — and it hits `hvc0` and the `krun-std*` ports too, so this is a
//! baseline-tier durability bug, not a SPICE one.
//!
//! The guest (`limina.port_reopen`) opens a data port, closes it, and opens it again. The
//! assertion is simply that the guest lives to say so and the VM shuts down cleanly: before
//! the fix the VM died mid-probe and no `reopened` marker was ever printed.
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn l1_reopening_a_virtio_console_port_does_not_kill_the_vm() {
    if !limina_test::require_hvf_or_skip("l1_reopening_a_virtio_console_port_does_not_kill_the_vm")
    {
        return;
    }

    // The probe needs *a* virtio-console **data** port (`/dev/vportNpM`) to reopen; console
    // ports have no `port_fops` chardev to open, so `hvc0` alone would not do. Every spawn
    // now carries two named data ports — `com.redhat.spice.0` and `org.qemu.guest_agent.0` —
    // and the probe takes the first. The bug is not specific to either: anything that
    // reopens a port hits it, `systemctl restart qemu-guest-agent` included.
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .append_cmdline("limina.port_reopen");
    eprintln!(
        "booting L1 guest with the port-reopen probe: {:?}",
        cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("port_reopen: begin", Duration::from_secs(30))
        .expect("guest never entered the port-reopen probe");

    // A guest with no data port would silently "pass" every later assertion, so fail loudly.
    let console = guest.console();
    assert!(
        !console.contains("port_reopen NO_DATA_PORT"),
        "no /dev/vportNpM data port in the guest — the probe never exercised a reopen, so \
         this run proves nothing. Console:\n{console}"
    );

    guest
        .wait_for("RESULT: port_reopen opened OK", Duration::from_secs(20))
        .expect("the guest could not open the data port even once");

    // THE regression: the second open. Before the fix the VMM panicked here and the VM
    // vanished, so this wait timed out rather than failing an assertion.
    guest
        .wait_for("RESULT: port_reopen reopened OK", Duration::from_secs(20))
        .expect(
            "reopening the virtio-console port killed the VM (or failed) — the console \
             device did not return the port queues on close",
        );

    guest
        .wait_for("RESULT: port_reopen SURVIVED", Duration::from_secs(20))
        .expect("the probe did not run to completion");

    // The VM must still be healthy enough to power off on its own, not merely to have
    // printed the marker before dying.
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(!outcome.forced, "harness had to force teardown");
}
