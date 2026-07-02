// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 networking test: the stock Fedora image gets user-mode NAT through limina.
//!
//! Drives the real `limina` supervisor → it spawns + supervises a gvproxy gateway and
//! connects the guest's virtio-net to it (`--net`). The oracle is **host-side**: gvproxy's
//! `-debug` packet log (`--net-log`), because the pristine Fedora image is silent on serial
//! after GRUB (no `console=`), so the guest console can't witness DHCP/DNS. A DHCP Ack plus
//! an outbound DNS query prove the whole path: VFKT handshake → link → DHCP lease → NAT.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn fedora_gets_nat_dhcp_and_outbound() {
    if !limina_test::require_hvf_or_skip("fedora_gets_nat_dhcp_and_outbound") {
        return;
    }

    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net();
    eprintln!(
        "booting Fedora (writable COW clone) with user-mode NAT via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // DHCP: gvproxy ACKs the guest's request once NetworkManager brings the link up. Full
    // userspace boot (firmware → GRUB → kernel → systemd → NM) takes a while, so be patient.
    guest
        .wait_for_gateway_log("MessageType:Ack", Duration::from_secs(150))
        .expect("guest never obtained a DHCP lease from gvproxy");

    // Outbound NAT: an outbound DNS query (to gvproxy's resolver at .1:53) proves the guest
    // can route off-link, not just hold a lease.
    guest
        .wait_for_gateway_log("DstPort=53(domain)", Duration::from_secs(45))
        .expect("guest never made an outbound DNS query");

    // The well-known vfkit MAC gets gvproxy's static `.2` lease (deterministic guest IP),
    // which is what makes the built-in 127.0.0.1:2222 → .2:22 forward land on the guest.
    let log = guest.gateway_log();
    assert!(
        log.contains("YourClientIP=192.168.127.2 "),
        "expected the static 192.168.127.2 lease in the gateway log"
    );

    // Inbound NAT: gvproxy's default port-forward reaches the guest's sshd — the path that
    // makes `ssh -p 2222 user@127.0.0.1` work (what the M3 SSH goal is for).
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(30))
        .expect("guest SSH not reachable through the gvproxy forward");
    eprintln!("guest SSH reachable: {banner}");

    // Clean teardown. Stock Fedora ignores the GPIO power button, so the supervisor
    // force-kills the worker after its short grace — still a clean supervisor stop. The
    // gateway must be torn down with it (the supervisor kills gvproxy on exit).
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(
        !outcome.forced,
        "harness had to force the supervisor down — supervisor teardown is broken"
    );
}

/// Per-VM network config → run two VMs in parallel (the multi-VM goal: design doc Phase 2).
///
/// Two stock Fedora guests boot AT ONCE, each its own gvproxy NAT island, each on a DISTINCT
/// host SSH-forward port (`--ssh-port`). gvproxy's built-in `127.0.0.1:<port> → guest:22` forward
/// used to be hard-wired to 2222, so a second VM collided on it; giving each VM its own port is
/// what lets more than one run side by side. The proof is end-to-end and concurrent: both gateways
/// must independently lease (two separate NAT islands) AND both guests must answer SSH on their own
/// host port at the same time — if only one VM were really up, only one port would answer.
#[test]
fn two_vms_run_in_parallel_on_distinct_ssh_ports() {
    if !limina_test::require_hvf_or_skip("two_vms_run_in_parallel_on_distinct_ssh_ports") {
        return;
    }

    // Distinct host ports are the only per-VM resource that used to collide (the gvproxy socket is
    // already pid-keyed). Each VM cow-clones its own writable disk via with_net, so the two share
    // nothing on disk either.
    let cfg_a = GuestConfig::fedora_from_env()
        .expect("resolving guest config A")
        .with_ssh_port(2222);
    let cfg_b = GuestConfig::fedora_from_env()
        .expect("resolving guest config B")
        .with_ssh_port(2223);

    // Boot BOTH before waiting on either, so they come up concurrently (wall-clock ~= one boot).
    eprintln!("booting two Fedora VMs in parallel: A on ssh-port 2222, B on ssh-port 2223");
    let mut a = Guest::boot(&cfg_a).expect("spawning supervisor A");
    let mut b = Guest::boot(&cfg_b).expect("spawning supervisor B");

    // Both gateways lease independently — two live NAT islands, not one shared (and proves B's
    // gvproxy didn't fail to start behind A's).
    a.wait_for_gateway_log("MessageType:Ack", Duration::from_secs(180))
        .expect("VM A never obtained a DHCP lease");
    b.wait_for_gateway_log("MessageType:Ack", Duration::from_secs(180))
        .expect("VM B never obtained a DHCP lease (second VM did not come up in parallel)");

    // The headline: both guests answer SSH on their OWN forward port, simultaneously.
    let banner_a = a
        .wait_for_ssh_banner(Duration::from_secs(60))
        .expect("VM A SSH not reachable on 127.0.0.1:2222");
    let banner_b = b
        .wait_for_ssh_banner(Duration::from_secs(60))
        .expect("VM B SSH not reachable on 127.0.0.1:2223");
    eprintln!("VM A SSH (port 2222): {banner_a}");
    eprintln!("VM B SSH (port 2223): {banner_b}");
    assert!(
        banner_a.starts_with("SSH-") && banner_b.starts_with("SSH-"),
        "both VMs must answer SSH on their distinct ports at once (A={banner_a:?}, B={banner_b:?})"
    );

    // Tear both down cleanly; neither teardown may need forcing (gateway killed with each).
    let oa = a
        .shutdown(Duration::from_secs(20))
        .expect("supervisor A did not stop");
    let ob = b
        .shutdown(Duration::from_secs(20))
        .expect("supervisor B did not stop");
    eprintln!("teardown A: {oa:?}, B: {ob:?}");
    assert!(
        !oa.forced && !ob.forced,
        "a supervisor had to be forced down — multi-VM teardown is broken"
    );
}

/// Per-VM MAC (managed VMs' persistent MAC from vm.toml): a custom `--net-mac` must not
/// break the deterministic network identity. The supervisor rebinds gvproxy's static `.2`
/// lease to the custom MAC via a generated config file (in config mode gvproxy installs
/// NO default forwards/leases, and `-ssh-port` is ignored — the config must carry both).
/// The proof is end-to-end: the DHCP Ack goes to the custom MAC, the guest still gets the
/// static 192.168.127.2, and the SSH forward still lands on it.
#[test]
fn custom_guest_mac_keeps_the_static_lease_and_ssh_forward() {
    if !limina_test::require_hvf_or_skip("custom_guest_mac_keeps_the_static_lease_and_ssh_forward")
    {
        return;
    }

    const MAC: &str = "5a:11:22:33:44:55";
    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net_mac(MAC);
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for_gateway_log("MessageType:Ack", Duration::from_secs(150))
        .expect("guest never obtained a DHCP lease from gvproxy (custom MAC)");

    let log = guest.gateway_log();
    assert!(
        log.contains(MAC),
        "the gateway packet log must show traffic from the custom MAC {MAC}"
    );
    assert!(
        !log.contains("5a:94:ef:e4:0c:ee"),
        "the well-known vfkit MAC must NOT appear — the NIC really carries the custom MAC"
    );
    assert!(
        log.contains("YourClientIP=192.168.127.2 "),
        "the static .2 lease must follow the custom MAC (config-file rebind)"
    );

    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(30))
        .expect("SSH forward broken with a custom MAC (config-mode forward missing?)");
    eprintln!("guest SSH reachable with custom MAC: {banner}");

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}
