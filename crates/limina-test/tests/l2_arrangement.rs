// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — when the arrangement relay's suggested offsets actually reach a seated mutter.
//!
//! Observed end to end on the seated enhanced guest (mutter 50.1), against the design's
//! original assumption that a bare in-place position re-push would make mutter re-derive
//! its layout. It does not, and cannot: mutter ≥ 50's `ensure_configured` applies the first
//! **existing** config (current, then previous) whose monitors are all still connected
//! *before* it ever consults suggested positions (`meta-monitor-manager.c`, the
//! `existing_configs` loop ahead of `create_suggested`). So:
//!
//! - An in-place position change diffs in the KMS layer (`suggested_x changed` →
//!   `META_KMS_RESOURCE_CHANGE_FULL` → reload) but the reload re-applies the current
//!   config — "Applied current based monitor configuration".
//! - A disconnect + corrected reconnect of the same monitor re-applies the *previous*
//!   config the same way.
//! - Suggested positions are consulted exactly when no complete existing config is
//!   available: at session start, or the first time a monitor set appears in a session.
//!
//! The test pins all three behaviors in one boot: the invalid-set linear fallback, the two
//! no-rearrange cases (if a future mutter starts honoring in-place moves, the assertion
//! flips and we get to simplify the host side), and the positive end-to-end acceptance —
//! after a seat restart the corrected offsets, still held by the device and re-read through
//! `GET_DISPLAY_INFO` → our kernel patch's connector props, produce the below-arrangement
//! that linear can never yield. That last step exercises the whole relay: host socket →
//! device → GDI response → suggested props → `create_suggested` → layout.
//!
//! The vehicle must run the guest's own installed kernel (EFI boot): the suggested-offset
//! properties and `hotplug_mode_update` are our kernel patch, and the property values must
//! update from every `GET_DISPLAY_INFO` response (`virtgpu_vq.c`) for any of this to reach
//! mutter at all.

use limina_test::{DisplayControl, EdidSpec, Guest, GuestConfig};
use std::time::{Duration, Instant};

/// Slot 0's boot mode; slot 1 is connected at the same pixel size. No expected coordinate
/// is derived from these: mutter picks each monitor's scale (the seated golden runs
/// Virtual-1 at a fractional 1.333, logical 960×600 — observed, and exactly the
/// unpredictability the metric correction exists for), so the test reads the logical
/// geometry back and derives its expectations from what mutter actually chose — the same
/// feedback `arrangement::correct_metric` runs on.
const W: u32 = 1280;
const H: u32 = 800;

fn spec(display_id: u32, serial: u32, position: (u32, u32)) -> DisplayControl {
    DisplayControl {
        display_id,
        size: Some((W, H)),
        position: Some(position),
        connected: Some(true),
        edid: Some(EdidSpec {
            refresh_hz: 60,
            dpi: 96,
            vendor: *b"LMN",
            product_id: 0x5151,
            serial,
            name: "L2 Second".into(),
            serial_string: None,
            range: None,
            modes: Vec::new(),
            alt_mode: None,
        }),
    }
}

/// One logical monitor as mutter has it: position, and the logical size derived from the
/// current mode over the scale mutter chose.
#[derive(Debug, Clone, PartialEq)]
struct Logical {
    connector: String,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

/// The seated session's logical-monitor layout.
///
/// `busctl --json` gives machine-parseable output, and the guest's python3 walks the
/// GetCurrentState tuple instead of us regex-scanning a GVariant print. An ssh shell has no
/// session bus address; `XDG_RUNTIME_DIR` is enough for `busctl --user`. The logical size is
/// not in the logical-monitor tuple — it is the current mode's resolution divided by the
/// logical monitor's scale, with the mode found via its `is-current` property.
fn logical_layout(guest: &Guest) -> Vec<Logical> {
    let cmd = r#"export XDG_RUNTIME_DIR=/run/user/$(id -u); \
busctl --user call --json=short org.gnome.Mutter.DisplayConfig \
  /org/gnome/Mutter/DisplayConfig org.gnome.Mutter.DisplayConfig GetCurrentState \
| python3 -c '
import json, sys
state = json.load(sys.stdin)["data"]
current = {}
for mon in state[1]:
    conn = mon[0][0]
    for mode in mon[1]:
        if mode[6].get("is-current", {}).get("data"):
            current[conn] = (mode[1], mode[2])
for lm in state[2]:
    x, y, scale = lm[0], lm[1], lm[2]
    for spec in lm[5]:
        conn = spec[0]
        w, h = current.get(conn, (0, 0))
        print(conn, x, y, round(w / scale), round(h / scale))
'"#;
    let out = guest.ssh_exec(cmd).unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some(Logical {
                connector: it.next()?.to_string(),
                x: it.next()?.parse().ok()?,
                y: it.next()?.parse().ok()?,
                w: it.next()?.parse().ok()?,
                h: it.next()?.parse().ok()?,
            })
        })
        .collect()
}

