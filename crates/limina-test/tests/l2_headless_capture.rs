// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2: frame capture on a run with no window (`--headless-capture`).
//!
//! A harness driving a guest compositor, and most L2 tests, boot headless, so they need to see
//! what the guest scans out without opening a window. With the gate on, the worker hands its
//! frames to the supervisor exactly as a windowed worker does, and `limina debug <vm> capture
//! still|start|stop` work on them; with it off (the default) the supervisor never sees a frame, and
//! every capture request is refused with how to turn it on.
//!
//! Oracle for the gated run, a stock guest's GNOME session at rest and then under `vkcube`:
//! - a `capture still` of the desktop is rich (real pixels, not a black or single-colour screen),
//!   says it is `headless`, has no `layer_iosurface`, is a guest flip naming the virtio-gpu
//!   resource the guest's own KMS state says it is scanning out (`guest/scanoutres.py`);
//! - a short `capture start`/`stop` around `vkcube` writes well-formed `format` 1 records, every one
//!   `headless` with no `layer_iosurface`, every guest flip naming its resource (and every resource
//!   the guest scanned out during the capture among them), `seq` increasing, every image present,
//!   decodable, of its record's size and not blank, and a summary that agrees and says `headless`;
//! - a still of a display the guest does not drive is refused;
//! - after `limina reset`, a still never shows the dead worker's frame: it is refused until the
//!   fresh guest flips, and then names the new epoch (the first worker's is 0, as a window's).
//!
//! Oracle for the ungated runs: `capture start` and `capture still` are refused, naming the gate,
//! on a run with a `--display-capture` display and on one with no display at all. Those boots are
//! never waited on: the debug socket is up within a second of the supervisor starting.
//!
//! Stock disk + our GOP firmware + a coexist display and no window, so not in the EXCLUSIVE set.
//! SKIPs cleanly without the KosmicKrisp ICD or the image. Gated behind LIMINA_HVF_TESTS.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use limina_framecap::{Line, Record, SIDECAR};
use limina_test::{Guest, GuestConfig};

const SCANOUTRES: &str = include_str!("../guest/scanoutres.py");

const DISPLAY: (u32, u32) = (1280, 800);

/// How long the sequence capture runs.
const WINDOW: Duration = Duration::from_secs(4);

/// Frames that must be captured in [`WINDOW`] of a spinning `vkcube`.
const MIN_CAPTURED: u64 = 60;

