// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! ≥7.1-kernel virtiofs `--share` guard (task #36).
//!
//! Regression guard for the virtio-fs used-ring-length bug (libkrun 0090). The fs worker used
//! to complete every request with `add_used(.., 0)`; Linux ≥7.1 added `virtio_fs_verify_response`,
//! which rejects any FUSE reply whose used length doesn't cover the out-header (`-EIO` → latches
//! `fc->conn_error` → surfaces as `fsconfig() failed: Connection refused` at `mount(2)`). That
//! bricked every share on a ≥7.1 guest — and it escaped review because **no automated test ran a
//! share on a ≥7.1 kernel**: L1 (`l1_share`) uses libkrunfw's bundled 6.12; the enhanced/seated L2
//! configs inject the 6.12 `Image-16k`; only the un-tested EFI path runs the real 7.1.4.
//!
//! This closes that gap with the light option from the task: a fast injected-kernel L2 booting a
//! **≥7.1** 16 KiB test kernel (`Image-16k-71`, distinct from the venus suite's 6.12 kernel so it
//! runs on its validated kernel), NAT + SSH, and a `mount -t virtiofs` of both a read-write and a
//! read-only share. On a pre-0090 worker the share is dead — `Connection refused` at mount or at
//! the first file access (RED-verified: mount returns, the first read fails); with the fix it
//! mounts, reads the host-staged file, and round-trips a write, while the read-only share still
//! refuses writes (GREEN).
//!
//! SKIPs cleanly if the ≥7.1 kernel or the disk is missing — build the kernel with
//! `KVER=v7.1 PAGESIZE=16k KIMAGE_NAME=Image-16k-71 PATCHES_OPTIONAL=1 scripts/build-test-kernel.sh`.
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// Parse the leading `MAJOR.MINOR` of a `uname -r` string into a comparable tuple.
fn kernel_major_minor(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.trim().split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    // The minor may carry a suffix (e.g. "1-dirty", "4-limina16k"); take the leading digits.
    let minor_field = parts.next()?;
    let minor_digits: String = minor_field
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let minor: u32 = minor_digits.parse().ok()?;
    Some((major, minor))
}

#[test]
fn share_mounts_and_round_trips_on_71_kernel() {
    if !limina_test::require_hvf_or_skip("share_mounts_and_round_trips_on_71_kernel") {
        return;
    }

    // Stage the read-write share with a file the guest must be able to read back.
    let share_dir = std::env::temp_dir().join(format!("limina-share71-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&share_dir);
    std::fs::create_dir_all(&share_dir).expect("creating the share dir");
    let ping = format!("ping-from-host-{}", std::process::id());
    std::fs::write(share_dir.join("ping"), &ping).expect("staging ping");

    // A second, read-only share: reads must work, writes must be refused.
    let ro_dir = std::env::temp_dir().join(format!("limina-share71-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ro_dir);
    std::fs::create_dir_all(&ro_dir).expect("creating the ro share dir");
    std::fs::write(ro_dir.join("ping"), "ro-ping").expect("staging ro ping");

    // The ≥7.1 injected-kernel enhanced path + NAT + both shares. No display: this exercises
    // virtio-fs, not venus, so it needs neither KosmicKrisp nor a coexist GPU (and doesn't SKIP
    // when they're absent — a lean, always-on guard once the kernel artifact exists).
    let cfg = match GuestConfig::enhanced_share_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_share("testshare", &share_dir)
            .with_share_ro("roshare", &ro_dir),
        Err(e) => {
            eprintln!("SKIPPED share_mounts_and_round_trips_on_71_kernel: {e}");
            return;
        }
    };
    eprintln!("booting Fedora on the ≥7.1 16 KiB kernel (NAT + rw/ro shares)");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Full Fedora userspace boot on the injected kernel (systemd → NM → sshd) takes a while.
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // Prove the premise this guard rests on: the guest kernel is actually ≥7.1 (the version that
    // verifies the used length). A 6.12 guest would mount fine even on the pre-0090 worker, so a
    // green mount below would prove nothing without this.
    let release = guest
        .ssh_exec("uname -r")
        .expect("reading guest kernel release");
    let (major, minor) =
        kernel_major_minor(&release).unwrap_or_else(|| panic!("unparseable uname -r: {release:?}"));
    assert!(
        (major, minor) >= (7, 1),
        "this guard must run on a ≥7.1 kernel (got {release:?}); build Image-16k-71 from a ≥7.1 source"
    );
    eprintln!("guest kernel {release:?} (>= 7.1, verifies the virtio-fs used length)");

    // Mount the read-write share, then read a file through it. This is the fix under test: on a
    // pre-0090 worker the zero used-length latches `fc->conn_error`, so the whole share is dead —
    // the failure surfaces as ECONNREFUSED ("Connection refused") either at mount(2) or at the
    // first file access (RED-verified: mount returns MOUNT_OK, the read below then fails). Both
    // steps `.expect(...)` so either failure point turns this guard red.
    let mount_out = guest
        .ssh_exec(
            "sudo mkdir -p /mnt/testshare && \
             sudo mount -t virtiofs limina-testshare /mnt/testshare && echo MOUNT_OK",
        )
        .expect(
            "mounting the virtiofs share failed — on a pre-0090 worker this is the \
             `Connection refused` (used-len) regression this test guards",
        );
    assert!(
        mount_out.contains("MOUNT_OK"),
        "virtiofs mount did not report success: {mount_out:?}"
    );

    // Guest reads the host-staged file (host → guest). The pre-0090 `Connection refused` lands
    // here (the FUSE connection is dead even though the mount call itself returned).
    let read_back = guest
        .ssh_exec("cat /mnt/testshare/ping")
        .expect("reading the shared file in the guest (pre-0090: `Connection refused`)");
    assert_eq!(
        read_back.trim(),
        ping,
        "the shared file did not round-trip host → guest"
    );

    // Guest write reaches the host directory (guest → host).
    guest
        .ssh_exec(&format!(
            "printf '%s' '{ping}+guest' | sudo tee /mnt/testshare/pong >/dev/null"
        ))
        .expect("writing pong from the guest");

    // The read-only share: mount + read work, writes are refused.
    let ro_mount = guest
        .ssh_exec(
            "sudo mkdir -p /mnt/roshare && \
             sudo mount -t virtiofs limina-roshare /mnt/roshare && echo RO_MOUNT_OK",
        )
        .expect("mounting the read-only virtiofs share");
    assert!(
        ro_mount.contains("RO_MOUNT_OK"),
        "read-only virtiofs mount did not report success: {ro_mount:?}"
    );
    let ro_read = guest
        .ssh_exec("cat /mnt/roshare/ping")
        .expect("reading the read-only share in the guest");
    assert_eq!(
        ro_read.trim(),
        "ro-ping",
        "read-only share read did not round-trip"
    );
    // A write attempt must fail; swallow the non-zero exit so the test proceeds to the host assert.
    let _ = guest.ssh_exec("sudo sh -c 'echo intruder > /mnt/roshare/intruder' 2>&1; true");

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");

    // Host-side proof the write actually reached the host directory.
    let pong = std::fs::read_to_string(share_dir.join("pong")).expect("guest never wrote pong");
    assert_eq!(
        pong,
        format!("{ping}+guest"),
        "pong content does not round-trip the ping"
    );

    // The read-only share must not have been written to.
    assert!(
        !ro_dir.join("intruder").exists(),
        "the read-only share accepted a guest write"
    );

    let _ = std::fs::remove_dir_all(&share_dir);
    let _ = std::fs::remove_dir_all(&ro_dir);
}
