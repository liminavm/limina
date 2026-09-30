// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Regression guard: a window showing a copy of a guest scanout must show the frame that was
//! presented, not whatever the guest drew into the buffer afterwards.
//!
//! A guest whose scanout flushes carry no fence is never held off the buffers it flips: its flip
//! completes on its own vblank timer, and a double-buffering compositor starts drawing its next
//! frame into the buffer the flip released. The window shows such a slot through a private copy
//! (`crates/limina/src/window/copy.rs`), but that copy reads the guest's surface when the blit
//! runs, not when the frame arrived, and nothing orders the read before the guest's next frame.
//! Under load the read lands late. A synoik desktop showed it as translucent surfaces dropping
//! out for a frame, and as frame counters stepping back, only on a host-side recording.
//!
//! **Vehicle.** `guest/kmschurn.py stamp-vk` flips two venus scanout buffers the way synoik
//! does: allocated in venus, unfenced flushes, the next frame drawn into the buffer the flip
//! released. Each frame first clears its buffer to a colour that is the frame number, waits for
//! it on a fence, then flips.
//!
//! **Forcing the late read.** `LIMINA_TEST_COPY_DELAY_MS` holds every copy back before it reads
//! the guest's surface, standing in for a busy supervisor main thread or GPU. The delay spans
//! several guest frames, so a copy that reads the guest's surface at blit time reads it after the
//! guest has drawn into it again, every time rather than when the host happens to be loaded.
//! Holding back the *showing* of a frame is harmless; holding back the *read* of it must be too.
//!
//! **Oracle.** `LIMINA_PRESENT_COPY_TRACE` logs, per copy shown, the guest surface's first pixel
//! when the frame arrived and the copy's first pixel when it went up. Every shown copy must hold
//! the frame number its frame arrived with.
//!
//! Same prereqs as the other venus L2 tests: seated EFI enhanced image + KosmicKrisp; SKIPs
//! cleanly if missing. Gated behind `LIMINA_HVF_TESTS`; run via `scripts/test-boot.sh`. It opens
//! a real NSWindow (the copy path lives in the window), so it is in the EXCLUSIVE set in
//! `.config/nextest.toml`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const KMSCHURN: &str = include_str!("../guest/kmschurn.py");

const DISPLAY: (u32, u32) = (1280, 800);

/// Venus ICD selection for a non-login ssh shell; the `-vk` arm needs nothing else.
const VENUS_ENV: &str = "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json";

/// About fifteen seconds of flips at 60 Hz.
const FRAMES: u32 = 900;

/// How long each copy is held back before it reads the guest's surface: three frames at 60 Hz.
const COPY_DELAY_MS: &str = "50";

/// Traced copies needed before the verdict means anything.
const MIN_TRACED: usize = 50;

/// A traced copy: the host frame (surface) id and the frame numbers read at arrival and on show.
#[derive(Debug)]
struct Traced {
    #[expect(dead_code, reason = "read through Debug, in the failure message")]
    id: u32,
    arrived: u32,
    shown: u32,
}

/// The frame number a `bgra` pixel (bytes in memory order) carries: red, green, blue are its bits
/// 23-16, 15-8, 7-0 (`stamp_rgb` in the vehicle).
fn stamp(bgra: u32) -> u32 {
    let [b, g, r, _a] = bgra.to_be_bytes();
    u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)
}

/// Every `copy trace:` line in `log` whose arrival pixel is one of the vehicle's frame numbers.
/// The buffers' first clears are other colours and decode far above `FRAMES`.
fn traced(log: &str) -> Vec<Traced> {
    log.lines()
        .filter_map(|line| {
            let rest = &line[line.find("copy trace: frame ")? + "copy trace: frame ".len()..];
            let mut words = rest.split_whitespace();
            let id = words.next()?.parse().ok()?;
            let hex = |w: &str, key: &str| u32::from_str_radix(w.strip_prefix(key)?, 16).ok();
            let (_, arrived) = (words.next()?, hex(words.next()?, "bgra=")?);
            let (_, shown) = (words.next()?, hex(words.next()?, "bgra=")?);
            Some(Traced {
                id,
                arrived: stamp(arrived),
                shown: stamp(shown),
            })
        })
        .filter(|t| (1..=FRAMES).contains(&t.arrived))
        .collect()
}