fn limina_debug(pid: libc::pid_t, args: &[&str]) -> (bool, String) {
    let mut cmd = std::process::Command::new(limina_test::limina_bin().expect("the limina binary"));
    cmd.arg("debug").arg(pid.to_string()).args(args);
    let out = cmd.output().expect("running limina debug");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// `limina debug` once the supervisor's debug socket answers at all.
fn limina_debug_when_up(pid: libc::pid_t, args: &[&str]) -> (bool, String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (ok, out) = limina_debug(pid, args);
        if ok || !out.contains("connecting to") || Instant::now() >= deadline {
            return (ok, out);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn decode(path: &Path) -> (u32, u32, usize) {
    let frame = limina_test::CapturedFrame::from_png(path)
        .unwrap_or_else(|e| panic!("decoding {path:?}: {e:#}"));
    (frame.width, frame.height, frame.distinct_colors())
}

/// Which virtio-gpu resource each display is scanning out, read in the guest.
fn scanout_resources(guest: &Guest, samples: u32, gap_ms: u32) -> Vec<(usize, u32)> {
    let out = guest
        .ssh_exec(&format!(
            "sudo -n python3 /tmp/scanoutres.py {samples} {gap_ms}"
        ))
        .expect("running scanoutres.py in the guest");
    let found: Vec<(usize, u32)> = out
        .lines()
        .filter_map(|l| {
            let l = l.strip_prefix("SCANOUTRES ")?;
            let field = |k: &str| {
                l.split_whitespace()
                    .find_map(|f| f.strip_prefix(k)?.strip_prefix('='))
            };
            Some((
                field("crtc_index")?.parse().ok()?,
                field("res")?.parse().ok()?,
            ))
        })
        .collect();
    assert!(
        !found.is_empty(),
        "the guest reported no scanout resource:\n{out}"
    );
    found
}

/// The refusal names the launch flag and its variable, and says it is a launch-time choice.
fn assert_refused_with_the_gate(what: &str, ok: bool, out: &str) {
    assert!(
        !ok,
        "{what} was not refused on an ungated headless run:\n{out}"
    );
    assert!(
        out.contains("--headless-capture") && out.contains("LIMINA_HEADLESS_CAPTURE=1"),
        "{what} was refused without saying how to turn capture on:\n{out}"
    );
    assert!(
        out.contains("cannot be turned on at runtime"),
        "{what}'s refusal does not say the gate is a launch-time choice:\n{out}"
    );
}

#[test]
fn an_ungated_headless_run_refuses_capture_and_says_how_to_enable_it() {
    let name = "an_ungated_headless_run_refuses_capture_and_says_how_to_enable_it";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIPPED {name}: no KosmicKrisp ICD — the coexist display is unavailable");
        return;
    }
    let base = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };
    for (label, cfg) in [
        (
            "a --display-capture run",
            base.clone().with_coexist_display(DISPLAY.0, DISPLAY.1),
        ),
        ("a run with no display", base),
    ] {
        let guest = Guest::boot(&cfg).unwrap_or_else(|e| panic!("booting {label}: {e:#}"));
        let pid = guest.supervisor_pid();
        let dir = guest.scratch_dir().join("frames");
        let (ok, out) = limina_debug_when_up(pid, &["capture", "start", &dir.to_string_lossy()]);
        eprintln!("{label}: capture start answered:\n{out}");
        assert_refused_with_the_gate(&format!("{label}: capture start"), ok, &out);
        assert!(
            !dir.join(SIDECAR).exists(),
            "{label}: a refused capture start still wrote a sidecar"
        );
        let png = guest.scratch_dir().join("still.png");
        let (ok, out) = limina_debug(pid, &["capture", "still", &png.to_string_lossy()]);
        eprintln!("{label}: capture still answered:\n{out}");
        assert_refused_with_the_gate(&format!("{label}: capture still"), ok, &out);
        let outcome = guest
            .force_shutdown(Duration::from_secs(20))
            .expect("stopping the ungated guest");
        eprintln!("{label}: teardown outcome: {outcome:?}");
    }
}

#[test]
fn a_headless_capture_run_takes_stills_and_sequences_of_real_frames() {
    let name = "a_headless_capture_run_takes_stills_and_sequences_of_real_frames";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIPPED {name}: no KosmicKrisp ICD — the coexist display is unavailable");
        return;
    }
    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg
            .with_headless_capture_display(DISPLAY.0, DISPLAY.1)
            .with_net()
            .with_supervisor_log()
            .with_env("RUST_LOG", "warn,limina::window::frame_capture=info")
            .with_env("LIMINA_WINDOW_CAPTURE_DIR_MAX_MB", "512"),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };
    let mut guest = Guest::boot(&cfg).expect("booting the stock guest headless");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd came up");
    guest
        .ssh_poll(
            "sudo journalctl -b _COMM=gnome-shell --no-pager 2>/dev/null \
             | grep -q 'GNOME Shell started'",
            Duration::from_secs(240),
        )
        .expect("gnome-shell never started on the stock guest");
    guest
        .ssh_exec(&format!(
            "cat > /tmp/scanoutres.py <<'SCANOUTRES_PY_EOF'\n{SCANOUTRES}\nSCANOUTRES_PY_EOF"
        ))
        .expect("writing scanoutres.py to the guest");
    let pid = guest.supervisor_pid();

    // The desktop at rest, as a still: rich, headless, a guest flip naming the guest's buffer.
    let stills = guest.stills();
    let (still, frame) = stills
        .wait_for(Duration::from_secs(120), |_, f| {
            let (colors, dominance) = f.richness();
            colors >= 1000 && dominance < 0.90
        })
        .expect("no rich still of the headless desktop");
    let (colors, dominance) = frame.richness();
    eprintln!("still: {still:?}; {colors} colours, dominant {dominance:.2}");
    assert_eq!(still.format, limina_framecap::FORMAT);
    assert!(
        still.headless,
        "a headless still does not say so: {still:?}"
    );
    assert_eq!(still.layer_iosurface, None, "{still:?}");
    assert_eq!((still.width, still.height), DISPLAY, "{still:?}");
    assert_eq!(
        still.epoch, 0,
        "a headless run's first worker is not epoch 0, as a windowed run's is: {still:?}"
    );
    assert_eq!((frame.width, frame.height), DISPLAY);
    let before = scanout_resources(&guest, 1, 0);
    let (still, _) = stills
        .take(None)
        .expect("a second still of the idle desktop");
    let after = scanout_resources(&guest, 1, 0);
    assert!(
        still.guest_flip,
        "the idle still is no guest flip: {still:?}"
    );
    let around: BTreeSet<u32> = before
        .iter()
        .chain(&after)
        .filter(|(slot, _)| *slot == still.slot)
        .map(|(_, res)| *res)
        .collect();
    assert!(
        still.guest_resource.is_some_and(|r| around.contains(&r)),
        "the still names guest resource {:?}, but the guest was scanning out {around:?}: {still:?}",
        still.guest_resource
    );
    let (ok, said) = limina_debug(
        pid,
        &[
            "capture",
            "still",
            &guest.scratch_dir().join("x.png").to_string_lossy(),
            "7",
        ],
    );
    assert!(
        !ok && said.contains("display 7"),
        "a still of a display the guest does not drive was not refused clearly:\n{said}"
    );

    // Something that flips every frame, and a sequence capture around it.
    let session = "env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0";
    guest
        .ssh_exec(&format!(
            "{session} nohup vkcube >/tmp/vkcube.log 2>&1 & sleep 1; echo started"
        ))
        .expect("could not launch vkcube in the guest session");
    std::thread::sleep(Duration::from_secs(5));
    let alive = guest.ssh_exec("pgrep -x vkcube >/dev/null && echo ALIVE || cat /tmp/vkcube.log");
    assert!(
        alive.as_deref().is_ok_and(|o| o.contains("ALIVE")),
        "vkcube is not running, so nothing presents continuously: {alive:?}"
    );
    let dir = guest.scratch_dir().join("frames");
    let (ok, out) = limina_debug(pid, &["capture", "start", &dir.to_string_lossy()]);
    assert!(
        ok,
        "capture start was refused on a gated headless run:\n{out}"
    );
    let started = Instant::now();
    let sampled = scanout_resources(&guest, 20, 150);
    std::thread::sleep(WINDOW.saturating_sub(started.elapsed()));
    let (ok, stopped) = limina_debug(pid, &["capture", "stop"]);
    assert!(ok, "capture stop was refused:\n{stopped}");
    eprintln!("capture stop answered:\n{stopped}");
    let _ = guest.ssh_exec("pkill -x vkcube; true");

    let sidecar = std::fs::read_to_string(dir.join(SIDECAR))
        .unwrap_or_else(|e| panic!("reading {SIDECAR} in {dir:?}: {e}"));
    let lines: Vec<Line> = sidecar
        .lines()
        .map(|l| limina_framecap::parse_line(l).unwrap_or_else(|e| panic!("{e}")))
        .collect();
    let Some(Line::Summary(summary)) = lines.last().cloned() else {
        panic!("{SIDECAR} does not end in a summary line:\n{sidecar}");
    };
    eprintln!("{summary:?}");
    assert!(summary.headless, "the summary does not say headless");
    let records: Vec<Record> = lines[..lines.len() - 1]
        .iter()
        .map(|l| match l {
            Line::Frame(r) => r.clone(),
            other => panic!("a headless capture wrote a line that is no frame: {other:?}"),
        })
        .collect();
    let mut last_seq = None;
    for r in &records {
        assert_eq!(r.format, limina_framecap::FORMAT, "{r:?}");
        assert!(r.headless, "a headless record does not say so: {r:?}");
        assert_eq!(r.layer_iosurface, None, "{r:?}");
        assert_eq!(r.guest_flip, r.cause.is_none(), "{r:?}");
        assert_eq!(r.dropped, r.reason.is_some(), "{r:?}");
        if let Some(seq) = r.seq {
            assert!(
                last_seq.is_none_or(|p| seq > p),
                "seq went {last_seq:?} -> {seq}"
            );
            last_seq = Some(seq);
            assert!(
                r.presented_iosurface.is_some()
                    && r.t_monotonic_raw_ns.is_some()
                    && r.t_realtime_ns.is_some()
                    && (r.width, r.height) == (Some(DISPLAY.0), Some(DISPLAY.1)),
                "a received frame missing its tags: {r:?}"
            );
            if r.guest_flip {
                assert!(
                    r.guest_resource.is_some(),
                    "a guest flip that does not name its resource: {r:?}"
                );
            }
        }
    }
    let on_glass: BTreeSet<(usize, u32)> = records
        .iter()
        .filter_map(|r| Some((r.slot, r.guest_resource?)))
        .collect();
    let sampled: BTreeSet<(usize, u32)> = sampled.into_iter().collect();
    eprintln!("guest scanout resources sampled: {sampled:?}; captured: {on_glass:?}");
    assert!(
        sampled.is_subset(&on_glass),
        "the guest scanned out {:?}, which no captured frame names",
        sampled.difference(&on_glass).collect::<Vec<_>>()
    );
    let captured: Vec<&Record> = records.iter().filter(|r| r.file.is_some()).collect();
    for r in &captured {
        let file = r.file.as_deref().unwrap();
        let (w, h, colours) = decode(&dir.join(file));
        assert_eq!((Some(w), Some(h)), (r.width, r.height), "{file}");
        assert!(colours > 16, "{file} is blank ({colours} colours)");
    }
    let presented = records.iter().filter(|r| r.seq.is_some()).count() as u64;
    assert_eq!(
        (summary.presented, summary.captured),
        (presented, captured.len() as u64),
        "the summary disagrees with the records"
    );
    assert_eq!(
        summary.incomplete, None,
        "the stop did not drain: {summary:?}"
    );
    assert!(
        summary.captured >= MIN_CAPTURED,
        "only {} frames captured in {WINDOW:?} of a spinning vkcube (need {MIN_CAPTURED}): \
         {summary:?}",
        summary.captured
    );
    assert!(
        2 * (summary.dropped + summary.not_presented) < summary.presented + summary.not_presented,
        "most received flips have no image: {summary:?}"
    );
    eprintln!(
        "headless capture at {}x{}: {:.1} captured fps, {} dropped {:?}, hook p50 {} us p99 {} us \
         max {} us",
        DISPLAY.0,
        DISPLAY.1,
        summary.captured_fps,
        summary.dropped,
        summary.by_reason,
        summary.present_hook_us_p50,
        summary.present_hook_us_p99,
        summary.present_hook_us_max
    );

    // A fresh worker (`limina reset`): its frames, never the dead worker's. A still before the
    // new guest's first flip is refused; one after it names the new epoch.
    let old_epoch = still.epoch;
    let (ok, out) = limina_debug(pid, &["lever", "lifecycle-control", "on"]);
    assert!(ok, "turning lifecycle-control on: {out}");
    let reset = std::process::Command::new(limina_test::limina_bin().expect("the limina binary"))
        .args(["reset", &pid.to_string()])
        .output()
        .expect("running limina reset");
    assert!(
        reset.status.success(),
        "limina reset failed: {}",
        String::from_utf8_lossy(&reset.stderr)
    );
    match stills.take(None) {
        Err(e) => eprintln!("still right after the reset, refused: {e:#}"),
        Ok((s, _)) => assert!(
            s.epoch > old_epoch,
            "a still right after the reset shows the dead worker's frame: {s:?}"
        ),
    }
    let (after, _) = stills
        .wait_for(Duration::from_secs(240), |s, f| {
            let (colors, dominance) = f.richness();
            s.epoch > old_epoch && colors >= 1000 && dominance < 0.90
        })
        .expect("no rich still of the reset guest's desktop");
    eprintln!("still after the reset: {after:?}");
    assert!(after.headless && after.guest_flip, "{after:?}");

    let outcome = guest
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
