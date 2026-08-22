// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 EDID test: prove the guest sees a *stable identity* across mode changes, that a pushed
//! identity actually lands, and that a pushed disconnect really disconnects the connector — all
//! without a display server.
//!
//! Why this is the right oracle: the guest kernel re-reads `GET_EDID` (alongside
//! `GET_DISPLAY_INFO`) whenever the device raises a config-change and feeds the result through
//! `drm_edid_connector_update`, so `/sys/class/drm/<conn>/edid` is the guest's own view of what
//! we advertised — no compositor needed. And `virtio_gpu_conn_detect` reports connector status
//! straight from the scanout's `enabled` flag, so `/sys/class/drm/<conn>/status` is the honest
//! oracle for a hotplug. See `docs/design/stable-edid-hotplug.md`.
//!
//! Build the guest first: `scripts/build-test-guest.sh`.
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{DisplayControl, EdidSpec, Guest, GuestConfig, RangeSpec};

const INIT_W: u32 = 1024;
const INIT_H: u32 = 768;

/// EDID byte offsets we assert on (EDID 1.4 §3.4).
const MANUFACTURER: std::ops::Range<usize> = 8..10;
const IDENTITY_BLOCK: std::ops::Range<usize> = 8..18;
const DESCRIPTOR_0: std::ops::Range<usize> = 54..72;

