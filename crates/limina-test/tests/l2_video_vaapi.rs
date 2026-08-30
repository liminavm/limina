// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — a **stock** Fedora guest hardware-decodes VP9 through VA-API, on a macOS host.
//!
//! # Why this lands on an unmodified guest
//!
//! Every piece of the guest half already ships in Fedora: `mesa-dri-drivers` contains
//! `/usr/lib64/dri/virtio_gpu_drv_video.so` (mesa builds it from `src/gallium/targets/va`
//! whenever `virgl` is in `gallium-drivers`), and libva picks a driver by DRM driver name —
//! which for virtio-gpu is `virtio_gpu`. So the driver loads and initializes on a guest with
//! nothing of ours installed; it simply advertises **no profiles**, because
//! `virgl_get_video_param()` walks `caps.v2.video_caps[]` and our host used to fill zero
//! entries. The whole feature is therefore host-side: virglrenderer's video backend, which
//! upstream implements only against libva, reimplemented against VideoToolbox.
//!
//! That also means the two-tier guarantee holds *by construction* here. A host without the
//! backend advertises nothing and the guest degrades to software decode — there is no flag
//! and nothing to gate.
//!
//! # Why VP9 and not H.264
//!
//! Two independent gates decide what a stock guest can ask for, and only VP9, AV1, MPEG-2 and
//! MJPEG clear both. Fedora builds mesa with the default `-Dvideo-codecs=all_free`, and
//! `src/gallium/auxiliary/vl/vl_codec.c` enforces that in the **VA frontend**, so H.264 and
//! HEVC are absent from the guest driver no matter what the host offers. On the host side
//! VideoToolbox has no MPEG-2 path on Apple silicon at all, and AV1 needs an M3 or later.
//! VP9 is the one codec in the intersection on every Apple machine we run — see
//! `spikes/videotoolbox-caps/RESULTS.md` for the measured matrix.
//!
//! # The oracles, weakest to strongest
//!
//! 1. **The guest driver exists at all.** An image property, so a miss SKIPs rather than fails.
//! 2. **`gst-inspect-1.0 va` offers `vavp9dec`.** GStreamer builds its element list from the
//!    driver's advertised profiles, so this proves the host's caps crossed the virgl protocol
//!    and were understood. On a host with no backend the same plugin offers only
//!    `vapostproc`/`vadeinterlace`/`vacompositor`. It is also the assertion that catches an
//!    enum skew: the profile number travels raw, and a stale `pipe_video_profile` in
//!    virglrenderer once made a host advertising VP9 publish a `vajpegdec` in the guest.
//! 3. **A real decode, checked bit-for-bit.** VP9 is a normatively exact codec: a conformant
//!    decoder must produce identical pixels, so the hardware decode must equal the software
//!    decoder's output byte for byte. That is far stronger than "the pipeline exited 0" — it
//!    catches a backend that decodes into the wrong plane, drops a frame, shears a stride, or
//!    hands back someone else's picture. Both runs go through the same `videoconvert` to I420,
//!    so only the decoder differs.
//! 4. **The output is not uniform.** Bit-equality alone would pass if both decoders emitted
//!    blank frames. Before the plane upload worked, the guest read a cleared surface — a
//!    single luma value across the whole frame — so this is the assertion that failure
//!    actually tripped.
//!
//! # A neighbouring bug this test must not be confused with
//!
//! `ffmpeg -hwaccel_output_format vaapi ... -vf hwdownload,format=yuv420p` — a download in the
//! surface's *own* format — is byte-identical to a software decode on a stock guest. Asking
//! for a different one (`format=nv12`) returns uniform zeroes, because mesa then allocates a
//! temporary surface, runs the VA post-proc compositor into it on the host, and reads that
//! back: the compositor draw itself renders black. It does so for a plain BGRA source that
//! never went near a decoder, so it is not a fault of this path. Tracked separately.
//!
//! Vehicle: the stock F44 autologin baseline on the coexist GPU with the zink-on-KK host-GL
//! worker env — video rides the **virgl/vrend** context, the same one baseline 3D uses, so it
//! is independent of which tier the guest's 3D is on. SKIPs cleanly without LIMINA_HVF_TESTS,
//! the KosmicKrisp ICD, the zink-on-KK Mesa prefix, the GOP firmware, or the baseline disk.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The VA driver Fedora ships for virtio-gpu. Its absence is an image property, not a bug.
const GUEST_VA_DRIVER: &str = "/usr/lib64/dri/virtio_gpu_drv_video.so";

/// Where the guest builds and decodes its clip.
const CLIP: &str = "/tmp/limina-vp9-oracle.ivf";
const CLIP_WEBM: &str = "/tmp/limina-vp9-oracle.webm";
const HW_OUT: &str = "/tmp/limina-vp9-hw.i420";
const SW_OUT: &str = "/tmp/limina-vp9-sw.i420";

/// One frame of 320x240 luma — the window the uniformity check looks at.
const LUMA_BYTES: u32 = 320 * 240;

/// Long enough to prove inter-frame prediction works (a decoder that only handled keyframes
/// would still match on frame 0), short enough that two full decodes stay quick.
const CLIP_SECONDS: u32 = 2;
const CLIP_FPS: u32 = 25;

