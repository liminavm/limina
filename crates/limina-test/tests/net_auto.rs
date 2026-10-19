// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 networking test: the stock Fedora image on a NIC that sits on vmnet shared and moves to
//! limina-netd when vmnet cannot serve (`--net-auto`, `docs/design/multi-vm-networking.md` §3.5,
//! Phase 4).
//!
//! The worker logs every move (`network: on vmnet`, `network: on limina-netd`), and the guest's
//! own address says which side it took its lease from: vmnet's subnet, or netd's 192.168.127.2.
//! The SSH forward is netd's on both sides, so every step after the boot runs through it, and
//! each side is shown to reach the Internet.
//!
//! The moves are forced through the `net-switch` lever, the way a VPN taking vmnet's route would
//! force one; a vmnet that will not start is forced with `LIMINA_NET_VMNET_FAIL`.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig, vmnet_lease};

/// A random locally administered MAC that holds no vmnet lease yet: bootpd keeps old leases, and
/// a stale one would end the lease wait before this guest had booted.
fn test_mac() -> String {
    loop {
        let mut b = [0u8; 3];
        // SAFETY: arc4random_buf fills exactly the buffer it is given.
        unsafe { libc::arc4random_buf(b.as_mut_ptr().cast(), b.len()) };
        let mac = format!("5a:76:6e:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2]);
        if vmnet_lease(&mac).is_none() {
            return mac;
        }
    }
}

fn config(mac: &str) -> GuestConfig {
    GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net_auto()
        .with_net_mac(mac)
        .with_supervisor_log()
        .with_ssh_target_check()
        .with_env("LIMINA_NET_SWITCH", "1")
        .with_env("RUST_LOG", "warn,limina_vmm=info,limina_netd=info")
}

/// Polls until `f` holds, for at most `timeout`.
fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    f()
}

/// How many times the supervisor log has `needle`.
fn count(guest: &Guest, needle: &str) -> usize {
    guest.supervisor_log().matches(needle).count()
}

/// The guest's IPv4 address on eth0, through the SSH forward; `None` while it has none or SSH
/// does not answer.
fn guest_addr(guest: &Guest) -> Option<String> {
    let out = guest
        .ssh_exec_timeout(
            "ip -4 -o addr show dev eth0 | awk '{print $4}' | cut -d/ -f1",
            Duration::from_secs(20),
        )
        .ok()?;
    let addr = out.trim().to_string();
    (!addr.is_empty()).then_some(addr)
}

