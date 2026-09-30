// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Regression guard: a copy of a guest scanout on glass must show the frame that was presented,
//! not whatever the guest drew into the buffer afterwards.
//!
//! A guest whose scanout flushes carry no fence is never held off the buffers it flips: its flip
//! completes on its own vblank timer, and a double-buffering compositor starts drawing its next
//! frame into the buffer the flip released. Such a scanout is shown through a copy, and a copy
//! made whenever the host gets round to it reads whatever the guest has drawn since. A synoik
//! desktop showed that as translucent surfaces dropping out for a frame, and as frame counters
//! stepping back, only on a host-side recording.
//!
//! For a venus scanout the copy is taken by the renderer on the guest's own queue, behind the
//! frame's work and ahead of what the guest submits after it (virglrs `venus/present_copy.rs`),
//! and the window shows that copy as it is.
//!
//! **Vehicle.** `guest/kmschurn.py stamp-vk` flips two venus scanout buffers the way synoik
//! does: allocated in venus, unfenced flushes, the next frame drawn into the buffer the flip
//! released. Each frame first clears its buffer to a colour that is the frame number, waits for
//! it on a fence, then flips.
//!
//! **Oracle.** `LIMINA_PRESENT_COPY_TRACE` logs, per copy, the guest scanout's first pixel as the
//! flush was handled and the copy's first pixel once it was made. Every copy must hold the frame
//! number its flush arrived with.
//!
//! **The two delays.** `LIMINA_TEST_COPY_DELAY_MS` holds back the supervisor, which once made the
//! copy itself when it got round to it: lateness there must now only delay the frame, never
//! change it. `LIMINA_TEST_PRESENT_COPY_DELAY_MS` holds back the renderer's copy submission, which
//! lets the guest's next frame reach the queue first. That is the case an unfenced guest leaves
//! open -- the flush and the guest's rendering arrive on different host threads -- and only a
//! flush fence closes it, so that test is ignored and documents the gap.
//!
//! **Two outputs.** A guest driving two monitors from one context presents two shapes in turn.
//! The copy surfaces are kept per output, so each reuses its own few: a ring shared between them
//! minted a surface on nearly every present, and a dogfood desktop ran the worker out of memory
//! that way in minutes. That test flips two outputs of different sizes (`stamp2-vk`) and bounds
//! the distinct copy surfaces the trace names.
//!
//! Same prereqs as the other venus L2 tests: seated EFI enhanced image + KosmicKrisp; SKIPs
//! cleanly if missing. Gated behind `LIMINA_HVF_TESTS`; run via `scripts/test-boot.sh`. It opens
//! a real NSWindow, so it is in the EXCLUSIVE set in `.config/nextest.toml`.

use std::time::Duration;

use std::collections::BTreeSet;

use limina_test::{DisplayControl, EdidSpec, Guest, GuestConfig};

const KMSCHURN: &str = include_str!("../guest/kmschurn.py");

const DISPLAY: (u32, u32) = (1280, 800);

/// The second output of the two-output test: another shape, so the two cannot share surfaces.
const SECOND: (u32, u32) = (1024, 768);

/// Copy surfaces one output may grow to (virglrs `MAX_SLOTS`).
const SURFACES_PER_OUTPUT: usize = 6;

/// Venus ICD selection for a non-login ssh shell; the `-vk` arm needs nothing else.
const VENUS_ENV: &str = "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json";

/// About fifteen seconds of flips at 60 Hz.
const FRAMES: u32 = 900;

/// How long a delay holds its step back: three frames at 60 Hz.
const DELAY_MS: &str = "50";

/// Traced copies needed before the verdict means anything.
const MIN_TRACED: usize = 50;

/// A traced copy: the host frame (surface) id and the frame numbers read at arrival and on show.
#[derive(Debug)]
struct Traced {
    id: u32,
    arrived: u32,
    shown: u32,
    /// How many copy surfaces some process held in use when this one was picked, where the
    /// renderer says.
    in_use: Option<u32>,
}

/// The frame number a `bgra` pixel (bytes in memory order) carries: red, green, blue are its bits
/// 23-16, 15-8, 7-0 (`stamp_rgb` in the vehicle).
fn stamp(bgra: u32) -> u32 {
    let [b, g, r, _a] = bgra.to_be_bytes();
    u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)
}

/// Every `copy trace:` line in `log` whose arrival pixel is one of the vehicle's `stamps` frame
/// numbers. The buffers' first clears are other colours and decode far above them.
fn traced(log: &str, stamps: u32) -> Vec<Traced> {
    log.lines()
        .filter_map(|line| {
            let rest = &line[line.find("copy trace: frame ")? + "copy trace: frame ".len()..];
            let mut words = rest.split_whitespace();
            let id = words.next()?.parse().ok()?;
            let hex = |w: &str, key: &str| u32::from_str_radix(w.strip_prefix(key)?, 16).ok();
            let (_, arrived) = (words.next()?, hex(words.next()?, "bgra=")?);
            let (_, shown) = (words.next()?, hex(words.next()?, "bgra=")?);
            let in_use = words
                .next()
                .and_then(|w| w.strip_prefix("in_use="))
                .and_then(|n| n.parse().ok());
            Some(Traced {
                id,
                arrived: stamp(arrived),
                shown: stamp(shown),
                in_use,
            })
        })
        .filter(|t| (1..=stamps).contains(&t.arrived))
        .collect()
}

#[test]
fn a_copied_scanout_shows_the_frame_that_was_presented() {
    run(
        "a_copied_scanout_shows_the_frame_that_was_presented",
        Some("LIMINA_TEST_COPY_DELAY_MS"),
        1,
    );
}

