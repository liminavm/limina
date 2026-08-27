// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 multi-display test: prove a **scanout pool** gives the guest several independent
//! connectors — that a slot which booted disconnected can be given its own identity and
//! connected, that each slot's EDID is its own, and that taking one down leaves the others
//! alone.
//!
//! Why a pool at all: `num_scanouts` is virtio-gpu config-space state the driver reads once at
//! probe, so a display cannot be added to a running device. Every display a VM may ever show has
//! to exist from boot as a disconnected scanout. See `spikes/scanout-pool/RESULTS.md`.
//!
//! The oracles are the same two the single-display EDID test uses, and for the same reasons:
//! `/sys/class/drm/<conn>/edid` is the guest kernel's own view of what we advertised (it re-reads
//! `GET_EDID` on every config-change and runs it through `drm_edid_connector_update`), and
//! `/sys/class/drm/<conn>/status` comes straight from the scanout's `enabled` flag via
//! `virtio_gpu_conn_detect`. No display server involved.
//!
//! Build the guest first: `scripts/build-test-guest.sh`.
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{DisplayControl, EdidSpec, Guest, GuestConfig};

const INIT_W: u32 = 1024;
const INIT_H: u32 = 768;
/// Two is enough to prove every property here: one slot booted connected, one booted
/// disconnected, and they have to stay independent. A bigger pool would only repeat it.
const POOL: u32 = 2;

/// EDID byte offsets we assert on (EDID 1.4 §3.4).
const MANUFACTURER: std::ops::Range<usize> = 8..10;
const PRODUCT_CODE: std::ops::Range<usize> = 10..12;
const SERIAL: std::ops::Range<usize> = 12..16;

