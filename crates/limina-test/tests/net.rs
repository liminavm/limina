// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 networking tests: the stock Fedora image gets user-mode NAT through limina, on each NIC
//! that provides it — a gvproxy gateway the supervisor runs (`--net`), and a `limina-netd` the
//! worker spawns (`--net-netd`).
//!
//! The oracles are **host-side**, because the pristine Fedora image is silent on serial after
//! GRUB (no `console=`), so the guest console can't witness DHCP or DNS: gvproxy's `-debug` packet
//! log (`--net-log`), and netd's own account of its leases and answers, which lands in the
//! supervisor's log. A lease plus an outbound DNS question prove the whole path: link → DHCP →
//! NAT; the SSH forward proves the way in.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The NIC a test runs on.
#[derive(Clone, Copy, Debug)]
enum Nic {
    Gvproxy,
    Netd,
}

impl Nic {
    fn config(self) -> GuestConfig {
        let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
        match self {
            Nic::Gvproxy => cfg.with_net(),
            Nic::Netd => cfg
                .with_net_netd()
                .with_supervisor_log()
                .with_env("RUST_LOG", "warn,limina_netd=debug"),
        }
    }

    /// Waits for the guest's DHCP lease of 192.168.127.2, the address the SSH forward lands on.
    /// Full userspace boot (firmware → GRUB → kernel → systemd → NM) takes a while.
    fn wait_for_lease(self, guest: &mut Guest, timeout: Duration) -> anyhow::Result<()> {
        match self {
            Nic::Gvproxy => {
                guest.wait_for_gateway_log("MessageType:Ack", timeout)?;
                anyhow::ensure!(
                    guest.gateway_log().contains("YourClientIP=192.168.127.2 "),
                    "expected the static 192.168.127.2 lease in the gateway log"
                );
                Ok(())
            }
            Nic::Netd => guest.wait_for_supervisor_log("DHCP: leased 192.168.127.2", timeout),
        }
    }

    /// Waits for the guest's first DNS question at the gateway: it routes off-link, not just
    /// holds a lease.
    fn wait_for_dns(self, guest: &mut Guest, timeout: Duration) -> anyhow::Result<()> {
        match self {
            Nic::Gvproxy => guest.wait_for_gateway_log("DstPort=53(domain)", timeout),
            Nic::Netd => guest.wait_for_supervisor_log("DNS: answered", timeout),
        }
    }

    /// What the NIC's log says about the guest's MAC.
    fn log(self, guest: &Guest) -> String {
        match self {
            Nic::Gvproxy => guest.gateway_log(),
            Nic::Netd => guest.supervisor_log(),
        }
    }
}

fn nat_dhcp_and_outbound(nic: Nic) {
    let cfg = nic.config().with_ssh_target_check();
    eprintln!("booting Fedora (writable COW clone) with user-mode NAT on {nic:?}");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    nic.wait_for_lease(&mut guest, Duration::from_secs(150))
        .expect("guest never obtained its DHCP lease");
    nic.wait_for_dns(&mut guest, Duration::from_secs(45))
        .expect("guest never made an outbound DNS query");

    // Inbound: the SSH forward reaches the guest's sshd — the path that makes
    // `ssh -p <port> user@127.0.0.1` work (what the M3 SSH goal is for).
    let banner = guest
        .wait_for_ssh(Duration::from_secs(30))
        .expect("guest SSH not reachable through the forward");
    eprintln!("guest SSH reachable: {banner}");

    // Clean teardown. Stock Fedora ignores the GPIO power button, so the supervisor
    // force-kills the worker after its short grace — still a clean supervisor stop. The
    // NIC's helper goes with it.
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(
        !outcome.forced,
        "harness had to force the supervisor down — supervisor teardown is broken"
    );
}

#[test]
fn fedora_gets_nat_dhcp_and_outbound() {
    if limina_test::require_hvf_or_skip("fedora_gets_nat_dhcp_and_outbound") {
        nat_dhcp_and_outbound(Nic::Gvproxy);
    }
}

