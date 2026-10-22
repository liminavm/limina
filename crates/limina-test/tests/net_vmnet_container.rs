// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 networking test: a VM on vmnet shared keeps its network while Apple's `container` starts a
//! VM on its own vmnet network, and a VM started afterwards still gets one
//! (docs/hardening-backlog.md, "vmnet interfaces leak until the host refuses new ones").
//!
//! `container`'s `default` network is 192.168.64.0/24, the subnet vmnet shared mode takes when
//! not told otherwise. When both claim it, InternetSharing deletes the bridge the running VM is
//! on, and every vmnet interface started afterwards fails to join a bridge and is never cleaned
//! up. Recovering takes `sudo killall InternetSharing`, so this test only runs when asked
//! (`LIMINA_CONTAINER_COLLISION_TEST=1`) on a host with `container` installed and its system
//! started.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::net::Ipv4Addr;
use std::process::Command;
use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig, ssh_exec_at, vmnet_lease};

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

/// Boot Fedora on a vmnet shared NIC and wait until it holds a lease and takes an SSH session.
fn boot_on_vmnet(what: &str) -> (Guest, String) {
    let mac = test_mac();
    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_supervisor_log()
        .with_supervisor_arg("--net-vmnet")
        .with_supervisor_arg("shared")
        .with_supervisor_arg("--net-mac")
        .with_supervisor_arg(&mac);
    let guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let deadline = Instant::now() + Duration::from_secs(180);
    let ip: Ipv4Addr = loop {
        if let Some(ip) = vmnet_lease(&mac) {
            break ip;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never leased from vmnet's DHCP server; supervisor log:\n{}",
            guest.supervisor_log()
        );
        std::thread::sleep(Duration::from_secs(1));
    };
    let host = ip.to_string();
    assert!(
        reachable(&host, Duration::from_secs(120)),
        "{what}: no SSH session at {host}"
    );
    eprintln!("{what} on vmnet shared at {host}");
    (guest, host)
}

fn reachable(host: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if ssh_exec_at(host, "true", None, Duration::from_secs(10)).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn vmenet_count() -> usize {
    let out = Command::new("/sbin/ifconfig")
        .arg("-l")
        .output()
        .expect("listing interfaces");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter(|i| i.starts_with("vmenet"))
        .count()
}

#[test]
fn vmnet_shared_survives_a_container_starting_on_its_default_network() {
    let name = "vmnet_shared_survives_a_container_starting_on_its_default_network";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    if std::env::var_os("LIMINA_CONTAINER_COLLISION_TEST").is_none() {
        eprintln!("SKIPPED {name}: set LIMINA_CONTAINER_COLLISION_TEST=1 to run it");
        return;
    }
    let image = std::env::var("LIMINA_CONTAINER_IMAGE").unwrap_or_else(|_| "alpine:3.22".into());

    let (first, first_host) = boot_on_vmnet("the first VM");

    let run = Command::new("container")
        .args(["run", "--rm", &image, "true"])
        .output()
        .expect("running `container` (is it installed?)");
    eprintln!(
        "container run {image}: {} {}",
        run.status,
        String::from_utf8_lossy(&run.stderr).trim()
    );
    assert!(run.status.success(), "the container itself did not run");

    assert!(
        reachable(&first_host, Duration::from_secs(30)),
        "the first VM lost its network when the container started; supervisor log:\n{}",
        first.supervisor_log()
    );

    let before = vmenet_count();
    let (second, _) = boot_on_vmnet("a VM started after the container");
    drop(second);
    std::thread::sleep(Duration::from_secs(3));
    let after = vmenet_count();
    assert!(
        after < before + 1,
        "a vmnet interface outlived its VM: {before} before it, {after} after"
    );
    drop(first);
}