#[test]
fn l1_a_pool_slot_becomes_its_own_display() {
    if !limina_test::require_hvf_or_skip("l1_a_pool_slot_becomes_its_own_display") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_display(INIT_W, INIT_H)
        .with_display_pool(POOL)
        .with_serial_input()
        .append_cmdline("limina.console_shell");
    eprintln!(
        "booting L1 guest for the multi-display test: {cfg:?}",
        cfg = cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("LIMINA_SHELL_READY", Duration::from_secs(20))
        .expect("guest did not enter console command mode over ttyAMA0");

    // ---- 1. The pool is visible as `POOL` connectors, and only slot 0 is connected.
    let connectors = find_connectors(&mut guest);
    assert_eq!(
        connectors.len(),
        POOL as usize,
        "a pool of {POOL} should give the guest {POOL} connectors, got {connectors:?}"
    );
    eprintln!("virtio-gpu connectors: {connectors:?}");

    assert_eq!(
        status_of(&mut guest, &connectors[0]),
        "connected",
        "slot 0 boots connected — it is the display the guest comes up on"
    );
    assert_eq!(
        status_of(&mut guest, &connectors[1]),
        "disconnected",
        "a spare pool slot must boot DISCONNECTED, or a stock guest would come up believing it \
         has a monitor that shows nothing"
    );

    // A disconnected connector must not advertise an EDID: the guest prunes its modes from it,
    // so a stale blob here is how a phantom monitor gets a mode list.
    assert!(
        read_edid(&mut guest, &connectors[1]).is_empty(),
        "a disconnected slot must expose no EDID"
    );

    // Slot 0 boots connected but *without* an EDID: the device only has one to serve once a
    // `DisplayControl` carries an `EdidSpec` down, which on a real boot is the host panel's
    // identity. Give it one here so the two slots can be compared as identities below.
    guest
        .update_display(spec(0, 0x1111, 0x1EC0_0000, "L1 First"))
        .expect("giving slot 0 an identity");
    let slot0_boot = wait_for_edid(&mut guest, &connectors[0])
        .expect("slot 0 never produced an EDID after its identity push");
    assert_eq!(
        checksum(&slot0_boot[..128]),
        0,
        "slot 0's pushed EDID has a bad checksum"
    );

    // ---- 2. Connecting the spare slot, with its own identity, makes it a display.
    guest
        .update_display(spec(1, 0x5151, 0x5EC0_0001, "L1 Second"))
        .expect("connecting pool slot 1");

    let slot1 = wait_for_edid(&mut guest, &connectors[1])
        .expect("slot 1 never produced an EDID after being connected");
    assert_eq!(
        status_of(&mut guest, &connectors[1]),
        "connected",
        "the connected slot should report its connector as connected"
    );
    assert_eq!(
        checksum(&slot1[..128]),
        0,
        "slot 1's pushed EDID has a bad checksum"
    );
    assert_identity(&slot1, 0x5151, 0x5EC0_0001);

    // ---- 3. The two slots carry DIFFERENT identities. This is the property the whole
    // multi-display design rests on: a compositor keys its saved per-monitor configuration on
    // the identity, so two scanouts sharing one would be one monitor as far as it is concerned.
    let slot0 = read_edid(&mut guest, &connectors[0]);
    assert_ne!(
        slot0[MANUFACTURER.start..SERIAL.end],
        slot1[MANUFACTURER.start..SERIAL.end],
        "two connected slots must not advertise the same identity"
    );

    // ---- 4. Pushing an identity to one slot leaves the other's alone. A shared EdidParams
    // would show up here and nowhere else.
    guest
        .update_display(spec(0, 0x3333, 0x3EC0_0000, "L1 First"))
        .expect("pushing an identity to slot 0");
    let slot0_new = wait_for_edid_change(&mut guest, &connectors[0], &slot0)
        .expect("slot 0's EDID never changed after its own identity push");
    assert_identity(&slot0_new, 0x3333, 0x3EC0_0000);
    assert_eq!(
        read_edid(&mut guest, &connectors[1])[..128],
        slot1[..128],
        "a push to slot 0 must not touch slot 1's EDID"
    );

    // ---- 5. Disconnecting one slot leaves the other connected. A pool whose slots share a
    // connected flag would take the whole desktop down here.
    guest
        .update_display(DisplayControl {
            display_id: 1,
            size: None,
            position: None,
            connected: Some(false),
            edid: None,
        })
        .expect("disconnecting pool slot 1");
    assert!(
        wait_for_status(&mut guest, &connectors[1], "disconnected"),
        "slot 1 never went back to disconnected"
    );
    assert_eq!(
        status_of(&mut guest, &connectors[0]),
        "connected",
        "disconnecting slot 1 must not disturb slot 0"
    );
}

/// A `DisplayControl` that connects `display_id` with an identity of its own.
fn spec(display_id: u32, product_id: u16, serial: u32, name: &str) -> DisplayControl {
    DisplayControl {
        display_id,
        size: Some((1280, 800)),
        position: None,
        connected: Some(true),
        edid: Some(EdidSpec {
            refresh_hz: 60,
            dpi: 109,
            vendor: *b"LMN",
            product_id,
            serial,
            name: name.into(),
            serial_string: None,
            range: None,
            modes: Vec::new(),
            alt_mode: None,
        }),
    }
}

fn assert_identity(edid: &[u8], product_id: u16, serial: u32) {
    // 'LMN' → five bits per letter, big-endian.
    let expected_vendor = (((12u16) << 10) | ((13u16) << 5) | 14u16).to_be_bytes();
    assert_eq!(
        &edid[MANUFACTURER], &expected_vendor,
        "the pushed manufacturer id did not reach the guest"
    );
    assert_eq!(
        &edid[PRODUCT_CODE],
        &product_id.to_le_bytes(),
        "the pushed product code did not reach the guest"
    );
    assert_eq!(
        &edid[SERIAL],
        &serial.to_le_bytes(),
        "the pushed serial did not reach the guest"
    );
}

/// Every virtio-gpu DRM connector, in sysfs order (which is scanout order — the driver creates
/// one output per scanout in index order). Same shape filter as `l1_edid.rs`: a kernel printk
/// interleaving with the `ls` output on ttyAMA0 must not be mistaken for a connector.
fn find_connectors(guest: &mut Guest) -> Vec<String> {
    let listing = guest
        .console_command("ls /sys/class/drm", Duration::from_secs(10))
        .expect("listing /sys/class/drm over the console");
    let mut connectors: Vec<String> = listing
        .lines()
        .map(str::trim)
        .filter(|e| e.starts_with("card") && e.contains('-'))
        .map(str::to_string)
        .collect();
    connectors.sort();
    assert!(
        !connectors.is_empty(),
        "no DRM connector found under /sys/class/drm; listing was:\n{listing}"
    );
    connectors
}

fn status_of(guest: &mut Guest, connector: &str) -> String {
    guest
        .console_command(
            &format!("cat /sys/class/drm/{connector}/status"),
            Duration::from_secs(10),
        )
        .expect("reading connector status")
        .lines()
        .map(str::trim)
        .find(|l| *l == "connected" || *l == "disconnected")
        .unwrap_or_default()
        .to_string()
}

/// The connector's EDID blob. Empty when the connector has none (disconnected), which is a
/// legitimate answer here rather than a failure.
///
/// `xxd` is a limina-init builtin taking a bare path — it emits the whole file as one line of
/// lowercase hex, because `cat` cannot survive a binary blob on a line-delimited console. The
/// console is shared with the kernel log, so an async printk can land mid-output; take the
/// longest pure-hex token rather than filtering the whole stream, which would fold the
/// message's letters in and silently shift every byte. Same reasoning as `l1_edid.rs`.
fn read_edid(guest: &mut Guest, connector: &str) -> Vec<u8> {
    let out = guest
        .console_command(
            &format!("xxd /sys/class/drm/{connector}/edid"),
            Duration::from_secs(10),
        )
        .expect("reading the connector EDID");
    let Some(hex) = out
        .split_whitespace()
        .filter(|t| t.len() >= 32 && t.len() % 2 == 0 && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .max_by_key(|t| t.len())
    else {
        return Vec::new();
    };
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex is ascii"), 16)
                .expect("valid hex")
        })
        .collect()
}