#[test]
fn a_copied_scanout_shows_the_frame_that_was_presented() {
    const NAME: &str = "a_copied_scanout_shows_the_frame_that_was_presented";
    if !limina_test::require_hvf_or_skip(NAME) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIPPED {NAME}: no KosmicKrisp ICD — venus unavailable");
        return;
    }
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {NAME}: {e:#}");
            return;
        }
    };
    // The bare `warn` keeps every other crate at warn. The copy module logs the trace; the
    // virtio-gpu module says whether the vehicle's flushes are fenced.
    let cfg = cfg
        .with_windowed_coexist_display(DISPLAY.0, DISPLAY.1)
        .with_net()
        .with_supervisor_log()
        .with_env(
            "RUST_LOG",
            "warn,limina::window::copy=info,krun_devices::virtio::gpu::virtio_gpu=info",
        )
        .with_env("LIMINA_PRESENT_COPY_TRACE", "1")
        .with_env("LIMINA_TEST_COPY_DELAY_MS", COPY_DELAY_MS);

    let mut guest = Guest::boot(&cfg).expect("booting the seated enhanced guest windowed");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd came up");

    // The presenter needs DRM master, which the session compositor holds.
    guest
        .ssh_exec("sudo -n systemctl isolate multi-user.target")
        .expect("isolating to multi-user.target");
    std::thread::sleep(Duration::from_secs(3));

    guest
        .ssh_exec(&format!(
            "cat > /tmp/kmschurn.py <<'KMSCHURN_PY_EOF'\n{KMSCHURN}\nKMSCHURN_PY_EOF"
        ))
        .expect("writing kmschurn.py to the guest");
    let offset = guest.supervisor_log().len();
    guest
        .ssh_exec(&format!(
            "sudo -n setsid -f env {VENUS_ENV} python3 /tmp/kmschurn.py stamp-vk {FRAMES} 2 \
             {} {} </dev/null >/tmp/kmschurn.out 2>&1",
            DISPLAY.0, DISPLAY.1
        ))
        .expect("starting the presenter");
    let done = guest
        .ssh_poll(
            "grep -E 'CHURN (DONE|FAIL)' /tmp/kmschurn.out",
            Duration::from_secs(120),
        )
        .unwrap_or_else(|e| panic!("the presenter never finished: {e:#}"));
    let out = guest.ssh_exec("cat /tmp/kmschurn.out").unwrap_or_default();
    assert!(
        done.contains("CHURN DONE stamp-vk"),
        "the presenter failed:\n{out}"
    );
    eprintln!("presenter:\n{out}");

    // Let the last held-back copies go up before reading the log.
    std::thread::sleep(Duration::from_secs(2));
    let log = guest.supervisor_log();
    let run = log.get(offset..).unwrap_or("");

    assert!(
        log.contains("flushes carry no fence; nothing holds the guest off buffers on glass"),
        "the worker never reported an unfenced scanout, so the window may not be copying at all \
         and this vehicle does not reach the race it guards"
    );
    let copies = traced(run);
    assert!(
        copies.len() >= MIN_TRACED,
        "only {} traced copies of the presenter's frames (need {MIN_TRACED}): either the window \
         is not showing this slot through a copy, or the trace (LIMINA_PRESENT_COPY_TRACE) no \
         longer logs at limina::window::copy=info",
        copies.len()
    );

    let wrong: Vec<&Traced> = copies.iter().filter(|t| t.shown != t.arrived).collect();
    eprintln!(
        "{} traced copies, {} showing a frame other than the one presented; first: {:?}",
        copies.len(),
        wrong.len(),
        copies.first()
    );
    assert!(
        wrong.is_empty(),
        "{} of {} copies showed a different frame from the one presented: the copy read the \
         guest's buffer after the guest had drawn into it again. First few: {:?}",
        wrong.len(),
        copies.len(),
        &wrong[..wrong.len().min(8)]
    );

    drop(guest);
}
