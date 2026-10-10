// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — **the debug port**: a stock guest reads which host build it runs on, with stock tools.
//!
//! The port is `org.limina.debug.0` on the guest's virtio-console device; the supervisor answers
//! `identity` with the build stamp and this launch's facts (`docs/design/debug-port.md`). A test
//! harness running *inside* a guest has no other way to attribute a result to the host build that
//! produced it.
//!
//! # Oracles
//!
//! 1. **The port is on the bus** of an unmodified Fedora guest (`/dev/virtio-ports/…`).
//! 2. **The documented stock one-liner reads a whole answer** — bash and coreutils only — and
//!    what it reads is the build this supervisor printed at spawn. The comparison is against the
//!    supervisor's own `limina: identity` lines, not against a revision this test computes: the
//!    test binary and `limina` can be built from different trees, and the supervisor's line is
//!    what the *running* process says about itself. Matching the `launch_id` too proves the
//!    answer came from this VM's port, not a stale one.
//! 3. **The shipped helper** (`guest/limina-debug-identity`) prints the same answer.
//! 4. **A guest reboot gets a fresh launch** — the relaunched worker has the port again (every
//!    spawn path must create it) and a new `launch_id`. One VM, two launches, for the price of a
//!    reboot rather than a second cold boot.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const PORT: &str = "/dev/virtio-ports/org.limina.debug.0";

/// The documented stock-tools reader (docs/design/debug-port.md), as root because stock udev
/// leaves `/dev/vport*` root-only. Skips anything before the first `format=` line (the tail of
/// an answer an earlier reader abandoned), stops at the `.` terminator, and gives up after 10 s
/// of silence rather than hang.
const ONE_LINER: &str = "sudo bash -c 'exec 3<>/dev/virtio-ports/org.limina.debug.0; \
     echo identity >&3; on=; while IFS= read -r -t 10 l <&3; do \
     case $l in format=*) on=1;; esac; [ -n \"$on\" ] || continue; \
     [ \"$l\" = . ] && break; echo \"$l\"; done'";

fn parse(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .collect()
}

/// The identity blocks the supervisor printed, one per worker spawn, in order.
fn logged_identities(log: &str) -> Vec<BTreeMap<String, String>> {
    let mut out: Vec<BTreeMap<String, String>> = Vec::new();
    for line in log.lines() {
        let Some(kv) = line.strip_prefix("limina: identity ") else {
            continue;
        };
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        if k == "format" {
            out.push(BTreeMap::new());
        }
        if let Some(block) = out.last_mut() {
            block.insert(k.to_string(), v.to_string());
        }
    }
    out
}

fn read_port(guest: &Guest) -> BTreeMap<String, String> {
    let out = guest
        .ssh_exec_timeout(ONE_LINER, Duration::from_secs(60))
        .unwrap_or_else(|e| panic!("the stock one-liner failed on {PORT}: {e:#}"));
    eprintln!("--- {PORT} answered ---\n{out}");
    parse(&out)
}

