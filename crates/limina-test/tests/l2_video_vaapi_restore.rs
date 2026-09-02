// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — a hardware decode in flight survives a suspend/restore.
//!
//! The regression this exists for: the classic re-creation journal filed the video codec
//! and video buffer creates under "durable-unknown", so a restored context had neither
//! while the guest kept its handles. Nothing told the guest: the per-frame commands report
//! success whatever the host finds, so the player kept "decoding" into nothing and showed
//! its surface pool of stale pictures back and forth (and never-written ones as green),
//! forever in GStreamer, until the decoder was torn down in Chrome. The GPU error counters
//! stayed at zero throughout, which is what once excluded the journal as the cause.
//!
//! # The workload
//!
//! `gst-launch-1.0` decoding a 12 s VP9 clip through `vavp9dec` into a raw I420 file, paced
//! to the clip's timestamps (`filesink sync=true`), so the suspend lands with the codec in
//! the middle of the stream and the same pipeline — the same VA context, the same codec
//! handle — carries on afterwards. A batch decode would finish before any suspend and each
//! `gst-launch` creates a fresh codec, so it cannot express the subject.
//!
//! The clip has a keyframe every second. A codec re-created at replay has no reference
//! pictures and drops inter frames until the next keyframe, so the pixels from the suspend
//! to that keyframe are legitimately stale; everything after it must be exact again.
//!
//! # The oracles
//!
//! 1. **The pipeline completes with every frame.** A stuck or torn-down codec leaves the
//!    output short; the byte count says how many pictures the guest was handed.
//! 2. **The tail is bit-exact against the software decoder.** VP9 is normatively exact, so
//!    the last two seconds of the hardware output must equal `avdec_vp9`'s. On the broken
//!    tree the pipeline still completes (the host reports success), but every post-restore
//!    frame is whatever the surface held before — this is the oracle that fails there.
//! 3. **The host never decoded into nothing.** The backend logs a rate-limited line when a
//!    per-frame command names a codec or buffer it does not have; after the restore there
//!    must be none.
//!
//! Vehicle: the stock F44 autologin baseline on the coexist GPU with the zink-on-KK host-GL
//! worker env, the same as `l2_video_vaapi`. SKIPs cleanly without LIMINA_HVF_TESTS, the
//! KosmicKrisp ICD, the zink-on-KK Mesa prefix, the GOP firmware, or the baseline disk.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const GUEST_VA_DRIVER: &str = "/usr/lib64/dri/virtio_gpu_drv_video.so";

const CLIP: &str = "/tmp/limina-vp9-restore.ivf";
const CLIP_WEBM: &str = "/tmp/limina-vp9-restore.webm";
const HW_OUT: &str = "/tmp/limina-vp9-restore-hw.i420";
const SW_OUT: &str = "/tmp/limina-vp9-restore-sw.i420";
const HW_LOG: &str = "/tmp/limina-vp9-restore-hw.log";

const WIDTH: u64 = 320;
const HEIGHT: u64 = 240;
const FRAME_BYTES: u64 = WIDTH * HEIGHT * 3 / 2;
const CLIP_SECONDS: u64 = 12;
const CLIP_FPS: u64 = 25;
/// One keyframe per second: bounds how long the re-created codec waits to re-seed.
const KEYFRAME_INTERVAL: u64 = 25;
const FRAMES: u64 = CLIP_SECONDS * CLIP_FPS;
/// The window compared bit-for-bit: the last two seconds, well past any keyframe that
/// follows a suspend landing anywhere in the first half of the clip.
const TAIL_FRAMES: u64 = 2 * CLIP_FPS;

/// How far into the decode the suspend is requested. Early enough that most of the clip
/// still has to decode on the restored codec.
const SUSPEND_AFTER: Duration = Duration::from_secs(3);

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

