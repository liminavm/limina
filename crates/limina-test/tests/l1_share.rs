// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M5 virtio-fs file sharing: `limina --share` exposes a host directory in the guest,
//! read/write both ways.
//!
//! The L1 init auto-mounts every `limina-`-tagged virtiofs device at `/media/<name>` —
//! the same convention the product `limina-agent` implements on a real distro. With the
//! `limina.sharecheck=<name>` cmdline token it then proves the share end-to-end: reads
//! `/media/<name>/ping` (staged by this test on the host), writes its content back to
//! `/media/<name>/pong` plus its own suffix, and logs a `LIMINA_SHARE_OK` marker. The
//! host asserts the marker (guest read the host's file) and the pong file (guest write
//! reached the host dir).

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn l1_share_read_write_both_ways() {
    if !limina_test::require_hvf_or_skip("l1_share_read_write_both_ways") {
        return;
    }

    let cfg = GuestConfig::l1_from_env().expect("resolving L1 guest config");

    // Stage the shared host directory with the file the guest must be able to read.
    let share_dir = std::env::temp_dir().join(format!("limina-share-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&share_dir);
    std::fs::create_dir_all(&share_dir).expect("creating the share dir");
    let ping = format!("ping-from-host-{}", std::process::id());
    std::fs::write(share_dir.join("ping"), &ping).expect("staging ping");

    // A second, read-only share: reads must work, writes must be refused.
    let ro_dir = std::env::temp_dir().join(format!("limina-share-ro-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ro_dir);
    std::fs::create_dir_all(&ro_dir).expect("creating the ro share dir");
    std::fs::write(ro_dir.join("ping"), "ro-ping").expect("staging ro ping");

    let cfg = cfg
        .with_share("testshare", &share_dir)
        .with_share_ro("roshare", &ro_dir)
        .with_cmdline_token("limina.sharecheck=testshare")
        .with_cmdline_token("limina.sharecheck_ro=roshare");
    eprintln!("booting L1 guest with --share {share_dir:?} + ro {ro_dir:?}");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Guest read our file: the marker carries the ping content back over the console.
    guest
        .wait_for(&format!("LIMINA_SHARE_OK {ping}"), Duration::from_secs(15))
        .expect("guest never proved the share (LIMINA_SHARE_OK marker)");
    guest
        .wait_for("LIMINA_SHARE_RO_OK", Duration::from_secs(15))
        .expect("guest never proved the read-only share (LIMINA_SHARE_RO_OK marker)");

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");

    // Guest write reached the host directory.
    let pong = std::fs::read_to_string(share_dir.join("pong")).expect("guest never wrote pong");
    assert_eq!(
        pong,
        format!("{ping}+guest"),
        "pong content does not round-trip the ping"
    );

    // The ro share must not have been written to.
    assert!(
        !ro_dir.join("intruder").exists(),
        "the read-only share accepted a guest write"
    );

    let _ = std::fs::remove_dir_all(&share_dir);
    let _ = std::fs::remove_dir_all(&ro_dir);
}
