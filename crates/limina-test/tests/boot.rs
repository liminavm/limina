// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 stock-baseline boot tests: the Fedora image must boot through limina via EFI.
//!
//! These drive the real `limina` supervisor → `limina-vmm` worker → libkrun/HVF chain. Three tiers:
//!
//! - [`fedora_stock_image_boots_to_bootloader`] — disk **read-only** (never mutated), asserts
//!   the guest reaches its **bootloader** (EDK2 firmware banner + GRUB) on the silent firmware's
//!   serial console: limina boots the firmware, the firmware reads the virtio-blk disk, finds the
//!   ESP, and runs the distro bootloader.
//! - [`fedora_stock_image_efi_boots_to_userspace`] — silent firmware, writable COW clone; asserts
//!   the full chain on **serial** (firmware → GRUB → kernel → getty `login:`) plus **sshd**,
//!   guarding against the SELinux autorelabel reboot loop. Needs an image prepared by
//!   `scripts/prepare-efi-image.sh` (relabel + `console=ttyAMA0` on the GRUB args).
//! - [`fedora_stock_image_efi_renders_to_gop`] — our **GOP firmware**; asserts the boot is
//!   **visually** present in the captured window (firmware/GRUB/kernel render to the scanout),
//!   the complement to the serial check above.
//! - [`fedora_stock_image_renders_graphical_desktop`] — the two-tier *floor*: the stock GNOME
//!   **desktop session** must actually come up and paint, not just serial+sshd. Boots the default
//!   GPU (venus advertised → graceful software fallback) and asserts gnome-shell started + rendered
//!   a rich frame.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

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
/// came up. The fix is a one-time **permissive relabel** of the image (`scripts/prepare-efi-image.sh`);
/// this test fails the moment an image relapses into that loop, because sshd never answers and
/// limina tears down on the reboot before the banner arrives.
///
/// Unlike [`fedora_stock_image_boots_to_bootloader`] (read-only, stops at GRUB), this boots a
/// writable COW clone (forced by `with_net`) so the shared `.test` fixture is never mutated,
/// and asserts the full chain on serial: firmware → GRUB → stock kernel → getty `login:`. The
/// cmdline is GRUB-owned on this path, so the kernel only reaches serial because the image was
/// prepared with `console=ttyAMA0` on the GRUB args (`scripts/prepare-efi-image.sh`); the getty
/// `login:` prompt that follows is also what proves a serial console is usable on the EFI path.
/// sshd answering is the independent oracle that userspace genuinely settled.
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
         (scripts/prepare-efi-image.sh); enforcing boots will reboot-loop"
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

    // Serial console works on the EFI path: the kernel printed to ttyAMA0 (only possible
    // because the image carries `console=ttyAMA0`) and systemd spawned a getty whose `login:`
    // prompt reached the same serial we capture. This is the getty-on-serial guarantee.
    guest
        .wait_for("login:", Duration::from_secs(30))
        .expect("no getty `login:` on serial — console=ttyAMA0 missing from the GRUB args?");

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}