/// Poll until the connector serves a **complete** EDID.
///
/// "Long enough" is not the settle condition: the blob comes back through the serial console
/// as `xxd` output, and a line arriving mid-read yields a plausible-length blob that is not an
/// EDID (observed once in the suite as a checksum of 95 on a freshly pushed identity, and not
/// reproducible in 3 isolated runs). A valid checksum is what says the read is whole. On
/// timeout the last long-enough blob is returned anyway, so a genuinely malformed EDID fails
/// the caller's checksum assert with its real value instead of the useless "never produced one".
fn wait_for_edid(guest: &mut Guest, connector: &str) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last: Option<Vec<u8>> = None;
    loop {
        let edid = read_edid(guest, connector);
        if edid.len() >= 128 {
            if checksum(&edid[..128]) == 0 {
                return Some(edid);
            }
            last = Some(edid);
        }
        if Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn wait_for_edid_change(guest: &mut Guest, connector: &str, previous: &[u8]) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let edid = read_edid(guest, connector);
        if edid.len() >= 128 && edid != previous && checksum(&edid[..128]) == 0 {
            return Some(edid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn wait_for_status(guest: &mut Guest, connector: &str, want: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if status_of(guest, connector) == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn checksum(block: &[u8]) -> u8 {
    block.iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}

/// The config-change ack race: the guest acks an event by writing back the bits it read, and
/// the device used to clear whatever that write named — wiping any event raised *between* the
/// guest's read and its ack. Updates split across back-to-back events lost the second one: on
/// the rig, an arrangement move arrived and the connect right behind it never woke the guest
/// (libkrun f03e94c closes it from both ends — the queue holds while an event is unconsumed,
/// and one event carries every update that can share it).
///
/// Hitting the window takes aim, not volume. The pre-fix worker drained its queue one update
/// per loop iteration without waiting for anything, so two sends with any small gap were both
/// applied before the guest even read the config — the loss needs the second SEND to land
/// inside the guest's window, which opens at the GET_DISPLAY_INFO answer and closes at the ack
/// (`virtio_gpu_config_changed_work_func` waits for the reply, runs the hotplug event, then
/// writes `events_clear`). That start jitters with irq and work-queue scheduling, so the shots
/// sweep the send gap finely across the plausible zone, several passes over; pre-fix, some
/// shot's connect vanishes and its wait times out with the slot stuck.
#[test]
fn l1_b_back_to_back_updates_survive_the_ack_race() {
    if !limina_test::require_hvf_or_skip("l1_b_back_to_back_updates_survive_the_ack_race") {
        return;
    }

    // The window only EXISTS where the guest driver carries upstream d1b894c5bbb3 (in stable
    // from 7.1.8, backport 2e0b1d51de9e — the rig's enhanced kernel has it): that commit moved
    // the hotplug event out of the response callback, so config_changed_work_func waits for
    // the GET_DISPLAY_INFO reply and runs the hotplug BEFORE acking — an update applied in
    // that stretch is answered by the earlier snapshot and its bit dies with the ack. On a
    // pre-fix driver (v7.1.0 and the default L1 kernel) the work func acks ~40us after the
    // read and the response callback ITSELF fires the hotplug, so every wiped update is still
    // healed by the in-flight reply — measured with a device-side trace: no observable window
    // at all, 2000 raced shots green against the raciest libkrun. Same skip contract as
    // l2_share_71, but the tag matters:
    // `KVER=v7.1.8 PAGESIZE=16k KIMAGE_NAME=Image-16k-71 scripts/build-test-kernel.sh`.
    let kernel_71 = std::env::var("LIMINA_TEST_KERNEL_71")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| limina_test::repo_root().join("target/test-guest/kernel/Image-16k-71"));
    if !kernel_71.exists() {
        eprintln!("SKIP l1_b_back_to_back_updates_survive_the_ack_race: {kernel_71:?} missing");
        return;
    }

    let mut cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_display(INIT_W, INIT_H)
        .with_display_pool(POOL)
        .with_serial_input()
        .with_supervisor_log()
        .append_cmdline("limina.console_shell");
    match &mut cfg.boot {
        limina_test::Boot::Kernel { kernel, .. } => *kernel = kernel_71,
        other => panic!("l1_from_env built an unexpected boot {other:?}"),
    }
    eprintln!(
        "booting L1 guest for the ack-race test: {cfg:?}",
        cfg = cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("LIMINA_SHELL_READY", Duration::from_secs(20))
        .expect("guest did not enter console command mode over ttyAMA0");

    // The window this test aims at only exists on the >=7.1 driver (see above) — a silently
    // substituted kernel would turn the whole test into a vacuous pass.
    let version = guest
        .console_command("cat /proc/version", Duration::from_secs(10))
        .expect("reading /proc/version");
    let version = version
        .lines()
        .find(|l| l.contains("Linux version"))
        .unwrap_or("")
        .to_string();
    eprintln!("ack-race guest kernel: {version}");
    let vnum = version
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()
        .unwrap_or("");
    let mut parts = vnum.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (maj, min, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    assert!(
        (maj, min, patch) >= (7, 1, 8),
        "the ack-race window needs the 7.1.8+ hotplug-before-ack driver (upstream \
         d1b894c5bbb3); a silently substituted kernel would make this test vacuous — got: \
         {version}"
    );

    let connectors = find_connectors(&mut guest);
    assert_eq!(connectors.len(), POOL as usize);

    // Give slot 0 an identity and wait for it: the race needs the OS driver up and *acking*,
    // and this round-trip proves the whole event path is live before the clock starts.
    guest
        .update_display(spec(0, 0x1111, 0x1EC0_0000, "L1 First"))
        .expect("giving slot 0 an identity");
    wait_for_edid(&mut guest, &connectors[0])
        .expect("slot 0 never produced an EDID after its identity push");

    // ---- 1. A move and a connect, back to back, at a fine sweep of send gaps. Both must
    // land, every time. The arrangement-relay shape that lost on the rig: an in-place
    // position push to the connected slot, then the spare slot's connect right behind it.
    // On the 7.1.8+ driver the window is fat — the hotplug reprobe runs between the reply
    // and the ack — so pre-fix libkrun failed this on the very first shot; the sweep stays
    // for robustness against timing shifts (LIMINA_ACKRACE_PASSES overrides the pass count).
    let passes: u32 = std::env::var("LIMINA_ACKRACE_PASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let mut shot = 0u32;
    for pass in 0u32..passes {
        for gap_us in (100u64..=4000).step_by(100) {
            shot += 1;
            let gap = Duration::from_micros(gap_us);
            let serial = 0x5EC0_0100 + shot;

            guest
                .update_display(DisplayControl {
                    display_id: 0,
                    size: None,
                    position: Some((256 * (shot % 2), 0)),
                    connected: None,
                    edid: None,
                })
                .expect("pushing a position to slot 0");
            std::thread::sleep(gap);
            let mut connect = spec(1, 0x5151, serial, "L1 Second");
            connect.position = Some((1280, 0));
            guest.update_display(connect).expect("connecting slot 1");

            assert!(
                wait_for_serial(&mut guest, &connectors[1], serial),
                "pass {pass}, gap {gap:?}: slot 1 never connected with serial {serial:#x} — \
                 the connect behind the move was lost"
            );

            // The teardown is not raced: a lost disconnect would be healed by the next
            // shot's events and prove nothing.
            guest
                .update_display(DisplayControl {
                    display_id: 1,
                    size: None,
                    position: None,
                    connected: Some(false),
                    edid: None,
                })
                .expect("disconnecting slot 1");
            assert!(
                wait_for_status(&mut guest, &connectors[1], "disconnected"),
                "pass {pass}, gap {gap:?}: slot 1 never disconnected"
            );
        }
    }

    // ---- 2. A disconnect and a reconnect, back to back, must NOT collapse into "nothing
    // happened": the batching that shares one event stops at a second connectivity flip for
    // the same display. The reconnect carries a new identity, so a guest that really saw the
    // cycle re-reads the EDID and shows the new serial; a collapsed pair leaves the old one.
    let before = 0x5EC0_0A00;
    let after = 0x5EC0_0B00;
    guest
        .update_display(spec(1, 0x5151, before, "L1 Second"))
        .expect("connecting slot 1");
    assert!(
        wait_for_serial(&mut guest, &connectors[1], before),
        "slot 1 never connected before the cycle"
    );
    guest
        .update_display(DisplayControl {
            display_id: 1,
            size: None,
            position: None,
            connected: Some(false),
            edid: None,
        })
        .expect("disconnecting slot 1");
    guest
        .update_display(spec(1, 0x5151, after, "L1 Second"))
        .expect("reconnecting slot 1");
    assert!(
        wait_for_serial(&mut guest, &connectors[1], after),
        "slot 1 kept its pre-cycle identity: the disconnect+reconnect pair collapsed"
    );
}

/// Wait until the connector is connected AND its EDID carries `serial` — the proof the guest
/// saw the specific update, not merely an update.
fn wait_for_serial(guest: &mut Guest, connector: &str, serial: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if status_of(guest, connector) == "connected" {
            let edid = read_edid(guest, connector);
            if edid.len() >= 128 && edid[SERIAL] == serial.to_le_bytes() {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
