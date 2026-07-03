// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A 4 KiB guest maps a host-visible blob whose size is NOT 16 KiB-aligned.
//!
//! The 16 KiB-host blob-map alignment bug (memory `limina-blob-map-16k-alignment`): hv_vm_map
//! rejects any size that is not a multiple of the HOST page (16 KiB on Apple Silicon) with
//! HV_BAD_ARGUMENT. A stock 4 KiB guest sizes host-visible blobs at its OWN page granularity,
//! so a blob of e.g. 0x21000 bytes (33 × 4 KiB, size%16k=4096) reaches
//! `Vm::add_mapping` → `HvfVm::map_memory` with an unmappable size and the guest's mmap fails
//! with EINVAL — first seen on a stock F44 desktop as `ResourceMapBlob -> ErrUnspec` +
//! `hv_vm_map failed … size%16k=4096` in the worker log. The fix rounds the mapped size up to
//! the host page granule in libkrun's HVF map/unmap wrappers (safe: host mmaps occupy whole
//! host pages, and any overlap-into-a-neighbor operation starts 16 KiB-misaligned and is
//! rejected before it can do damage).
//!
//! Vehicle: the L1 tiny guest (4 KiB kernel) with the coexist GPU. `limina.blob_probe` in
//! limina-init hand-rolls the exact Mesa-virgl sequence — virgl context init, an EXECBUFFER
//! creating an untyped persistently-mappable PIPE_BUFFER of the odd size, then
//! RESOURCE_CREATE_BLOB(HOST3D|MAPPABLE) and mmap — and emits one RESULT marker per step.
//! The first vram allocation of the boot lands at shm-window offset 0 (16 KiB-aligned), so
//! only the odd SIZE is under test, deterministically.
//!
//! Needs vrend's host GL (zink-on-KosmicKrisp) to allocate the buffer storage, so it SKIPs
//! without the machine-local KK ICD / zink Mesa prefix (same as virgl.rs). Gated behind
//! LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn odd_size_blob_maps_into_a_4k_guest() {
    if !limina_test::require_hvf_or_skip("odd_size_blob_maps_into_a_4k_guest") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED odd_size_blob_maps_into_a_4k_guest: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    if limina_test::zink_kk_mesa_prefix().is_none() {
        eprintln!(
            "SKIPPED odd_size_blob_maps_into_a_4k_guest: no zink-on-KK Mesa prefix \
             (build spikes/virgl-zink-kk/build-mesa-zink-kk.sh; or set MESA_PREFIX)"
        );
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_coexist_display(1024, 768)
        .with_cmdline_token("limina.blob_probe")
        .with_supervisor_log();
    eprintln!("booting L1 blob-probe guest (coexist GPU): {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("blob_probe: begin", Duration::from_secs(30))
        .expect("guest did not start the blob probe");

    // The scaffolding steps must all pass — a FAIL here means the probe's vehicle broke
    // (no card0 / capset / vrend storage), not the bug under test. Waiting for each OK
    // marker keeps a scaffolding failure distinguishable from the map failure below.
    for step in [
        "blob_card0",
        "blob_ctx_init",
        "blob_get_caps",
        "blob_execbuffer",
        "blob_create",
        "blob_map_offset",
    ] {
        let marker = format!("RESULT: {step} OK");
        guest
            .wait_for(&marker, Duration::from_secs(20))
            .unwrap_or_else(|e| panic!("blob-probe scaffolding step {step} failed: {e}"));
        eprintln!("  ✓ {step}");
    }

    // THE assertion: the odd-size blob must map. RED (pre-fix): `RESULT: blob_map FAIL
    // errno=22` arrives instead and this wait times out — dump the worker log tail so the
    // host-side failure (e.g. `hv_vm_map failed … size%16k=4096`) is visible in the output.
    if let Err(e) = guest.wait_for("RESULT: blob_map OK", Duration::from_secs(20)) {
        let log = guest.supervisor_log();
        panic!(
            "the 0x21000-byte (4k-but-not-16k-aligned) blob did not map into the guest: {e}\n\
             worker log tail:\n{}",
            log.lines()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    guest
        .wait_for("RESULT: blob_rw OK", Duration::from_secs(10))
        .expect("the mapped blob was not readable/writable end to end");
    guest
        .wait_for("blob_probe: done", Duration::from_secs(10))
        .expect("blob probe did not finish");

    // Defense in depth: the worker must not have logged ANY hv mapping failure. The probe's
    // GEM close (before "done", already waited on) drives the host-side unmap, so this also
    // guards the paired unmap (`Error removing memory map`) staying consistent with the
    // rounded map.
    let log = guest.supervisor_log();
    for needle in [
        "hv_vm_map failed",
        "Error adding memory map",
        "Error removing memory map",
    ] {
        assert!(
            !log.contains(needle),
            "worker logged a mapping failure ({needle}); tail:\n{}",
            log.lines()
                .rev()
                .take(25)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    // The init powers off cleanly after the probe.
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(
        !outcome.forced,
        "harness had to force teardown: {outcome:?}"
    );
    assert_eq!(
        outcome.code,
        Some(0),
        "expected clean power-off: {outcome:?}"
    );
}
