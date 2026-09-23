// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 virtio-snd test: guest playback is paced by the host's audio hardware, across a device
//! reset.
//!
//! Drives the real `limina` supervisor on the STOCK Fedora image (EFI/BLS boot, `--net` for SSH):
//! the guest's stock `virtio_snd` driver binds libkrun's device, and `aplay` writes three seconds
//! of silence straight to the card (`hw:0,0`, bypassing any sound server). Silence, so a run is
//! never audible on the host's speakers; the pacing does not depend on what the samples are.
//!
//! The device's worker thread completes each tx buffer only once the CoreAudio render callback
//! has played its frames, so the guest's drain returns, and it sends STOP, only once the clip has
//! played. The oracle is the host's own timeline of the stream, PCM_START to PCM_STOP in the
//! worker's log, which has to match the clip's length. RED if the host sink never came up (the
//! silent-sink fallback completes buffers at once, so STOP follows START almost immediately),
//! or if completions stop reaching the guest (no STOP).
//!
//! Not the guest's clock. Around the device reset, the guest measured the same clip anywhere from
//! 2.86 s to 5.7 s while the host timeline read 3.06 s every time, and a measurement that can come
//! in under the clip's own length is not measuring the device.
//!
//! Unbinding and rebinding the driver resets the device and activates it again: the worker is
//! stopped and joined, and a new one starts and has to bring up a new sink. The clip is played
//! again after that. Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const CLIP_SECS: u64 = 3;

/// Play `CLIP_SECS` of S16_LE 48 kHz stereo silence on the virtio card, then check the host's
/// timeline of the stream it played.
fn play_clip(guest: &Guest, when: &str) {
    let bytes = 48_000 * 4 * CLIP_SECS;
    let before = guest.supervisor_log().len();
    let cmd = format!(
        "head -c {bytes} /dev/zero > /tmp/silence.raw && \
         sudo aplay -q -D hw:0,0 -t raw -f S16_LE -r 48000 -c 2 /tmp/silence.raw"
    );
    guest
        .ssh_exec_timeout(&cmd, Duration::from_secs(CLIP_SECS * 10))
        .unwrap_or_else(|e| panic!("{when}: playing the clip: {e}"));
    let log = guest.supervisor_log();
    let stream = &log[before..];
    let at = |code: &str| -> f64 {
        let line = stream
            .lines()
            .find(|l| l.contains(&format!("snd: control req code={code}")))
            .unwrap_or_else(|| panic!("{when}: the host never saw {code} for the clip"));
        seconds_of_day(line).unwrap_or_else(|| panic!("{when}: no timestamp in {line:?}"))
    };
    let (start, stop) = (at(START), at(STOP));
    let ms = ((stop - start).rem_euclid(86_400.0) * 1000.0) as u64;
    let clip_ms = CLIP_SECS * 1000;
    eprintln!("{when}: the host played the {clip_ms} ms clip from START to STOP in {ms} ms");
    assert!(
        ms >= clip_ms - 100,
        "{when}: STOP came {ms} ms after START for a {clip_ms} ms clip: buffers were completed \
         without being played (no host sink?)"
    );
    assert!(
        ms <= clip_ms + 500,
        "{when}: STOP came {ms} ms after START for a {clip_ms} ms clip: completions reached the \
         guest late"
    );
}

/// PCM_START and PCM_STOP, as the worker's debug log names them.
const START: &str = "0x104";
const STOP: &str = "0x105";

/// The time of day a log line was written, from its `[YYYY-MM-DDTHH:MM:SS.ffffffZ` prefix.
fn seconds_of_day(line: &str) -> Option<f64> {
    let time = line.split('T').nth(1)?.split('Z').next()?;
    let mut parts = time.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

fn wait_for_card(guest: &Guest) {
    guest
        .ssh_poll(
            "test -e /dev/snd/pcmC0D0p && echo ok",
            Duration::from_secs(60),
        )
        .expect("the virtio sound card's playback PCM never appeared");
}

#[test]
fn snd_playback_is_paced_by_the_host_across_a_device_reset() {
    if !limina_test::require_hvf_or_skip("snd_playback_is_paced_by_the_host_across_a_device_reset")
    {
        return;
    }

    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net()
        .with_supervisor_log()
        .with_env("RUST_LOG", "warn,krun_devices::virtio::snd=debug");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest did not reach sshd");

    let have_aplay = guest
        .ssh_exec("command -v aplay || echo missing")
        .expect("probing for aplay");
    assert!(
        !have_aplay.contains("missing"),
        "the stock image has no aplay (alsa-utils)"
    );

    wait_for_card(&guest);
    guest
        .wait_for_supervisor_log("snd worker: starting", Duration::from_secs(10))
        .expect("the snd device never started its worker thread");
    play_clip(&guest, "first activation");

    // Unbinding resets the device even while something holds the card open, which a module
    // unload refuses to do.
    guest
        .ssh_exec(
            "d=$(basename $(ls -d /sys/bus/virtio/drivers/virtio_snd/virtio*)) && \
             echo $d | sudo tee /sys/bus/virtio/drivers/virtio_snd/unbind && \
             echo $d | sudo tee /sys/bus/virtio/drivers/virtio_snd/bind",
        )
        .expect("rebinding the virtio sound device");
    guest
        .ssh_exec("sudo udevadm settle --timeout=30")
        .expect("waiting for udev to finish with the new card");
    guest
        .wait_for_supervisor_log("snd worker: stopping", Duration::from_secs(10))
        .expect("the device reset did not stop the worker");
    wait_for_card(&guest);
    let starts = guest
        .supervisor_log()
        .matches("snd worker: starting")
        .count();
    assert!(
        starts >= 2,
        "the re-activation started no new worker ({starts} start(s) logged)"
    );
    play_clip(&guest, "after a device reset");

    let outcome = guest
        .shutdown(Duration::from_secs(60))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
