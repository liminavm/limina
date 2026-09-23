// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — a slow hardware decode does not hold up the virtio-gpu control thread, and still
//! delivers exact pictures.
//!
//! Every VA-API picture used to be decoded inside END_FRAME on the one thread that serves the
//! whole control queue, so each frame's decode time was also time every other context's
//! commands (the desktop's present among them) waited. The design moving it off that thread is
//! `docs/design/async-video-decode.md`.
//!
//! `VIRGLRS_DECODE_DELAY_MS` makes every decode 40 ms late, which is the stall this test is
//! about, made deterministic: a fast media engine would keep it too short to measure. Two
//! oracles, both on the same run:
//!
//! 1. **The decode has left END_FRAME.** `VIRGLRS_SUBMIT_STATS` reports the worst single video
//!    command's time on the submitting thread. With the decode inside it, that is at least the
//!    delay; with the decode queued to its own thread, a command only parses and queues. RED
//!    before the change. It does not by itself prove the control thread never waits: this
//!    pipeline reads every picture back right after decoding it, and that read has to wait for
//!    the picture. Those waits are reported separately, and the desktop-level claim belongs to a
//!    stack sample under real playback.
//! 2. **The pictures are still exact.** The hardware decode is compared byte for byte with the
//!    software decoder's (VP9 is normatively exact), as `l2_video_vaapi` does. A 40 ms window
//!    between a picture being queued and landing is exactly what a missing wait before a read
//!    of the target would turn into wrong or stale frames.
//!
//! Vehicle and SKIP conditions are `l2_video_vaapi`'s: the stock F44 baseline on the coexist
//! GPU with zink-on-KK host GL.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const GUEST_VA_DRIVER: &str = "/usr/lib64/dri/virtio_gpu_drv_video.so";
const CLIP: &str = "/tmp/limina-vp9-slow.ivf";
const CLIP_WEBM: &str = "/tmp/limina-vp9-slow.webm";
const HW_OUT: &str = "/tmp/limina-vp9-slow-hw.i420";
const SW_OUT: &str = "/tmp/limina-vp9-slow-sw.i420";
const CLIP_SECONDS: u32 = 2;
const CLIP_FPS: u32 = 25;

/// How late each decode is made. Long next to a frame's real decode time on any Apple host, so
/// a command that includes one cannot pass for one that does not.
const DELAY_MS: f64 = 40.0;

/// The worst a video command may take on the submitting thread once the decode is off it: a
/// command then parses and queues. Half the delay, so the two cases cannot meet.
const MAX_COMMAND_MS: f64 = DELAY_MS / 2.0;