/// The stock-baseline EFI boot must be **visually** present in the window, not just on serial.
///
/// Complements [`fedora_stock_image_efi_boots_to_userspace`] (which proves the *serial* chain):
/// this boots our **GOP firmware** (VirtioGpuDxe), which drives firmware → GRUB → kernel onto the
/// virtio-gpu scanout, and asserts the captured window actually shows rendered **boot-console**
/// content — specifically *before* userspace settles. We sample the captured scanout throughout
/// boot and keep the richest frame seen before the serial getty `login:` prompt; a rich frame in
/// that window can only be the firmware splash / GRUB menu / kernel fbcon, not the steady-state
/// GDM greeter (which comes up with or after the getty). That's the tightening over the old
/// "rich content *eventually*" check, which GDM alone satisfied. The oracle is robust against
/// transient blanks (we keep the best frame), and a blank/single-color screen never crosses the
/// many-distinct-colors / no-dominant-color bar. This guards the GOP windowed-boot path that was
/// historically wedged (it used to freeze on the firmware logo behind the SELinux reboot loop).
#[test]
fn fedora_stock_image_efi_renders_to_gop() {
    if !limina_test::require_hvf_or_skip("fedora_stock_image_efi_renders_to_gop") {
        return;
    }

    let cfg = match GuestConfig::fedora_gop_from_env() {
        Ok(cfg) => cfg,
        // No GOP firmware on this machine — skip rather than fail (it's a built artifact).
        Err(e) => {
            eprintln!("SKIP fedora_stock_image_efi_renders_to_gop: {e:#}");
            return;
        }
    };
    eprintln!("booting Fedora via the GOP firmware: {:?}", cfg.boot);

    let guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Sample the captured scanout throughout boot and keep the richest frame seen BEFORE
    // userspace settles — the serial getty `login:` prompt (the image carries console=ttyAMA0,
    // so the kernel + getty reach the same serial we capture via --console). A rich frame in
    // that window can only be the boot console itself (firmware splash / GRUB menu / kernel
    // fbcon), not the GDM greeter that comes up with or after the getty. Real boot content
    // yields many distinct colors and no single dominant color; a blank/uniform screen yields
    // 1 color at ~100% dominance.
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let mut best_colors = 0usize;
    let mut best_dominance = 1.0f64;
    let mut reached_userspace = false;
    while std::time::Instant::now() < deadline {
        if let Ok(frame) = guest.read_capture() {
            let colors = frame.distinct_colors();
            let (_, dominance) = frame.dominant_color();
            if colors > best_colors {
                best_colors = colors;
                best_dominance = dominance;
            }
        }
        // Stop the moment userspace's getty prints `login:` — everything sampled so far is
        // strictly the pre-userspace boot console.
        if guest.console().contains("login:") {
            reached_userspace = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!(
        "richest pre-userspace GOP frame: {best_colors} distinct colors, dominant {best_dominance:.2}"
    );
    assert!(
        reached_userspace,
        "guest never reached the serial getty `login:` within 240s — boot stalled, or \
         console=ttyAMA0 missing from the GRUB args (scripts/prepare-efi-image.sh)?"
    );
    assert!(
        best_colors >= 8 && best_dominance < 0.99,
        "the GOP boot console never rendered before userspace (richest pre-login frame: \
         {best_colors} colors, {best_dominance:.2} dominant) — did firmware/GRUB/kernel \
         paint the scanout?"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}

/// The stock-baseline **GNOME desktop session** must come up and render — the two-tier "stock
/// baseline usable" floor that the serial/sshd tests don't cover.
///
/// Closes the gap behind the venus black-window investigation: the other boot tests prove serial
/// getty + sshd ([`fedora_stock_image_efi_boots_to_userspace`]) and that the *boot console* paints
/// ([`fedora_stock_image_efi_renders_to_gop`]) — but neither proves the *graphical session*
/// initializes and renders. This boots the **default GPU** (the coexist device, i.e. venus
/// advertised — exactly what a stock user gets): a stock 4 KiB guest can't map venus host-visible
/// blobs (the `hv_vm_map` 16k/4k wall), so Mesa degrades to `kms_swrast`/llvmpipe on its own and
/// the desktop renders in software. So this also guards that graceful fallback end-to-end.
///
/// Two oracles, both required: (1) gnome-shell logged `GNOME Shell started` (deterministic, over
/// SSH — true for the autologin session or a GDM greeter), and (2) the captured scanout shows a
/// rich frame *after* that point (so it's the desktop, not the boot console). A software-GL or
/// scanout regression that left the session black would start but never paint → (2) fails; a
/// session that never initialized → (1) fails.
#[test]
fn fedora_stock_image_renders_graphical_desktop() {
    if !limina_test::require_hvf_or_skip("fedora_stock_image_renders_graphical_desktop") {
        return;
    }

    // Default GPU (coexist / venus advertised) + GOP firmware + captured display + NAT. We override
    // the GOP config's software-2D capture device with the coexist one so this exercises the real
    // stock default (venus→kms_swrast graceful fallback), not the forced-software path. The coexist
    // device's vrend half brings up host GL via zink-on-KK, so it needs the Mesa/KK `DYLD_*` env —
    // wire it explicitly (`with_virgl_host_gl`) rather than depending on an ambient export, so the
    // test passes under the bare `test-boot.sh` runner too. It SKIPs if the KK prefix is absent.
    let cfg = match GuestConfig::fedora_gop_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_virgl_host_gl(),
        Err(e) => {
            eprintln!("SKIP fedora_stock_image_renders_graphical_desktop: {e:#}");
            return;
        }
    };
    eprintln!(
        "booting the Fedora desktop via GOP, default coexist GPU: {:?}",
        cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // sshd comes up first; the autologin graphical session follows.
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest never reached sshd over the EFI path");
    assert!(
        banner.starts_with("SSH-"),
        "unexpected SSH banner: {banner}"
    );

    // Oracle 1 — the graphical session initialized. gnome-shell logs this line once it is up
    // (autologin user session or GDM greeter alike); `grep -q` exits 0 only once it appears, so
    // ssh_poll returns as soon as the session starts and fails if it never does.
    guest
        .ssh_poll(
            "sudo journalctl -b _COMM=gnome-shell --no-pager 2>/dev/null \
             | grep -q 'GNOME Shell started'",
            Duration::from_secs(180),
        )
        .expect(
            "gnome-shell never logged 'GNOME Shell started' — the stock graphical session failed \
             to initialize (venus→kms_swrast fallback broken, or no software GL?)",
        );

    // Oracle 2 — it actually painted. Sample the scanout AFTER the session started and keep the
    // richest frame. A real desktop (wallpaper) yields thousands of distinct colors with no single
    // dominant color; a black/stuck session yields ~1 color at ~100% dominance. The capture is
    // sparse for a static screen, so poll with a generous window and keep the best frame seen.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut best_colors = 0usize;
    let mut best_dominance = 1.0f64;
    while Instant::now() < deadline {
        if let Ok(frame) = guest.read_capture() {
            let colors = frame.distinct_colors();
            let (_, dominance) = frame.dominant_color();
            if colors > best_colors {
                best_colors = colors;
                best_dominance = dominance;
            }
            if best_colors >= 1000 && best_dominance < 0.90 {
                break; // unambiguously a rendered desktop — stop early
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("richest desktop frame: {best_colors} distinct colors, dominant {best_dominance:.2}");
    assert!(
        best_colors >= 1000 && best_dominance < 0.90,
        "the GNOME desktop never rendered a rich frame after the session started (richest: \
         {best_colors} colors, {best_dominance:.2} dominant) — software GL / scanout regression?"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}

/// The two-tier **compatibility floor**: a stock guest on the **forced software-2D** GPU
/// (`fedora_gop_from_env`'s default capture device — no venus/virgl 3D advertised at all) must still
/// bring up a usable GNOME desktop in llvmpipe. This is strictly stronger than
/// [`fedora_stock_image_renders_graphical_desktop`], which uses the coexist device and relies on the
/// venus→`kms_swrast` *fallback*: here there is no 3D device to fall back from, so it exercises the
/// pure-CPU path that must work even on a GL-less host. No KosmicKrisp / host GL needed. This is the
/// guarantee that "missing limina enhancements degrade gracefully; they don't break the VM" — if it
/// regresses, the baseline tier itself is broken.
#[test]
fn fedora_stock_image_software_2d_floor_renders_desktop() {
    if !limina_test::require_hvf_or_skip("fedora_stock_image_software_2d_floor_renders_desktop") {
        return;
    }

    // fedora_gop_from_env() already gives the software-2D capture device + GOP firmware + NAT —
    // exactly the floor. Do NOT add a coexist/venus device (that would be the other test's path).
    let cfg = match GuestConfig::fedora_gop_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIP fedora_stock_image_software_2d_floor_renders_desktop: {e:#}");
            return;
        }
    };
    eprintln!(
        "booting the stock Fedora desktop on the SOFTWARE-2D floor (no 3D device): {:?}",
        cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest never reached sshd over the EFI path");

    // Oracle 1 — the graphical session initialized (autologin session or GDM greeter).
    guest
        .ssh_poll(
            "sudo journalctl -b _COMM=gnome-shell --no-pager 2>/dev/null \
             | grep -q 'GNOME Shell started'",
            Duration::from_secs(180),
        )
        .expect(
            "gnome-shell never started on the software-2D floor — software GL (llvmpipe) is broken, \
             which breaks the two-tier baseline guarantee",
        );

    // Oracle 2 — it actually painted a rich desktop frame (same oracle as the coexist render test).
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut best_colors = 0usize;
    let mut best_dominance = 1.0f64;
    while Instant::now() < deadline {
        if let Ok(frame) = guest.read_capture() {
            let colors = frame.distinct_colors();
            let (_, dominance) = frame.dominant_color();
            if colors > best_colors {
                best_colors = colors;
                best_dominance = dominance;
            }
            if best_colors >= 1000 && best_dominance < 0.90 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!(
        "richest software-2D desktop frame: {best_colors} distinct colors, dominant {best_dominance:.2}"
    );
    assert!(
        best_colors >= 1000 && best_dominance < 0.90,
        "the GNOME desktop never rendered on the software-2D floor (richest: {best_colors} colors, \
         {best_dominance:.2} dominant) — an llvmpipe/scanout regression breaks the baseline tier"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