#[test]
fn two_outputs_from_one_context_keep_their_own_copy_surfaces() {
    run(
        "two_outputs_from_one_context_keep_their_own_copy_surfaces",
        None,
        2,
    );
}

#[test]
#[ignore = "known open: a copy submitted after the guest's next frame reaches the queue reads \
            that frame; only a guest flush fence orders it"]
fn a_copy_submitted_late_still_shows_the_frame_that_was_presented() {
    run(
        "a_copy_submitted_late_still_shows_the_frame_that_was_presented",
        Some("LIMINA_TEST_PRESENT_COPY_DELAY_MS"),
        1,
    );
}

/// Present the stamped frames on `outputs` outputs, with `delay` holding its step back, and check
/// every copy.
fn run(name: &str, delay: Option<&str>, outputs: u32) {
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIPPED {name}: no KosmicKrisp ICD — venus unavailable");
        return;
    }
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };
    // The bare `warn` keeps every other crate at warn. The virtio-gpu module says how the
    // vehicle's scanout is kept from it; the renderer's trace goes to stderr regardless.
    // Windowed for both: a parked present, which is what takes the copy, is armed only where
    // the window acknowledges what it showed.
    let mut cfg = cfg
        .with_windowed_coexist_display(DISPLAY.0, DISPLAY.1)
        .with_display_pool(outputs)
        .with_net()
        .with_supervisor_log()
        .with_env(
            "RUST_LOG",
            "warn,limina::window::copy=info,krun_devices::virtio::gpu::virtio_gpu=info",
        )
        .with_env("LIMINA_PRESENT_COPY_TRACE", "1");
    if let Some(delay) = delay {
        cfg = cfg.with_env(delay, DELAY_MS);
    }

    let mut guest = Guest::boot(&cfg).expect("booting the seated enhanced guest windowed");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd came up");

    if outputs == 2 {
        guest
            .update_display(DisplayControl {
                display_id: 1,
                size: Some(SECOND),
                position: None,
                connected: Some(true),
                edid: Some(EdidSpec {
                    refresh_hz: 60,
                    dpi: 96,
                    vendor: *b"LMN",
                    product_id: 0x5152,
                    serial: 0x5EC0_0002,
                    name: "L2 Copy Two".into(),
                    serial_string: None,
                    range: None,
                    modes: Vec::new(),
                    alt_mode: None,
                }),
            })
            .expect("connecting the second output");
        guest
            .ssh_poll(
                "test $(grep -lx connected /sys/class/drm/card*-Virtual-*/status | wc -l) -ge 2",
                Duration::from_secs(60),
            )
            .expect("the second output never showed up connected in the guest");
    }

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
    let arm = if outputs == 2 {
        "stamp2-vk"
    } else {
        "stamp-vk"
    };
    guest
        .ssh_exec(&format!(
            "sudo -n setsid -f env {VENUS_ENV} python3 /tmp/kmschurn.py {arm} {FRAMES} 2 \
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
        done.contains(&format!("CHURN DONE {arm}")),
        "the presenter failed:\n{out}"
    );
    eprintln!("presenter:\n{out}");

    // Let the last held-back copies go up before reading the log.
    std::thread::sleep(Duration::from_secs(2));
    let log = guest.supervisor_log();
    let run = log.get(offset..).unwrap_or("");

    assert!(
        log.contains("flushes carry no fence"),
        "the worker never reported an unfenced scanout, so this vehicle does not reach the race \
         it guards"
    );
    assert!(
        log.contains("presents copies taken on the guest's own queue"),
        "the worker never said it presents the scanout as copies taken on the guest's queue, so \
         whatever copies are traced below are not the ordered ones this guards"
    );
    let copies = traced(run, FRAMES * outputs);
    assert!(
        copies.len() >= MIN_TRACED,
        "only {} traced copies of the presenter's frames (need {MIN_TRACED}): either this slot \
         is not presented through a copy, or the trace (LIMINA_PRESENT_COPY_TRACE) no longer \
         logs",
        copies.len()
    );

    let wrong: Vec<&Traced> = copies.iter().filter(|t| t.shown != t.arrived).collect();
    // Reported, not asserted: whether the window server marks a surface it composites as in use
    // is what keeps the renderer from copying into one on glass, and this is where to see it.
    let with_in_use = copies
        .iter()
        .filter(|t| t.in_use.is_some_and(|n| n > 0))
        .count();
    eprintln!(
        "{with_in_use} of {} copies found another copy surface in use when picked",
        copies.len()
    );
    eprintln!(
        "{} traced copies, {} showing a frame other than the one presented; first: {:?}",
        copies.len(),
        wrong.len(),
        copies.first()
    );
    // Distinct surfaces, not mints: a copy surface is never freed while its ring stands, so its id
    // cannot be recycled for another one, and a ring that kept minting shows here as ids.
    let surfaces: BTreeSet<u32> = copies.iter().map(|t| t.id).collect();
    eprintln!("the copies landed in {} distinct surfaces", surfaces.len());
    assert!(
        surfaces.len() <= SURFACES_PER_OUTPUT * outputs as usize,
        "{} outputs' copies landed in {} distinct surfaces (at most {} each): the copy surfaces \
         are not being reused, and every one minted is memory the worker keeps",
        outputs,
        surfaces.len(),
        SURFACES_PER_OUTPUT
    );
    if outputs == 2 {
        for scanout in 0..outputs {
            assert!(
                run.contains(&format!("present copy: scanout {scanout}: minted surface")),
                "no copy surface was minted for scanout {scanout}, so its presents were not \
                 copied and this does not test what it says"
            );
        }
    }
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
