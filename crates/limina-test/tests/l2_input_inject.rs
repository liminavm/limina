// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — **host-side input injection lands on the guest's real virtio-input devices.**
//!
//! `limina input` writes evdev events straight into the supervisor's ends of the worker's
//! input sockets, so a harness can drive a guest with no human and no window focus. What a
//! system-compositor test needs from that is the *real* device path — the kernel's
//! `virtio_input` driver, the evdev node, logind's TakeDevice — which an in-guest uinput
//! injector bypasses. So the oracle here is the guest's own `/dev/input/event*` nodes for the
//! virtio keyboard, tablet, mouse and touchpad, read raw (`struct input_event`) and compared
//! event for event with what each verb promises to send. The touchpad's positions follow the
//! host's trackpad size, so its stream is checked for its shape: the fingers, their spacing,
//! the button and the lifts.
//!
//! # Traps this test is shaped around
//!
//! - **Keys go to the USB HID gadget until the guest binds `virtio_input`** (the key router,
//!   `limina_input::router`). The virtio nodes are therefore waited for by NAME before the
//!   first injection; their presence is the driver binding the router flips on.
//! - **Nodes are found by name**, never by number: the HID gadget, the spice tablet and the
//!   boot order all move the numbers.
//! - **The reader grabs the nodes** (`EVIOCGRAB`), so gdm sees none of this — a chord or a
//!   click landing on the greeter would make the run's outcome depend on its UI.
//! - **The kernel drops what changes nothing** (a repeated ABS value, an EV_KEY already in
//!   that state, an empty frame), so every expected event below is a real change.
//!
//! # Off by default
//!
//! Injection is refused unless the VM's `input-inject` lever is on. The headless run boots with
//! it off, has a verb refused (nonzero exit, the refusal naming how to enable it) while the
//! reader is already grabbing the nodes, then turns it on with `limina debug <pid> lever
//! input-inject on`: the exact keyboard comparison is what proves the refused tap never arrived,
//! and the script landing proves the runtime toggle. The windowed run enables it the other way,
//! with `LIMINA_INPUT_INJECT=1` in the supervisor's environment.
//!
//! # Failing loudly
//!
//! Before the main script, both runs check that `limina input <vm> keys` lists `KEY_ESC 1`, and
//! send a script whose second verb is an unknown key (`escape`): it must exit non-zero, name the
//! line and suggest `KEY_ESC`, and the exact keyboard comparison proves its first verb arrived
//! and its third never did.
//!
//! # Per display
//!
//! `l2_input_inject_per_display_headless` boots two displays and points at each in its own
//! coordinates (`--display N`), with the layout reported the way the guest agent reports it;
//! see that test for its oracle.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The in-guest reader: grab the four virtio nodes (found by name), print every event as
/// `<dev> <type> <code> <value>`, and stop on the end marker (KEY_F14 released).
const READER: &str = include_str!("../guest/injectreader.py");

/// A script whose second verb names no key: it must stop there, after the first verb reached the
/// guest and before the third is sent, and exit non-zero naming the line, the nearest key name
/// and where the names are listed. The exact keyboard comparison is what proves F16 never left.
const FAILING_SCRIPT: &str = "\
key tap KEY_F15
key tap escape
key tap KEY_F16
";

/// What the harness sends, one verb per line — the same text a compositor team's script would.
const SCRIPT: &str = "\
# a tap, then a chord (pressed in order, released in reverse)
key tap KEY_F13
key tap KEY_LEFTCTRL+KEY_LEFTSHIFT+KEY_F13
type aB!
abs 16384 8192
abs-norm 0.25 0.75
abs-px 640 400 1280x800
rel 7 -3
button click right
scroll 1
touchpad click 2 --at 0.5,0.5
touchpad tap 1 --at 0.25,0.75
sleep 50
key tap KEY_F14
";

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;

/// One evdev event as the reader prints it: `(type, code, value)`.
type Event = (u16, u16, i32);

