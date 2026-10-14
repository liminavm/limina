// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 guard: a desktop on the Metal transport is held off the buffers it has on glass.
//!
//! A compositor on the vendor drivers scans out transport surfaces, rendered out of this process.
//! Unless the guest fences those flushes and the host holds the fence until it is done with the
//! buffer -- the worker's copy of it taken, or with copies off the frame off glass -- the
//! compositor draws its next frame into a buffer still being read. The worker says which it is,
//! per scanout, as it changes: `flushes are fenced; the guest is held until the host is done with
//! each buffer` against `flushes carry no fence`.
//!
//! `LIMINA_FENCE_PRESENT=1` is forced, as in `venus_fence_present`: the harness display is the
//! ack-less capture sink, where the default leaves the chain off.
//!
//! Prereqs as the other metal L2s: the `metal.test` image with the vendor drivers and KosmicKrisp;
//! SKIPs cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const HELD: &str = "flushes are fenced; the guest is held until the host is done with each buffer";

#[test]
fn metal_desktop_is_held_off_buffers_on_glass() {
    let test = "metal_desktop_is_held_off_buffers_on_glass";
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
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log()
            .with_env("LIMINA_FENCE_PRESENT", "1"),
        Err(e) => {
            eprintln!("SKIPPED {test}: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared");

    let stack = limina_test::metal::Stack::probe(&guest).expect("probing the guest's GPU stack");
    assert!(
        stack.extension && stack.shell_on_vendor_drivers(),
        "gnome-shell is not on the Metal transport, so this would not test its scanout: {stack:#?}"
    );

    guest
        .wait_for_supervisor_log(HELD, Duration::from_secs(120))
        .unwrap_or_else(|e| {
            let log = guest.supervisor_log();
            let said: Vec<_> = log.lines().filter(|l| l.contains("flushes")).collect();
            panic!(
                "the transport desktop's scanout was never held ({e}); the worker said:\n{}",
                said.join("\n")
            )
        });

    // A hold the host never completes starves the scanout pipeline within its 500 ms ceiling;
    // a desktop still up and answering after frames flowed is the check.
    std::thread::sleep(Duration::from_secs(10));
    let shell = guest
        .ssh_exec("pgrep -x gnome-shell >/dev/null && echo ALIVE")
        .expect("ssh died after held frames flowed");
    assert_eq!(
        shell.trim(),
        "ALIVE",
        "gnome-shell exited under held presents"
    );

    let log = guest.supervisor_log();
    assert!(
        !log.contains("present fence injection failed"),
        "a transport surface's present was still sent to the renderer to fence"
    );
    // The last word on each scanout is the one that stands.
    let last = log
        .lines()
        .rfind(|l| l.contains("virtio-gpu: scanout 0 flushes"));
    assert!(
        last.is_some_and(|l| l.contains(HELD)),
        "scanout 0 stopped being held: {last:?}"
    );

    guest
        .shutdown(Duration::from_secs(30))
        .expect("guest shutdown");
}
