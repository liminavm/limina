// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M9.3 seamless-resume acceptance: the seated GNOME session must SURVIVE a
//! snapshot/restore round-trip — same gnome-shell process, no compositor crash.
//!
//! This is the gate test for `docs/design/venus-snapshot-replay.md`. The transport
//! layer already resumes clean (libkrun 0072 sticky queue re-arm: same boot_id, SSH
//! back, CRTC lights, rendering works), but the host restores with an EMPTY GPU
//! world while the guest's mesa still believes in its pre-suspend venus context and
//! rings. Its first real submission after thaw spins in `vn_relax` until mesa's
//! dead-ring abort threshold (~17 s) and the shell SIGABRTs — every Wayland client
//! dies and GNOME starts a FRESH session. What looks like recovery is a restart
//! (root-caused 2026-07-19 from the coredump: abort ← vn_relax ←
//! vn_ring_submit_locked ← vn_CreateImage ← cogl glyph upload; see
//! `spikes/m9-freeze-trigger/RESULTS.md` round 10).
//!
//! RED until the venus snapshot-replay phases land (P1: replay the re-creation
//! journals + ring-blob contents so the guest's rings stay serviced). The oracles
//! are deliberately about *identity*, not rendering: same boot_id (it resumed, not
//! rebooted), same gnome-shell PID across the round-trip AND past the ~17 s abort
//! window, and zero new gnome-shell coredumps. Visual fidelity is P2's gate, not
//! this test's.
//!
//! Same prereqs as `venus_fd_census`: the seated enhanced golden + KosmicKrisp;
//! SKIPs cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::path::PathBuf;
use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// How long past SSH-back we insist the shell stays alive. The observed abort fires
/// ~17 s after restore (mesa's vn_relax dead-ring threshold); 35 s covers it with
/// margin without meaningfully slowing a green run.
const ABORT_WINDOW: Duration = Duration::from_secs(35);

/// The fd-census ctypes plumbing (staged next to the holder, which imports it).
const VKFDCYCLE: &str = include_str!("../guest/vkfdcycle.py");
/// Parks a freed-while-exported cross-context blob across the suspend (ctx-15 hazard).
const VKFDHOLD: &str = include_str!("../guest/vkfdhold.py");
/// Parks a pattern in a never-mapped (non-blob) VkDeviceMemory across the suspend
/// and verifies it after restore (P2 content capture).
const VKCONTENT: &str = include_str!("../guest/vkcontent.py");
/// Parks a live compute pipeline whose shader module + layout were destroyed after
/// creation (the journal create-arg closure hazard — the 2026-07-20 vkmark crash),
/// heartbeating dispatches that reference the pipeline across the suspend.
const VKPIPELINE: &str = include_str!("../guest/vkpipeline.py");

