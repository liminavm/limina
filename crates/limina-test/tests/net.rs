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
