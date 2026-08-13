// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The guest must not be told it has SME.
//!
//! Apple Silicon from M4 on implements SME **without** SVE. Guest userspace
//! generally does not expect that combination: Chrome 151's SME path issues a
//! non-streaming SVE `cntd`, which UNDEFs there, and the renderer takes a SIGILL
//! (2026-08-13 dogfood report). Under nested virt the same advertisement breaks
//! the guest outright once it enables the MMU — libkrun already masked SME for
//! that reason, but only on the EL2 path, so ordinary guests still saw it.
//!
//! The masking happens at vCPU init (`hvf::HvfVcpu::set_initial_state`, clearing
//! `ID_AA64PFR1_EL1.SME`), and it is genuinely observed by the guest — proven by
//! masking a field this dev Mac *does* have and watching it disappear from the
//! guest's `/proc/cpuinfo` (`spikes/sme-mask/RESULTS.md`).
//!
//! HONESTY NOTE about what this test can catch. It can only fail on a host that
//! HAS SME — on an M1/M2/M3 the host advertises none, so the guest cannot see
//! any and the assertion passes no matter what the code does. It prints exactly
//! that when `hw.optional.arm.FEAT_SME` is 0, so a green run on this dev Mac is
//! never mistaken for coverage. On an M4-class host it is a real guard.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::process::Command;
use std::time::Duration;

use limina_test::{Guest, GuestConfig};

fn host_has_sme() -> bool {
    Command::new("sysctl")
        .args(["-n", "hw.optional.arm.FEAT_SME"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false)
}

#[test]
fn sme_is_not_advertised_to_the_guest() {
    if !limina_test::require_hvf_or_skip("sme_is_not_advertised_to_the_guest") {
        return;
    }

    let has_sme = host_has_sme();
    if !has_sme {
        eprintln!(
            "NOTE sme_is_not_advertised_to_the_guest: this host reports \
             hw.optional.arm.FEAT_SME=0, so the guest could not see SME even \
             unmasked — the assertion below cannot fail here. Running it anyway \
             (it costs one boot and guards the M4-class hosts), but do NOT read \
             a green result on this machine as coverage."
        );
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg.with_net(),
        Err(e) => {
            eprintln!("SKIPPED sme_is_not_advertised_to_the_guest: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");

    // The kernel prints one Features line per CPU, built from the ID registers
    // it read at boot — so this is the guest's own view of what it was told.
    let features = guest
        .ssh_exec("grep -m1 '^Features' /proc/cpuinfo")
        .expect("reading /proc/cpuinfo in the guest");
    eprintln!("guest features: {features}");

    let has = |flag: &str| features.split_whitespace().any(|f| f == flag);

    assert!(
        !has("sme"),
        "the guest was told it has SME. On an SME-without-SVE host that makes \
         guest userspace (Chrome's SME path, at least) issue SVE instructions \
         that UNDEF — and under nested virt it breaks the guest at MMU enable. \
         Check that the ID_AA64PFR1_EL1.SME mask in hvf's set_initial_state \
         still runs on the NON-nested path.\nFeatures: {features}"
    );

    // Sanity that we are reading a real feature line at all, so a parsing or
    // ssh mishap cannot masquerade as "SME absent".
    assert!(
        has("asimd"),
        "no 'asimd' in the guest's feature list — this line is not what we think \
         it is, so the SME assertion above proves nothing.\nFeatures: {features}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
