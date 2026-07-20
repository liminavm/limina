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

/// Run a command in the guest over SSH (key auth, BatchMode so a missing key fails fast instead of
/// prompting). `None` if SSH isn't up or the command failed. The managed VM's `--ssh-port` gives a
/// deterministic host port (gvproxy forwards it to the guest's sshd).
fn ssh_exec(port: &str, cmd: &str) -> Option<String> {
    let out = Command::new("ssh")
        .args([
            "-p",
            port,
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

/// Poll until the guest answers SSH or the timeout expires.
fn wait_ssh(port: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ssh_exec(port, "true").is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

/// M9.2 managed suspend/resume happy path — the flagship suspend feature end-to-end through the real
/// verbs: `create → start → suspend → start`. A full-distro disk VM (systemd + logind s2idle the
/// guest when the supervisor pulses the GPIO suspend button) is snapshotted and torn down by
/// `limina suspend` (state.toml records `[suspended]` + a snapshot on disk), and the next
/// `limina start` RESTORES it — the guest resumes the **same boot_id** (resumed, not rebooted) with
/// live networking, and the `[suspended]` record is consumed. Needs the Fedora image + SSH; skips
/// (via `fedora_from_env`) if the image/firmware are absent.
#[test]
fn managed_vm_suspends_and_resumes() {
    if !limina_test::require_hvf_or_skip("managed_vm_suspends_and_resumes") {
        return;
    }

    let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
    let Boot::Firmware { firmware, disk, .. } = &cfg.boot else {
        panic!("fedora_from_env must give a firmware boot");
    };
    const PORT: &str = "2244";

    let scratch = std::env::temp_dir().join(format!("limina-vmdef-sr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch library");

    // create: COW-clone the shared image into the bundle (never mutate the source). 4 GiB is
    // load-bearing, NOT arbitrary: the guest-RAM region is then exactly 0x1_0000_0000 bytes, which is
    // the value that overflowed the snapshot format's byte-section length when it was a u32 (truncated
    // to 0 → restore wrote back an empty region → guest resumed into blank RAM). This test is the
    // ≥4 GiB restore regression guard for libkrun patch 0067; a ≤2 GiB VM cannot reach the bug. The
    // managed VM is dynamic with a 1 GiB floor, so the resident cost stays modest.
    run_ok(
        limina_cmd(&cfg.limina_bin, &scratch)
            .args(["create", "susp-test", "--memory", "4G", "--disk"])
            .arg(disk),
        "limina create",
    );
    let bundle = scratch.join("susp-test.liminavm");

    // A fresh `limina start` command (headless; deterministic SSH port). Reused for the cold boot
    // and the restore boot — the disk/firmware/vmm all come from the definition + overrides.
    let start_cmd = || {
        let mut c = limina_cmd(&cfg.limina_bin, &scratch);
        c.args([
            "start",
            "susp-test",
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
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        c
    };

    // --- cold boot ---
    let mut boot1 = KillOnDrop(start_cmd().spawn().expect("spawning limina start #1"));
    assert!(
        wait_ssh(PORT, Duration::from_secs(150)),
        "guest did not reach SSH on the cold boot"
    );
    let pre = ssh_exec(PORT, "cat /proc/sys/kernel/random/boot_id")
        .expect("reading the pre-suspend boot_id over SSH");
    assert!(!pre.is_empty(), "empty pre-suspend boot_id");

    // --- suspend: `limina suspend` relays SIGTSTP → the supervisor runs the bracket (snapshot +
    // teardown), persists [suspended], and the start #1 supervisor exits 126. cmd_suspend blocks
    // until the VM stops, so a success here already means the teardown happened.
    run_ok(
        limina_cmd(&cfg.limina_bin, &scratch).args(["suspend", "susp-test"]),
        "limina suspend",
    );
    let st = boot1.0.wait().expect("reaping the suspended start #1");
    assert_eq!(
        st.code(),
        Some(126),
        "the suspended supervisor should exit 126 (snapshotted); got {st:?}"
    );
    let state = std::fs::read_to_string(bundle.join("state.toml")).unwrap_or_default();
    assert!(
        state.contains("[suspended]"),
        "state.toml must record the suspend:\n{state}"
    );
    let snap = bundle.join("run/snapshot.bin");
    let snap_len = std::fs::metadata(&snap).map(|m| m.len()).unwrap_or(0);
    assert!(
        snap_len > 4096,
        "snapshot.bin missing or implausibly small ({snap_len} bytes)"
    );

    // --- restore: the next start reads [suspended] and boots --restore ---
    let mut boot2 = KillOnDrop(
        start_cmd()
            .spawn()
            .expect("spawning limina start #2 (restore)"),
    );
    assert!(
        wait_ssh(PORT, Duration::from_secs(150)),
        "guest did not come back on SSH after the restore"
    );
    let post = ssh_exec(PORT, "cat /proc/sys/kernel/random/boot_id")
        .expect("reading the post-restore boot_id over SSH");
    assert_eq!(
        pre, post,
        "boot_id changed → the VM REBOOTED instead of resuming from the snapshot"
    );
    // The [suspended] record was consumed on start (so a later start cold-boots, not restore-loops).
    let state2 = std::fs::read_to_string(bundle.join("state.toml")).unwrap_or_default();
    assert!(
        !state2.contains("[suspended]"),
        "the [suspended] record must be consumed on restore:\n{state2}"
    );
    // SINGLE-USE (M9.4-1b): the snapshot must be renamed off its canonical path at consume time —
    // a snapshot restored twice against the advanced disk destroys the filesystem (stale btrfs
    // metadata), so after a restore NOTHING may find it at the canonical name.
    assert!(
        !snap.exists(),
        "snapshot.bin must be renamed away (single-use) once a restore consumed it"
    );

    // --- teardown: stop the restored VM, then rm the bundle ---
    run_ok(
        limina_cmd(&cfg.limina_bin, &scratch).args(["stop", "susp-test"]),
        "limina stop",
    );
    let _ = boot2.0.wait();
    // The worker exit also reaps the consumed copy (the ~half-GB cleanup).
    assert!(
        !bundle.join("run/snapshot.bin.consumed").exists(),
        "the consumed snapshot copy must be deleted once the worker exits"
    );
    run_ok(
        limina_cmd(&cfg.limina_bin, &scratch).args(["rm", "susp-test"]),
        "limina rm",
    );
    assert!(disk.exists(), "the shared source image must survive");
    let _ = std::fs::remove_dir_all(&scratch);
}