/// Wait until the supervisor has printed `n` identity blocks (the line lands right after the
/// spawn; the guest is still booting when it does, but a test that reads the log the moment ssh
/// answers must not race it).
fn wait_for_identities(guest: &mut Guest, n: usize) -> Vec<BTreeMap<String, String>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ids = logged_identities(&guest.supervisor_log());
        if ids.len() >= n && ids[n - 1].contains_key("worker_pid") {
            return ids;
        }
        assert!(
            Instant::now() < deadline,
            "the supervisor printed {} identity block(s), wanted {n}. Log:\n{}",
            ids.len(),
            guest.supervisor_log()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn a_stock_guest_reads_the_host_build_from_the_debug_port() {
    const NAME: &str = "a_stock_guest_reads_the_host_build_from_the_debug_port";
    if !limina_test::require_hvf_or_skip(NAME) {
        return;
    }
    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg.with_net().with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED {NAME}: {e:#}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");

    // --- Oracle 1: the port reached a guest with nothing of ours installed ---
    let ls = guest
        .ssh_exec(&format!("ls -l {PORT} 2>&1 || true"))
        .unwrap_or_default();
    assert!(
        ls.contains("org.limina.debug.0") && !ls.contains("No such file"),
        "the guest has no {PORT} — the supervisor did not pass --debug-port-fd or the worker \
         did not attach it (crates/limina-vmm/src/krun/console.rs). Got: {ls}"
    );

    // --- Oracle 2: the stock one-liner reads this supervisor's identity ---
    let first = read_port(&guest);
    let logged = wait_for_identities(&mut guest, 1);
    let want = &logged[0];
    eprintln!("supervisor printed: {want:?}");
    assert_eq!(first.get("format").map(String::as_str), Some("1"));
    for key in [
        "limina_git_rev",
        "limina_version",
        "limina_build_date",
        "launch_id",
        "supervisor_pid",
        "worker_pid",
    ] {
        assert!(
            first.get(key).is_some_and(|v| !v.is_empty()),
            "the answer has no {key}: {first:?}"
        );
        assert_eq!(
            first.get(key),
            want.get(key),
            "{key} the guest read differs from what the supervisor printed at spawn"
        );
    }
    assert_ne!(
        first.get("limina_git_rev").map(String::as_str),
        Some("unknown")
    );
    assert!(
        first.keys().any(|k| k.starts_with("dep.libkrun")),
        "no libkrun revision in the answer: {first:?}"
    );
    // This run's shape: a flat, headless (no display device) EFI boot with the harness's sizes.
    for (key, val) in [
        ("vm_kind", "flat"),
        ("boot", "efi"),
        ("display", "none"),
        ("gpu", "none"),
        ("cpus", &cfg.cpus.to_string()),
        ("ram_mib", &cfg.ram_mib.to_string()),
        ("resumed", "no"),
    ] {
        assert_eq!(first.get(key).map(String::as_str), Some(val), "{key}");
    }
    assert!(
        first
            .get("host_os")
            .is_some_and(|v| v.starts_with("macOS "))
    );
    assert!(first.get("host_model").is_some_and(|v| !v.is_empty()));

    // An unknown request is answered (with an error), never left hanging.
    let err = guest
        .ssh_exec_timeout(
            &ONE_LINER.replace("echo identity", "echo bogus"),
            Duration::from_secs(60),
        )
        .expect("an unknown request must still get an answer");
    assert!(
        err.contains("error="),
        "no error for a bogus request: {err}"
    );

    // --- Oracle 3: the shipped helper prints the same answer ---
    let helper = limina_test::repo_root().join("guest/limina-debug-identity");
    guest
        .scp_to_guest(Path::new(&helper), "/tmp/limina-debug-identity")
        .expect("copying the helper into the guest");
    let via_helper = guest
        .ssh_exec_timeout(
            "sudo bash /tmp/limina-debug-identity",
            Duration::from_secs(60),
        )
        .expect("the helper failed");
    assert_eq!(
        parse(&via_helper),
        first,
        "the helper and the one-liner disagree"
    );

    // --- Oracle 4: a reboot relaunches the worker, with the port and a fresh launch id ---
    guest
        .ssh_exec("sudo systemd-run --on-active=1 systemctl reboot")
        .expect("scheduling guest reboot");
    let deadline = Instant::now() + Duration::from_secs(300);
    let second = loop {
        assert!(
            Instant::now() < deadline,
            "the guest never answered with a new launch after the reboot"
        );
        std::thread::sleep(Duration::from_secs(3));
        let Ok(out) = guest.ssh_exec_timeout(ONE_LINER, Duration::from_secs(30)) else {
            continue;
        };
        let ids = parse(&out);
        if ids
            .get("launch_id")
            .is_some_and(|id| Some(id) != first.get("launch_id"))
        {
            break ids;
        }
    };
    let logged = wait_for_identities(&mut guest, 2);
    assert_eq!(
        second.get("launch_id"),
        logged[1].get("launch_id"),
        "after the reboot the guest read a launch id the supervisor never printed"
    );
    assert_eq!(second.get("limina_git_rev"), first.get("limina_git_rev"));
    assert_ne!(second.get("worker_pid"), first.get("worker_pid"));
    assert_eq!(second.get("supervisor_pid"), first.get("supervisor_pid"));

    guest
        .shutdown(Duration::from_secs(60))
        .expect("supervisor did not stop");
}
