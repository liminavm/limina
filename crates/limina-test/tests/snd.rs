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
//! has played its frames, so a three-second clip takes three seconds. RED if the host sink never
//! came up (the silent-sink fallback completes buffers at once and the clip "plays" in well under
//! a second), or if completions stop reaching the guest (aplay's drain never returns).
//!
//! Unbinding and rebinding the driver resets the device and activates it again: the worker is
//! stopped and joined, and a new one starts and has to bring up a new sink. The clip is played
//! again after that. Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const CLIP_SECS: u64 = 3;

/// Play `CLIP_SECS` of S16_LE 48 kHz stereo silence on the virtio card; return the wall time in
/// milliseconds, measured inside the guest.
fn play_clip(guest: &Guest) -> u64 {
    let bytes = 48_000 * 4 * CLIP_SECS;
    let cmd = format!(
        "head -c {bytes} /dev/zero > /tmp/silence.raw && \
         s=$(date +%s%N) && \
         sudo aplay -q -D hw:0,0 -t raw -f S16_LE -r 48000 -c 2 /tmp/silence.raw && \
         e=$(date +%s%N) && echo $(( (e - s) / 1000000 ))"
    );
    let out = guest
        .ssh_exec_timeout(&cmd, Duration::from_secs(CLIP_SECS * 10))
        .unwrap_or_else(|e| panic!("playing the clip: {e}"));
    out.trim()
        .parse()
        .unwrap_or_else(|e| panic!("parsing the clip's wall time from {out:?}: {e}"))
}

fn assert_paced(ms: u64, when: &str) {
    let clip_ms = CLIP_SECS * 1000;
    eprintln!("{when}: {CLIP_SECS} s clip took {ms} ms");
    assert!(
        ms >= clip_ms - 200,
        "{when}: a {clip_ms} ms clip finished in {ms} ms — buffers were completed without being \
         played (no host sink?)"
    );
    assert!(
        ms <= clip_ms + 2000,
        "{when}: a {clip_ms} ms clip took {ms} ms — completions reached the guest late"
    );
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
    assert_paced(play_clip(&guest), "first activation");

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
    assert_paced(play_clip(&guest), "after a device reset");

    let outcome = guest
        .shutdown(Duration::from_secs(60))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
