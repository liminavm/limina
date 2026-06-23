// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! An unmodeled guest trap must **not** crash the VMM worker.
//!
//! libkrun's HVF vCPU loop used to `panic!` on anything it didn't expect — an unknown PSCI/SMC
//! function, an unhandled exception class, an unmodeled system register, an unexpected exit
//! reason. A `panic!` aborts the whole worker process (SIGABRT), taking the VM down hard. That
//! breaks the two-tier floor: a stock guest that probes an optional PSCI function (PSCI_FEATURES,
//! CPU_OFF, …) would crash the VMM instead of degrading gracefully.
//!
//! The fix: PSCI/SMC functions we don't model return `PSCI_RET_NOT_SUPPORTED` and the guest keeps
//! running; the other unhandled traps log and stop the VM cleanly instead of panicking.
//!
//! Vehicle: `spikes/hvf-trap-probe` — a ~96-byte bare-metal arm64 Image that issues an UNMODELED
//! PSCI function (`0x8400000A`, PSCI_FEATURES) and then a standard PSCI `SYSTEM_OFF`. Oracle: the
//! VM **powers itself off cleanly** (supervisor exits 0). Before the fix the first HVC panicked the
//! worker, so the VM never reached SYSTEM_OFF and the supervisor exited abnormally.
//!
//! Build the probe with `scripts/build-hvf-trap-probe.sh`. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn unmodeled_psci_does_not_crash_the_worker() {
    if !limina_test::require_hvf_or_skip("unmodeled_psci_does_not_crash_the_worker") {
        return;
    }

    let probe = limina_test::repo_root().join("target/hvf-trap-probe/Image");
    if !probe.exists() {
        eprintln!(
            "SKIP unmodeled_psci_does_not_crash_the_worker: {probe:?} missing \
             (build with scripts/build-hvf-trap-probe.sh)"
        );
        return;
    }

    let cfg = GuestConfig::raw_kernel(probe).expect("building the bare-metal probe config");
    let guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The probe self-terminates in microseconds: unknown PSCI HVC (→ NOT_SUPPORTED) then
    // SYSTEM_OFF. Wait for the supervisor to exit on its own — no signal sent.
    let outcome = guest
        .wait_for_exit(Duration::from_secs(30))
        .expect("waiting for the probe VM to power off");

    // GREEN: clean power-off (the unknown PSCI returned NOT_SUPPORTED and the guest fell through
    // to SYSTEM_OFF). RED (pre-fix): the unknown PSCI HVC panicked the worker (SIGABRT), so the VM
    // never powered off — the supervisor would exit non-zero / be force-killed here.
    assert_eq!(
        outcome.code,
        Some(0),
        "VM did not power off cleanly (outcome={outcome:?}) — an unmodeled PSCI call likely \
         crashed the worker instead of returning NOT_SUPPORTED"
    );
    assert_eq!(
        outcome.signal, None,
        "supervisor died by signal (outcome={outcome:?})"
    );
    assert!(
        !outcome.forced,
        "the probe VM had to be force-killed — it never powered itself off (outcome={outcome:?})"
    );
}
