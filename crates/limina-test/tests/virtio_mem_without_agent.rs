// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A split boot whose guest has no agent to set movable onlining still gets its memory
//! (`elastic_mem`). A cold split boot holds its virtio-mem region until the stock guest agent has
//! set `auto_online_blocks` to `online_movable`; a guest with `qemu-guest-agent` masked never
//! does, and the hold must end anyway, a grace after the guest binds the device, with the blocks
//! onlined as the guest's kernel defaults (ZONE_NORMAL on Fedora).
//!
//! The VM is forced to split (`LIMINA_VIRTIO_MEM=split`). Its first boot masks the agent and
//! reboots, and the supervisor's relaunch is a cold split boot without one. Driven through the
//! real `limina create/start/stop` verbs, on a clone of the stock image.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use limina_test::{Boot, GuestConfig};

const PORT: &str = "2250";
const MIB: u64 = 1 << 20;

struct KillOnDrop(std::process::Child, std::path::PathBuf, std::path::PathBuf);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        limina_test::force_stop_child(&mut self.0, &self.1, &self.2, Duration::from_secs(15));
    }
}

fn ssh_exec(cmd: &str) -> Option<String> {
    let out = Command::new("ssh")
        .args([
            "-p",
            PORT,
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "claude@127.0.0.1",
            cmd,
        ])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

/// The pages `zone` spans on the guest's node 0, in bytes; 0 when the zone is empty.
fn zone_present(zone: &str) -> Option<u64> {
    let pages: u64 = ssh_exec(&format!(
        "awk '/^Node 0, zone/ {{z = $4}} z == \"{zone}\" && $1 == \"present\" {{print $2; exit}}' /proc/zoneinfo"
    ))?
    .parse()
    .ok()?;
    Some(pages * ssh_exec("getconf PAGESIZE")?.parse::<u64>().ok()?)
}

#[test]
fn a_split_guest_without_its_agent_still_plugs_its_region() {
    let name = "a_split_guest_without_its_agent_still_plugs_its_region";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
    let Boot::Firmware { firmware, disk, .. } = &cfg.boot else {
        panic!("fedora_from_env must give a firmware boot");
    };
    let scratch = std::env::temp_dir().join(format!("limina-vmem-noagent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch library");
    let limina = |args: &[&str]| {
        let mut c = Command::new(&cfg.limina_bin);
        c.env("LIMINA_VM_LIBRARY", &scratch).args(args);
        c
    };
    let out = limina(&["create", "vmem-noagent", "--memory", "4G", "--disk"])
        .arg(disk)
        .output()
        .expect("spawning limina create");
    assert!(out.status.success(), "limina create: {out:?}");

    let log = scratch.join("start.log");
    let log_file = std::fs::File::create(&log).expect("creating the start log");
    let child = limina(&[
        "start",
        "vmem-noagent",
        "--no-window",
        "--shutdown-grace-secs",
        "5",
        "--ssh-port",
        PORT,
    ])
    .arg("--firmware")
    .arg(firmware)
    .arg("--vmm-bin")
    .arg(&cfg.vmm_bin)
    .env("RUST_LOG", "warn,limina=info")
    .env("LIMINA_VIRTIO_MEM", "split")
    .stdin(Stdio::null())
    .stdout(log_file.try_clone().expect("cloning the start log"))
    .stderr(log_file)
    .spawn()
    .expect("spawning limina start");
    let mut boot = KillOnDrop(child, cfg.limina_bin.clone(), cfg.vmm_bin.clone());
    let read_log = || std::fs::read_to_string(&log).unwrap_or_default();

    assert!(
        wait_until(Duration::from_secs(180), || ssh_exec("true").is_some()),
        "the first boot never reached SSH"
    );
    let first_boot = ssh_exec("cat /proc/sys/kernel/random/boot_id").expect("reading boot_id");
    ssh_exec("sudo systemctl mask qemu-guest-agent && (sudo systemctl reboot &)")
        .expect("masking the guest agent and rebooting");
    assert!(
        wait_until(Duration::from_secs(240), || {
            ssh_exec("cat /proc/sys/kernel/random/boot_id")
                .is_some_and(|id| !id.is_empty() && id != first_boot)
        }),
        "the guest never came back from its reboot:\n{}",
        read_log()
    );
    assert_eq!(
        ssh_exec("systemctl is-active qemu-guest-agent").as_deref(),
        None,
        "the guest agent still runs after it was masked"
    );

    // The hold ends 30 s after the guest binds the device; then the guest plugs its 2 GiB region.
    let mut total = 0;
    let plugged = wait_until(Duration::from_secs(120), || {
        total = ssh_exec("awk '/^MemTotal:/ {print $2 * 1024}' /proc/meminfo")
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        total > (2048 + 1024) * MIB
    });
    let normal = zone_present("Normal").unwrap_or(0);
    let movable = zone_present("Movable").unwrap_or(0);
    eprintln!(
        "MemTotal {} MiB; zones: Normal {} MiB, Movable {} MiB",
        total / MIB,
        normal / MIB,
        movable / MIB
    );
    let text = read_log();
    for line in text.lines() {
        if line.contains("memory: ") || line.contains("virtio-mem: ") {
            eprintln!("{line}");
        }
    }
    assert!(
        plugged,
        "the split guest without an agent never plugged its region ({} MiB):\n{text}",
        total / MIB
    );
    assert!(
        text.contains("nothing set movable onlining"),
        "the region was released without the no-agent fallback:\n{text}"
    );
    assert!(
        normal > 1024 * MIB,
        "without an agent the region should come online as the kernel defaults, in ZONE_NORMAL"
    );

    let out = limina(&["stop", "vmem-noagent"])
        .output()
        .expect("spawning limina stop");
    assert!(out.status.success(), "limina stop: {out:?}");
    let _ = boot.0.wait();
    let _ = std::fs::remove_dir_all(&scratch);
}
