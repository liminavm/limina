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
//! 1. **The port is on the bus** of an unmodified Fedora guest (`/dev/virtio-ports/…`), with
//!    the `debug-port` lever off — its default, which this boot keeps.
//! 2. **Off, it says nothing but that.** `identity` and `help` both read `format=1`,
//!    `error=disabled` and how to enable it, with no build fact; the helper exits 4. Then
//!    `limina debug <pid> lever debug-port on` turns it on at runtime, and the next request is
//!    answered in full.
//! 3. **The documented stock one-liner reads a whole answer** — bash and coreutils only — and
//!    what it reads is the build this supervisor printed at spawn. The comparison is against the
//!    supervisor's own `limina: identity` lines, not against a revision this test computes: the
//!    test binary and `limina` can be built from different trees, and the supervisor's line is
//!    what the *running* process says about itself. Matching the `launch_id` too proves the
//!    answer came from this VM's port, not a stale one.
//! 4. **The shipped helper** (`guest/limina-debug-identity`) prints the same answer.
//! 5. **A guest reboot gets a fresh launch** — the relaunched worker has the port again (every
//!    spawn path must create it) and a new `launch_id`. One VM, two launches, for the price of a
//!    reboot rather than a second cold boot. It also proves a lever turned on at runtime outlives
//!    the worker it was turned on under: the supervisor holds it.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig, debug_port_reader, parse_port_answer};

const PORT: &str = "/dev/virtio-ports/org.limina.debug.0";

fn read_port(guest: &Guest) -> BTreeMap<String, String> {
    let out = guest
        .debug_port("identity")
        .unwrap_or_else(|e| panic!("the stock one-liner failed on {PORT}: {e:#}"));
    eprintln!("--- {PORT} answered ---\n{out:?}");
    out
}

#[test]
fn a_stock_guest_reads_the_host_build_from_the_debug_port() {
    const NAME: &str = "a_stock_guest_reads_the_host_build_from_the_debug_port";
    if !limina_test::require_hvf_or_skip(NAME) {
        return;
    }
    assert!(
        std::env::var_os("LIMINA_DEBUG_PORT").is_none(),
        "unset LIMINA_DEBUG_PORT; the supervisor inherits it, and this test boots with the lever \
         at its default"
    );
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

    // --- Oracle 2: off by default, the port answers that and nothing else ---
    for request in ["identity", "help"] {
        let off = guest
            .debug_port(request)
            .unwrap_or_else(|e| panic!("`{request}` got no answer while disabled: {e:#}"));
        eprintln!("--- {request} while disabled: {off:?}");
        assert_eq!(
            off.get("format").map(String::as_str),
            Some("1"),
            "{request}"
        );
        assert_eq!(
            off.get("error").map(String::as_str),
            Some("disabled"),
            "{request}"
        );
        assert!(
            off.get("enable")
                .is_some_and(|v| v.contains("LIMINA_DEBUG_PORT=1")),
            "{request}: no enable hint: {off:?}"
        );
        assert_eq!(
            off.keys().collect::<Vec<_>>(),
            ["enable", "error", "format"],
            "{request}: the disabled answer carries more than it should: {off:?}"
        );
    }
    let helper = limina_test::repo_root().join("guest/limina-debug-identity");
    guest
        .scp_to_guest(Path::new(&helper), "/tmp/limina-debug-identity")
        .expect("copying the helper into the guest");
    let disabled = guest
        .ssh_exec_timeout(
            "sudo bash /tmp/limina-debug-identity limina_git_rev 2>&1; echo exit=$?",
            Duration::from_secs(60),
        )
        .expect("running the helper");
    assert!(
        disabled.contains("exit=4") && disabled.contains("disabled"),
        "the helper must exit 4 on a disabled port: {disabled}"
    );
    let pid = guest.supervisor_pid().to_string();
    let out = Command::new(&cfg.limina_bin)
        .args(["debug", &pid, "lever", "debug-port", "on"])
        .stdin(Stdio::null())
        .output()
        .expect("running limina debug");
    assert!(
        out.status.success(),
        "turning the debug-port lever on failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // --- Oracle 3: the stock one-liner reads this supervisor's identity ---
    let first = read_port(&guest);
    let want = &guest
        .wait_for_launch(1, Duration::from_secs(30))
        .expect("the supervisor printed no identity block");
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
        .ssh_exec_timeout(&debug_port_reader("bogus"), Duration::from_secs(60))
        .expect("an unknown request must still get an answer");
    assert!(
        err.contains("error="),
        "no error for a bogus request: {err}"
    );

    // --- Oracle 4: the shipped helper prints the same answer ---
    let via_helper = guest
        .ssh_exec_timeout(
            "sudo bash /tmp/limina-debug-identity",
            Duration::from_secs(60),
        )
        .expect("the helper failed");
    assert_eq!(
        parse_port_answer(&via_helper),
        first,
        "the helper and the one-liner disagree"
    );

    // --- Oracle 5: a reboot relaunches the worker, with the port, a fresh launch id, and the
    // lever still on ---
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
        let Ok(out) =
            guest.ssh_exec_timeout(&debug_port_reader("identity"), Duration::from_secs(30))
        else {
            continue;
        };
        let ids = parse_port_answer(&out);
        if ids
            .get("launch_id")
            .is_some_and(|id| Some(id) != first.get("launch_id"))
        {
            break ids;
        }
    };
    let relaunch = guest
        .wait_for_launch(2, Duration::from_secs(30))
        .expect("the supervisor printed no identity block for the relaunch");
    assert_eq!(
        second.get("launch_id"),
        relaunch.get("launch_id"),
        "after the reboot the guest read a launch id the supervisor never printed"
    );
    assert_eq!(second.get("limina_git_rev"), first.get("limina_git_rev"));
    assert_ne!(second.get("worker_pid"), first.get("worker_pid"));
    assert_eq!(second.get("supervisor_pid"), first.get("supervisor_pid"));

    guest
        .shutdown(Duration::from_secs(60))
        .expect("supervisor did not stop");
}
