// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 managed-VM definition tests: the `.liminavm` lifecycle end-to-end through the
//! shipped `limina` binary's subcommands (create → start → ls → double-start
//! fail-fast → stop → rm), booting the real supervisor → worker → HVF chain.
//!
//! Unlike the other L2 tests this does NOT go through `Guest::boot` (that drives the
//! flat flags); the subject here is the definition layer itself, so every step spawns
//! `limina <verb>` exactly as a user would. The shared `.test` image is never
//! mutated: `create --disk` clones it into the bundle (APFS COW), and the guest
//! boots the clone.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use limina_test::{Boot, GuestConfig};

/// Kill the `limina start` supervisor (and let its own teardown net the worker) if
/// the test dies mid-way, so a failed assertion never leaks a running VM.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // Two SIGTERMs = the force path (skip the grace), then reap.
            for _ in 0..2 {
                unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if self.0.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn limina_cmd(limina_bin: &Path, library: &Path) -> Command {
    let mut cmd = Command::new(limina_bin);
    cmd.env("LIMINA_VM_LIBRARY", library);
    cmd
}

fn run_ok(cmd: &mut Command, what: &str) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawning {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed ({}):\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn managed_vm_lifecycle_create_start_stop_rm() {
    if !limina_test::require_hvf_or_skip("managed_vm_lifecycle_create_start_stop_rm") {
        return;
    }

    // Reuse the harness's binary/firmware/image resolution (LIMINA_BIN,
    // LIMINA_FIRMWARE, LIMINA_TEST_DISK all honored).
    let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
    let Boot::Firmware { firmware, disk, .. } = &cfg.boot else {
        panic!("fedora_from_env must give a firmware boot");
    };

    let scratch = std::env::temp_dir().join(format!("limina-vmdef-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch library");

    // --- create: import the shared image as a managed VM (COW clone into the bundle).
    let mut create = limina_cmd(&cfg.limina_bin, &scratch);
    create.args(["create", "vmdef-test", "--disk"]).arg(disk);
    run_ok(&mut create, "limina create");
    let bundle = scratch.join("vmdef-test.liminavm");
    assert!(bundle.join("vm.toml").is_file(), "vm.toml written");
    assert!(
        bundle.join("disks/root.raw").is_file(),
        "disk cloned into the bundle"
    );

    // --- start: headless boot from the definition. The bundle's NAT default exercises
    // the gateway path; the console override gives us the boot oracle.
    let console = scratch.join("console.log");
    let mut start = limina_cmd(&cfg.limina_bin, &scratch);
    start
        .args([
            "start",
            "vmdef-test",
            "--no-window",
            "--shutdown-grace-secs",
            "3",
        ])
        .arg("--firmware")
        .arg(firmware)
        .arg("--vmm-bin")
        .arg(&cfg.vmm_bin)
        .arg("--console")
        .arg(&console)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = KillOnDrop(start.spawn().expect("spawning limina start"));
    let supervisor_pid = child.0.id();

    // Boot oracle: the firmware banner then GRUB on the captured serial console —
    // proves the definition resolved to a real bootable VM (firmware read the cloned
    // virtio-blk disk, found the ESP, ran the bootloader).
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let text = std::fs::read_to_string(&console).unwrap_or_default();
        if text.contains("GRUB") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "guest did not reach GRUB within 90s; console so far:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // The run lock + pidfile are live and truthful.
    let pidfile = bundle.join("run/supervisor.pid");
    let recorded: u32 = std::fs::read_to_string(&pidfile)
        .expect("pidfile written")
        .trim()
        .parse()
        .expect("pidfile holds a pid");
    assert_eq!(recorded, supervisor_pid, "pidfile names the start process");

    // --- ls sees it running.
    let ls = run_ok(limina_cmd(&cfg.limina_bin, &scratch).arg("ls"), "limina ls");
    assert!(
        ls.contains("vmdef-test") && ls.contains("running"),
        "ls should show the VM running:\n{ls}"
    );

    // --- double start fails fast on the flock (well under the boot timescale).
    let t0 = Instant::now();
    let mut again = limina_cmd(&cfg.limina_bin, &scratch);
    again
        .args([
            "start",
            "vmdef-test",
            "--no-window",
            "--shutdown-grace-secs",
            "3",
        ])
        .arg("--firmware")
        .arg(firmware)
        .arg("--vmm-bin")
        .arg(&cfg.vmm_bin);
    let out = again.output().expect("spawning the second start");
    let elapsed = t0.elapsed();
    assert!(
        !out.status.success(),
        "a second start of a running VM must fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already running"),
        "second start should say why:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "flock fail-fast took {elapsed:?}"
    );

    // --- stop: SIGTERM ladder via the pidfile. The guest sits in GRUB/early boot and
    // ignores the power button, so this exercises the bounded escalation (agent grace
    // + the 3s shutdown grace + SIGKILL) and must still exit promptly.
    run_ok(
        limina_cmd(&cfg.limina_bin, &scratch).args(["stop", "vmdef-test"]),
        "limina stop",
    );
    let mut child = child;
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = child.0.try_wait().expect("reaping limina start") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "limina start did not exit after stop"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    eprintln!("limina start exited: {status}");

    let ls = run_ok(limina_cmd(&cfg.limina_bin, &scratch).arg("ls"), "limina ls");
    assert!(
        ls.contains("stopped"),
        "ls should show the VM stopped after stop:\n{ls}"
    );

    // --- rm deletes the bundle (with its cloned disk) but never the source image.
    run_ok(
        limina_cmd(&cfg.limina_bin, &scratch).args(["rm", "vmdef-test"]),
        "limina rm",
    );
    assert!(!bundle.exists(), "bundle deleted");
    assert!(disk.exists(), "the shared source image must survive rm");

    let _ = std::fs::remove_dir_all(&scratch);
}

/// `create --in-place` + `start <bundle-path>` (no library lookup): the definition
/// references the image where it is; nothing is copied into the bundle. Pure
/// host-side (no boot), so it runs even without HVF — but it still drives the real
/// binary, so keep it here with the lifecycle test rather than in unit tests.
#[test]
fn managed_vm_in_place_import_references_the_source() {
    let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
    let Boot::Firmware { disk, .. } = &cfg.boot else {
        panic!("fedora_from_env must give a firmware boot");
    };

    let scratch = std::env::temp_dir().join(format!("limina-vmdef-ip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch library");

    let mut create = limina_cmd(&cfg.limina_bin, &scratch);
    create
        .args(["create", "inplace-test", "--in-place", "--disk"])
        .arg(disk);
    run_ok(&mut create, "limina create --in-place");

    let bundle = scratch.join("inplace-test.liminavm");
    let toml = std::fs::read_to_string(bundle.join("vm.toml")).expect("vm.toml");
    let canonical: PathBuf = disk.canonicalize().expect("canonicalizing the image");
    assert!(
        toml.contains(canonical.to_str().unwrap()),
        "vm.toml must reference the source image absolutely:\n{toml}"
    );
    assert!(
        !bundle.join("disks/root.raw").exists(),
        "--in-place must not copy the image"
    );

    run_ok(
        limina_cmd(&cfg.limina_bin, &scratch).args(["rm", "inplace-test"]),
        "limina rm",
    );
    assert!(disk.exists(), "the referenced image must survive rm");

    let _ = std::fs::remove_dir_all(&scratch);
}
