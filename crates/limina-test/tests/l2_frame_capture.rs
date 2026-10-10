// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2: the frame-sequence capture records every frame the window presents, tagged, in order.
//!
//! A harness iterating on a compositor needs every frame the guest actually got on glass,
//! attributed to its flip, without a human looking at the window. The capture is started and
//! stopped at runtime over the debug plane (`limina debug <vm> capture start <dir>` / `stop`),
//! which is how a harness captures just the window of interest, around a guest animation: `vkcube`
//! under the stock image's GNOME session, which presents continuously.
//!
//! Oracle, all from what the capture wrote: at least [`MIN_CAPTURED`] frames captured in the
//! [`WINDOW`] (vkcube flips at 60 Hz, so ~240 are on offer; the floor leaves room for a loaded
//! shared host) with frames lost to the capture or never presented a minority of the guest's
//! flips; `seq` strictly increasing and `flip` never decreasing within the slot, in file order;
//! every image file named by exactly one record and every record's file present, decodable, of the
//! size it says and not blank; the summary line agreeing with the records; the stop's answer
//! carrying that summary. Every line carries `format` 1, every record's `guest_flip` agrees with its
//! `cause`, and no guest flip is on glass twice unmarked. Then, with vkcube gone and no capture
//! running, `capture still` writes the idle screen: tagged (format, the last epoch, a flip no older
//! than the capture's last, both clocks), of the size its tag says, not blank — and a still of a
//! display no window shows is refused.
//!
//! The vehicle is part of the image (`vulkan-tools` is in the stock test image, docs/images.md), so
//! a missing or dead `vkcube` FAILS the test rather than skipping it: a capture of a still desktop
//! would pass the frame checks while proving nothing about rate. Images are capped at
//! [`MAX_MB`] so a run stays small (~0.45 MB a frame at this size; the cap is not reached).
//!
//! Stock disk + our GOP firmware + a coexist display in a real window, so it is in the EXCLUSIVE
//! set in `.config/nextest.toml`. SKIPs cleanly without the KosmicKrisp ICD or the image. Gated
//! behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use limina_framecap::{Line, Record, SIDECAR, Still, Summary};
use limina_test::{Guest, GuestConfig};

const DISPLAY: (u32, u32) = (1280, 800);

/// How long the capture runs.
const WINDOW: Duration = Duration::from_secs(4);

/// Frames that must be captured in [`WINDOW`]: a quarter of what 60 Hz offers.
const MIN_CAPTURED: u64 = 60;

/// How long the desktop is left alone before the still: long enough that the frame on glass is
/// an idle screen's, not the animation's last.
const IDLE: Duration = Duration::from_secs(3);

/// `LIMINA_WINDOW_CAPTURE_DIR_MAX_MB` for the run.
const MAX_MB: &str = "512";

fn limina_debug(guest: &Guest, args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new(limina_test::limina_bin().expect("the limina binary"))
        .arg("debug")
        .arg(guest.supervisor_pid().to_string())
        .args(args)
        .output()
        .expect("running limina debug");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// Decode one captured image: its size and how many distinct colours it holds.
fn decode(path: &Path) -> (u32, u32, usize) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("opening {path:?}: {e}"));
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("decoding {path:?}: {e}"));
    assert_eq!(info.color_type, png::ColorType::Rgb, "{path:?}");
    let colours: BTreeSet<[u8; 3]> = buf[..info.buffer_size()]
        .as_chunks::<3>()
        .0
        .iter()
        .step_by(97)
        .copied()
        .collect();
    (info.width, info.height, colours.len())
}