fn field(report: &str, name: &str) -> String {
    report
        .lines()
        .find_map(|l| l.trim().strip_prefix(&format!("{name}=")))
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[test]
fn hardware_decode_in_flight_survives_restore() {
    const NAME: &str = "hardware_decode_in_flight_survives_restore";

    if !limina_test::require_hvf_or_skip(NAME) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIPPED {NAME}: no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk");
        return;
    }
    if limina_test::zink_kk_mesa_prefix().is_none() {
        eprintln!("SKIPPED {NAME}: no zink-on-KK Mesa prefix (or set MESA_PREFIX)");
        return;
    }

    let base_cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {NAME}: {e}");
            return;
        }
    };
    let cfg = base_cfg
        .clone()
        .with_coexist_display(1280, 800)
        .with_virgl_host_gl()
        .with_net()
        .with_supervisor_log()
        .with_snapshot();
    eprintln!("booting stock F44 (coexist GPU, virgl/zink-on-KK host GL, NAT, snapshot armed)");

    let mut g1 = Guest::boot(&cfg).expect("spawning the limina supervisor");
    g1.wait_for_supervisor_log("software_2d = false", Duration::from_secs(60))
        .expect("coexist GPU did not come up (degraded to software-2D?)");
    let banner = g1
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    let have_driver = ssh_retry(
        &g1,
        &format!("test -e {GUEST_VA_DRIVER} && echo yes || echo no"),
    );
    if have_driver != "yes" {
        eprintln!("SKIPPED {NAME}: guest has no {GUEST_VA_DRIVER}");
        return;
    }
    let va_elements = ssh_retry(&g1, "gst-inspect-1.0 va 2>&1 || true");
    assert!(
        va_elements.contains("vavp9dec"),
        "GStreamer's va plugin offers no vavp9dec — the host is filling no video caps.\n\
         {va_elements}"
    );

    // The clip: long enough to straddle the suspend, keyframe every second.
    g1.ssh_exec_timeout(
        &format!(
            "ffmpeg -hide_banner -loglevel error -f lavfi \
             -i testsrc2=size={WIDTH}x{HEIGHT}:rate={CLIP_FPS}:duration={CLIP_SECONDS} \
             -c:v libvpx-vp9 -b:v 300k -g {KEYFRAME_INTERVAL} -keyint_min {KEYFRAME_INTERVAL} \
             -f ivf {CLIP} -y && \
             ffmpeg -hide_banner -loglevel error -i {CLIP} -c:v copy -y {CLIP_WEBM} && \
             stat -c %s {CLIP_WEBM}"
        ),
        Duration::from_secs(240),
    )
    .expect("the guest could not encode a VP9 clip (no libvpx-vp9 encoder?)");

    let boot_id = ssh_retry(&g1, "cat /proc/sys/kernel/random/boot_id");

    // The decode under test, paced to the clip so it is still running at the suspend. It
    // must outlive the ssh session and the suspend: systemd-run detaches it from both.
    ssh_retry(
        &g1,
        &format!(
            "export XDG_RUNTIME_DIR=/run/user/1000 \
             DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; \
             rm -f {HW_OUT} {HW_LOG}; systemd-run --user --unit limina-hwdec --collect \
             -p StandardOutput=file:{HW_LOG} -p StandardError=file:{HW_LOG} \
             gst-launch-1.0 -q filesrc location={CLIP_WEBM} ! matroskademux ! vp9parse \
             ! vavp9dec ! videoconvert ! video/x-raw,format=I420 \
             ! filesink location={HW_OUT} sync=true >/dev/null 2>&1; echo started"
        ),
    );
    std::thread::sleep(SUSPEND_AFTER);
    let mid_bytes: u64 = ssh_retry(&g1, &format!("stat -c %s {HW_OUT} 2>/dev/null || echo 0"))
        .parse()
        .unwrap_or(0);
    assert!(
        (FRAME_BYTES..FRAMES * FRAME_BYTES).contains(&mid_bytes),
        "the paced decode should be mid-stream at the suspend, but its output holds {mid_bytes} \
         bytes ({} frames of {FRAMES})",
        mid_bytes / FRAME_BYTES
    );
    eprintln!(
        "suspending with {} of {FRAMES} frames decoded",
        mid_bytes / FRAME_BYTES
    );

    // --- Suspend through the production bracket: the button pulse alone. An in-guest
    // `systemctl suspend` timer on top of it (as some restore tests arm) fires a second time
    // after the resume here, and the restored guest goes straight back to sleep with the
    // decode still running. ---
    g1.suspend_bracket().expect("sending the suspend bracket");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(120))
        .expect("supervisor did not exit after the suspend bracket");
    assert_eq!(
        outcome.code,
        Some(126),
        "the guest should suspend (worker exit 126); got {outcome:?}\n\
         === supervisor+worker log ===\n{}",
        g1.supervisor_log()
    );

    let pid = std::process::id();
    let scratch = g1.scratch_dir().to_path_buf();
    let snap: PathBuf = std::env::temp_dir().join(format!("limina-vaapi-restore-{pid}.snap"));
    let disk: PathBuf = std::env::temp_dir().join(format!("limina-vaapi-restore-{pid}.raw"));
    std::fs::copy(g1.snapshot_path().expect("snapshot path configured"), &snap)
        .expect("preserving the snapshot file");
    limina_test::cow_clone(&scratch.join("disk.raw"), &disk)
        .expect("preserving the suspended guest's disk");
    drop(g1);

    let snap_consumed = snap.with_extension("bin.consumed");
    let cleanup = || {
        if std::env::var_os("LIMINA_KEEP_ARTIFACTS").is_some() {
            eprintln!(
                "keeping artifacts: snap={} disk={}",
                snap.display(),
                disk.display()
            );
            return;
        }
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(&snap_consumed);
        let _ = std::fs::remove_file(&disk);
    };

    // --- Restore into a fresh worker against the preserved disk ---
    let mut cfg2 = base_cfg
        .with_coexist_display(1280, 800)
        .with_virgl_host_gl()
        .with_net()
        .with_supervisor_log()
        .restore_from(&snap);
    if let limina_test::Boot::Firmware { disk: d, .. } = &mut cfg2.boot {
        *d = disk.clone();
    }
    let mut g2 = Guest::boot(&cfg2).expect("spawning the restoring supervisor");
    g2.wait_for_supervisor_log("restoring from snapshot", Duration::from_secs(30))
        .unwrap_or_else(|e| {
            cleanup();
            panic!("restore worker never entered the restore path: {e}");
        });
    g2.wait_for_ssh_banner(Duration::from_secs(120))
        .unwrap_or_else(|e| {
            eprintln!("--- restore supervisor log tail ---");
            let slog = g2.supervisor_log();
            for line in slog.lines().rev().take(25).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
            cleanup();
            panic!("restored guest never became reachable over SSH: {e}");
        });
    if ssh_retry(&g2, "cat /proc/sys/kernel/random/boot_id") != boot_id {
        cleanup();
        panic!("boot_id changed across restore — the guest rebooted instead of resuming");
    }

    // The same pipeline, same codec handle, finishing on the restored host. The guest's
    // network comes back a little after its sshd banner does (the restored NetworkManager
    // re-runs its address setup), so a refused connection here is a retry, not a verdict.
    let deadline = Instant::now() + Duration::from_secs(CLIP_SECONDS + 90);
    let unit_state = "export XDG_RUNTIME_DIR=/run/user/1000 \
                      DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; \
                      systemctl --user is-active limina-hwdec 2>/dev/null || echo gone";
    loop {
        match g2.ssh_exec(unit_state) {
            Ok(out) => {
                let state = out.trim();
                if state != "active" && state != "activating" {
                    eprintln!("decode unit finished: {state}");
                    break;
                }
            }
            Err(e) => eprintln!("ssh to the restored guest not ready yet: {e}"),
        }
        if Instant::now() > deadline {
            let log = g2
                .ssh_exec(&format!("cat {HW_LOG} 2>/dev/null"))
                .unwrap_or_default();
            eprintln!("--- restore supervisor log tail ---");
            let slog = g2.supervisor_log();
            for line in slog.lines().rev().take(40).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
            cleanup();
            panic!(
                "the hardware decode never finished after the restore\n\
                 --- gst-launch log ---\n{log}"
            );
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    // Software reference for the whole clip, and the comparison of the tails.
    let tail_bytes = TAIL_FRAMES * FRAME_BYTES;
    let report = g2
        .ssh_exec_timeout(
            &format!(
                "gst-launch-1.0 -q filesrc location={CLIP_WEBM} ! matroskademux ! vp9parse \
                 ! avdec_vp9 ! videoconvert ! video/x-raw,format=I420 \
                 ! filesink location={SW_OUT} 2>&1 >/dev/null | tail -2; \
                 echo \"hw_bytes=$(stat -c %s {HW_OUT})\"; \
                 echo \"sw_bytes=$(stat -c %s {SW_OUT})\"; \
                 echo \"hw_tail_md5=$(tail -c {tail_bytes} {HW_OUT} | md5sum | cut -d' ' -f1)\"; \
                 echo \"sw_tail_md5=$(tail -c {tail_bytes} {SW_OUT} | md5sum | cut -d' ' -f1)\"; \
                 echo \"hw_tail_distinct_luma=$(tail -c {tail_bytes} {HW_OUT} \
                     | head -c {} | od -An -tu1 -v | tr ' ' '\\n' | grep -v '^$' \
                     | sort -u | wc -l)\"; \
                 echo \"gst_log=$(tr '\\n' ' ' < {HW_LOG})\"",
                WIDTH * HEIGHT
            ),
            Duration::from_secs(240),
        )
        .unwrap_or_else(|e| {
            cleanup();
            panic!("the reference decode failed to run: {e}");
        });
    eprintln!("{report}");
    let slog = g2.supervisor_log();
    cleanup();

    // ORACLE 1 — every frame was handed to the guest.
    let hw_bytes: u64 = field(&report, "hw_bytes").parse().unwrap_or(0);
    let sw_bytes: u64 = field(&report, "sw_bytes").parse().unwrap_or(0);
    let expected = FRAMES * FRAME_BYTES;
    assert_eq!(
        sw_bytes, expected,
        "the software reference produced {sw_bytes} bytes, expected {expected} — the clip \
         itself is wrong.\n{report}"
    );
    assert_eq!(
        hw_bytes,
        expected,
        "the VA-API pipeline produced {hw_bytes} bytes ({} frames of {FRAMES}) across the \
         restore — the decode stalled or lost pictures.\n{report}",
        hw_bytes / FRAME_BYTES
    );

    // ORACLE 3 — the host never decoded into a codec it did not have. Checked before the
    // pixels so a stale-picture failure comes with its cause.
    let misses: Vec<&str> = slog
        .lines()
        .filter(|l| l.contains("decoding into nothing"))
        .collect();
    assert!(
        misses.is_empty(),
        "after the restore the host received {} video command(s) naming a codec or buffer it \
         does not have — the codec was not re-created at replay:\n{}",
        misses.len(),
        misses.join("\n")
    );
    for line in slog
        .lines()
        .filter(|l| l.contains("dropping inter frames") || l.contains("re-seeded by a keyframe"))
    {
        eprintln!("resync: {line}");
    }

    // ORACLE 2 — the tail is exact. A codec that came back but never re-seeded, or that
    // decoded against an empty reference set, fails here while oracle 1 passes.
    let distinct: u32 = field(&report, "hw_tail_distinct_luma").parse().unwrap_or(0);
    assert!(
        distinct > 16,
        "the hardware output's tail has only {distinct} distinct luma values — a cleared or \
         never-written surface, not a decoded picture.\n{report}"
    );
    assert_eq!(
        field(&report, "hw_tail_md5"),
        field(&report, "sw_tail_md5"),
        "the last {TAIL_FRAMES} frames decoded after the restore differ from the software \
         decoder's; VP9 is normatively exact, so the restored codec is producing stale or \
         wrong pictures.\n{report}"
    );

    eprintln!(
        "hardware decode carried across the restore: {FRAMES} frames, last {TAIL_FRAMES} \
         bit-exact against the software decoder"
    );
}