/// The sequence each device must carry, SYN_REPORTs included.
fn expected() -> [(&'static str, Vec<Event>); 3] {
    const SYN: Event = (EV_SYN, 0, 0);
    let tap = |k: u16| vec![(EV_KEY, k, 1), SYN, (EV_KEY, k, 0), SYN];
    let (f13, f14, f15, lctrl, lshift, a, b, one) = (183, 184, 185, 29, 42, 30, 48, 2);
    // FAILING_SCRIPT's first verb, and nothing of the rest of it.
    let mut kbd = tap(f15);
    kbd.extend(tap(f13));
    kbd.extend([(EV_KEY, lctrl, 1), SYN, (EV_KEY, lshift, 1), SYN]);
    kbd.extend(tap(f13));
    kbd.extend([(EV_KEY, lshift, 0), SYN, (EV_KEY, lctrl, 0), SYN]);
    // "aB!": a bare letter, then each shifted character inside its own Shift press.
    kbd.extend(tap(a));
    kbd.push((EV_KEY, lshift, 1));
    kbd.push(SYN);
    kbd.extend(tap(b));
    kbd.extend([(EV_KEY, lshift, 0), SYN, (EV_KEY, lshift, 1), SYN]);
    kbd.extend(tap(one));
    kbd.extend([(EV_KEY, lshift, 0), SYN]);
    kbd.extend(tap(f14));

    let (abs_x, abs_y, btn_right, wheel, wheel_hi) = (0, 1, 0x111, 8, 11);
    let ptr = vec![
        (EV_ABS, abs_x, 16384),
        (EV_ABS, abs_y, 8192),
        SYN,
        // abs-norm: round(u * ABS_MAX).
        (EV_ABS, abs_x, 8192),
        (EV_ABS, abs_y, 24575),
        SYN,
        // abs-px: floor((p + 0.25) * 32768 / mode) — maps back onto pixel p whether the
        // compositor truncates or rounds.
        (EV_ABS, abs_x, 16390),
        (EV_ABS, abs_y, 16394),
        SYN,
        (EV_KEY, btn_right, 1),
        SYN,
        (EV_KEY, btn_right, 0),
        SYN,
        (EV_REL, wheel_hi, 120),
        (EV_REL, wheel, 1),
        SYN,
    ];
    let rel = vec![(EV_REL, 0, 7), (EV_REL, 1, -3), SYN];
    [("kbd", kbd), ("ptr", ptr), ("rel", rel)]
}

/// The touchpad verbs' stream, by its shape: `touchpad click 2` lands two fingers side by side
/// 18 mm apart (tracking ids from 32768, the half of the range injection takes), presses and
/// releases the pad's button with them down, and lifts them; `touchpad tap 1` lands one and
/// lifts it, pressing nothing.
fn check_touchpad(name: &str, got: &[Event]) {
    const ABS_MT_SLOT: u16 = 0x2f;
    const ABS_MT_POSITION_X: u16 = 0x35;
    const ABS_MT_POSITION_Y: u16 = 0x36;
    const ABS_MT_TRACKING_ID: u16 = 0x39;
    const BTN_LEFT: u16 = 0x110;
    const BTN_TOOL_FINGER: u16 = 0x145;
    const BTN_TOUCH: u16 = 0x14a;
    const BTN_TOOL_DOUBLETAP: u16 = 0x14d;
    let at = |e: Event| got.iter().position(|&g| g == e);
    let must = |e: Event, what: &str| {
        at(e).unwrap_or_else(|| panic!("{name}: the touchpad never carried {what} {e:?}: {got:?}"))
    };
    let two = must((EV_KEY, BTN_TOOL_DOUBLETAP, 1), "two fingers");
    let first = must((EV_ABS, ABS_MT_TRACKING_ID, 32768), "the first finger");
    let second = must((EV_ABS, ABS_MT_TRACKING_ID, 32769), "the second finger");
    let press = must((EV_KEY, BTN_LEFT, 1), "the press");
    let release = must((EV_KEY, BTN_LEFT, 0), "the release");
    let lift = must((EV_KEY, BTN_TOUCH, 0), "the lift");
    assert!(
        first < second && second < two && two < press && press < release && release < lift,
        "{name}: the click is out of order: {got:?}"
    );
    // The two fingers' positions, from the landing frame (slot 0, then slot 1).
    let landing = &got[..two];
    let xs: Vec<i32> = landing
        .iter()
        .filter(|e| e.0 == EV_ABS && e.1 == ABS_MT_POSITION_X)
        .map(|e| e.2)
        .collect();
    let ys: Vec<i32> = landing
        .iter()
        .filter(|e| e.0 == EV_ABS && e.1 == ABS_MT_POSITION_Y)
        .map(|e| e.2)
        .collect();
    assert!(
        landing.contains(&(EV_ABS, ABS_MT_SLOT, 1)) && xs.len() == 2 && ys.len() == 2,
        "{name}: two fingers did not land in two slots: {landing:?}"
    );
    assert_eq!(
        xs[1] - xs[0],
        1800,
        "{name}: the fingers are not 18 mm apart"
    );
    assert_eq!(ys[0], ys[1], "{name}: the fingers are not side by side");
    // The tap: one finger, no button, lifted.
    let tap = &got[lift + 1..];
    let one = tap
        .iter()
        .position(|&e| e == (EV_KEY, BTN_TOOL_FINGER, 1))
        .unwrap_or_else(|| panic!("{name}: the tap's finger never landed: {tap:?}"));
    assert!(
        tap.contains(&(EV_ABS, ABS_MT_TRACKING_ID, 32770)),
        "{name}: the tap took no fresh tracking id: {tap:?}"
    );
    assert!(
        tap[one..].contains(&(EV_KEY, BTN_TOUCH, 0)),
        "{name}: the tap never lifted: {tap:?}"
    );
    assert!(
        !tap.iter().any(|e| e.0 == EV_KEY && e.1 == BTN_LEFT),
        "{name}: the tap pressed the button: {tap:?}"
    );
}

fn parse_reader(out: &str, dev: &str) -> Vec<Event> {
    out.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next()? == dev).then_some(())?;
            Some((
                w.next()?.parse().ok()?,
                w.next()?.parse().ok()?,
                w.next()?.parse().ok()?,
            ))
        })
        .collect()
}

