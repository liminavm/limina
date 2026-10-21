// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 guard: a stock guest on the Metal transport, booted in a real window, reaches its desktop.
//!
//! A window re-announces the display's identity once the guest's driver is up, and the guest
//! answers that with a hotplug. In the 6.19 virtio-gpu a stock Fedora 44 guest runs, the
//! display-info callback raised that hotplug from the control queue's dequeue worker, and the
//! console's re-commit then waited on its primary-plane flush fence -- which a limina host
//! attaches to every primary flush, and which only that same worker can signal. The worker
//! never woke: plymouth, gdm and every later login queued behind the fb-helper lock, and the
//! guest never reached a desktop (four boots in four on macOS 27, where the console's fence was
//! still outstanding when the re-announce landed). Upstream took the hotplug off the worker in
//! `d1b894c5bbb3` (7.1); the module carries it as a backport for older series.
//!
//! The capture-sink boots of the other metal L2s never get the re-announce, so they pass either
//! way: this one needs the window.
//!
//! Prereqs as the other metal L2s: the `metal.test` image with the vendor drivers and KosmicKrisp;
//! SKIPs cleanly if missing. Opens a real window. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn metal_guest_in_a_window_reaches_its_desktop() {
    let test = "metal_guest_in_a_window_reaches_its_desktop";
    if !limina_test::require_hvf_or_skip(test) {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED {test}: no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk (mount \
             third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::metal_efi_fedora_from_env() {
        Ok(cfg) => cfg
            .with_windowed_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED {test}: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("no ssh login: a guest wedged on its console's flush fence refuses every session");
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared: gdm is stuck behind the console");

    // Plymouth hands the display to gdm and quits; a wedged guest leaves it blocked in the
    // kernel on the fb-helper lock, and nothing else in uninterruptible sleep for long. A task
    // passing through D (gnome-shell faulting in pages as the desktop starts, on either machine)
    // is not a wedge, so only a task blocked in every one of five samples a second apart counts.
    guest
        .ssh_poll(
            "! systemctl is-active -q plymouth-quit-wait.service",
            Duration::from_secs(60),
        )
        .expect("plymouth never quit");
    let blocked = guest
        .ssh_exec(
            "for i in 1 2 3 4 5; do ps -eo pid=,stat=,comm= | awk '$2 ~ /^D/ {print $1, $3}'; \
             sleep 1; done | sort | uniq -c | awk '$1 == 5 {print $3}'",
        )
        .expect("ssh died");
    assert!(
        blocked.trim().is_empty(),
        "tasks blocked in the kernel after the desktop came up: {blocked}"
    );

    guest
        .shutdown(Duration::from_secs(30))
        .expect("guest shutdown");
}
