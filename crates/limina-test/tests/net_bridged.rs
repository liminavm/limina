// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 networking test: the stock Fedora image bridged onto the host's own uplink
//! (`--net-vmnet bridged:<interface>`, `docs/design/multi-vm-networking.md` §3.3).
//!
//! A bridged guest takes its address from the LAN's DHCP server, so the host's lease file knows
//! nothing of it. The supervisor does: the worker reads the acknowledgement on its way to the
//! guest and reports it, and `limina address` asks for it — the same answer the control center
//! shows. The test takes the address from there and nowhere else, then proves it is the guest's:
//! it is on the uplink's subnet, the guest answers SSH at it (Apple's `/usr/bin/ssh`, which Local
//! Network privacy lets through, see [`limina_test::ssh_exec_at`]), and the guest names it as its
//! own. Out through the LAN's router: a name and an HTTPS fetch.
//!
//! A NIC bridged onto an interface that does not exist cannot start; that is reported the same
//! way, as the reason, instead of a guest that silently has no network. (A managed VM never gets
//! that far: its pre-flight refuses an uplink the host does not have.)
//!
//! A managed VM is bridged the way the control center's Configure sheet does it, through
//! `vm.toml`'s `[[network]]` (`mode = "bridged"`, `uplink`), and reached the same way; one whose
//! uplink the host does not have is refused at start, before anything boots.
//!
//! Skipped on a host whose way out is not a physical interface (a VPN tunnel, or no network): a
//! bridged guest goes around the host's routing, so there is nothing to bridge onto then.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use limina_test::{Boot, Guest, GuestConfig, limina_bin, ssh_exec_at, test_mac};