/// Feed `script` to `limina input <supervisor pid> -`; whether it succeeded, and its output.
fn inject(limina: &Path, pid: libc::pid_t, script: &str) -> (bool, String) {
    let mut child = Command::new(limina)
        .args(["input", &pid.to_string(), "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning `limina input`");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child
        .wait_with_output()
        .expect("`limina input` did not run");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// `keys` lists the key names, and a script stops loudly at its first bad verb.
fn names_and_a_failing_script(name: &str, limina: &Path, pid: libc::pid_t) {
    let (ok, keys) = limina_cli(limina, &["input", &pid.to_string(), "keys"]);
    assert!(ok, "{name}: `limina input keys` failed: {keys}");
    assert!(
        keys.lines().any(|l| l == "KEY_ESC 1"),
        "{name}: `keys` does not list KEY_ESC with its code:\n{keys}"
    );
    let (ok, said) = inject(limina, pid, FAILING_SCRIPT);
    eprintln!("{name}: the failing script said: {said}");
    assert!(!ok, "{name}: a script with an unknown key exited 0: {said}");
    for needle in ["line 2", "\"escape\"", "did you mean KEY_ESC?", "keys"] {
        assert!(
            said.contains(needle),
            "{name}: the failure lacks {needle:?}: {said}"
        );
    }
}

/// Run `limina <args>`; whether it succeeded, and everything it printed.
fn limina_cli(limina: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(limina)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("running limina");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// With the lever off, a verb is refused, loudly; then turn it on at runtime.
fn refused_then_enabled(name: &str, limina: &Path, pid: libc::pid_t) {
    let pid = pid.to_string();
    let (ok, said) = limina_cli(limina, &["input", &pid, "key", "tap", "KEY_F13"]);
    eprintln!("{name}: with the lever off, limina input said: {said}");
    assert!(
        !ok,
        "{name}: injection was accepted with the lever off: {said}"
    );
    for needle in [
        "input injection is off",
        "LIMINA_INPUT_INJECT=1",
        "Debug menu",
        "limina debug <vm> lever input-inject on",
    ] {
        assert!(
            said.contains(needle),
            "{name}: the refusal lacks {needle:?}: {said}"
        );
    }
    let (ok, said) = limina_cli(limina, &["debug", &pid, "lever", "input-inject", "on"]);
    assert!(ok, "{name}: turning the lever on failed: {said}");
}

/// Split a device's events into frames, each ending at its SYN_REPORT.
fn frames(events: &[Event]) -> Vec<Vec<Event>> {
    events
        .split_inclusive(|e| e.0 == EV_SYN)
        .map(<[Event]>::to_vec)
        .collect()
}

/// Boot, inject, compare. `window_live`: the window's own input path shares the pointer
/// devices, so those are matched frame by frame as an in-order subsequence; the keyboard,
/// which the window only writes to while it is key and typed on, stays exact. `enabled`: the
/// supervisor starts with the lever on; otherwise a refusal is checked first and the lever is
/// turned on at runtime.
fn run(name: &str, cfg: GuestConfig, window_live: bool, enabled: bool) {
    assert!(
        std::env::var_os("LIMINA_INPUT_INJECT").is_none(),
        "{name}: unset LIMINA_INPUT_INJECT; the supervisor inherits it, and this test owns the \
         lever's starting value"
    );
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest never reached sshd");

    // The virtio devices must be bound before the first key, or the router sends it to the
    // USB gadget instead.
    guest
        .ssh_poll(
            "grep -q 'limina Virtual Keyboard' /proc/bus/input/devices && \
             grep -q 'limina Virtual Pointer' /proc/bus/input/devices && \
             grep -q 'limina Virtual Mouse' /proc/bus/input/devices && \
             grep -q 'limina Virtual Touchpad' /proc/bus/input/devices",
            Duration::from_secs(120),
        )
        .unwrap_or_else(|e| panic!("{name}: the virtio input devices never appeared: {e:#}"));

    let script = guest.scratch_dir().join("inject-reader.py");
    std::fs::write(&script, READER).unwrap();
    guest
        .scp_to_guest(&script, "/tmp/inject-reader.py")
        .expect("copying the reader into the guest");
    let _ = guest.ssh_exec("rm -f /tmp/limina-inject-ready");

    let output = std::thread::scope(|s| {
        let reader = s.spawn(|| {
            guest.ssh_exec_timeout(
                "sudo -n python3 /tmp/inject-reader.py 60",
                Duration::from_secs(90),
            )
        });
        guest
            .ssh_poll("test -e /tmp/limina-inject-ready", Duration::from_secs(30))
            .expect("the reader never opened the nodes");
        if !enabled {
            refused_then_enabled(name, &cfg.limina_bin, guest.supervisor_pid());
        }
        names_and_a_failing_script(name, &cfg.limina_bin, guest.supervisor_pid());
        let (ok, said) = inject(&cfg.limina_bin, guest.supervisor_pid(), SCRIPT);
        eprintln!("{name}: limina input said:\n{said}");
        assert!(ok, "{name}: `limina input` failed: {said}");
        reader.join().unwrap().expect("the guest reader failed")
    });
    eprintln!("{name}: the guest read:\n{output}");
    assert!(
        output.contains("END"),
        "{name}: the end marker never reached the virtio keyboard:\n{output}"
    );
    check_touchpad(name, &parse_reader(&output, "pad"));
    for (dev, want) in expected() {
        let got = parse_reader(&output, dev);
        if !window_live || dev == "kbd" {
            assert_eq!(
                got, want,
                "{name}: the virtio {dev} node did not carry exactly the injected events"
            );
        } else {
            // A live window may put its own frames on the pointer devices (a host pointer
            // crossing it); what injection owes is its frames, whole and in order.
            let (got, want) = (frames(&got), frames(&want));
            let mut rest = got.iter();
            for f in &want {
                assert!(
                    rest.any(|g| g == f),
                    "{name}: the virtio {dev} node is missing the injected frame {f:?} \
                     (in order); it carried {got:?}"
                );
            }
        }
    }

    let outcome = guest
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("{name}: teardown outcome: {outcome:?}");
}

/// Headless — the compositor team's unattended loop: no window, a captured coexist display,
/// and `--input` wiring the virtio input devices. Started with the lever off: refused first,
/// then enabled at runtime.
#[test]
fn l2_input_inject_headless_reaches_virtio_nodes() {
    let name = "l2_input_inject_headless_reaches_virtio_nodes";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_arg("--input"),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };
    run(name, cfg, false, false);
}

/// One monitor as the guest's mutter has it: connector, logical position and logical size
/// (the current mode over the monitor's scale) — what `limina-agent-session` reports.
#[derive(Debug, Clone, PartialEq)]
struct Logical {
    connector: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// The seated session's logical-monitor layout, from mutter's own `GetCurrentState` (the
/// recipe `l2_arrangement.rs` documents).
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

/// The guest's bounding box over `layout`: origin and size, in logical units.
fn bounding_box(layout: &[Logical]) -> (i32, i32, f64, f64) {
    let ox = layout.iter().map(|m| m.x).min().unwrap();
    let oy = layout.iter().map(|m| m.y).min().unwrap();
    let dw = layout.iter().map(|m| m.x + m.w as i32).max().unwrap() - ox;
    let dh = layout.iter().map(|m| m.y + m.h as i32).max().unwrap() - oy;
    (ox, oy, f64::from(dw), f64::from(dh))
}

/// Two displays, each addressed by its own coordinates. Headless on the stock image, with a
/// display pool of two: slot 1 is connected through the display-control socket, mutter lays
/// it out, and the harness then plays `limina-agent-session` — it reads mutter's own logical
/// layout and reports it over the control plane, exactly the message the agent sends. Before
/// that report, the supervisor knows only display 0 and `--display 1` is refused.
///
/// The oracle is the raw evdev values the guest's tablet carries, compared with the mapping
/// computed here from mutter's layout, and then each `abs-px` value is carried back through
/// that layout the way libinput and mutter do (`v · desktop / 32768` logical units across the
/// bounding box) to the display and pixel it lands on. The pointer's final on-screen position
/// is not read back: the reader grabs the nodes so the session sees nothing.
#[test]
fn l2_input_inject_per_display_headless() {
    let name = "l2_input_inject_per_display_headless";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    assert!(
        std::env::var_os("LIMINA_INPUT_INJECT").is_none(),
        "{name}: unset LIMINA_INPUT_INJECT; this test sets it for the supervisor itself"
    );
    // Slot 1's mode differs from slot 0's, so a mapping that confused the two would show.
    const W1: u32 = 1024;
    const H1: u32 = 768;
    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_display_pool(2)
            .with_net()
            .with_supervisor_arg("--input")
            .with_control_socket()
            .with_supervisor_log()
            .with_env("LIMINA_INPUT_INJECT", "1"),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };
    let limina = cfg.limina_bin.clone();
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest never reached sshd");
    let pid = guest.supervisor_pid().to_string();
    guest
        .ssh_poll(
            "grep -q 'limina Virtual Pointer' /proc/bus/input/devices && \
             grep -q 'limina Virtual Keyboard' /proc/bus/input/devices && \
             grep -q 'limina Virtual Mouse' /proc/bus/input/devices && \
             grep -q 'limina Virtual Touchpad' /proc/bus/input/devices",
            Duration::from_secs(120),
        )
        .unwrap_or_else(|e| panic!("{name}: the virtio input devices never appeared: {e:#}"));
    guest
        .ssh_poll(
            "export XDG_RUNTIME_DIR=/run/user/$(id -u); \
             busctl --user status org.gnome.Mutter.DisplayConfig >/dev/null",
            Duration::from_secs(180),
        )
        .unwrap_or_else(|e| panic!("{name}: mutter's DisplayConfig never appeared: {e:#}"));

    // ---- 1. One display known: `info` lists it, and display 1 does not exist yet.
    let (ok, info) = limina_cli(&limina, &["input", &pid, "info"]);
    eprintln!("{name}: info with one display:\n{info}");
    assert!(ok, "{name}: info failed: {info}");
    assert!(
        info.lines().any(
            |l| l.starts_with("display 0 connector Virtual-1 scanout 1280x800")
                && l.contains("device ptr mapping identity x 0..32767 y 0..32767")
        ),
        "{name}: info does not list display 0 as the whole range:\n{info}"
    );
    let (ok, said) = limina_cli(
        &limina,
        &["input", &pid, "abs-px", "--display", "1", "1", "1"],
    );
    eprintln!("{name}: --display 1 before it exists: {said}");
    assert!(
        !ok,
        "{name}: a display that does not exist was accepted: {said}"
    );
    assert!(
        said.contains("display 1 does not exist"),
        "{name}: the refusal does not say why: {said}"
    );

    // ---- 2. Connect slot 1 and let mutter place it.
    guest
        .update_display(limina_test::DisplayControl {
            display_id: 1,
            size: Some((W1, H1)),
            position: None,
            connected: Some(true),
            edid: Some(limina_test::EdidSpec {
                refresh_hz: 60,
                dpi: 96,
                vendor: *b"LMN",
                product_id: 0x5152,
                serial: 0x5EC0_0002,
                name: "L2 Inject Two".into(),
                serial_string: None,
                range: None,
                modes: Vec::new(),
                alt_mode: None,
            }),
        })
        .expect("connecting slot 1");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let layout = loop {
        let l = logical_layout(&guest);
        let both = ["Virtual-1", "Virtual-2"]
            .iter()
            .all(|c| l.iter().any(|m| m.connector == *c && m.w > 0 && m.h > 0));
        if both {
            break l;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{name}: mutter never laid out both monitors: {l:?}"
        );
        std::thread::sleep(Duration::from_secs(2));
    };
    eprintln!("{name}: mutter's layout: {layout:?}");

    // ---- 3. Report the layout as the agent would; the supervisor maps through it.
    let mut conn = guest
        .connect_control(Duration::from_secs(30))
        .expect("connecting to the control plane");
    conn.send(&limina_proto::Message::Hello(limina_proto::Hello {
        agent: "limina-test-layout/0".to_string(),
        caps: Vec::new(),
        pagesize: 4096,
    }))
    .expect("sending Hello");
    conn.send(&limina_proto::Message::DisplayLayout(
        limina_proto::DisplayLayout {
            monitors: layout
                .iter()
                .map(|m| limina_proto::GuestMonitor {
                    connector: m.connector.clone(),
                    x: m.x,
                    y: m.y,
                    width: m.w,
                    height: m.h,
                })
                .collect(),
        },
    ))
    .expect("sending the layout");
    guest
        .wait_for_supervisor_log(
            "display: the guest reports its monitors as",
            Duration::from_secs(20),
        )
        .expect("the supervisor never took the layout");
    let (ok, info) = limina_cli(&limina, &["input", &pid, "info"]);
    eprintln!("{name}: info with two displays:\n{info}");
    assert!(ok, "{name}: info failed: {info}");
    for (slot, scanout) in [(0, "1280x800"), (1, "unknown")] {
        let prefix = format!(
            "display {slot} connector Virtual-{} scanout {scanout} logical ",
            slot + 1
        );
        assert!(
            info.lines()
                .any(|l| l.starts_with(&prefix) && l.contains("device ptr mapping report x ")),
            "{name}: info does not list display {slot} as mapped through the report:\n{info}"
        );
    }

    // ---- 4. Point at each display in its own coordinates and read what the guest got.
    let script = guest.scratch_dir().join("inject-reader.py");
    std::fs::write(&script, READER).unwrap();
    guest
        .scp_to_guest(&script, "/tmp/inject-reader.py")
        .expect("copying the reader into the guest");
    let _ = guest.ssh_exec("rm -f /tmp/limina-inject-ready");
    let points = format!(
        "abs-norm --display 0 0.5 0.5\n\
         abs-px --display 1 100 200 {W1}x{H1}\n\
         abs-px 10 20 --display 0\n\
         sleep 50\n\
         key tap KEY_F14\n"
    );
    let output = std::thread::scope(|s| {
        let reader = s.spawn(|| {
            guest.ssh_exec_timeout(
                "sudo -n python3 /tmp/inject-reader.py 60",
                Duration::from_secs(90),
            )
        });
        guest
            .ssh_poll("test -e /tmp/limina-inject-ready", Duration::from_secs(30))
            .expect("the reader never opened the nodes");
        let (ok, said) = inject(&limina, guest.supervisor_pid(), &points);
        eprintln!("{name}: limina input said:\n{said}");
        assert!(ok, "{name}: `limina input` failed: {said}");
        reader.join().unwrap().expect("the guest reader failed")
    });
    eprintln!("{name}: the guest read:\n{output}");
    assert!(output.contains("END"), "{name}: no end marker:\n{output}");

    // What each verb must put on the tablet, from mutter's layout alone.
    let (ox, oy, dw, dh) = bounding_box(&layout);
    let mon = |c: &str| layout.iter().find(|m| m.connector == c).unwrap().clone();
    let (v1, v2) = (mon("Virtual-1"), mon("Virtual-2"));
    let max = 32767.0;
    let norm = |m: &Logical, u: f64, v: f64| {
        (
            ((f64::from(m.x - ox) + u * f64::from(m.w)) / dw * max).round() as i32,
            ((f64::from(m.y - oy) + v * f64::from(m.h)) / dh * max).round() as i32,
        )
    };
    let px = |m: &Logical, p: u32, q: u32, (sw, sh): (u32, u32)| {
        let at = |o: i32, len: u32, p: u32, size: u32, d: f64| {
            let f = (f64::from(o) + (f64::from(p) + 0.25) / f64::from(size) * f64::from(len)) / d;
            (f * (max + 1.0)).floor().clamp(0.0, max) as i32
        };
        (at(m.x - ox, m.w, p, sw, dw), at(m.y - oy, m.h, q, sh, dh))
    };
    let want = [
        norm(&v1, 0.5, 0.5),
        px(&v2, 100, 200, (W1, H1)),
        px(&v1, 10, 20, (1280, 800)),
    ];
    let mut expected = Vec::new();
    for (x, y) in want {
        expected.extend([(EV_ABS, 0, x), (EV_ABS, 1, y), (EV_SYN, 0, 0)]);
    }
    assert_eq!(
        parse_reader(&output, "ptr"),
        expected,
        "{name}: the tablet did not carry the per-display positions mutter's layout implies"
    );

    // And carried back through the guest's own transform, each pixel lands where it was aimed:
    // on the named monitor, on that pixel of its mode.
    for ((x, y), m, (p, q), (sw, sh)) in [
        (want[1], &v2, (100u32, 200u32), (W1, H1)),
        (want[2], &v1, (10, 20), (1280, 800)),
    ] {
        let lx = f64::from(x) * dw / (max + 1.0) + f64::from(ox);
        let ly = f64::from(y) * dh / (max + 1.0) + f64::from(oy);
        let on = layout
            .iter()
            .find(|o| {
                lx >= f64::from(o.x)
                    && lx < f64::from(o.x + o.w as i32)
                    && ly >= f64::from(o.y)
                    && ly < f64::from(o.y + o.h as i32)
            })
            .map(|o| o.connector.clone());
        assert_eq!(
            on.as_deref(),
            Some(m.connector.as_str()),
            "{name}: ({x},{y})"
        );
        let pix = (
            (lx - f64::from(m.x)) * f64::from(sw) / f64::from(m.w),
            (ly - f64::from(m.y)) * f64::from(sh) / f64::from(m.h),
        );
        assert_eq!(
            (pix.0.floor() as u32, pix.1.floor() as u32),
            (p, q),
            "{name}: ({x},{y}) lands on {pix:?} of {}",
            m.connector
        );
    }

    let outcome = guest
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("{name}: teardown outcome: {outcome:?}");
}

/// Windowed — the window's own input path is live alongside; injection must reach the same
/// devices through the worker connection the window uses. Enabled by the environment variable.
#[test]
fn l2_input_inject_windowed_reaches_virtio_nodes() {
    let name = "l2_input_inject_windowed_reaches_virtio_nodes";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg
            .with_windowed_coexist_display(1280, 800)
            .with_net()
            .with_env("LIMINA_INPUT_INJECT", "1"),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };
    run(name, cfg, true, true);
}
