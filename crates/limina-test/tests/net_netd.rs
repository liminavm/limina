// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 networking test: the stock Fedora image on a NIC served by limina-netd (`--net-netd`,
//! `docs/design/multi-vm-networking.md` §3.2, Phase 3).
//!
//! The worker spawns netd itself and shares two frame rings with it; netd's log lands in the
//! worker's, which reaches the supervisor's. The oracle is netd's own account of the DHCP
//! handshake: it logs a lease only on the guest's REQUEST, so the line proves the whole exchange
//! crossed the rings both ways and the guest's DHCP client took the offer.
//!
//! Past the link, the guest's own boot is the traffic. chronyd resolves the NTP pool through the
//! gateway's DNS (answered by the host's resolver) and asks the servers it gets over UDP, and
//! NetworkManager's connectivity check makes an HTTP request: netd's account of an answered
//! question with records, of a reply from port 123, and of a port-80 connection from connect to
//! close proves DNS, UDP and TCP out to the Internet and back to the guest.
//!
//! It also holds netd to its lifecycle: killed, it is replaced by the worker without the VM
//! noticing more than a dropped link, and no netd outlives the VM.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const STARTED: &str = "started limina-netd (pid ";

/// The pids of every netd the worker reports starting, in order.
fn netd_pids(log: &str) -> Vec<libc::pid_t> {
    log.match_indices(STARTED)
        .filter_map(|(i, _)| {
            let rest = &log[i + STARTED.len()..];
            rest[..rest.find(')')?].parse().ok()
        })
        .collect()
}

fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only probes for the process.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Polls `f` until it holds or `timeout` passes.
fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    f()
}

#[test]
fn fedora_on_netd_resolves_and_reaches_out_and_netd_is_replaced_when_killed() {
    if !limina_test::require_hvf_or_skip(
        "fedora_on_netd_resolves_and_reaches_out_and_netd_is_replaced_when_killed",
    ) {
        return;
    }

    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_supervisor_log()
        .with_supervisor_arg("--net-netd")
        .with_env("RUST_LOG", "info,limina_netd=debug");
    eprintln!("booting Fedora on a limina-netd NIC");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Firmware → GRUB → kernel → systemd → NetworkManager, so be patient.
    guest
        .wait_for_supervisor_log("DHCP: leased 192.168.127.2", Duration::from_secs(180))
        .unwrap_or_else(|e| panic!("the guest never leased from limina-netd: {e:#}"));

    // The NTP pool, resolved and asked.
    let resolved = |log: &str| {
        log.lines().any(|l| {
            l.contains("DNS: answered")
                && !l.contains("rcode 0 and 0 records")
                && l.contains("rcode 0")
        })
    };
    assert!(
        wait_until(Duration::from_secs(120), || resolved(
            &guest.supervisor_log()
        )),
        "no DNS question answered with records; supervisor log:\n{}",
        guest.supervisor_log()
    );
    assert!(
        wait_until(Duration::from_secs(120), || guest
            .supervisor_log()
            .lines()
            .any(|l| l.contains(":123 answered the guest's port"))),
        "no NTP reply reached the guest; supervisor log:\n{}",
        guest.supervisor_log()
    );

    // NetworkManager's connectivity check: an HTTP request at link-up, which netd carries from
    // its connect to both sides' close.
    let http = |log: &str, what: &str| {
        log.lines()
            .any(|l| l.contains("TCP") && l.contains(":80") && l.contains(what))
    };
    assert!(
        wait_until(Duration::from_secs(120), || {
            let log = guest.supervisor_log();
            http(&log, "connected") && http(&log, "closed")
        }),
        "no HTTP connection carried from connect to close; supervisor log:\n{}",
        guest.supervisor_log()
    );
    let first = *netd_pids(&guest.supervisor_log())
        .first()
        .expect("the worker logged no netd start");
    assert!(alive(first), "netd {first} is not running");

    // netd dies; the worker notices the hang-up and starts another on a fresh channel.
    // SAFETY: a process this VM's worker started, named in its log.
    unsafe { libc::kill(first, libc::SIGKILL) };
    assert!(
        wait_until(Duration::from_secs(10), || netd_pids(
            &guest.supervisor_log()
        )
        .len()
            >= 2),
        "no new netd after killing {first}; supervisor log:\n{}",
        guest.supervisor_log()
    );
    let second = netd_pids(&guest.supervisor_log())[1];
    assert_ne!(second, first);
    assert!(
        alive(second),
        "the replacement netd {second} is not running"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
    // The worker's exit closes netd's channel, and netd exits on the EOF.
    assert!(
        wait_until(Duration::from_secs(5), || !alive(second)),
        "netd {second} outlived its VM"
    );
}