/// Wait until `pred` holds over the layout, polling GetCurrentState; returns the last layout
/// either way.
fn wait_for_layout(guest: &Guest, pred: impl Fn(&[Logical]) -> bool) -> Vec<Logical> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let last = logical_layout(guest);
        if pred(&last) || Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn find<'a>(layout: &'a [Logical], connector: &str) -> Option<&'a Logical> {
    layout.iter().find(|l| l.connector == connector)
}

/// Wait for a fully seated session: gnome-shell up and mutter's DisplayConfig answering on
/// the session bus. Used at boot and again after the seat restart.
fn wait_for_seated_session(guest: &Guest) {
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated session didn't come up");
    guest
        .ssh_poll(
            "export XDG_RUNTIME_DIR=/run/user/$(id -u); \
             busctl --user status org.gnome.Mutter.DisplayConfig >/dev/null",
            Duration::from_secs(120),
        )
        .expect("mutter's DisplayConfig never appeared on the session bus");
}

#[test]
fn suggested_positions_apply_at_seat_but_never_rearrange_a_live_session() {
    if !limina_test::require_hvf_or_skip(
        "suggested_positions_apply_at_seat_but_never_rearrange_a_live_session",
    ) {
        return;
    }
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(W, H)
            .with_display_pool(2)
            .with_net()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!(
                "SKIPPED suggested_positions_apply_at_seat_but_never_rearrange_a_live_session: {e:#}"
            );
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    wait_for_seated_session(&guest);

    // The metric mutter chose for the primary, read back rather than predicted — the seated
    // golden runs Virtual-1 at a fractional scale, so nothing below may assume scale 1.
    let layout = wait_for_layout(&guest, |l| find(l, "Virtual-1").is_some());
    let v1 = find(&layout, "Virtual-1")
        .expect("Virtual-1 never appeared in GetCurrentState")
        .clone();
    eprintln!("Virtual-1 as mutter has it: {v1:?}");
    assert!(v1.w > 0 && v1.h > 0, "Virtual-1 has no current mode");

    // --- 1. An invalid suggested set falls back to linear. The connect carries a position
    // inside Virtual-1's logical rect, so mutter rejects the whole set ("Suggested monitor
    // config has overlapping region"); the linear default aligns tops (y = 0) and never
    // applies the suggested y. This is the first-appearance event, so `create_suggested`
    // demonstrably DID run — the set was just invalid. It also plants the current config
    // the next step must fail to displace.
    guest
        .update_display(spec(1, 0x5EC0_0001, (v1.w as u32 / 2, 100)))
        .expect("connecting slot 1 with an overlapping suggestion");
    let layout = wait_for_layout(&guest, |l| find(l, "Virtual-2").is_some());
    let v2 = find(&layout, "Virtual-2")
        .expect("Virtual-2 never appeared after its connect")
        .clone();
    assert_eq!(
        (v2.x, v2.y),
        (v1.w, 0),
        "an overlapping suggested set should fall back to linear (Virtual-2 right of \
         Virtual-1 at ({}, 0)), got {layout:?}",
        v1.w
    );

    // The corrected position: Virtual-2 directly below Virtual-1's OBSERVED logical rect —
    // adjacent, non-overlapping, and unreachable by the linear fallback, so seeing it later
    // proves the suggested set was applied, not defaulted.
    let below = (0i64, v1.h);

    // --- 2. A bare in-place position move does NOT rearrange the live session: the KMS
    // diff fires and mutter reloads, but the existing-config loop re-applies the current
    // (linear) config before suggested is consulted. The wait is generous next to the
    // sub-second reload; if this assertion ever fails with Virtual-2 at `below`, mutter has
    // started honoring in-place suggested moves and the host side can lean on it.
    guest
        .update_display(DisplayControl {
            display_id: 1,
            size: None,
            position: Some((below.0 as u32, below.1 as u32)),
            connected: None,
            edid: None,
        })
        .expect("re-pushing the corrected position");
    std::thread::sleep(Duration::from_secs(12));
    let layout = logical_layout(&guest);
    let v2 = find(&layout, "Virtual-2")
        .expect("Virtual-2 disappeared after the in-place re-push")
        .clone();
    assert_eq!(
        (v2.x, v2.y),
        (v1.w, 0),
        "mutter ≥ 50 re-applies the current config on an in-place suggested change; a move \
         to {below:?} means that precedence changed — revisit the relay design, got {layout:?}"
    );

    // --- 3. A disconnect + corrected reconnect doesn't rearrange either: the same loop
    // re-applies the *previous* {V1,V2} config. The disconnect must outlive mutter's
    // connection-change debounce (a sub-second cycle is deliberately swallowed), so wait
    // until the monitor is really gone before reconnecting.
    guest
        .update_display(DisplayControl {
            display_id: 1,
            size: None,
            position: None,
            connected: Some(false),
            edid: None,
        })
        .expect("disconnecting slot 1 for the corrected reconnect");
    let gone = wait_for_layout(&guest, |l| find(l, "Virtual-2").is_none());
    assert!(
        find(&gone, "Virtual-2").is_none(),
        "Virtual-2 never left the layout after its disconnect: {gone:?}"
    );
    guest
        .update_display(DisplayControl {
            display_id: 1,
            size: None,
            position: Some((below.0 as u32, below.1 as u32)),
            connected: Some(true),
            edid: None,
        })
        .expect("reconnecting slot 1 with the corrected position");
    std::thread::sleep(Duration::from_secs(12));
    let layout = wait_for_layout(&guest, |l| find(l, "Virtual-2").is_some());
    let v2 = find(&layout, "Virtual-2")
        .expect("Virtual-2 never came back after the corrected reconnect")
        .clone();
    assert_eq!(
        (v2.x, v2.y),
        (v1.w, 0),
        "a same-set reconnect re-applies the previous config; Virtual-2 at {below:?} means \
         mutter's precedence changed — revisit the relay design, got {layout:?}"
    );

    // --- 4. The positive acceptance: a fresh seat has no existing configs, the stored
    // config can't cover a hotplug_mode_update set, so `create_suggested` runs against the
    // device's current property values — the corrected offsets pushed above, still held by
    // the device and re-read through GET_DISPLAY_INFO at driver init. Virtual-2 below
    // Virtual-1 proves the whole relay end to end.
    guest
        .ssh_exec("sudo systemctl restart gdm; true")
        .expect("restarting the seat");
    std::thread::sleep(Duration::from_secs(5));
    wait_for_seated_session(&guest);
    let layout = wait_for_layout(&guest, |l| {
        find(l, "Virtual-2").is_some_and(|v| (v.x, v.y) == below)
    });
    let v2 = find(&layout, "Virtual-2")
        .expect("Virtual-2 missing after the seat restart")
        .clone();
    assert_eq!(
        (v2.x, v2.y),
        below,
        "at seat time the corrected suggested set must apply (Virtual-2 below Virtual-1 at \
         {below:?}), got {layout:?} — the relay's device→GDI→props→create_suggested chain \
         is broken"
    );
}
