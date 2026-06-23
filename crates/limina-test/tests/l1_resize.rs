// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 runtime-resize test: prove the host→guest display-resize mechanism end to end without a
//! display server. Boot the tiny guest with a virtio-gpu display *and* the serial console shell,
//! read the virtio-gpu connector's advertised modes from sysfs, push a runtime resize from the
//! host (`Guest::resize_display`), and assert the new — deliberately non-standard — resolution
//! appears in the connector's `modes`.
//!
//! Why this is the right oracle (see docs/design/runtime-display-resize.md): a window resize
//! updates the libkrun virtio-gpu `DisplayInfo` and raises a CONFIG-CHANGE interrupt; the guest
//! virtio-gpu DRM driver re-reads `GET_DISPLAY_INFO` and re-probes the connector, which updates
//! `/sys/class/drm/<conn>/modes` — no compositor needed. A non-standard size (900x650 is not in
//! virtio-gpu's built-in common-modes list) proves the *host* preferred mode propagated, rather
//! than a mode that would have been advertised anyway.
//!
//! Build the guest first: `scripts/build-test-guest.sh` (needs the DRM virtio-gpu kernel).
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

/// Initial advertised mode (matches the virtio-gpu's common-modes list, so it's present from boot).
const INIT_W: u32 = 1024;
const INIT_H: u32 = 768;
/// Target resize: deliberately NOT a standard mode, so its appearance proves the host preferred
/// mode propagated through the config-change → connector re-probe path.
const NEW_W: u32 = 900;
const NEW_H: u32 = 650;

#[test]
fn l1_runtime_resize_updates_connector_modes() {
    if !limina_test::require_hvf_or_skip("l1_runtime_resize_updates_connector_modes") {
        return;
    }

    // A display (virtio-gpu present → /sys/class/drm connector) + the serial console shell
    // (`limina.console_shell`) so we can `cat` sysfs before/after the resize while the guest
    // stays alive. software-2D keeps it deterministic and venus/Metal-independent — the
    // config-change mechanism lives in the device layer and works in either GPU mode.
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_display(INIT_W, INIT_H)
        .with_serial_input()
        .append_cmdline("limina.console_shell");
    eprintln!(
        "booting L1 guest (display + console shell) for runtime resize: {cfg:?}",
        cfg = cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("LIMINA_SHELL_READY", Duration::from_secs(20))
        .expect("guest did not enter console command mode over ttyAMA0");

    // Discover the virtio-gpu DRM connector (its sysfs name is kernel-version dependent).
    let connector = find_connector(&mut guest);
    let modes_path = format!("/sys/class/drm/{connector}/modes");
    eprintln!("virtio-gpu connector: {connector} → {modes_path}");

    // The virtio-gpu connector must advertise the boot mode to start with.
    let initial = read_modes(&mut guest, &modes_path);
    eprintln!("initial {modes_path}:\n{initial}");
    assert!(
        initial.contains(&format!("{INIT_W}x{INIT_H}")),
        "connector modes don't advertise the boot mode {INIT_W}x{INIT_H}: {initial:?}"
    );
    assert!(
        !initial.contains(&format!("{NEW_W}x{NEW_H}")),
        "the target mode {NEW_W}x{NEW_H} is already advertised before the resize — pick another"
    );

    // Push a runtime resize from the host (as the window-resize gesture would).
    guest
        .resize_display(NEW_W, NEW_H)
        .expect("sending the runtime resize");

    // Poll the connector modes until the new, non-standard resolution appears (config-change →
    // GET_DISPLAY_INFO re-read → connector re-probe). This needs no userspace modeset.
    let want = format!("{NEW_W}x{NEW_H}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last;
    let appeared = loop {
        last = read_modes(&mut guest, &modes_path);
        if last.contains(&want) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    eprintln!("post-resize {modes_path}:\n{last}");
    assert!(
        appeared,
        "the resized mode {want} never appeared in the connector modes after a runtime resize \
         (config-change not delivered, or the guest didn't re-probe); last saw:\n{last}"
    );

    // Clean power-off.
    guest.console_send("exit").expect("sending exit");
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(
        !outcome.forced,
        "harness had to force teardown: {outcome:?}"
    );
}

/// `cat` the virtio-gpu connector's modes file over the console shell.
fn read_modes(guest: &mut Guest, modes_path: &str) -> String {
    guest
        .console_command(&format!("cat {modes_path}"), Duration::from_secs(10))
        .expect("reading the connector modes over the console")
}

/// Discover the virtio-gpu DRM connector under `/sys/class/drm` (e.g. `card0-Virtio-0`). Its
/// exact name is kernel-version dependent, so we enumerate rather than hardcode. Connector
/// entries contain a `-` (`card<N>-<NAME>-<M>`); cards/render/control nodes don't. Prefer a
/// Virtio connector; panic with the full listing if none is found.
fn find_connector(guest: &mut Guest) -> String {
    let listing = guest
        .console_command("ls /sys/class/drm", Duration::from_secs(10))
        .expect("listing /sys/class/drm over the console");
    let connectors: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|e| e.contains('-') && !e.starts_with("render") && !e.starts_with("control"))
        .collect();
    connectors
        .iter()
        .find(|e| e.contains("Virtio"))
        .or_else(|| connectors.first())
        .unwrap_or_else(|| {
            panic!("no DRM connector found under /sys/class/drm; listing was:\n{listing}")
        })
        .to_string()
}