#[test]
fn stock_guest_hardware_decodes_vp9_through_vaapi() {
    const NAME: &str = "stock_guest_hardware_decodes_vp9_through_vaapi";

    if !limina_test::require_hvf_or_skip(NAME) {
        return;
    }

    // Video rides the virgl/vrend context, whose host GL is zink-on-KosmicKrisp. Both are
    // machine-local dev builds; without them Guest::boot degrades the display to software-2D
    // and vrend — and therefore the video backend — never comes up at all.
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
            .with_coexist_display(1280, 800)
            .with_virgl_host_gl()
            .with_net()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED {NAME}: {e}");
            return;
        }
    };
    eprintln!("booting stock F44 (coexist GPU, virgl/zink-on-KK host GL, NAT)");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Without the coexist device there is no vrend, hence no video context. We SKIPped on a
    // missing ICD above, so a degrade here is a real failure, not a missing dev build.
    guest
        .wait_for_supervisor_log("software_2d = false", Duration::from_secs(60))
        .expect("coexist GPU did not come up (degraded to software-2D?)");

    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // ORACLE 1 — the guest half exists. An image without mesa's VA driver cannot express the
    // subject of this test, so that is a SKIP.
    let have_driver = guest
        .ssh_exec(&format!("test -e {GUEST_VA_DRIVER} && echo yes || echo no"))
        .expect("ssh to the guest failed");
    if have_driver.trim() != "yes" {
        eprintln!("SKIPPED {NAME}: guest has no {GUEST_VA_DRIVER} (mesa-dri-drivers missing?)");
        return;
    }

    // ORACLE 2 — the host's caps crossed the protocol and GStreamer understood them. The `va`
    // plugin enumerates the driver's profiles at registration; a VP9 decode profile is what
    // makes it publish `vavp9dec` at all.
    let va_elements = guest
        .ssh_exec("gst-inspect-1.0 va 2>&1 || true")
        .expect("ssh to the guest failed");
    assert!(
        va_elements.contains("vavp9dec"),
        "GStreamer's va plugin offers no vavp9dec, so the guest driver advertises no VP9 \
         decode profile — the host is filling no video caps.\ngst-inspect-1.0 va:\n{va_elements}"
    );

    // A clip the guest encodes itself: no fixture, no network, and long enough that inter
    // frames dominate (a decoder that only handled keyframes would still match on frame 0).
    // WebM because GStreamer's ivfparse will not source this pipeline.
    guest
        .ssh_exec_timeout(
            &format!(
                "ffmpeg -hide_banner -loglevel error -f lavfi \
                 -i testsrc2=size=320x240:rate={CLIP_FPS}:duration={CLIP_SECONDS} \
                 -c:v libvpx-vp9 -b:v 300k -f ivf {CLIP} -y && \
                 ffmpeg -hide_banner -loglevel error -i {CLIP} -c:v copy -y {CLIP_WEBM} && \
                 stat -c %s {CLIP_WEBM}"
            ),
            Duration::from_secs(240),
        )
        .expect("the guest could not encode a VP9 clip (no libvpx-vp9 encoder?)");

    // ORACLE 3 + 4 — decode the same clip twice, differing only in the decoder element, and
    // hash both. `videoconvert ! I420` normalizes the layout so the comparison is of pixels.
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
                 echo \"hw_bytes=$(stat -c %s {HW_OUT})\"; \
                 echo \"sw_bytes=$(stat -c %s {SW_OUT})\"; \
                 echo \"hw_distinct_luma=$(head -c {LUMA_BYTES} {HW_OUT} \
                     | od -An -tu1 -v | tr ' ' '\\n' | grep -v '^$' | sort -u | wc -l)\"",
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

    let hw_bytes: u64 = field("hw_bytes").parse().unwrap_or(0);
    let sw_bytes: u64 = field("sw_bytes").parse().unwrap_or(0);
    let expected = (320 * 240 * 3 / 2) * (CLIP_SECONDS * CLIP_FPS) as u64;
    assert_eq!(
        hw_bytes,
        expected,
        "the VA-API pipeline produced {hw_bytes} bytes, expected {expected} \
         ({} frames of 320x240 I420) — it dropped or duplicated pictures.\n{report}",
        CLIP_SECONDS * CLIP_FPS
    );
    assert_eq!(
        sw_bytes, expected,
        "the software reference produced {sw_bytes} bytes, expected {expected} — the clip \
         itself is wrong.\n{report}"
    );

    // The guard against both sides being blank: before the decoded planes reached the guest,
    // it read a cleared surface, which is one luma value for the whole frame.
    let distinct: u32 = field("hw_distinct_luma").parse().unwrap_or(0);
    assert!(
        distinct > 16,
        "the VA-API decode's first frame has only {distinct} distinct luma values — that is a \
         cleared surface, not a decoded picture.\n{report}"
    );

    // VP9 is bit-exact by specification: a conformant hardware decoder must agree with the
    // software one on every pixel of every frame.
    assert_eq!(
        field("hw_md5"),
        field("sw_md5"),
        "the VA-API and software decoders disagree; VP9 is normatively exact, so the hardware \
         path is producing wrong pixels.\n{report}"
    );

    eprintln!(
        "VA-API VP9 decode matched the software decoder byte for byte across {} frames",
        CLIP_SECONDS * CLIP_FPS
    );
}
