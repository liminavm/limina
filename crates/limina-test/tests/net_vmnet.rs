// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 networking test: the stock Fedora image on a vmnet shared (NAT) NIC (`--net-vmnet shared`,
//! `docs/design/multi-vm-networking.md` Phase 1).
//!
//! The worker starts the vmnet interface itself; no gateway process runs, so there is no packet
//! log to read. The host-side oracle is macOS's own DHCP server: its lease file names the guest's
//! MAC once the guest has leased. Everything after the lease runs over SSH to the guest's own
//! address, through Apple's `/usr/bin/ssh` (see [`limina_test::ssh_exec_at`]).
//!
//! The bulk copies in both directions check the guest's offloads end to end: large segments and
//! partial checksums go through vmnet in its virtio-header mode, and a frame corrupted on the way
//! fails the digest (or stalls the copy) instead of passing unnoticed.
//!
//! A host VPN can take a vmnet shared network off the Internet
//! (`spikes/vmnet-network-probe/RESULTS.md`): a failure here under one is not a limina failure.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::fs;
use std::io::Write;
use std::net::Ipv4Addr;
use std::process::Command;
use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig, ssh_exec_at, vmnet_lease};

/// A random locally administered MAC that holds no lease yet: bootpd keeps old leases, and a
/// stale one would end the lease wait before this guest had booted.
fn test_mac() -> String {
    loop {
        let mut b = [0u8; 3];
        // SAFETY: arc4random_buf fills exactly the buffer it is given.
        unsafe { libc::arc4random_buf(b.as_mut_ptr().cast(), b.len()) };
        let mac = format!("5a:76:6d:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2]);
        if vmnet_lease(&mac).is_none() {
            return mac;
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    let dir = tempfile_dir();
    let path = dir.join("blob");
    fs::write(&path, bytes).unwrap();
    let out = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(&path)
        .output()
        .expect("running shasum");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("limina-net-vmnet-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn fedora_on_vmnet_shared_leases_reaches_the_internet_and_moves_bulk_intact() {
    if !limina_test::require_hvf_or_skip(
        "fedora_on_vmnet_shared_leases_reaches_the_internet_and_moves_bulk_intact",
    ) {
        return;
    }

    let mac = test_mac();
    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_supervisor_log()
        .with_supervisor_arg("--net-vmnet")
        .with_supervisor_arg("shared")
        .with_supervisor_arg("--net-mac")
        .with_supervisor_arg(&mac);
    eprintln!("booting Fedora on a vmnet shared NIC, guest MAC {mac}");
    let guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The lease: firmware → GRUB → kernel → systemd → NetworkManager, so be patient.
    let deadline = Instant::now() + Duration::from_secs(180);
    let ip: Ipv4Addr = loop {
        if let Some(ip) = vmnet_lease(&mac) {
            break ip;
        }
        assert!(
            Instant::now() < deadline,
            "the guest never leased from vmnet's DHCP server; supervisor log:\n{}",
            guest.supervisor_log()
        );
        std::thread::sleep(Duration::from_secs(1));
    };
    let host = ip.to_string();
    eprintln!("guest leased {host}");

    // A lease names the address; the session proves the guest holds it.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match ssh_exec_at(&host, "true", None, Duration::from_secs(30)) {
            Ok(_) => break,
            Err(e) if Instant::now() >= deadline => {
                panic!("no SSH session to the guest at {host}: {e:#}")
            }
            Err(_) => std::thread::sleep(Duration::from_secs(1)),
        }
    }

    // Out through vmnet's NAT: a name resolved by the gateway's DNS proxy, then HTTPS.
    let run = |cmd: &str| {
        String::from_utf8_lossy(
            &ssh_exec_at(&host, cmd, None, Duration::from_secs(60))
                .unwrap_or_else(|e| panic!("`{cmd}` in the guest: {e:#}")),
        )
        .trim()
        .to_string()
    };
    let resolved = run("getent hosts one.one.one.one");
    assert!(!resolved.is_empty(), "the guest resolved nothing");
    let status =
        run("curl -sS -o /dev/null -w '%{http_code}' --max-time 30 https://one.one.one.one/");
    assert!(
        status.starts_with('2') || status.starts_with('3'),
        "HTTPS from the guest answered {status:?}"
    );

    // Host → guest, 64 MiB of random bytes, checked on arrival.
    let mut blob = vec![0u8; 64 << 20];
    // SAFETY: arc4random_buf fills exactly the buffer it is given.
    unsafe { libc::arc4random_buf(blob.as_mut_ptr().cast(), blob.len()) };
    let path = tempfile_dir().join("upload");
    fs::File::create(&path)
        .and_then(|mut f| f.write_all(&blob))
        .expect("writing the upload");
    let arrived = String::from_utf8_lossy(
        &ssh_exec_at(
            &host,
            "sha256sum | cut -d' ' -f1",
            Some(&path),
            Duration::from_secs(300),
        )
        .expect("copying 64 MiB to the guest"),
    )
    .trim()
    .to_string();
    assert_eq!(arrived, sha256(&blob), "the upload arrived corrupted");

    // Guest → host, the same size from the guest's own random source.
    let digest =
        run("head -c 67108864 /dev/urandom > /tmp/blob && sha256sum /tmp/blob | cut -d' ' -f1");
    let fetched = ssh_exec_at(&host, "cat /tmp/blob", None, Duration::from_secs(300))
        .expect("copying 64 MiB from the guest");
    assert_eq!(fetched.len(), 64 << 20, "the download came back short");
    assert_eq!(sha256(&fetched), digest, "the download arrived corrupted");

    let _ = fs::remove_dir_all(tempfile_dir());
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}
