// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A managed VM learns that its guest drives virtio-mem, and its next boot takes RAM at its
//! minimum and plugs the rest (docs/design/guest-ram-objects.md §3, `elastic_mem`).
//!
//! The first boot takes all of its RAM up front, beside a small virtio-mem region it is never
//! asked to plug; once the stock guest binds the device, `state.toml` records it. After a stop,
//! the next boot splits: the guest comes up on its minimum and plugs the region it is asked for.
//! Driven through the real `limina create/start/stop` verbs, like `vmdef.rs`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use limina_test::{Boot, GuestConfig};

const PORT: &str = "2246";
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

fn mem_total() -> Option<u64> {
    ssh_exec("awk '/^MemTotal:/ {print $2 * 1024}' /proc/meminfo")?
        .parse()
        .ok()
}

#[test]
fn a_managed_vm_learns_virtio_mem_and_splits_its_next_boot() {
    let name = "a_managed_vm_learns_virtio_mem_and_splits_its_next_boot";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
    let Boot::Firmware { firmware, disk, .. } = &cfg.boot else {
        panic!("fedora_from_env must give a firmware boot");
    };
    let scratch = std::env::temp_dir().join(format!("limina-vmem-learn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch library");
    let limina = |args: &[&str]| {
        let mut c = Command::new(&cfg.limina_bin);
        c.env("LIMINA_VM_LIBRARY", &scratch).args(args);
        c
    };
    let out = limina(&["create", "vmem-learn", "--memory", "4G", "--disk"])
        .arg(disk)
        .output()
        .expect("spawning limina create");
    assert!(out.status.success(), "limina create: {out:?}");
    let state_toml = scratch.join("vmem-learn.liminavm/state.toml");
    let learned = || {
        std::fs::read_to_string(&state_toml)
            .unwrap_or_default()
            .lines()
            .any(|l| l.trim() == "virtio_mem = true")
    };
    let start = |log: &Path| {
        let log = std::fs::File::create(log).expect("creating a start log");
        let child = limina(&[
            "start",
            "vmem-learn",
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
        .stdin(Stdio::null())
        .stdout(log.try_clone().expect("cloning the start log"))
        .stderr(log)
        .spawn()
        .expect("spawning limina start");
        KillOnDrop(child, cfg.limina_bin.clone(), cfg.vmm_bin.clone())
    };
    let stop = |boot: &mut KillOnDrop| {
        let out = limina(&["stop", "vmem-learn"])
            .output()
            .expect("spawning limina stop");
        assert!(out.status.success(), "limina stop: {out:?}");
        let _ = boot.0.wait();
    };

    // --- boot 1: all of RAM up front, and a probe the guest binds ---
    let log1 = scratch.join("start1.log");
    let mut boot1 = start(&log1);
    assert!(
        wait_until(Duration::from_secs(180), || ssh_exec("true").is_some()),
        "the first boot never reached SSH"
    );
    let total1 = mem_total().expect("reading MemTotal");
    assert!(
        wait_until(Duration::from_secs(90), learned),
        "the guest never taught the VM it drives virtio-mem:\n{}\n{}",
        std::fs::read_to_string(&state_toml).unwrap_or_default(),
        std::fs::read_to_string(&log1).unwrap_or_default()
    );
    stop(&mut boot1);
    let text1 = std::fs::read_to_string(&log1).unwrap_or_default();
    assert!(
        text1.contains("memory: Probe"),
        "boot 1 did not probe:\n{text1}"
    );

    // --- boot 2: RAM at the minimum, the region plugged ---
    let log2 = scratch.join("start2.log");
    let mut boot2 = start(&log2);
    assert!(
        wait_until(Duration::from_secs(180), || ssh_exec("true").is_some()),
        "the split boot never reached SSH"
    );
    let text2 = std::fs::read_to_string(&log2).unwrap_or_default();
    assert!(
        text2.contains("memory: Split"),
        "boot 2 did not split:\n{text2}"
    );
    // A split boot starts on 2 GiB, so a 4 GiB VM has a 2 GiB region. The policy may start
    // reclaiming once the plug settles, so this asks for most of the region, not all of it.
    let mut total2 = 0;
    let plugged = wait_until(Duration::from_secs(60), || {
        total2 = mem_total().unwrap_or(0);
        total2 > (2048 + 1024) * MIB
    });
    eprintln!(
        "MemTotal: whole boot {} MiB, split boot {} MiB",
        total1 / MIB,
        total2 / MIB
    );
    assert!(
        plugged,
        "the split guest has {} MiB (the whole boot had {} MiB): it did not plug its region",
        total2 / MIB,
        total1 / MIB
    );
    stop(&mut boot2);
    let _ = std::fs::remove_dir_all(&scratch);
}