#[test]
fn fedora_gets_nat_dhcp_and_outbound_on_netd() {
    if limina_test::require_hvf_or_skip("fedora_gets_nat_dhcp_and_outbound_on_netd") {
        nat_dhcp_and_outbound(Nic::Netd);
    }
}

/// Per-VM network config → run two VMs in parallel (the multi-VM goal).
///
/// Two stock Fedora guests boot AT ONCE, each its own NAT island, each on a DISTINCT host
/// SSH-forward port — the harness's AUTO-ALLOCATED ephemeral ports (the default path every test
/// rides), not explicit ones. The proof is end-to-end and concurrent: both must independently
/// lease (two separate NAT islands), both guests must answer SSH on their own host port at the
/// same time, AND each harness handle must demonstrably reach ITS OWN guest (identity markers) —
/// banner-answering alone can't catch a port mix-up because all test guests share creds, which is
/// exactly how the assume-2222 crosstalk stayed invisible.
fn two_vms_in_parallel(nic: Nic) {
    // NO explicit ports: this exercises the harness's pre-allocated ephemeral ssh-forward
    // ports (the default path every other test rides). Assuming the supervisor default (2222)
    // here once sent every ssh_exec into a BYSTANDER VM's guest when a dev VM held 2222 —
    // identical test creds made every check "pass" against the wrong guest.
    // Each VM cow-clones its own writable disk, so the two share nothing on disk.
    let cfg_a = nic.config().with_ssh_target_check();
    let cfg_b = nic.config().with_ssh_target_check();

    // Boot BOTH before waiting on either, so they come up concurrently (wall-clock ~= one boot).
    eprintln!("booting two Fedora VMs in parallel on {nic:?}, on auto-allocated ssh ports");
    let mut a = Guest::boot(&cfg_a).expect("spawning supervisor A");
    let mut b = Guest::boot(&cfg_b).expect("spawning supervisor B");
    assert_ne!(
        a.ssh_port(),
        b.ssh_port(),
        "the two VMs were given the same ssh-forward port — parallel VMs would collide"
    );

    // Both lease independently — two live NAT islands, not one shared (and proves B's NIC
    // didn't fail to start behind A's).
    nic.wait_for_lease(&mut a, Duration::from_secs(180))
        .expect("VM A never obtained a DHCP lease");
    nic.wait_for_lease(&mut b, Duration::from_secs(180))
        .expect("VM B never obtained a DHCP lease (second VM did not come up in parallel)");

    // The headline: both guests answer SSH on their OWN forward port, simultaneously.
    let banner_a = a
        .wait_for_ssh(Duration::from_secs(60))
        .unwrap_or_else(|e| panic!("VM A SSH not reachable on port {}: {e}", a.ssh_port()));
    let banner_b = b
        .wait_for_ssh(Duration::from_secs(60))
        .unwrap_or_else(|e| panic!("VM B SSH not reachable on port {}: {e}", b.ssh_port()));
    eprintln!("VM A SSH (port {}): {banner_a}", a.ssh_port());
    eprintln!("VM B SSH (port {}): {banner_b}", b.ssh_port());
    assert!(
        banner_a.starts_with("SSH-") && banner_b.starts_with("SSH-"),
        "both VMs must answer SSH on their distinct ports at once (A={banner_a:?}, B={banner_b:?})"
    );

    // `with_ssh_target_check` already made each wait_for_ssh read its guest's debug port and
    // require that guest's own supervisor_pid. The markers below prove the same from inside
    // the guests, without the lever.
    //
    // Identity proof — a banner answering is NOT proof the harness reached the RIGHT guest
    // (all test guests share creds, so crosstalk is invisible to every other check). Stamp
    // each guest with its own marker through its handle, then read BOTH back: a port mix-up
    // makes the markers land in (or read from) the same guest and the assert catches it.
    a.ssh_exec("echo vm-a > /tmp/limina-vm-identity")
        .expect("stamping VM A");
    b.ssh_exec("echo vm-b > /tmp/limina-vm-identity")
        .expect("stamping VM B");
    let id_a = a
        .ssh_exec("cat /tmp/limina-vm-identity")
        .expect("reading VM A identity");
    let id_b = b
        .ssh_exec("cat /tmp/limina-vm-identity")
        .expect("reading VM B identity");
    assert_eq!(id_a.trim(), "vm-a", "handle A is not talking to guest A");
    assert_eq!(id_b.trim(), "vm-b", "handle B is not talking to guest B");

    // Tear both down cleanly; neither teardown may need forcing (the NIC's helper goes with
    // each).
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

#[test]
fn two_vms_run_in_parallel_on_distinct_ssh_ports() {
    if limina_test::require_hvf_or_skip("two_vms_run_in_parallel_on_distinct_ssh_ports") {
        two_vms_in_parallel(Nic::Gvproxy);
    }
}

#[test]
fn two_vms_run_in_parallel_on_distinct_ssh_ports_on_netd() {
    if limina_test::require_hvf_or_skip("two_vms_run_in_parallel_on_distinct_ssh_ports_on_netd") {
        two_vms_in_parallel(Nic::Netd);
    }
}

/// Per-VM MAC (managed VMs' persistent MAC from vm.toml): a custom `--net-mac` must not break the
/// deterministic network identity. On gvproxy the supervisor rebinds its static `.2` lease to the
/// custom MAC via a generated config file (in config mode gvproxy installs NO default
/// forwards/leases, and `-ssh-port` is ignored — the config must carry both); netd leases `.2` to
/// whichever MAC asks. The proof is end-to-end: the lease goes to the custom MAC, the guest still
/// gets the static 192.168.127.2, and the SSH forward still lands on it.
fn custom_mac(nic: Nic) {
    const MAC: &str = "5a:11:22:33:44:55";
    let cfg = nic.config().with_net_mac(MAC).with_ssh_target_check();
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    nic.wait_for_lease(&mut guest, Duration::from_secs(150))
        .expect("guest never obtained its DHCP lease (custom MAC)");

    let log = nic.log(&guest);
    assert!(
        log.contains(MAC),
        "the NIC's log must show traffic from the custom MAC {MAC}"
    );
    assert!(
        !log.contains("5a:94:ef:e4:0c:ee"),
        "the well-known vfkit MAC must NOT appear — the NIC really carries the custom MAC"
    );

    let banner = guest
        .wait_for_ssh(Duration::from_secs(30))
        .expect("SSH forward broken with a custom MAC");
    eprintln!("guest SSH reachable with custom MAC: {banner}");

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}

#[test]
fn custom_guest_mac_keeps_the_static_lease_and_ssh_forward() {
    if limina_test::require_hvf_or_skip("custom_guest_mac_keeps_the_static_lease_and_ssh_forward") {
        custom_mac(Nic::Gvproxy);
    }
}

#[test]
fn custom_guest_mac_keeps_the_static_lease_and_ssh_forward_on_netd() {
    if limina_test::require_hvf_or_skip(
        "custom_guest_mac_keeps_the_static_lease_and_ssh_forward_on_netd",
    ) {
        custom_mac(Nic::Netd);
    }
}

/// The SSH forward moves while the VM runs (`limina ssh-port <vm> <port>`): on netd the worker
/// binds the new port and hands it to netd before the old one closes, so the guest is reachable on
/// the new port and no longer on the old.
#[test]
fn the_ssh_forward_moves_while_the_vm_runs_on_netd() {
    if !limina_test::require_hvf_or_skip("the_ssh_forward_moves_while_the_vm_runs_on_netd") {
        return;
    }
    let cfg = Nic::Netd.config().with_ssh_target_check();
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    Nic::Netd
        .wait_for_lease(&mut guest, Duration::from_secs(150))
        .expect("guest never obtained its DHCP lease");
    guest
        .wait_for_ssh(Duration::from_secs(60))
        .expect("guest SSH not reachable through the forward");

    let old = guest.ssh_port();
    let new = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("probing a free port")
        .port();
    let pid = guest.supervisor_pid();
    let out = std::process::Command::new(&cfg.limina_bin)
        .args(["ssh-port", &pid.to_string(), &new.to_string()])
        .output()
        .expect("running limina ssh-port");
    assert!(
        out.status.success(),
        "limina ssh-port {new} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let banner = limina_test::wait_for_ssh_on(new, Duration::from_secs(30), || {
        limina_test::pid_path(pid).is_some()
    })
    .expect("guest SSH not reachable on the moved port");
    eprintln!("guest SSH reachable on the moved port {new}: {banner}");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", old)).is_err(),
        "the old port {old} still accepts"
    );
    let now = std::process::Command::new(&cfg.limina_bin)
        .args(["ssh-port", &pid.to_string()])
        .output()
        .expect("running limina ssh-port");
    assert_eq!(String::from_utf8_lossy(&now.stdout).trim(), new.to_string());

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}

