// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — **host-side input injection lands on the guest's real virtio-input devices.**
//!
//! `limina input` writes evdev events straight into the supervisor's ends of the worker's
//! input sockets, so a harness can drive a guest with no human and no window focus. What a
//! system-compositor test needs from that is the *real* device path — the kernel's
//! `virtio_input` driver, the evdev node, logind's TakeDevice — which an in-guest uinput
//! injector bypasses. So the oracle here is the guest's own `/dev/input/event*` nodes for the
//! virtio keyboard, tablet and mouse, read raw (`struct input_event`) and compared event for
//! event with what each verb promises to send.
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

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The in-guest reader: grab the three virtio nodes (found by name), print every event as
/// `<dev> <type> <code> <value>`, and stop on the end marker (KEY_F14 released).
const READER: &str = r#"
import fcntl, os, select, struct, sys, time
NAMES = {"kbd": "limina Virtual Keyboard", "ptr": "limina Virtual Pointer",
         "rel": "limina Virtual Mouse"}
nodes = {}
for block in open("/proc/bus/input/devices").read().split("\n\n"):
    name = ev = None
    for line in block.splitlines():
        if line.startswith("N: Name="):
            name = line[len("N: Name="):].strip().strip('"')
        if line.startswith("H: Handlers="):
            ev = next((h for h in line.split("=", 1)[1].split() if h.startswith("event")), None)
    for k, v in NAMES.items():
        if name == v and ev:
            nodes[k] = "/dev/input/" + ev
assert len(nodes) == 3, nodes
fds = {}
for k, p in nodes.items():
    fd = os.open(p, os.O_RDONLY | os.O_NONBLOCK)
    fcntl.ioctl(fd, 0x40044590, 1)  # EVIOCGRAB
    fds[fd] = k
print("NODES", nodes, flush=True)
open("/tmp/limina-inject-ready", "w").write("ready")
deadline = time.time() + float(sys.argv[1])
done = False
while time.time() < deadline and not done:
    ready, _, _ = select.select(list(fds), [], [], 0.5)
    for fd in ready:
        while True:
            try:
                data = os.read(fd, 24 * 64)
            except BlockingIOError:
                break
            if not data:
                break
            for i in range(0, len(data), 24):
                _, _, t, c, v = struct.unpack("qqHHi", data[i:i + 24])
                print(fds[fd], t, c, v, flush=True)
                if fds[fd] == "kbd" and t == 1 and c == 184 and v == 0:
                    done = True
print("END" if done else "TIMEOUT", flush=True)
"#;

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
    let (f13, f14, lctrl, lshift, a, b, one) = (183, 184, 29, 42, 30, 48, 2);
    let mut kbd = tap(f13);
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

/// Feed the script to `limina input <supervisor pid> -` and return its output; panics on failure.
fn inject(limina: &Path, pid: libc::pid_t) -> String {
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
        .write_all(SCRIPT.as_bytes())
        .unwrap();
    let out = child
        .wait_with_output()
        .expect("`limina input` did not run");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "`limina input` failed: {text}");
    text
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
/// which the window only writes to while it is key and typed on, stays exact.
fn run(name: &str, cfg: GuestConfig, window_live: bool) {
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
             grep -q 'limina Virtual Mouse' /proc/bus/input/devices",
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
        let said = inject(&cfg.limina_bin, guest.supervisor_pid());
        eprintln!("{name}: limina input said:\n{said}");
        reader.join().unwrap().expect("the guest reader failed")
    });
    eprintln!("{name}: the guest read:\n{output}");
    assert!(
        output.contains("END"),
        "{name}: the end marker never reached the virtio keyboard:\n{output}"
    );
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
/// and `--input` wiring the virtio input devices.
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
    run(name, cfg, false);
}

/// Windowed — the window's own input path is live alongside; injection must reach the same
/// devices through the worker connection the window uses.
#[test]
fn l2_input_inject_windowed_reaches_virtio_nodes() {
    let name = "l2_input_inject_windowed_reaches_virtio_nodes";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg.with_windowed_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };
    run(name, cfg, true);
}
