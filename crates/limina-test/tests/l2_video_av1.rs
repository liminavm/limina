// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — a **stock** Fedora guest plays AV1 through VA-API, paced like a player, on any Apple
//! silicon host.
//!
//! virglrs always offers AV1: a host with AV1 silicon (M3 and later) decodes it in
//! VideoToolbox, and one without (M1, M2) decodes every unit with dav1d on the host. Either way
//! the guest sees an AV1 decode profile and the stream leaves the guest's own CPU. The same
//! two gates as VP9 (`l2_video_vaapi.rs`) let a stock guest reach it: AV1 is in mesa's
//! `all_free` codec set, so nothing in the guest's VA frontend refuses it.
//!
//! # The oracles
//!
//! 1. **`gst-inspect-1.0 va` offers `vaav1dec`.** The host's AV1 caps crossed the virgl
//!    protocol and the guest driver advertises a decode profile for them.
//! 2. **Paced playback reaches the supervisor as video.** `ffmpeg -re` feeds the decoder at
//!    real time, as a player does, and the worker's decode observer turns the units into the
//!    supervisor's `video decoding` / `video still` edges — the video half of the display-wake
//!    heuristic (`crates/limina/src/window/wake_policy.rs`). That observer is only installed
//!    with a window sink, hence the windowed boot.
//! 3. **No control-queue drain of 100 ms or more during that playback.** The worker warns at
//!    100 ms when the control queue kept it from everything else, scanout included. Paced
//!    decoding has no business doing that, so a warning here is decode cost landing on the
//!    worker thread. The pacing is load-bearing: unpaced, the guest hands the host the whole
//!    clip in one go and the render thread waits on the decode queue until the decoder catches
//!    up — a flood, and a drain warning by design, whatever the decoder.
//! 4. **The hardware decode equals the software one, frame for frame.** AV1 is normatively
//!    exact, so the host's pictures must hash identically to the guest's own libdav1d. Both
//!    runs download into system memory and convert to yuv420p in software, so only the decoder
//!    differs and the VA post-processing draw is not involved (`l2_video_vaapi.rs` covers it).
//! 5. **The frames are not uniform.** testsrc2 moves, so most frames must hash differently.
//!    Oracle 4 alone would pass if both decoders returned the same blank picture every frame.
//!
//! Vehicle: the stock F44 autologin baseline in a real window on the coexist GPU with the
//! zink-on-KK host-GL worker env (video rides the virgl/vrend context). Exclusive in
//! `.config/nextest.toml`: it opens a window, and oracle 3 is a timing verdict a co-resident
//! guest could flip. SKIPs cleanly without LIMINA_HVF_TESTS, the KosmicKrisp ICD, the zink-on-KK
//! Mesa prefix, the GOP firmware, or the baseline disk.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The VA driver Fedora ships for virtio-gpu. Its absence is an image property, not a bug.
const GUEST_VA_DRIVER: &str = "/usr/lib64/dri/virtio_gpu_drv_video.so";

const CLIP: &str = "/tmp/limina-av1-oracle.mkv";

/// Long enough that playback holds `video decoding` well past the one-second quiet threshold
/// and that inter frames dominate, short enough that the paced run stays quick.
const CLIP_SECONDS: u32 = 6;
const CLIP_FPS: u32 = 30;

/// The worker's warning for a control-queue drain of 100 ms or more
/// (`third_party/libkrun/src/devices/src/virtio/gpu/worker.rs`).
const DRAIN_WARNING: &str = "control queue drain ran";

/// The supervisor's record of the worker's decode edges (`window/present.rs`).
const DECODING: &str = "<- video decoding";
const STILL: &str = "<- video still";

