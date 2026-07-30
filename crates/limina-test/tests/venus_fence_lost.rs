// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! §22 wedge invariant: **a guest fence must never vanish** — an exported venus
//! sync_file has to signal even when the host rejects the command that carried it.
//!
//! The live wedge (dogfood-mac, 2026-07-30): a host sleep *attempt* s2idled the guest;
//! the resume classifier took the fail-closed session wipe; niri's in-flight
//! explicit-sync fence was then decoded against the wiped session —
//! `create_fence 30247 -> ErrRutabaga(InvalidContextId)` — and the worker parked
//! the response descriptor waiting on a fence that was never created. The guest's
//! dma_fence (no timeout, no error path) never signaled, the KMS atomic commit
//! hung in `commit_tail` (D-state kworker), and the DRM device wedged for every
//! later session until reboot.
//!
//! This guard reproduces the create-fence failure deterministically with the
//! worker's one-shot fault seam (`LIMINA_GPU_TEST_FAIL_NEXT_FENCE=1`) instead of
//! racing a real suspend: the guest boots **headless** (multi-user.target), so
//! the vkfencestorm client's exports are the only context-ring fences and the
//! seam poisons exactly one of them. Oracles:
//!
//!  - every exported sync_file signals (the storm prints `SIGNAL n`, no `STUCK`);
//!  - the worker log shows the seam engaged AND the fence was retired as lost
//!    (guards against a vacuous green where the seam never fired);
//!  - the guest's fence ledger reconverges: `virtio-gpu-irq-fence` last-signaled
//!    catches up to last-emitted (the exact counter pair that stayed
//!    `30246 30247` forever on dogfood-mac).
//!
//! Same prereqs as the other venus L2 guards (the enhanced golden + KosmicKrisp);
//! SKIPs cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The ctypes plumbing vkfencestorm imports (staged next to it).
const VKFDCYCLE: &str = include_str!("../guest/vkfdcycle.py");
const VKPIPELINE: &str = include_str!("../guest/vkpipeline.py");
const VKFENCESTORM: &str = include_str!("../guest/vkfencestorm.py");

/// Exports per run. The seam kills exactly one; the rest prove the storm itself
/// is healthy (a stack where *every* export sticks is a different bug).
const STORM_ITERATIONS: usize = 50;