/// The guest reaches the Internet: a name resolved and an HTTPS answer. Right after a move the
/// guest's resolver may still be settling on the new side's server, so it gets a minute.
fn reaches_out(guest: &Guest, side: &str) {
    let started = Instant::now();
    let mut last = String::new();
    while started.elapsed() < Duration::from_secs(60) {
        match guest.ssh_exec_timeout(
            "curl -sS -o /dev/null -w '%{http_code}' --max-time 20 https://one.one.one.one/",
            Duration::from_secs(40),
        ) {
            Ok(status) if status.trim().starts_with('2') || status.trim().starts_with('3') => {
                eprintln!("HTTPS on {side} after {:?}", started.elapsed());
                return;
            }
            Ok(status) => last = format!("answered {status:?}"),
            Err(e) => last = format!("{e:#}"),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let resolver = guest
        .ssh_exec("resolvectl status eth0; cat /etc/resolv.conf")
        .unwrap_or_default();
    panic!(
        "no HTTPS from the guest on {side}: {last}\nresolver:\n{resolver}\nsupervisor log:\n{}",
        guest.supervisor_log()
    );
}

/// Asks the supervisor to move the NIC.
fn switch(cfg: &GuestConfig, guest: &Guest, side: &str) {
    let out = std::process::Command::new(&cfg.limina_bin)
        .args(["net-switch", &guest.supervisor_pid().to_string(), side])
        .output()
        .expect("running limina net-switch");
    assert!(
        out.status.success(),
        "limina net-switch {side} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fedora_on_vmnet_moves_to_netd_and_back_with_ssh_throughout() {
    if !limina_test::require_hvf_or_skip(
        "fedora_on_vmnet_moves_to_netd_and_back_with_ssh_throughout",
    ) {
        return;
    }
    let mac = test_mac();
    let cfg = config(&mac);
    eprintln!("booting Fedora on a --net-auto NIC, guest MAC {mac}");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // On vmnet from the start: its DHCP server leases the guest an address of its own.
    guest
        .wait_for_supervisor_log("network: on vmnet", Duration::from_secs(180))
        .unwrap_or_else(|e| panic!("the NIC did not start on vmnet: {e:#}"));
    let deadline = Instant::now() + Duration::from_secs(180);
    let vmnet_ip = loop {
        if let Some(ip) = vmnet_lease(&mac) {
            break ip.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "the guest never leased from vmnet; supervisor log:\n{}",
            guest.supervisor_log()
        );
        std::thread::sleep(Duration::from_secs(1));
    };
    eprintln!("guest leased {vmnet_ip} from vmnet");

    // The SSH forward reaches it through netd while the NIC is on vmnet.
    guest
        .wait_for_ssh(Duration::from_secs(120))
        .unwrap_or_else(|e| {
            panic!(
                "no SSH through the forward on vmnet: {e:#}\nsupervisor log:\n{}",
                guest.supervisor_log()
            )
        });
    assert_eq!(guest_addr(&guest).as_deref(), Some(vmnet_ip.as_str()));
    reaches_out(&guest, "vmnet");

    // To netd: the carrier drops, and the guest leases netd's address.
    let moved = Instant::now();
    switch(&cfg, &guest, "netd");
    assert!(
        wait_until(Duration::from_secs(60), || count(
            &guest,
            "network: on limina-netd"
        ) >= 1),
        "the NIC did not move to netd; supervisor log:\n{}",
        guest.supervisor_log()
    );
    assert!(
        wait_until(Duration::from_secs(90), || guest_addr(&guest).as_deref()
            == Some("192.168.127.2")),
        "the guest did not take netd's lease after the move; it has {:?}; supervisor log:\n{}",
        guest_addr(&guest),
        guest.supervisor_log()
    );
    eprintln!(
        "on netd at 192.168.127.2, {:?} after the move",
        moved.elapsed()
    );
    reaches_out(&guest, "netd");

    // And back: vmnet's DHCP server keys its leases on the MAC, so the address comes back.
    let moved = Instant::now();
    switch(&cfg, &guest, "vmnet");
    assert!(
        wait_until(Duration::from_secs(60), || count(
            &guest,
            "network: on vmnet"
        ) >= 2),
        "the NIC did not move back to vmnet; supervisor log:\n{}",
        guest.supervisor_log()
    );
    assert!(
        wait_until(Duration::from_secs(90), || guest_addr(&guest).as_deref()
            == Some(vmnet_ip.as_str())),
        "the guest did not take its vmnet lease again; it has {:?}; supervisor log:\n{}",
        guest_addr(&guest),
        guest.supervisor_log()
    );
    eprintln!(
        "back on vmnet at {vmnet_ip}, {:?} after the move",
        moved.elapsed()
    );
    reaches_out(&guest, "vmnet again");

    let log = guest.supervisor_log();
    for l in log.lines().filter(|l| l.contains("network:")) {
        eprintln!("{l}");
    }
    // The host's routes did not change under the test, so every move was one it asked for.
    let unasked: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("network: moving to") && !l.contains("(asked to)"))
        .collect();
    assert!(unasked.is_empty(), "moves nobody asked for: {unasked:#?}");
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}

#[test]
fn a_vmnet_that_will_not_start_leaves_the_vm_on_netd() {
    if !limina_test::require_hvf_or_skip("a_vmnet_that_will_not_start_leaves_the_vm_on_netd") {
        return;
    }
    let mac = test_mac();
    let cfg = config(&mac).with_env("LIMINA_NET_VMNET_FAIL", "1");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for_supervisor_log(
            "network: on limina-netd: vmnet would not start",
            Duration::from_secs(180),
        )
        .unwrap_or_else(|e| panic!("the NIC did not fall back to netd: {e:#}"));
    guest
        .wait_for_supervisor_log("DHCP: leased 192.168.127.2", Duration::from_secs(180))
        .unwrap_or_else(|e| panic!("the guest never leased from netd: {e:#}"));
    guest
        .wait_for_ssh(Duration::from_secs(120))
        .expect("no SSH through the forward on netd");
    reaches_out(&guest, "netd");

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}