/// `ssh_exec` with a few retries: a loaded host (the suite runs several VMs) can
/// drop a single connection (exit 255) right after the banner poll succeeded.
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
fn seated_gnome_session_survives_snapshot_restore() {
    if !limina_test::require_hvf_or_skip("seated_gnome_session_survives_snapshot_restore") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED seated_gnome_session_survives_snapshot_restore: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let base_cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED seated_gnome_session_survives_snapshot_restore: {e}");
            return;
        }
    };

    // The harness's injected 16 KiB test kernel (a 6.12 build) has no freeze support in
    // virtio_i2c/virtio_snd, so those two devices hold out the s2idle quiesce and the
    // suspend bracket aborts. The test needs neither battery nor audio — drop them on
    // BOTH sides of the round-trip (restore requires an identical device topology).
    //
    // The MAC is pinned on both sides: the restored guest keeps the NIC identity it read
    // at boot (it does not re-probe config space mid-resume), and production restore
    // carries the managed VM's MAC forward the same way. A fresh random MAC on the
    // restore worker orphans the guest's cached one and SSH never comes back.
    const NET_MAC: &str = "5a:94:ef:44:0f:aa";
    let devices = |cfg: GuestConfig| {
        cfg.with_supervisor_arg("--no-snd")
            .with_supervisor_arg("--no-battery")
            .with_net_mac(NET_MAC)
    };

    // --- Guest 1: seated venus desktop, snapshot-armed ---
    let cfg1 = devices(base_cfg.clone())
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .with_snapshot();
    // Inherited by supervisor -> worker: this test suspends from INSIDE the guest, so
    // the bracket must not add a second (button) suspend trigger — see the bracket
    // comment below.
    std::env::set_var("LIMINA_BRACKET_NO_BUTTON", "1");
    eprintln!("booting the seated enhanced venus desktop (snapshot-armed)");
    let mut g1 = Guest::boot(&cfg1).expect("spawning the limina supervisor");
    let banner = g1
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    g1.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated enhanced session didn't come up");

    // Let the session settle: the shell's venus world (rings, glyph caches, scanouts)
    // should be steady-state, matching the real suspend-a-working-desktop scenario.
    std::thread::sleep(Duration::from_secs(10));

    // --- Seed the ctx-15 replay hazard (multi-context; eyeball item 3) ---
    // A second venus client parks a cross-context blob whose EXPORTING VkDeviceMemory
    // it has already freed while the import keeps the resource attached — Xwayland's
    // window-buffer flow. Replaying its context needs the freed alloc back before the
    // blob create (virgl journal pin + retained free); pre-fix this was a structural
    // CREATE_BLOB failure that killed the whole replay.
    ssh_retry(
        &g1,
        &format!("cat > /tmp/vkfdcycle.py <<'VKFDCYCLE_PY_EOF'\n{VKFDCYCLE}\nVKFDCYCLE_PY_EOF"),
    );
    ssh_retry(
        &g1,
        &format!("cat > /tmp/vkfdhold.py <<'VKFDHOLD_PY_EOF'\n{VKFDHOLD}\nVKFDHOLD_PY_EOF"),
    );
    ssh_retry(
        &g1,
        "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
         setsid nohup python3 /tmp/vkfdhold.py Venus </dev/null >/tmp/vkfdhold.out 2>&1 & \
         echo spawned",
    );
    g1.ssh_poll(
        "grep -q 'FDHOLD READY' /tmp/vkfdhold.out",
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| {
        let out = g1.ssh_exec("cat /tmp/vkfdhold.out").unwrap_or_default();
        panic!("the fd-hold venus client never reached READY: {e}\n--- vkfdhold.out ---\n{out}");
    });
    let holder_pid = ssh_retry(&g1, "pgrep -f 'python3 /tmp/vkfdhold.py' | head -1");
    assert!(!holder_pid.is_empty(), "no vkfdhold pid after READY");
    eprintln!("fd-hold venus client parked (pid {holder_pid})");

    // --- Seed the P2 content oracle ---
    // A third venus client parks a known pattern in a NEVER-MAPPED VkDeviceMemory
    // (vn defers blob creation until vkMapMemory, so this memory is a plain host
    // allocation — in neither the guest-RAM dump nor the mapped-blob capture).
    // Post-restore it copies the memory back and verifies the pattern: the direct
    // oracle for full VkDeviceMemory content capture.
    ssh_retry(
        &g1,
        &format!("cat > /tmp/vkcontent.py <<'VKCONTENT_PY_EOF'\n{VKCONTENT}\nVKCONTENT_PY_EOF"),
    );
    ssh_retry(
        &g1,
        "rm -f /tmp/vkcontent-go; \
         VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
         setsid nohup python3 /tmp/vkcontent.py Venus </dev/null >/tmp/vkcontent.out 2>&1 & \
         echo spawned",
    );
    g1.ssh_poll(
        "grep -q 'CONTENT READY' /tmp/vkcontent.out",
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| {
        let out = g1.ssh_exec("cat /tmp/vkcontent.out").unwrap_or_default();
        panic!("the content venus client never reached READY: {e}\n--- vkcontent.out ---\n{out}");
    });
    eprintln!("content venus client parked (pattern staged in non-blob device memory)");

    // --- Seed the create-arg closure hazard (the 2026-07-20 vkmark crash) ---
    // A fourth venus client creates a compute pipeline, destroys its shader module
    // and pipeline layout (legal, and what every real app does), then heartbeats
    // dispatches that reference the pipeline. Replaying its context needs the
    // pruned module/layout creates back before the pipeline create (journal
    // create-arg pinning); pre-fix the pipeline create drops at replay and the
    // first post-restore beat hits a sticky ring FATAL — the host ring thread
    // exits, the guest sees VK_RING_STATUS_FATAL_BIT_MESA, and the client aborts
    // in vn_ring_submit_locked exactly like vkmark did
    // (spikes/m9-vkmark-resume-crash/RESULTS.md).
    ssh_retry(
        &g1,
        &format!("cat > /tmp/vkpipeline.py <<'VKPIPELINE_PY_EOF'\n{VKPIPELINE}\nVKPIPELINE_PY_EOF"),
    );
    ssh_retry(
        &g1,
        "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
         setsid nohup python3 /tmp/vkpipeline.py Venus </dev/null >/tmp/vkpipeline.out 2>&1 & \
         echo spawned",
    );
    g1.ssh_poll(
        "grep -q 'PIPE READY' /tmp/vkpipeline.out",
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| {
        let out = g1.ssh_exec("cat /tmp/vkpipeline.out").unwrap_or_default();
        panic!("the pipeline venus client never reached READY: {e}\n--- vkpipeline.out ---\n{out}");
    });
    let pipe_pid = ssh_retry(&g1, "pgrep -f '[v]kpipeline.py' | head -1");
    assert!(!pipe_pid.is_empty(), "no vkpipeline pid after READY");
    // Like the fd-hold client: the heartbeat must be ADVANCING at suspend time so a
    // post-restore stall can't be blamed on a pre-existing wedge.
    g1.ssh_poll(
        "test \"$(grep -c 'PIPE BEAT' /tmp/vkpipeline.out)\" -ge 2",
        Duration::from_secs(30),
    )
    .unwrap_or_else(|e| {
        let out = g1.ssh_exec("cat /tmp/vkpipeline.out").unwrap_or_default();
        panic!(
            "the pipeline heartbeat wedged BEFORE the suspend (pre-existing guest-side \
             stall, not a restore failure): {e}\n--- vkpipeline.out ---\n{out}"
        );
    });
    eprintln!("pipeline venus client parked (pid {pipe_pid}, module+layout destroyed)");

    // The fd-hold heartbeat must be ADVANCING at suspend time, or the post-restore
    // beat assert can blame the restore for a wedge that predates it (run 35: the
    // holder froze at beat 1, seconds after READY and long before the snapshot).
    g1.ssh_poll(
        "test \"$(grep -c 'FDHOLD BEAT' /tmp/vkfdhold.out)\" -ge 2",
        Duration::from_secs(30),
    )
    .unwrap_or_else(|e| {
        let out = g1.ssh_exec("cat /tmp/vkfdhold.out").unwrap_or_default();
        panic!(
            "the fd-hold heartbeat wedged BEFORE the suspend (pre-existing guest-side \
             stall, not a restore failure): {e}\n--- vkfdhold.out ---\n{out}"
        );
    });

    // Identity baseline: the resumed guest must present the SAME kernel boot and the
    // SAME compositor process.
    let boot_id = ssh_retry(&g1, "cat /proc/sys/kernel/random/boot_id");
    let shell_pid = ssh_retry(&g1, "pgrep -x gnome-shell | head -1");
    assert!(
        !shell_pid.is_empty(),
        "no gnome-shell pid before the snapshot"
    );
    // Coredump baseline (count, not emptiness: the golden may carry old cores).
    let cores_before = ssh_retry(
        &g1,
        "sudo coredumpctl list --no-legend gnome-shell 2>/dev/null | wc -l",
    );
    eprintln!("pre-suspend: boot_id={boot_id} gnome-shell pid={shell_pid} cores={cores_before}");

    // --- Suspend: s2idle bracket -> quiesce -> snapshot -> worker exits 126 ---
    // The bracket's suspend-button pulse is swallowed depending on the desktop's
    // session state (observed: a fresh seated session ignores it and no device ever
    // leaves DRIVER_OK), so trigger the s2idle from INSIDE the guest, scheduled a
    // beat ahead and inhibitor-ignoring, then arm the bracket for quiesce+snapshot.
    // The bracket must NOT also pulse the button (LIMINA_BRACKET_NO_BUTTON, inherited
    // by the worker): with two suspend triggers, whichever lands after userspace
    // freezes replays on resume and re-suspends the restored guest into an
    // unwakeable sleep — the ~50% SSH-after-restore flake (run-11 MMIO trace).
    ssh_retry(
        &g1,
        "sudo systemd-run --on-active=2 systemctl suspend -i >/dev/null 2>&1; echo armed",
    );
    g1.suspend_bracket().expect("sending the suspend bracket");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(120))
        .expect("supervisor did not exit after the suspend bracket");
    assert_eq!(
        outcome.code,
        Some(126),
        "the seated guest should suspend (worker exit 126); got {outcome:?}\n\
         === supervisor+worker log ===\n{}",
        g1.supervisor_log()
    );

    // Preserve the snapshot AND the disk the guest was running on — the restore must
    // resume against the exact filesystem state it suspended with, not a fresh clone
    // of the golden.
    let scratch = g1.scratch_dir().to_path_buf();
    let pid = std::process::id();
    let snap: PathBuf = std::env::temp_dir().join(format!("limina-venus-session-{pid}.snap"));
    let disk: PathBuf = std::env::temp_dir().join(format!("limina-venus-session-{pid}.raw"));
    std::fs::copy(g1.snapshot_path().expect("snapshot path configured"), &snap)
        .expect("preserving the snapshot file");
    limina_test::cow_clone(&scratch.join("disk.raw"), &disk)
        .expect("preserving the suspended guest's disk");
    drop(g1);

    // Auto-resume consumes the snapshot by renaming it (M9.4 single-use), so the file may
    // survive under either name.
    let snap_consumed = snap.with_extension("bin.consumed");
    let cleanup = || {
        // LIMINA_KEEP_ARTIFACTS=1 preserves the snapshot+disk pair on failure so a
        // failing restore can be re-run by hand (the pair is the whole repro).
        if std::env::var_os("LIMINA_KEEP_ARTIFACTS").is_some() {
            eprintln!(
                "keeping artifacts: snap={} (or {}) disk={}",
                snap.display(),
                snap_consumed.display(),
                disk.display()
            );
            return;
        }
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(&snap_consumed);
        let _ = std::fs::remove_file(&disk);
    };

    // --- Guest 2: fresh worker restoring the snapshot against the preserved disk ---
    let mut cfg2 = devices(base_cfg.clone())
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .restore_from(&snap);
    if let limina_test::Boot::KernelDisk { disk: d, .. } = &mut cfg2.boot {
        *d = disk.clone();
    }
    let mut g2 = Guest::boot(&cfg2).expect("spawning the restoring supervisor");
    g2.wait_for_supervisor_log("restoring from snapshot", Duration::from_secs(30))
        .unwrap_or_else(|e| {
            cleanup();
            panic!("restore worker never entered the restore path: {e}");
        });
    let banner = g2
        .wait_for_ssh_banner(Duration::from_secs(120))
        .unwrap_or_else(|e| {
            // Liveness forensics: the console tail shows whether the guest thaw
            // completed (vs a wedged resume), and a present frame proves the
            // compositor is alive even with the network down.
            let console = g2.console();
            let tail: Vec<&str> = console.lines().rev().take(30).collect();
            eprintln!("--- restored guest console tail ---");
            for line in tail.iter().rev() {
                eprintln!("{line}");
            }
            match g2.read_capture() {
                Ok(f) => eprintln!("display capture: {}x{} frame present", f.width, f.height),
                Err(e) => eprintln!("display capture: none ({e})"),
            }
            // The host-side net oracle: did the restored guest transmit ANYTHING?
            let gvlog =
                std::fs::read_to_string(g2.scratch_dir().join("gvproxy.log")).unwrap_or_default();
            eprintln!("--- gvproxy log: {} lines, tail ---", gvlog.lines().count());
            for line in gvlog.lines().rev().take(6).collect::<Vec<_>>().iter().rev() {
                eprintln!("{}", &line[..line.len().min(200)]);
            }
            eprintln!("--- restore supervisor log tail ---");
            let slog = g2.supervisor_log();
            for line in slog.lines().rev().take(20).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
            cleanup();
            panic!("restored guest never became reachable over SSH: {e}");
        });
    eprintln!("restored guest SSH up: {banner}");

    // Same kernel boot — a differing boot_id means the guest REBOOTED, which is a
    // transport regression (0072), not the session gap this test measures.
    let boot_id_after = ssh_retry(&g2, "cat /proc/sys/kernel/random/boot_id");
    assert_eq!(
        boot_id_after, boot_id,
        "boot_id changed across restore — the guest rebooted instead of resuming"
    );

    // Ride out the abort window: the pre-fix failure mode is a DELAYED crash (~17 s
    // in vn_relax), so a same-pid reading right after SSH-back proves nothing yet.
    eprintln!(
        "riding out the {}s abort window before the identity checks",
        ABORT_WINDOW.as_secs()
    );
    std::thread::sleep(ABORT_WINDOW);

    let shell_pid_after = ssh_retry(&g2, "pgrep -x gnome-shell | head -1");
    let cores_after = ssh_retry(
        &g2,
        "sudo coredumpctl list --no-legend gnome-shell 2>/dev/null | wc -l",
    );
    eprintln!("post-restore: gnome-shell pid={shell_pid_after} cores={cores_after}");
    // NOTE: no cleanup() here anymore — the snapshot/disk pair must survive into the
    // generation-2 leg below; the leg's failure paths and the final teardown clean up.

    assert_eq!(
        cores_after, cores_before,
        "gnome-shell dumped core across the restore — the venus session was lost \
         (mesa vn_relax dead-ring abort; the host GPU world was not replayed)"
    );
    assert_eq!(
        shell_pid_after, shell_pid,
        "gnome-shell is a DIFFERENT process after restore — the session restarted \
         instead of being preserved (the seamless-resume goal)"
    );

    // The multi-context oracle: the parked fd-hold client (freed-while-exported
    // cross-context blob) must be the same live process AND its heartbeats — real
    // host-touching allocs on its venus ring — must still be advancing. A partial
    // replay that lost only ITS context leaves gnome-shell intact (all the asserts
    // above pass) while this client's next ring submission wedges in vn_relax and
    // aborts: exactly what run 22 proved a pid check alone cannot see. (Don't grep
    // host logs for "replay FAILED" here — the worker's stderr does not land in
    // the supervisor-log file this reads; run 22 proved that oracle blind too.)
    let holder_after = ssh_retry(&g2, "pgrep -f 'python3 /tmp/vkfdhold.py' | head -1");
    assert_eq!(
        holder_after, holder_pid,
        "the fd-hold venus client did not survive the restore (vn_relax abort — \
         its re-created contexts were unusable)"
    );
    let beat_a = ssh_retry(&g2, "grep -c 'FDHOLD BEAT' /tmp/vkfdhold.out");
    std::thread::sleep(Duration::from_secs(12));
    let beat_b = ssh_retry(&g2, "grep -c 'FDHOLD BEAT' /tmp/vkfdhold.out");
    let (a, b): (u64, u64) = (beat_a.parse().unwrap_or(0), beat_b.parse().unwrap_or(0));
    if b <= a {
        let out = g2.ssh_exec("cat /tmp/vkfdhold.out").unwrap_or_default();
        // Where is it wedged? A stack of the live (not aborted) holder tells the
        // restore-bug class apart: vn_relax spin = dead ring; futex/poll = lost
        // reply; nanosleep = the beat loop is fine and something else is wrong.
        let stack = g2
            .ssh_exec(&format!("sudo eu-stack -p {holder_pid} 2>&1 | head -40"))
            .unwrap_or_default();
        panic!(
            "the fd-hold client's heartbeat stalled after restore ({a} -> {b}) — its \
             re-created venus contexts do not service submissions\n\
             --- vkfdhold.out ---\n{out}\n--- eu-stack ---\n{stack}"
        );
    }

    // The create-arg closure oracle: the pipeline client must be the SAME live
    // process and its beats — live wire commands referencing a pipeline whose
    // shader module + layout were destroyed pre-suspend — must still advance.
    // Pre-fix this is the vkmark death: the pipeline's create dropped at replay
    // (its module/layout creates were pruned), the first post-restore beat set a
    // sticky ring FATAL, and the client aborted in vn_ring_submit_locked.
    let pipe_after = ssh_retry(&g2, "pgrep -f '[v]kpipeline.py' | head -1");
    if pipe_after != pipe_pid {
        let out = g2.ssh_exec("cat /tmp/vkpipeline.out").unwrap_or_default();
        let cores = g2
            .ssh_exec("sudo coredumpctl list --no-legend python3 2>/dev/null | tail -3")
            .unwrap_or_default();
        panic!(
            "the pipeline venus client did not survive the restore (pid {pipe_pid} -> \
             {pipe_after:?}) — its re-created context lost the pipeline whose module/layout \
             were destroyed pre-suspend (journal create-arg closure)\n\
             --- vkpipeline.out ---\n{out}\n--- python3 cores ---\n{cores}"
        );
    }
    let pbeat_a = ssh_retry(&g2, "grep -c 'PIPE BEAT' /tmp/vkpipeline.out");
    std::thread::sleep(Duration::from_secs(12));
    let pbeat_b = ssh_retry(&g2, "grep -c 'PIPE BEAT' /tmp/vkpipeline.out");
    let (pa, pb): (u64, u64) = (pbeat_a.parse().unwrap_or(0), pbeat_b.parse().unwrap_or(0));
    if pb <= pa {
        let out = g2.ssh_exec("cat /tmp/vkpipeline.out").unwrap_or_default();
        let stack = g2
            .ssh_exec(&format!("sudo eu-stack -p {pipe_pid} 2>&1 | head -40"))
            .unwrap_or_default();
        panic!(
            "the pipeline client's heartbeat stalled after restore ({pa} -> {pb}) — its \
             re-created context does not service pipeline-referencing submissions\n\
             --- vkpipeline.out ---\n{out}\n--- eu-stack ---\n{stack}"
        );
    }

    // The content oracle: trigger the parked client's copy-back and require the
    // pattern it staged in a never-mapped (non-blob) VkDeviceMemory to have
    // survived the restore byte-for-byte. Without full content capture the
    // replayed allocation is a fresh host heap — the copy-back returns garbage
    // (CONTENT BAD) even though every liveness assert above passes.
    ssh_retry(&g2, "touch /tmp/vkcontent-go");
    g2.ssh_poll(
        "grep -Eq 'CONTENT (OK|BAD|FAIL)' /tmp/vkcontent.out",
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| {
        let out = g2.ssh_exec("cat /tmp/vkcontent.out").unwrap_or_default();
        panic!("the content client never delivered a verdict: {e}\n--- vkcontent.out ---\n{out}");
    });
    let verdict = ssh_retry(&g2, "grep -E 'CONTENT (OK|BAD|FAIL)' /tmp/vkcontent.out");
    assert_eq!(
        verdict, "CONTENT OK",
        "device-memory contents did not survive the restore — the never-mapped \
         VkDeviceMemory came back as a fresh (garbage) allocation: {verdict}"
    );

    // --- Generation 2: suspend the RESUMED session and restore it AGAIN ---
    // Dogfood does this daily (suspend at night, resume in the morning, repeat), and it
    // crashed 2/2 on 2026-07-20: the second restore's replay cascades stale-reference
    // failures and aborts the WORKER in a KK assert (kk_descriptor_set.c:74
    // sampled_gpu_resource_id). First restores are green across codec/build combos, so
    // the failure is generation-correlated — the GPU journal re-baselined after a first
    // replay must still describe a re-creatable world. The oracles repeat generation 1's:
    // the worker survives the replay, SSH returns, same boot_id, same shell pid past the
    // abort window, no new cores.
    eprintln!("generation 2: suspending the restored session");
    ssh_retry(
        &g2,
        "sudo systemd-run --on-active=2 systemctl suspend -i >/dev/null 2>&1; echo armed",
    );
    g2.suspend_bracket()
        .expect("sending the gen-2 suspend bracket");
    let outcome = g2
        .wait_supervisor_exit(Duration::from_secs(120))
        .expect("supervisor did not exit after the gen-2 suspend bracket");
    assert_eq!(
        outcome.code,
        Some(126),
        "the restored guest should suspend again (worker exit 126); got {outcome:?}\n\
         === supervisor+worker log ===\n{}",
        g2.supervisor_log()
    );
    // The gen-2 suspend wrote its snapshot at g2's armed path (restore_from arms
    // --snapshot-file at `snap`, and the suspend recreates the canonical name there).
    // The gen-2 DISK state lives in g2's scratch (the harness boots a clone of the
    // preserved disk, not the preserved file itself) — carry it out before g2 drops.
    let disk2: PathBuf = std::env::temp_dir().join(format!("limina-venus-session-{pid}-g2.raw"));
    limina_test::cow_clone(&g2.scratch_dir().join("disk.raw"), &disk2)
        .expect("preserving the gen-2 suspended guest's disk");
    drop(g2);
    let disk2_for_cleanup = disk2.clone();
    let cleanup2 = move || {
        cleanup();
        if std::env::var_os("LIMINA_KEEP_ARTIFACTS").is_none() {
            let _ = std::fs::remove_file(&disk2_for_cleanup);
        }
    };

    let mut cfg3 = devices(base_cfg)
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .restore_from(&snap);
    if let limina_test::Boot::KernelDisk { disk: d, .. } = &mut cfg3.boot {
        *d = disk2.clone();
    }
    let mut g3 = Guest::boot(&cfg3).expect("spawning the gen-2 restoring supervisor");
    let banner = g3
        .wait_for_ssh_banner(Duration::from_secs(120))
        .unwrap_or_else(|e| {
            eprintln!("--- gen-2 restore supervisor log tail ---");
            let slog = g3.supervisor_log();
            for line in slog.lines().rev().take(25).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
            cleanup2();
            panic!(
                "gen-2 restored guest never became reachable over SSH (the second-generation \
                 replay crashed or wedged the worker): {e}"
            );
        });
    eprintln!("gen-2 restored guest SSH up: {banner}");

    let boot_id_gen2 = ssh_retry(&g3, "cat /proc/sys/kernel/random/boot_id");
    assert_eq!(
        boot_id_gen2, boot_id,
        "boot_id changed across the SECOND restore — the guest rebooted instead of resuming"
    );
    eprintln!(
        "riding out the {}s abort window before the gen-2 identity checks",
        ABORT_WINDOW.as_secs()
    );
    std::thread::sleep(ABORT_WINDOW);
    let shell_pid_gen2 = ssh_retry(&g3, "pgrep -x gnome-shell | head -1");
    let cores_gen2 = ssh_retry(
        &g3,
        "sudo coredumpctl list --no-legend gnome-shell 2>/dev/null | wc -l",
    );
    eprintln!("post-gen-2-restore: gnome-shell pid={shell_pid_gen2} cores={cores_gen2}");
    cleanup2();
    assert_eq!(
        cores_gen2, cores_before,
        "gnome-shell dumped core across the SECOND restore — the re-baselined GPU journal \
         did not survive another replay"
    );
    assert_eq!(
        shell_pid_gen2, shell_pid,
        "gnome-shell is a DIFFERENT process after the second restore — the session was \
         lost on generation 2"
    );

    // The pipeline client rides through BOTH generations: gen 2's re-baselined
    // journal must still pin the (long-destroyed) module/layout creates for the
    // still-live pipeline.
    let pipe_gen2 = ssh_retry(&g3, "pgrep -f '[v]kpipeline.py' | head -1");
    if pipe_gen2 != pipe_pid {
        let out = g3.ssh_exec("cat /tmp/vkpipeline.out").unwrap_or_default();
        panic!(
            "the pipeline venus client did not survive the SECOND restore (pid {pipe_pid} \
             -> {pipe_gen2:?})\n--- vkpipeline.out ---\n{out}"
        );
    }
    let gbeat_a = ssh_retry(&g3, "grep -c 'PIPE BEAT' /tmp/vkpipeline.out");
    std::thread::sleep(Duration::from_secs(12));
    let gbeat_b = ssh_retry(&g3, "grep -c 'PIPE BEAT' /tmp/vkpipeline.out");
    let (ga, gb): (u64, u64) = (gbeat_a.parse().unwrap_or(0), gbeat_b.parse().unwrap_or(0));
    if gb <= ga {
        let out = g3.ssh_exec("cat /tmp/vkpipeline.out").unwrap_or_default();
        panic!(
            "the pipeline client's heartbeat stalled after the SECOND restore \
             ({ga} -> {gb})\n--- vkpipeline.out ---\n{out}"
        );
    }

    let outcome = g3
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the gen-2 restored guest");
    eprintln!("teardown outcome: {outcome:?}");
}
