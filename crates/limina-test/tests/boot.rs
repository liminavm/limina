// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 stock-baseline boot tests: the Fedora image must boot through limina via EFI.
//!
//! These drive the real `limina` supervisor → `limina-vmm` worker → libkrun/HVF chain. Two tiers:
//!
//! - [`fedora_stock_image_boots_to_bootloader`] — disk **read-only** (never mutated), asserts
//!   the guest reaches its **bootloader** (EDK2 firmware banner + GRUB). On a pristine image
//!   the kernel has no `console=`, so it goes silent on serial after GRUB; reaching GRUB proves
//!   limina boots the firmware, the firmware reads the virtio-blk disk, finds the ESP, and runs
//!   the distro bootloader.
//! - [`fedora_stock_image_efi_boots_to_userspace`] — boots a writable COW clone all the way to
//!   a running **sshd**, guarding against the SELinux autorelabel reboot loop (see that test).
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{assert_console_has, Guest, GuestConfig};

#[test]
fn fedora_stock_image_boots_to_bootloader() {
    if !limina_test::require_hvf_or_skip("fedora_stock_image_boots_to_bootloader") {
        return;
    }

    let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
    eprintln!(
        "booting Fedora (read-only) via {:?}: {:?}",
        cfg.limina_bin, cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Firmware comes up first — this alone proves limina created the VM and the guest is
    // executing on HVF with our serial wired as the EDK2 console.
    guest
        .wait_for("UEFI firmware", Duration::from_secs(30))
        .expect("guest did not reach EFI firmware");

    // GRUB proves the firmware read the disk, found the ESP, and ran the bootloader.
    guest
        .wait_for("GRUB", Duration::from_secs(60))
        .expect("guest did not reach the GRUB bootloader");

    // Sanity-check both markers are present together in the final capture.
    assert_console_has(&guest.console(), &["UEFI firmware", "GRUB"])
        .expect("expected boot markers");

    // Clean teardown. Stock Fedora ignores the GPIO power button, so the supervisor will
    // force-kill the worker after its (short, test-configured) grace — that's expected
    // and still a clean stop. We just require the supervisor itself exits.
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(
        !outcome.forced,
        "harness had to force the supervisor down — supervisor teardown is broken"
    );
}

/// The stock-baseline EFI path must reach a **settled userspace**, not just the bootloader.
///
/// This guards the regression that wedged the EFI boot for a long time: the guest image had
/// `SELINUX=enforcing` + a stale `/.autorelabel` flag while *every* file we ever installed
/// through the SELinux-less custom kernel was unlabeled. Booting the stock Fedora kernel
/// (which *does* enforce) tried to relabel the whole tree under enforcing, got denied
/// mid-relabel, and rebooted — forever — so the VM never reached sshd and the desktop never
/// came up. The fix is a one-time **permissive relabel** of the image (`scripts/relabel-image.sh`);
/// this test fails the moment an image relapses into that loop, because sshd never answers and
/// limina tears down on the reboot before the banner arrives.
///
/// Unlike [`fedora_stock_image_boots_to_bootloader`] (read-only, stops at GRUB), this boots a
/// writable COW clone (forced by `with_net`) so the shared `.test` fixture is never mutated,
/// and asserts the full chain: firmware → GRUB → stock kernel → systemd → sshd. The cmdline is
/// GRUB-owned on this path (no `console=`), so the kernel goes silent on serial after GRUB —
/// sshd answering is the oracle that userspace was actually reached.
#[test]
fn fedora_stock_image_efi_boots_to_userspace() {
    if !limina_test::require_hvf_or_skip("fedora_stock_image_efi_boots_to_userspace") {
        return;
    }

    // `with_net` makes the harness boot a writable APFS COW clone of the shared image, so the
    // guest can complete boot (Fedora needs a writable root) without mutating `.test`.
    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net();
    eprintln!(
        "booting Fedora via EFI to userspace ({:?}): {:?}",
        cfg.limina_bin, cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Foundation: firmware → GRUB (same chain the bootloader test proves).
    guest
        .wait_for("UEFI firmware", Duration::from_secs(30))
        .expect("guest did not reach EFI firmware");
    guest
        .wait_for("GRUB", Duration::from_secs(60))
        .expect("guest did not reach the GRUB bootloader");

    // The regression guard: the stock kernel must boot all the way to a running sshd. If the
    // image were back in the SELinux autorelabel reboot loop, the VM would reboot (and limina
    // would tear down) long before sshd starts, so this call fails instead of hanging forever.
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("stock Fedora did not reach sshd over the EFI path (SELinux relabel loop?)");
    eprintln!("guest sshd banner: {banner}");
    assert!(
        banner.starts_with("SSH-"),
        "unexpected SSH banner: {banner}"
    );

    // Userspace is genuinely settled, not mid-relabel: the autorelabel flag is consumed and
    // multi-user.target is active. A looping image would have `/.autorelabel` present.
    let relabel = guest
        .ssh_exec("test -e /.autorelabel && echo PENDING || echo clean")
        .expect("ssh into guest");
    assert_eq!(
        relabel.trim(),
        "clean",
        "/.autorelabel still present — the image needs a one-time permissive relabel \
         (scripts/relabel-image.sh); enforcing boots will reboot-loop"
    );
    let target = guest
        .ssh_poll(
            "systemctl is-active multi-user.target",
            Duration::from_secs(60),
        )
        .expect("multi-user.target did not become active");
    assert_eq!(target.trim(), "active");
    eprintln!(
        "guest SELinux mode: {}",
        guest
            .ssh_exec("getenforce")
            .unwrap_or_else(|_| "unknown".into())
            .trim()
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