/// The host's uplink, its address on it and the mask: what the guest's lease must fall in.
fn uplink() -> Option<(String, Ipv4Addr, Ipv4Addr)> {
    let route = Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    let ifname = String::from_utf8_lossy(&route.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface: ").map(str::to_string))?;
    if !ifname.starts_with("en") {
        return None;
    }
    let ipconfig = |args: &[&str]| -> Option<Ipv4Addr> {
        let out = Command::new("/usr/sbin/ipconfig")
            .args(args)
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    };
    let host = ipconfig(&["getifaddr", &ifname])?;
    let mask = ipconfig(&["getoption", &ifname, "subnet_mask"])?;
    Some((ifname, host, mask))
}

/// `limina address <vm>`: the address, or the error it printed.
fn limina_address(vm: impl ToString) -> Result<Ipv4Addr, String> {
    address_of(
        Command::new(limina_bin().expect("locating limina")),
        &vm.to_string(),
    )
}

fn address_of(mut limina: Command, vm: &str) -> Result<Ipv4Addr, String> {
    let out = limina
        .args(["address", vm])
        .output()
        .map_err(|e| format!("running limina address: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        stdout
            .trim()
            .parse()
            .map_err(|e| format!("{stdout:?}: {e}"))
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn in_subnet(ip: Ipv4Addr, net: Ipv4Addr, mask: Ipv4Addr) -> bool {
    u32::from(ip) & u32::from(mask) == u32::from(net) & u32::from(mask)
}

fn boot_bridged(ifname: &str) -> Guest {
    let mac = test_mac();
    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_supervisor_log()
        .with_supervisor_arg("--net-vmnet")
        .with_supervisor_arg(&format!("bridged:{ifname}"))
        .with_supervisor_arg("--net-mac")
        .with_supervisor_arg(&mac);
    eprintln!("booting Fedora bridged on {ifname}, guest MAC {mac}");
    Guest::boot(&cfg).expect("spawning the limina supervisor")
}

#[test]
fn a_bridged_guest_leases_from_the_lan_and_the_supervisor_names_its_address() {
    if !limina_test::require_hvf_or_skip(
        "a_bridged_guest_leases_from_the_lan_and_the_supervisor_names_its_address",
    ) {
        return;
    }
    let Some((ifname, host_ip, mask)) = uplink() else {
        eprintln!("SKIP: the host's way out is not a physical interface; nothing to bridge onto");
        return;
    };
    let guest = boot_bridged(&ifname);

    // The lease: firmware → GRUB → kernel → systemd → NetworkManager → the LAN's DHCP server.
    let deadline = Instant::now() + Duration::from_secs(180);
    let ip = loop {
        match limina_address(guest.supervisor_pid()) {
            Ok(ip) => break ip,
            Err(why) if why.contains("did not start") => {
                panic!("the bridged NIC did not start: {why}")
            }
            Err(why) => assert!(
                Instant::now() < deadline,
                "no address for the guest: {why}; supervisor log:\n{}",
                guest.supervisor_log()
            ),
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    eprintln!("the supervisor says the guest holds {ip}");
    assert!(
        in_subnet(ip, host_ip, mask) && ip != host_ip,
        "{ip} is not a LAN address beside the host's {host_ip}/{mask} on {ifname}"
    );
    assert!(
        guest
            .supervisor_log()
            .contains(&format!("guest address: {ip}")),
        "the VM log does not name the address"
    );

    let host = ip.to_string();
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
    let run = |cmd: &str| {
        String::from_utf8_lossy(
            &ssh_exec_at(&host, cmd, None, Duration::from_secs(60))
                .unwrap_or_else(|e| panic!("`{cmd}` in the guest: {e:#}")),
        )
        .trim()
        .to_string()
    };
    let own = run("ip -4 -o addr show scope global");
    assert!(
        own.contains(&format!("inet {host}/")),
        "the guest does not hold {host}: {own}"
    );
    assert!(
        !run("getent hosts one.one.one.one").is_empty(),
        "the guest resolved nothing"
    );
    let status =
        run("curl -sS -o /dev/null -w '%{http_code}' --max-time 30 https://one.one.one.one/");
    assert!(
        status.starts_with('2') || status.starts_with('3'),
        "HTTPS from the guest answered {status:?}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "teardown had to be forced");
}

#[test]
fn a_nic_bridged_onto_no_interface_says_why_it_did_not_start() {
    if !limina_test::require_hvf_or_skip(
        "a_nic_bridged_onto_no_interface_says_why_it_did_not_start",
    ) {
        return;
    }
    let guest = boot_bridged("en99");

    // The NIC starts when the guest's driver takes the device, early in the kernel's boot.
    let deadline = Instant::now() + Duration::from_secs(120);
    let why = loop {
        match limina_address(guest.supervisor_pid()) {
            Err(why) if why.contains("did not start") => break why,
            Ok(ip) => panic!("a NIC bridged onto en99 holds {ip}"),
            Err(why) => assert!(
                Instant::now() < deadline,
                "the NIC's failure was never reported: {why}; supervisor log:\n{}",
                guest.supervisor_log()
            ),
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    // vmnet does not refuse an interface it does not have: its start never answers, and the
    // worker's wait for it is what ends. The reason says which.
    eprintln!("limina address: {why}");
    assert!(
        why.contains("vmnet_start_interface"),
        "the reason does not say vmnet would not start: {why}"
    );
    assert!(
        guest.supervisor_log().contains("guest network failed:"),
        "the VM log does not say the network failed"
    );

    // Still booting, and with no network to log in over: it cannot power itself off yet.
    let outcome = guest
        .force_shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    assert!(!outcome.forced, "the supervisor wedged on the way down");
}

/// A managed VM in a scratch library of its own, made from the test image; removed on drop
/// (after its supervisor, if it was started).
struct Managed {
    library: PathBuf,
    cfg: GuestConfig,
    supervisor: Option<std::process::Child>,
}

impl Managed {
    const NAME: &str = "bridged-test";

    fn create(tag: &str) -> Managed {
        let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
        let Boot::Firmware { disk, .. } = &cfg.boot else {
            panic!("fedora_from_env must give a firmware boot");
        };
        let library = std::env::temp_dir().join(format!("limina-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&library);
        std::fs::create_dir_all(&library).expect("creating the scratch library");
        let m = Managed {
            library,
            cfg: cfg.clone(),
            supervisor: None,
        };
        let out = m
            .limina()
            .args(["create", Self::NAME, "--disk"])
            .arg(disk)
            .output()
            .expect("running limina create");
        assert!(
            out.status.success(),
            "limina create: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        m
    }

    fn limina(&self) -> Command {
        let mut cmd = Command::new(&self.cfg.limina_bin);
        cmd.env("LIMINA_VM_LIBRARY", &self.library);
        cmd
    }

    fn vm_toml(&self) -> PathBuf {
        self.library
            .join(format!("{}.liminavm", Self::NAME))
            .join("vm.toml")
    }

    /// Bridge the VM's NIC onto `ifname`, as the Configure sheet does, with this test's own
    /// MAC so the LAN's DHCP server keeps one lease for it from run to run.
    fn bridge(&self, ifname: &str) {
        let path = self.vm_toml();
        let text = std::fs::read_to_string(&path).expect("reading vm.toml");
        let mut in_network = false;
        let mut lines = Vec::new();
        for line in text.lines() {
            if line.starts_with('[') {
                in_network = line.trim() == "[[network]]";
            }
            let key = line.split('=').next().unwrap_or_default().trim();
            match (in_network, key) {
                (true, "mode") => {
                    lines.push("mode = \"bridged\"".to_string());
                    lines.push(format!("uplink = \"{ifname}\""));
                }
                (true, "mac") => lines.push(format!("mac = \"{}\"", test_mac())),
                _ => lines.push(line.to_string()),
            }
        }
        std::fs::write(&path, lines.join("\n") + "\n").expect("writing vm.toml");
    }

    fn start(&mut self) -> std::process::Output {
        let Boot::Firmware { firmware, .. } = &self.cfg.boot else {
            unreachable!()
        };
        let mut start = self.limina();
        start
            .args([
                "start",
                Self::NAME,
                "--no-window",
                "--shutdown-grace-secs",
                "3",
            ])
            .arg("--firmware")
            .arg(firmware)
            .arg("--vmm-bin")
            .arg(&self.cfg.vmm_bin)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = start.spawn().expect("spawning limina start");
        // A refused start exits at once; a started one runs until stopped.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().expect("polling limina start").is_some() {
                return child.wait_with_output().expect("reading limina start");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.supervisor = Some(child);
        std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        if let Some(mut child) = self.supervisor.take() {
            limina_test::force_stop_child(
                &mut child,
                &self.cfg.limina_bin,
                &self.cfg.vmm_bin,
                Duration::from_secs(15),
            );
        }
        let _ = std::fs::remove_dir_all(&self.library);
    }
}

fn wait_for_ssh_at(host: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match ssh_exec_at(host, "true", None, Duration::from_secs(30)) {
            Ok(_) => return,
            Err(e) if Instant::now() >= deadline => {
                panic!("no SSH session to the guest at {host}: {e:#}")
            }
            Err(_) => std::thread::sleep(Duration::from_secs(1)),
        }
    }
}

#[test]
fn a_managed_vm_bridged_in_its_definition_is_reached_at_its_lan_address() {
    if !limina_test::require_hvf_or_skip(
        "a_managed_vm_bridged_in_its_definition_is_reached_at_its_lan_address",
    ) {
        return;
    }
    let Some((ifname, host_ip, mask)) = uplink() else {
        eprintln!("SKIP: the host's way out is not a physical interface; nothing to bridge onto");
        return;
    };
    let mut vm = Managed::create("bridged-managed");
    vm.bridge(&ifname);
    let refused = vm.start();
    assert!(
        vm.supervisor.is_some(),
        "limina start exited at once: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let ls = String::from_utf8_lossy(&vm.limina().arg("ls").output().unwrap().stdout).into_owned();
    assert!(
        ls.contains("bridged"),
        "ls does not say the VM is bridged:\n{ls}"
    );

    let deadline = Instant::now() + Duration::from_secs(180);
    let ip = loop {
        match address_of(vm.limina(), Managed::NAME) {
            Ok(ip) => break ip,
            Err(why) if why.contains("did not start") => {
                panic!("the bridged NIC did not start: {why}")
            }
            Err(why) => assert!(Instant::now() < deadline, "no address for the guest: {why}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    eprintln!("{} holds {ip}", Managed::NAME);
    assert!(
        in_subnet(ip, host_ip, mask) && ip != host_ip,
        "{ip} is not a LAN address beside the host's {host_ip}/{mask} on {ifname}"
    );
    wait_for_ssh_at(&ip.to_string());
}

#[test]
fn a_managed_vm_bridged_onto_an_interface_the_host_lacks_is_refused_at_start() {
    if !limina_test::require_hvf_or_skip(
        "a_managed_vm_bridged_onto_an_interface_the_host_lacks_is_refused_at_start",
    ) {
        return;
    }
    let mut vm = Managed::create("bridged-refused");
    vm.bridge("en99");
    let out = vm.start();
    assert!(
        vm.supervisor.is_none(),
        "a start bridged onto en99 was let through"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("en99"),
        "the refusal does not name the interface: {stderr}"
    );
}