#[test]
fn a_slow_decode_leaves_the_control_thread_free() {
    const NAME: &str = "a_slow_decode_leaves_the_control_thread_free";
    if !limina_test::require_hvf_or_skip(NAME) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() || limina_test::zink_kk_mesa_prefix().is_none() {
        eprintln!("SKIPPED {NAME}: no KosmicKrisp ICD or zink-on-KK Mesa prefix");
        return;
    }
    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_virgl_host_gl()
            .with_net()
            .with_supervisor_log()
            .with_env("VIRGLRS_DECODE_DELAY_MS", &format!("{}", DELAY_MS as u64))
            .with_env("VIRGLRS_SUBMIT_STATS", "2"),
        Err(e) => {
            eprintln!("SKIPPED {NAME}: {e}");
            return;
        }
    };
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_supervisor_log("software_2d = false", Duration::from_secs(60))
        .expect("coexist GPU did not come up (degraded to software-2D?)");
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    let have_driver = guest
        .ssh_exec(&format!("test -e {GUEST_VA_DRIVER} && echo yes || echo no"))
        .expect("ssh to the guest failed");
    if have_driver.trim() != "yes" {
        eprintln!("SKIPPED {NAME}: guest has no {GUEST_VA_DRIVER}");
        return;
    }

    guest
        .ssh_exec_timeout(
            &format!(
                "ffmpeg -hide_banner -loglevel error -f lavfi \
                 -i testsrc2=size=320x240:rate={CLIP_FPS}:duration={CLIP_SECONDS} \
                 -c:v libvpx-vp9 -b:v 300k -f ivf {CLIP} -y && \
                 ffmpeg -hide_banner -loglevel error -i {CLIP} -c:v copy -y {CLIP_WEBM}"
            ),
            Duration::from_secs(240),
        )
        .expect("the guest could not encode a VP9 clip");

    let decode = |element: &str, out: &str| {
        format!(
            "gst-launch-1.0 -q filesrc location={CLIP_WEBM} ! matroskademux ! vp9parse \
             ! {element} ! videoconvert ! video/x-raw,format=I420 ! filesink location={out}"
        )
    };
    let report = guest
        .ssh_exec_timeout(
            &format!(
                "{} 2>&1 >/dev/null | tail -2; {} 2>&1 >/dev/null | tail -2; \
                 echo \"hw_md5=$(md5sum < {HW_OUT} | cut -d' ' -f1)\"; \
                 echo \"sw_md5=$(md5sum < {SW_OUT} | cut -d' ' -f1)\"; \
                 echo \"hw_bytes=$(stat -c %s {HW_OUT})\"",
                decode("vavp9dec", HW_OUT),
                decode("avdec_vp9", SW_OUT),
            ),
            Duration::from_secs(240),
        )
        .expect("the decode pipelines failed to run");
    eprintln!("{report}");
    let field = |name: &str| -> String {
        report
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{name}=")))
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    // ORACLE 2 first: a free control thread that delivered wrong pictures is no result.
    let frames = u64::from(CLIP_SECONDS * CLIP_FPS);
    let expected = (320 * 240 * 3 / 2) * frames;
    assert_eq!(
        field("hw_bytes").parse::<u64>().unwrap_or(0),
        expected,
        "the VA-API pipeline dropped or duplicated pictures under a slow decode.\n{report}"
    );
    assert_eq!(
        field("hw_md5"),
        field("sw_md5"),
        "with every decode {DELAY_MS} ms late, the hardware pictures differ from the software \
         decoder's: something read a target before its picture landed.\n{report}"
    );

    // A stats window prints only when a submit lands after it has elapsed, and an idle desktop
    // may submit nothing. So wait the window out and decode a few more frames: that prints the
    // window holding the clip's decodes, and adds only frames to the count.
    std::thread::sleep(Duration::from_secs(3));
    guest
        .ssh_exec_timeout(
            &format!(
                "gst-launch-1.0 -q filesrc location={CLIP_WEBM} ! matroskademux ! vp9parse \
                 ! vavp9dec ! fakesink num-buffers=2"
            ),
            Duration::from_secs(60),
        )
        .expect("the nudge decode failed to run");
    std::thread::sleep(Duration::from_secs(1));
    let log = guest.supervisor_log();
    assert!(
        log.contains("VIRGLRS_DECODE_DELAY_MS makes every decode"),
        "the delay knob never reached a decode session, so this run proves nothing"
    );
    let (mut decoded, mut worst) = (0u64, 0f64);
    for line in log.lines().filter(|l| l.contains("[virglrs] vrend video:")) {
        let number_before = |unit: &str| -> Option<f64> {
            let head = &line[..line.find(unit)?];
            head.split_whitespace().last()?.parse().ok()
        };
        decoded += number_before(" frames").unwrap_or(0.0) as u64;
        worst = worst.max(number_before(" ms/command").unwrap_or(0.0));
    }
    assert!(
        decoded >= frames,
        "the stats counted {decoded} END_FRAMEs for a {frames}-frame clip; the video line is \
         not covering the decode"
    );
    eprintln!("worst video command on the submitting thread: {worst:.2} ms");
    assert!(
        worst < MAX_COMMAND_MS,
        "a video command held the submitting thread for {worst:.2} ms with decodes made \
         {DELAY_MS} ms late: the decode runs on the thread that serves the control queue"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(60))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