#[test]
fn stock_guest_plays_av1_through_vaapi() {
    const NAME: &str = "stock_guest_plays_av1_through_vaapi";

    if !limina_test::require_hvf_or_skip(NAME) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED {NAME}: no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk \
             (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    if limina_test::zink_kk_mesa_prefix().is_none() {
        eprintln!(
            "SKIPPED {NAME}: no zink-on-KK Mesa prefix \
             (build spikes/virgl-zink-kk/build-mesa-zink-kk.sh; or set MESA_PREFIX)"
        );
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg
            .with_windowed_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED {NAME}: {e}");
            return;
        }
    };
    eprintln!("booting stock F44 (windowed coexist GPU, virgl/zink-on-KK host GL, NAT)");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_gpu("coexist", Duration::from_secs(60))
        .expect("coexist GPU did not come up (degraded to software-2D?)");
    let banner = guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    let have_driver = guest
        .ssh_exec(&format!("test -e {GUEST_VA_DRIVER} && echo yes || echo no"))
        .expect("ssh to the guest failed");
    if have_driver.trim() != "yes" {
        eprintln!("SKIPPED {NAME}: guest has no {GUEST_VA_DRIVER} (mesa-dri-drivers missing?)");
        return;
    }

    // ORACLE 1 — the host offers AV1 and the guest driver says so.
    let va_elements = guest
        .ssh_exec("gst-inspect-1.0 va 2>&1 || true")
        .expect("ssh to the guest failed");
    assert!(
        va_elements.contains("vaav1dec"),
        "GStreamer's va plugin offers no vaav1dec, so the guest driver advertises no AV1 \
         decode profile — the host is not offering AV1.\ngst-inspect-1.0 va:\n{va_elements}"
    );

    // A clip the guest encodes itself: no fixture, no network. SVT-AV1 is the fast encoder;
    // libaom is the fallback for an ffmpeg built without it.
    guest
        .ssh_exec_timeout(
            &format!(
                "src='testsrc2=size=1280x720:rate={CLIP_FPS}:duration={CLIP_SECONDS}'; \
                 ffmpeg -hide_banner -loglevel error -f lavfi -i \"$src\" -pix_fmt yuv420p \
                   -c:v libsvtav1 -preset 10 -y {CLIP} \
                 || ffmpeg -hide_banner -loglevel error -f lavfi -i \"$src\" -pix_fmt yuv420p \
                   -c:v libaom-av1 -cpu-used 8 -row-mt 1 -y {CLIP}; \
                 stat -c %s {CLIP}"
            ),
            Duration::from_secs(300),
        )
        .expect("the guest could not encode an AV1 clip (no libsvtav1 or libaom-av1 encoder?)");

    // ORACLE 2 + 3 — paced playback, judged on the log it leaves. Everything the boot and the
    // encode logged comes before this offset and is not this playback's.
    let before = guest.supervisor_log().len();
    let playback = guest
        .ssh_exec_timeout(
            &format!(
                "ffmpeg -hide_banner -loglevel error -stats -re -hwaccel vaapi \
                 -hwaccel_output_format vaapi -i {CLIP} -f null - 2>&1 | tr '\\r' '\\n' | tail -1"
            ),
            Duration::from_secs(120),
        )
        .expect("paced VA-API playback failed to run");
    eprintln!("paced playback: {}", playback.trim());
    guest
        .wait_for_supervisor_log(STILL, Duration::from_secs(10))
        .expect("the supervisor never heard the decoding stop");
    let log = guest.supervisor_log();
    let during = &log[before.min(log.len())..];
    let decoding_at = during.find(DECODING).unwrap_or_else(|| {
        panic!(
            "paced AV1 playback never reached the supervisor as `video decoding` — the decode \
             observer saw no units, so the display-wake heuristic cannot see this video.\n\
             playback: {playback}\n--- supervisor log since playback ---\n{during}"
        )
    });
    assert!(
        during[decoding_at..].contains(STILL),
        "the supervisor heard `video decoding` but never `video still` after it.\n{during}"
    );
    let drains: Vec<&str> = during
        .lines()
        .filter(|l| l.contains(DRAIN_WARNING))
        .collect();
    assert!(
        drains.is_empty(),
        "paced AV1 playback held the GPU worker's control queue for 100 ms or more {} time(s) — \
         decode cost is landing on the worker thread instead of the decode thread:\n{}",
        drains.len(),
        drains.join("\n")
    );

    // ORACLE 4 + 5 — the host's pictures against the guest's own libdav1d, frame by frame.
    let frames = |input_opts: &str| {
        format!(
            "ffmpeg -hide_banner -loglevel error {input_opts} -i {CLIP} -pix_fmt yuv420p \
             -f framemd5 - | grep -v '^#'"
        )
    };
    let report = guest
        .ssh_exec_timeout(
            &format!(
                "{hw} > /tmp/limina-av1-hw.md5; {sw} > /tmp/limina-av1-sw.md5; \
                 echo \"hw_frames=$(wc -l < /tmp/limina-av1-hw.md5)\"; \
                 echo \"sw_frames=$(wc -l < /tmp/limina-av1-sw.md5)\"; \
                 echo \"hw_distinct=$(awk -F, '{{print $NF}}' /tmp/limina-av1-hw.md5 \
                     | sort -u | wc -l)\"; \
                 echo \"hw_md5=$(md5sum < /tmp/limina-av1-hw.md5 | cut -d' ' -f1)\"; \
                 echo \"sw_md5=$(md5sum < /tmp/limina-av1-sw.md5 | cut -d' ' -f1)\"",
                hw = frames("-hwaccel vaapi"),
                sw = frames("-c:v libdav1d"),
            ),
            Duration::from_secs(300),
        )
        .expect("the decode comparisons failed to run");
    eprintln!("{report}");
    let field = |name: &str| -> String {
        report
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{name}=")))
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    let expected = CLIP_SECONDS * CLIP_FPS;
    let hw_frames: u32 = field("hw_frames").parse().unwrap_or(0);
    let sw_frames: u32 = field("sw_frames").parse().unwrap_or(0);
    assert_eq!(
        sw_frames, expected,
        "the software reference decoded {sw_frames} frames, expected {expected} — the clip \
         itself is wrong.\n{report}"
    );
    assert_eq!(
        hw_frames, expected,
        "the VA-API decode produced {hw_frames} frames, expected {expected} — it dropped or \
         duplicated pictures.\n{report}"
    );
    let distinct: u32 = field("hw_distinct").parse().unwrap_or(0);
    assert!(
        distinct > expected / 2,
        "only {distinct} of {expected} VA-API frames hash differently, but testsrc2 moves every \
         frame — the host is returning the same picture over and over.\n{report}"
    );
    assert_eq!(
        field("hw_md5"),
        field("sw_md5"),
        "the VA-API and libdav1d decodes disagree; AV1 is normatively exact, so the host path is \
         producing wrong pixels.\n{report}"
    );

    eprintln!(
        "paced AV1 playback reached the supervisor as video with no worker drain, and the \
         VA-API decode matched libdav1d across {expected} frames"
    );
}