#[test]
fn a_runtime_capture_records_every_presented_frame_tagged() {
    let name = "a_runtime_capture_records_every_presented_frame_tagged";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIPPED {name}: no KosmicKrisp ICD — the coexist display is unavailable");
        return;
    }
    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg
            .with_windowed_coexist_display(DISPLAY.0, DISPLAY.1)
            .with_net()
            .with_supervisor_log()
            .with_env("RUST_LOG", "warn,limina::window::frame_capture=info")
            .with_env("LIMINA_WINDOW_CAPTURE_DIR_MAX_MB", MAX_MB),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };
    let mut guest = Guest::boot(&cfg).expect("booting the stock guest windowed");
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

    // Something that presents every frame for as long as the capture runs.
    let session = "env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0";
    guest
        .ssh_exec(&format!(
            "{session} sh -c 'command -v vkcube' >/dev/null || {{ echo NOVKCUBE; exit 1; }}"
        ))
        .expect("vkcube is not installed on the stock image — the vehicle is missing");
    guest
        .ssh_exec(&format!(
            "{session} nohup vkcube >/tmp/vkcube.log 2>&1 & sleep 1; echo started"
        ))
        .expect("could not launch vkcube in the guest session");
    // Let it map and start spinning, and make sure it is still there to be captured.
    std::thread::sleep(Duration::from_secs(5));
    let alive = guest.ssh_exec("pgrep -x vkcube >/dev/null && echo ALIVE || cat /tmp/vkcube.log");
    assert!(
        alive.as_deref().is_ok_and(|o| o.contains("ALIVE")),
        "vkcube is not running, so nothing presents continuously: {alive:?}"
    );

    let dir = guest.scratch_dir().join("frames");
    let dir_s = dir.to_string_lossy().into_owned();
    let (ok, out) = limina_debug(&guest, &["capture", "start", &dir_s]);
    assert!(ok, "capture start was refused:\n{out}");
    std::thread::sleep(WINDOW);
    let (ok, stopped) = limina_debug(&guest, &["capture", "stop"]);
    assert!(ok, "capture stop was refused:\n{stopped}");
    eprintln!("capture stop answered:\n{stopped}");

    let sidecar = std::fs::read_to_string(dir.join(SIDECAR))
        .unwrap_or_else(|e| panic!("reading {SIDECAR} in {dir:?}: {e}"));
    let lines: Vec<Line> = sidecar
        .lines()
        .map(|l| limina_framecap::parse_line(l).unwrap_or_else(|e| panic!("{e}")))
        .collect();
    let Some(Line::Summary(summary)) = lines.last().cloned() else {
        panic!("{SIDECAR} does not end in a summary line:\n{sidecar}");
    };
    let records: Vec<Record> = lines[..lines.len() - 1]
        .iter()
        .map(|l| match l {
            Line::Frame(r) => r.clone(),
            Line::Summary(_) => panic!("a summary line before the end of {SIDECAR}"),
        })
        .collect();
    eprintln!("{summary:?}");

    // In file order, per slot: seq strictly increasing over presented frames, flip never
    // decreasing over everything within one worker, and a flip number repeated on glass only by
    // a record that says it is no new guest flip.
    let mut last_seq = std::collections::HashMap::new();
    let mut last_flip = std::collections::HashMap::new();
    let mut last_guest_flip = std::collections::HashMap::new();
    for r in &records {
        if r.seq.is_some() && r.guest_flip {
            let prev = last_guest_flip.insert(r.slot, (r.epoch, r.flip));
            assert_ne!(
                prev,
                Some((r.epoch, r.flip)),
                "a guest flip on glass twice, unmarked: {r:?}"
            );
        }
        if let Some(seq) = r.seq {
            let prev = last_seq.insert(r.slot, seq);
            assert!(
                prev.is_none_or(|p| seq > p),
                "seq went {prev:?} -> {seq} on slot {}",
                r.slot
            );
        }
        let prev = last_flip.insert((r.slot, r.epoch), r.flip);
        assert!(
            prev.is_none_or(|p| r.flip >= p),
            "flip went {prev:?} -> {} on slot {}",
            r.flip,
            r.slot
        );
        assert_eq!(
            r.dropped,
            r.reason.is_some(),
            "a record dropped without a reason, or with one and not dropped: {r:?}"
        );
        assert_eq!(r.file.is_some(), !r.dropped, "{r:?}");
        assert_eq!(r.format, limina_framecap::FORMAT, "{r:?}");
        assert_eq!(
            r.guest_flip,
            r.cause.is_none(),
            "guest_flip and cause disagree: {r:?}"
        );
        if r.seq.is_some() {
            assert!(
                r.presented_iosurface.is_some()
                    && r.layer_iosurface.is_some()
                    && r.width.is_some()
                    && r.height.is_some()
                    && r.t_monotonic_raw_ns.is_some()
                    && r.t_realtime_ns.is_some(),
                "a presented frame missing its tags: {r:?}"
            );
        }
    }

    // Files and records agree, both ways.
    let captured: Vec<&Record> = records.iter().filter(|r| r.file.is_some()).collect();
    let named: BTreeSet<String> = captured.iter().filter_map(|r| r.file.clone()).collect();
    assert_eq!(named.len(), captured.len(), "two records name one file");
    let on_disk: BTreeSet<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png"))
        .collect();
    assert_eq!(
        on_disk, named,
        "the images on disk and the records naming them disagree"
    );
    for r in &captured {
        let file = r.file.as_deref().unwrap();
        assert_eq!(
            file,
            limina_framecap::file_name(r.slot, r.seq.unwrap()),
            "a file not named by its sequence"
        );
        let (w, h, colours) = decode(&dir.join(file));
        assert_eq!((Some(w), Some(h)), (r.width, r.height), "{file}");
        assert!(colours > 16, "{file} is blank ({colours} colours)");
    }

    // The summary agrees with the records it closes.
    let presented = records.iter().filter(|r| r.seq.is_some()).count() as u64;
    let not_presented: u64 = records
        .iter()
        .filter(|r| r.reason == Some(limina_framecap::Reason::NotPresented))
        .map(|r| {
            let (from, to, n) = (r.flip_from.unwrap(), r.flip_to.unwrap(), r.count.unwrap());
            assert_eq!(to + 1 - from, n, "a not_presented run miscounted: {r:?}");
            n
        })
        .sum();
    assert_eq!(
        (summary.presented, summary.captured, summary.not_presented),
        (presented, captured.len() as u64, not_presented),
        "the summary disagrees with the records"
    );
    assert_eq!(summary.presented, summary.captured + summary.dropped);
    assert!(
        stopped.contains(&format!("{} captured", summary.captured)),
        "the stop's answer does not carry the summary:\n{stopped}"
    );
    assert!(
        summary.captured >= MIN_CAPTURED,
        "only {} frames captured in {WINDOW:?} of a spinning vkcube (need {MIN_CAPTURED}): \
         {summary:?}",
        summary.captured
    );
    let flips = summary.presented + summary.not_presented;
    assert!(
        2 * (summary.dropped + summary.not_presented) < flips,
        "most of the guest's {flips} flips have no image ({} dropped, {} never presented): \
         {summary:?}",
        summary.dropped,
        summary.not_presented
    );
    assert_eq!(
        summary.incomplete, None,
        "the stop did not drain: {summary:?}"
    );
    report(&summary);

    // A still of the screen once nothing animates: written now, tagged in the same vocabulary,
    // with no sequence capture running.
    let _ = guest.ssh_exec("pkill -x vkcube; true");
    std::thread::sleep(IDLE);
    let still_png = guest.scratch_dir().join("still.png");
    let (ok, said) = limina_debug(&guest, &["capture", "still", &still_png.to_string_lossy()]);
    assert!(ok, "capture still was refused:\n{said}");
    let still = said
        .lines()
        .find_map(|l| Still::parse(l).ok())
        .unwrap_or_else(|| panic!("capture still printed no tag line:\n{said}"));
    eprintln!("still: {still:?}");
    assert_eq!(still.format, limina_framecap::FORMAT);
    assert_eq!(still.file, still_png.to_string_lossy());
    assert_eq!(still.guest_flip, still.cause.is_none(), "{still:?}");
    let last = records.iter().rev().find(|r| r.seq.is_some()).unwrap();
    assert_eq!(
        (still.slot, still.epoch),
        (last.slot, last.epoch),
        "{still:?}"
    );
    assert!(
        still.flip >= last.flip,
        "the still's flip went backwards: {still:?}"
    );
    assert!(
        still.taken_t_realtime_ns >= still.t_realtime_ns
            && still.taken_t_monotonic_raw_ns >= still.t_monotonic_raw_ns,
        "the still was taken before its frame went up: {still:?}"
    );
    eprintln!(
        "still: its frame had been on glass {:.2} s",
        (still.taken_t_monotonic_raw_ns - still.t_monotonic_raw_ns) as f64 / 1e9
    );
    let (w, h, colours) = decode(&still_png);
    assert_eq!((w, h), (still.width, still.height), "{still:?}");
    assert!(colours > 16, "the still is blank ({colours} colours)");
    // A display no window shows is refused, loudly.
    let (ok, said) = limina_debug(
        &guest,
        &["capture", "still", &still_png.to_string_lossy(), "7"],
    );
    assert!(
        !ok && said.contains("no window shows guest display 7"),
        "a still of a display with no window was not refused clearly:\n{said}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}

fn report(s: &Summary) {
    let shown = s.presented.max(1) as f64;
    eprintln!(
        "frame capture at {}x{}: {:.1} captured fps, {:.1}% of presented frames dropped, \
         {} flips never presented; main thread p50 {} us, p99 {} us, max {} us",
        DISPLAY.0,
        DISPLAY.1,
        s.captured_fps,
        100.0 * s.dropped as f64 / shown,
        s.not_presented,
        s.present_hook_us_p50,
        s.present_hook_us_p99,
        s.present_hook_us_max
    );
}