/// `ssh_exec` with a few retries: a loaded host can drop a single connection
/// right after the banner poll succeeded (same helper as the other venus guards).
fn ssh_retry(guest: &Guest, cmd: &str) -> String {
    let mut last_err = String::new();
    for _ in 0..4 {
        match guest.ssh_exec(cmd) {
            Ok(out) => return out.trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("ssh `{cmd}` kept failing: {last_err}");
}

#[test]
fn lost_context_fence_still_signals_its_sync_file() {
    if !limina_test::require_hvf_or_skip("lost_context_fence_still_signals_its_sync_file") {
        return;
    }
    let cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED lost_context_fence_still_signals_its_sync_file: {e}");
            return;
        }
    };
    // Headless: no compositor, no KMS flips — the storm's exports are the only
    // context-ring fences in the whole session, so the one-shot seam
    // deterministically poisons one of them (and nothing else can wedge).
    let cfg = cfg
        .append_cmdline("systemd.unit=multi-user.target")
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log();

    // Inherited by supervisor -> worker: the file-armed create-fence failure seam.
    // The trigger file does NOT exist yet — phase 1 must run clean; the test drops
    // the file right before the poisoned export (a later healthy fence on the same
    // ring would mask the loss, so the poisoned one must be the LAST).
    let seam_file = std::env::temp_dir().join(format!("limina-fail-fence-{}", std::process::id()));
    let _ = std::fs::remove_file(&seam_file);
    std::env::set_var("LIMINA_GPU_TEST_FAIL_FENCE_FILE", &seam_file);
    std::env::set_var("LIMINA_GPU_TEST_TRACE_FENCES", "1");

    eprintln!("booting the enhanced golden headless (fence-failure seam armed)");
    let mut g = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = g
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    ssh_retry(
        &g,
        &format!("cat > /tmp/vkfdcycle.py <<'VKFDCYCLE_PY_EOF'\n{VKFDCYCLE}\nVKFDCYCLE_PY_EOF"),
    );
    ssh_retry(
        &g,
        &format!("cat > /tmp/vkpipeline.py <<'VKPIPELINE_PY_EOF'\n{VKPIPELINE}\nVKPIPELINE_PY_EOF"),
    );
    ssh_retry(
        &g,
        &format!(
            "cat > /tmp/vkfencestorm.py <<'VKFENCESTORM_PY_EOF'\n{VKFENCESTORM}\nVKFENCESTORM_PY_EOF"
        ),
    );

    // sudo everywhere below: a headless (multi-user.target) boot has no logind
    // seat, so no uaccess ACL lands on /dev/dri and the loader enumerates nothing
    // as a plain user.
    const STORM_ENV: &str =
        "sudo env VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json";

    // Phase 1 — storm health: with the seam unarmed, every export must signal.
    let out = g
        .ssh_exec_timeout(
            &format!(
                "{STORM_ENV} python3 /tmp/vkfencestorm.py {STORM_ITERATIONS} 2>&1 || \
                 echo STORM-EXIT-$?"
            ),
            Duration::from_secs(180),
        )
        .expect("running vkfencestorm (phase 1) in the guest");
    eprintln!("--- vkfencestorm phase 1 ---\n{out}\n--------------------");
    if out.contains("UNSUPPORTED") || out.contains("NODEV") {
        panic!("venus missing on the enhanced golden — not a fence bug:\n{out}");
    }
    assert!(
        out.contains(&format!("STORM DONE {STORM_ITERATIONS}/{STORM_ITERATIONS}"))
            && !out.contains("STUCK"),
        "unarmed storm is not healthy — a different bug:\n{out}"
    );

    // Phase 2 — the poisoned export. Start the one-shot client, let it finish
    // device init (STORM READY), arm the seam, THEN release it: its single
    // export is the last fence on its ring, so nothing can mask the loss.
    ssh_retry(&g, "sudo rm -f /tmp/storm-go /tmp/storm-go2 /tmp/storm-out");
    let wait_marker = |g: &Guest, marker: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let peek = ssh_retry(g, "cat /tmp/storm-out 2>/dev/null || true");
            if peek.contains(marker) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "one-shot storm never reached {marker}: {peek}"
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    };
    let g2 = std::thread::scope(|s| {
        let handle = s.spawn(|| {
            g.ssh_exec_timeout(
                &format!(
                    "{STORM_ENV} python3 /tmp/vkfencestorm.py 1 --wait-go > /tmp/storm-out 2>&1; \
                     cat /tmp/storm-out"
                ),
                Duration::from_secs(180),
            )
        });
        // Gate 1: device init + command recording done — release into the submit.
        wait_marker(&g, "STORM READY");
        ssh_retry(&g, "touch /tmp/storm-go");
        // Gate 2: the submit (and its own fence-out) are behind us; arm the seam NOW.
        // The next queue-ring (ring != 0) fence is the EXPORT's execbuf fence — the
        // one that backs the polled fd (ledger-verified: export bumps emitted by two,
        // a CPU-ring kick the seam skips plus the queue fence the fd rides), and the
        // last fence this ring will ever see, so nothing can mask the loss.
        wait_marker(&g, "STORM SUBMITTED");
        std::fs::write(&seam_file, b"poison").expect("arming the fence-failure seam");
        ssh_retry(&g, "touch /tmp/storm-go2");
        handle.join().expect("phase-2 ssh thread panicked")
    })
    .expect("running vkfencestorm (phase 2) in the guest");
    eprintln!("--- vkfencestorm phase 2 ---\n{g2}\n--------------------");
    let slog = g.supervisor_log();
    let tail: Vec<&str> = slog
        .lines()
        .filter(|l| l.contains("CTXFENCE") || l.contains("refused") || l.contains("fence"))
        .collect();
    eprintln!(
        "--- worker fence lines ---\n{}\n--------------------",
        tail.join("\n")
    );

    // The seam must actually have fired, and the poisoned fence must have been
    // retired as lost — otherwise a green run proves nothing.
    assert!(
        !seam_file.exists(),
        "the fence-failure seam never engaged (trigger file still present) — no \
         context fence was created after arming"
    );
    g.wait_for_supervisor_log("refused by renderer", Duration::from_secs(15))
        .expect("worker log never recorded the injected create-fence failure");

    // THE invariant: the poisoned export's sync_file still signals (pre-fix it
    // parks forever host-side and the guest fd never sees POLLIN — the §22 wedge).
    assert!(
        !g2.contains("STUCK"),
        "the lost fence's sync_file never signaled — a guest waiting on it (a KMS \
         atomic commit on dogfood-mac) wedges forever. The §22 signature:\n{g2}"
    );
    assert!(
        g2.contains("STORM DONE 1/1"),
        "poisoned one-shot export did not complete:\n{g2}"
    );

    // Ledger reconvergence, best-effort: last-signaled catches up to last-emitted
    // (the pair that stayed `fence 30246 30247` forever on dogfood-mac). The 6.12 test
    // kernel may not expose the debugfs file — skip the oracle if absent.
    let ledger_path = ssh_retry(
        &g,
        "sudo find /sys/kernel/debug/dri -name '*fence*' 2>/dev/null | head -1",
    );
    if ledger_path.is_empty() {
        eprintln!("no virtio-gpu fence debugfs on this kernel; skipping the ledger oracle");
    } else {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let ledger = ssh_retry(&g, &format!("sudo cat {ledger_path} | head -1"));
            let nums: Vec<u64> = ledger
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            if nums.len() == 2 && nums[0] == nums[1] {
                eprintln!("fence ledger reconverged: {ledger}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "guest fence ledger never reconverged (emitted != signaled 15s after \
                 the storm): `{ledger}` — a fence vanished"
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    g.shutdown(Duration::from_secs(60))
        .expect("guest failed to shut down cleanly");
}
