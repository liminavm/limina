// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2: with nested virtualization on, the firmware and GRUB still take keys (issue #16).
//!
//! With `--nested-virt` the firmware runs at EL2, where Hypervisor.framework never fires the EL1
//! virtual timer. A firmware that drives it gets no timer interrupts, so no UEFI timer event ever
//! fires, and every console input driver (virtio keyboard, USB keyboard, serial terminal) polls
//! from one: the firmware and GRUB ignore every key while GRUB's countdown, which reads the
//! counter, still boots. The fix is the edk2 fork's `ArmGenericTimerVirtPhyCounterLib` (the
//! physical timer at EL2); `spikes/nested-virt-probe/RESULTS.md`, *Timer interrupts at EL2*,
//! has the measurements.
//!
//! The vehicle is an installer ISO, whose GRUB shows its menu on the serial console: once the
//! menu is up, `c` over serial must open GRUB's command line (`grub>`). Serial stands in for
//! the keyboard because it dies the same way; the issue was reported on the window keyboard.
//!
//! HONESTY NOTE: this can only fail on a Mac that offers EL2 (M3 or later). Elsewhere the
//! worker refuses `--nested-virt`, so the test SKIPs, which is what it does on an M1/M2 suite
//! host.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`. Overrides: `LIMINA_TEST_ISO`
//! (any EFI-bootable aarch64 installer ISO whose GRUB prints its menu on serial),
//! `LIMINA_FIRMWARE`.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_config_get_el2_supported(el2_supported: *mut bool) -> i32;
}

fn host_offers_el2() -> bool {
    let mut supported = false;
    // SAFETY: the out-pointer is a live bool for the duration of the call.
    let rc = unsafe { hv_vm_config_get_el2_supported(&mut supported) };
    rc == 0 && supported
}

#[test]
fn nested_virt_grub_takes_keys() {
    const NAME: &str = "nested_virt_grub_takes_keys";
    if !limina_test::require_hvf_or_skip(NAME) {
        return;
    }
    if !host_offers_el2() {
        eprintln!("SKIP {NAME}: this Mac offers no EL2 (nested virtualization needs M3 or later)");
        return;
    }
    let cfg = match GuestConfig::iso_boot_from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP {NAME}: {e}");
            return;
        }
    };
    let cfg = cfg.with_serial_input().with_supervisor_arg("--nested-virt");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor on the ISO");
    guest
        .wait_for("GNU GRUB", Duration::from_secs(90))
        .expect("the firmware did not reach the ISO's GRUB menu");

    // Keep pressing `c` until GRUB opens its command line. A firmware without timer events never
    // hands GRUB a key, and its menu counts down (or waits) with the keys unread.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut opened = false;
    while Instant::now() < deadline {
        guest
            .console_write(b"c")
            .expect("writing to the serial console");
        if guest.console().contains("grub>") {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        opened,
        "GRUB ignored `c` over serial for 20 s with nested virtualization on: the firmware is \
         getting no timer interrupts at EL2, so no console input reaches GRUB (issue #16). Is the \
         firmware built with ArmGenericTimerVirtPhyCounterLib?"
    );
    eprintln!("nested virtualization on: GRUB took a key from the serial console ✓");

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