/// Pings from the guest: the gateway answers its own, and one past it goes out from netd's
/// unprivileged ICMP socket and comes back.
#[test]
fn the_guest_pings_the_gateway_and_beyond_on_netd() {
    if !limina_test::require_hvf_or_skip("the_guest_pings_the_gateway_and_beyond_on_netd") {
        return;
    }
    let cfg = Nic::Netd.config().with_ssh_target_check();
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    Nic::Netd
        .wait_for_lease(&mut guest, Duration::from_secs(150))
        .expect("guest never obtained its DHCP lease");
    guest
        .wait_for_ssh(Duration::from_secs(60))
        .expect("guest SSH not reachable through the forward");
    for dst in ["192.168.127.1", "1.1.1.1"] {
        let out = guest
            .ssh_exec(&format!("ping -c 2 -W 5 {dst}"))
            .unwrap_or_else(|e| panic!("pinging {dst}: {e}"));
        eprintln!("{out}");
        assert!(
            out.contains(" 0% packet loss"),
            "pings of {dst} were lost:\n{out}"
        );
    }
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}

/// The SHA-256 of a file, as hex.
fn sha256_file(path: &std::path::Path) -> String {
    let out = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .expect("running shasum");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Bulk both ways through the SSH forward on netd: what the guest sends goes out as segments
/// far larger than the MSS (TSO), which netd takes whole.
#[test]
fn bulk_crosses_the_forward_intact_both_ways_on_netd() {
    if !limina_test::require_hvf_or_skip("bulk_crosses_the_forward_intact_both_ways_on_netd") {
        return;
    }
    let cfg = Nic::Netd.config().with_ssh_target_check();
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    Nic::Netd
        .wait_for_lease(&mut guest, Duration::from_secs(150))
        .expect("guest never obtained its DHCP lease");
    guest
        .wait_for_ssh(Duration::from_secs(60))
        .expect("guest SSH not reachable through the forward");

    // Host → guest.
    let dir = guest.scratch_dir().join("bulk");
    std::fs::create_dir_all(&dir).unwrap();
    let up = dir.join("up");
    let mut blob = vec![0u8; 32 << 20];
    // SAFETY: arc4random_buf fills exactly the buffer it is given.
    unsafe { libc::arc4random_buf(blob.as_mut_ptr().cast(), blob.len()) };
    std::fs::write(&up, &blob).unwrap();
    if let Err(e) = guest.scp_to_guest(&up, "/tmp/up") {
        panic!(
            "copying 32 MiB to the guest: {e:#}\nsupervisor log:\n{}",
            guest.supervisor_log()
        );
    }
    let arrived = guest
        .ssh_exec("sha256sum /tmp/up | cut -d' ' -f1")
        .expect("hashing the upload");
    assert_eq!(
        arrived.trim(),
        sha256_file(&up),
        "the upload arrived corrupted"
    );

    // Guest → host.
    let digest = guest
        .ssh_exec(
            "head -c 33554432 /dev/urandom > /tmp/down && sha256sum /tmp/down | cut -d' ' -f1",
        )
        .expect("making the download");
    let back = dir.join("back");
    if let Err(e) = guest.scp_from_guest("/tmp/down", &back) {
        panic!(
            "copying 32 MiB from the guest: {e:#}\nsupervisor log:\n{}",
            guest.supervisor_log()
        );
    }
    let fetched = back.join("down");
    assert_eq!(std::fs::metadata(&fetched).unwrap().len(), 32 << 20);
    assert_eq!(
        sha256_file(&fetched),
        digest.trim(),
        "the download arrived corrupted"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}