#[test]
fn l1_edid_identity_is_stable_and_pushable() {
    if !limina_test::require_hvf_or_skip("l1_edid_identity_is_stable_and_pushable") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_display(INIT_W, INIT_H)
        .with_serial_input()
        .append_cmdline("limina.console_shell");
    eprintln!(
        "booting L1 guest for the EDID test: {cfg:?}",
        cfg = cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("LIMINA_SHELL_READY", Duration::from_secs(20))
        .expect("guest did not enter console command mode over ttyAMA0");

    let connector = find_connector(&mut guest);
    let edid_path = format!("/sys/class/drm/{connector}/edid");
    let status_path = format!("/sys/class/drm/{connector}/status");
    eprintln!("virtio-gpu connector: {connector}");

    // ---- 1. The boot EDID is well-formed and carries the historical anonymous identity.
    let boot_edid = read_edid(&mut guest, &edid_path);
    assert!(
        boot_edid.len() >= 128,
        "expected at least one 128-byte EDID block, got {} bytes",
        boot_edid.len()
    );
    assert_eq!(
        checksum(&boot_edid[..128]),
        0,
        "the guest accepted an EDID whose checksum is wrong"
    );

    // ---- 2. A resize must NOT disturb the identity. This is the regression this test exists
    // for: if a mode change rewrote vendor/product/serial, a guest compositor would treat every
    // window resize as a different monitor and lose its per-monitor configuration.
    guest
        .resize_display(900, 650)
        .expect("sending the runtime resize");
    let resized = wait_for_edid_change(&mut guest, &edid_path, &boot_edid)
        .expect("the EDID never changed after a resize (config-change not delivered?)");
    assert_ne!(
        resized[DESCRIPTOR_0], boot_edid[DESCRIPTOR_0],
        "the preferred timing should have moved with the resize"
    );
    assert_eq!(
        resized[IDENTITY_BLOCK], boot_edid[IDENTITY_BLOCK],
        "a resize must not touch the identity bytes"
    );

    // ---- 3. A pushed identity lands, and the guest re-reads it.
    let control = DisplayControl {
        display_id: 0,
        position: None,
        size: Some((1280, 800)),
        connected: None,
        edid: Some(EdidSpec {
            refresh_hz: 120,
            dpi: 226,
            vendor: *b"LMN",
            product_id: 0x4242,
            serial: 0x0BAD_F00D,
            name: "L1 Panel".into(),
            serial_string: Some("L1-SERIAL".into()),
            range: Some(RangeSpec {
                min_vertical_hz: 48,
                max_vertical_hz: 120,
                min_horizontal_khz: 41,
                max_horizontal_khz: 103,
                max_pixel_clock_mhz: 200,
            }),
            modes: Vec::new(),
            alt_mode: Some((1280, 800, 60)),
        }),
    };
    guest
        .update_display(control)
        .expect("sending the display identity update");

    let identified = wait_for_edid_change(&mut guest, &edid_path, &resized)
        .expect("the EDID never changed after an identity push");
    assert_eq!(
        checksum(&identified[..128]),
        0,
        "the pushed EDID has a bad checksum"
    );
    // 'LMN' → five bits per letter, big-endian.
    let expected_vendor = (((12u16) << 10) | ((13u16) << 5) | 14u16).to_be_bytes();
    assert_eq!(
        &identified[MANUFACTURER], &expected_vendor,
        "the pushed manufacturer id did not reach the guest"
    );
    assert_eq!(
        &identified[10..12],
        &0x4242u16.to_le_bytes(),
        "the pushed product code did not reach the guest"
    );
    assert_eq!(
        &identified[12..16],
        &0x0BAD_F00Du32.to_le_bytes(),
        "the pushed serial did not reach the guest"
    );
    let text = String::from_utf8_lossy(&identified[..128]);
    assert!(
        text.contains("L1 Panel"),
        "the pushed product name is not in the EDID the guest read back"
    );
    // A base EDID block holds exactly four descriptors, and this push wants five: preferred
    // timing, product name, range limits, the alternate timing, and the serial string. The
    // serial string is last in the priority order, so it is the one that doesn't fit — the
    // numeric serial above already carries the identity. (This is the configuration a real
    // ProMotion panel produces, so it is worth pinning rather than dodging.)
    assert!(
        !text.contains("L1-SERIAL"),
        "the serial string should have been dropped for want of a descriptor slot"
    );
    // The range descriptor DID fit, and it is what a guest reads a refresh range from.
    assert!(
        identified[54..126]
            .chunks(18)
            .any(|block| block[0..2] == [0, 0] && block[3] == 0xFD),
        "the pushed monitor range-limits descriptor never reached the guest"
    );

    // ---- 3b. A mode the base block cannot express arrives with a DisplayID extension.
    // 3024x1964 @ 120 Hz needs ~866 MHz of pixel clock; a base detailed timing tops out at
    // 655.35 MHz, so the honest timing rides a DisplayID 2.0 type VII block instead.
    guest
        .update_display(DisplayControl {
            display_id: 0,
            position: None,
            size: Some((3024, 1964)),
            connected: None,
            edid: Some(EdidSpec {
                refresh_hz: 120,
                dpi: 254,
                vendor: *b"LMN",
                product_id: 0x4242,
                serial: 0x0BAD_F00D,
                name: "L1 Panel".into(),
                serial_string: None,
                range: None,
                modes: Vec::new(),
                alt_mode: None,
            }),
        })
        .expect("sending the high-clock mode");

    let hidpi = wait_for_edid_change(&mut guest, &edid_path, &identified)
        .expect("the EDID never changed after the high-clock push");
    assert_eq!(
        hidpi.len(),
        256,
        "an over-ceiling mode must arrive with an extension block"
    );
    assert_eq!(hidpi[126], 1, "the base block must declare one extension");
    assert_eq!(checksum(&hidpi[..128]), 0, "base block checksum");
    assert_eq!(hidpi[128], 0x70, "the extension is a DisplayID structure");
    // The DisplayID structure carries its own checksum, over the 4-byte header, the blocks and
    // itself, starting at extension byte 1 — the EDID extension checksum at byte 127 is
    // explicitly not part of it. This is the exact sum `validate_displayid` computes, so a
    // guest kernel that reads the blob at all reads it the same way.
    let bytes = hidpi[130] as usize;
    let structure = &hidpi[129..129 + 4 + bytes + 1];
    assert_eq!(
        checksum(structure),
        0,
        "the DisplayID structure checksum is invalid; the kernel would reject the whole block"
    );
    assert_eq!(hidpi[133], 0x22, "type VII detailed timing block");
    assert_eq!(
        hidpi[135] as usize % 20,
        0,
        "a type VII block whose length isn't a multiple of 20 is dropped whole"
    );
    // The kernel offers the resolution: the size reached it through GET_DISPLAY_INFO and the
    // EDID alike. (sysfs `modes` carries no refresh rate, so the honest 120 Hz is pinned by the
    // generator's unit tests, which decode the block the way `drm_edid.c` does.)
    // NOT asserted here: that the guest builds a *mode* from the extension. This L1 vehicle
    // does not surface large detailed timings in `<connector>/modes` at all — a 2560x1440 @
    // 60 Hz push, comfortably inside the base block's clock ceiling and touching none of this
    // code, collapses the list the same way, so it is a property of the minimal guest and not
    // of the extension. What this test does prove is that the block reaches the guest byte for
    // byte and satisfies every framing rule the kernel checks before it will read a timing;
    // that those bytes decode to 3024x1964 @ 120 Hz is pinned by the generator's unit tests,
    // which mirror `drm_mode_displayid_detailed`. The end-to-end mode selection is verified on
    // a real desktop guest — see docs/design/stable-edid-hotplug.md.

    // ---- 4. A pushed disconnect really disconnects the connector, and reconnecting restores
    // it. This is the mechanism a genuine display unplug will ride on.
    assert_eq!(
        read_trimmed(&mut guest, &status_path),
        "connected",
        "the connector should start out connected"
    );
    guest
        .update_display(DisplayControl {
            display_id: 0,
            position: None,
            connected: Some(false),
            ..Default::default()
        })
        .expect("sending the disconnect");
    assert!(
        wait_for_status(&mut guest, &status_path, "disconnected"),
        "the connector never went disconnected after a pushed unplug"
    );

    guest
        .update_display(DisplayControl {
            display_id: 0,
            position: None,
            connected: Some(true),
            ..Default::default()
        })
        .expect("sending the reconnect");
    assert!(
        wait_for_status(&mut guest, &status_path, "connected"),
        "the connector never came back after a pushed re-plug"
    );

    guest.console_send("exit").expect("sending exit");
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(
        !outcome.forced,
        "harness had to force teardown: {outcome:?}"
    );
}

/// Read the connector's EDID as bytes. `cat` would mangle it (the blob is binary and the
/// console protocol is line-delimited), so the guest hexdumps it for us.
fn read_edid(guest: &mut Guest, path: &str) -> Vec<u8> {
    let output = guest
        .console_command(&format!("xxd {path}"), Duration::from_secs(10))
        .expect("reading the connector EDID over the console");

    // The console is shared with the kernel's own log, and an async message ("usb 1-1: new
    // full-speed USB device…") can land in the middle of our output. Filtering the whole thing
    // for hex digits folds that message's letters into the stream and silently shifts every
    // byte — which reads as a corrupt EDID rather than as noise. Take the longest pure-hex
    // token instead, and prove it really is an EDID by its header.
    let hex = output
        .split_whitespace()
        .filter(|token| {
            token.len() >= 32
                && token.len() % 2 == 0
                && token.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .max_by_key(|token| token.len())
        .unwrap_or_else(|| {
            panic!(
                "no EDID hex found in the console output (is VIRTIO_GPU_F_EDID negotiated?); \
                 output was:\n{output}"
            )
        });

    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex is ascii"), 16)
                .expect("valid hex")
        })
        .collect();
    assert_eq!(
        &bytes[..8],
        &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
        "what we parsed is not an EDID block (console noise?): {hex}"
    );
    bytes
}

fn read_trimmed(guest: &mut Guest, path: &str) -> String {
    guest
        .console_command(&format!("cat {path}"), Duration::from_secs(10))
        .expect("reading a sysfs attribute over the console")
        .trim()
        .to_string()
}

/// Poll until the EDID differs from `previous` — the guest re-reads it asynchronously, on the
/// config-change work queue.
fn wait_for_edid_change(guest: &mut Guest, path: &str, previous: &[u8]) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let current = read_edid(guest, path);
        if current != previous {
            return Some(current);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn wait_for_status(guest: &mut Guest, path: &str, want: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let current = read_trimmed(guest, path);
        if current == want {
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!("connector status stuck at {current:?}, wanted {want:?}");
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Discover the virtio-gpu DRM connector (its sysfs name is kernel-version dependent).
/// Mirrors `l1_resize.rs`.
fn find_connector(guest: &mut Guest) -> String {
    let listing = guest
        .console_command("ls /sys/class/drm", Duration::from_secs(10))
        .expect("listing /sys/class/drm over the console");
    // Connectors are always `cardN-<type>-<n>`. Filtering on that shape (not just "has a
    // dash") keeps a kernel printk that interleaves with the ls output on ttyAMA0 (e.g.
    // "[    4.2] input: gpio-keys...") from being mistaken for a connector name — that
    // produced `xxd /sys/class/drm/[ ...` and a baffling one-in-many-suites flake.
    let connectors: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|e| e.starts_with("card") && e.contains('-'))
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

fn checksum(block: &[u8]) -> u8 {
    block.iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}
