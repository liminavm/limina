// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Host-side input injection: evdev events written straight into the worker's virtio-input
//! sockets, for a harness that has to drive a guest with no human at the keyboard.
//!
//! The supervisor holds its ends of the worker's input socketpairs (the same [`WorkerConn`] the
//! window writes to), so an injected event takes exactly the path a real one does from there:
//! the worker's virtio-input backends, the guest kernel's `virtio_input` driver, its evdev node,
//! and whatever the guest's seat does with that (logind, libinput, the compositor). Nothing
//! above the socket is involved — no `NSEvent`, no window focus, no capture state — which is the
//! point, and also the caveat: the window's own bookkeeping does not know about injected input
//! (`docs/input-and-windows.md` §9).
//!
//! The verbs ride the per-VM runtime socket (`runtime_ctl`) as `input <verb…>` lines and are
//! answered like every other request there (`ok` / `err <why>`). [`HELP`] is the reference;
//! `limina input` is the command-line client.
//!
//! **What a connection holds dies with it.** Keys and buttons a connection pressed and did not
//! release are released when it closes, so a harness that crashes mid-chord cannot leave Ctrl
//! held in the guest. A harness that wants a key held across commands keeps one connection open
//! (`limina input <vm> -`, verbs on stdin), or says `keep-held`, which leaves them pressed until
//! a later connection's `release`.
//!
//! **Off by default.** Every verb is refused until the VM's `input-inject` lever is on
//! (`crate::debug_ctl::INPUT_INJECT`: `LIMINA_INPUT_INJECT=1`, the window's Debug menu, or
//! `limina debug <vm> lever input-inject on`). The lever is read per verb, so a change applies to
//! the next one; turning it off also releases everything injection holds pressed, in every live
//! connection and every `keep-held` one ([`lever_off`]).

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use limina_debug::wire;
use limina_input::InputEvent;
use limina_input::constants::*;
use limina_input::names;
use limina_input::touchpad::{
    Contact, MAX_SLOTS, MAX_TRACKING_ID, TOUCHPAD_RES, Touchpad, TouchpadGeometry,
};

use crate::window::{InputDev, WorkerConn, WorkerIo};

/// The reference for the verbs, printed by `limina input --help`.
pub const HELP: &str = "\
VERBS (one per line on stdin, or one on the command line):
  key tap|down|up KEY[+KEY...]   KEY_* name (case-insensitive, KEY_ optional) or code;
                                 a chord is pressed in order and released in reverse
  type TEXT                      the rest of the line, typed on a US layout
                                 (escapes: \\n \\t \\\\); each shifted character is
                                 wrapped in its own LEFTSHIFT press
  abs X Y                        absolute pointer, device units 0..=32767
  abs-norm [--display N] U V     absolute pointer, 0..1 of the device range; with
                                 --display, 0..1 of display N
  abs-px [--display N] X Y [WxH] absolute pointer at scanout pixel (X,Y) of a WxH mode;
                                 with --display, a pixel of display N. WxH defaults to the
                                 guest's current mode (windowed) or the boot's
                                 --display-size (headless, display 0 only)
  rel DX DY                      relative motion on the virtual mouse
  button down|up|click BTN [N]   BTN_LEFT|BTN_RIGHT|BTN_MIDDLE (or left/right/middle);
                                 click N times, transitions 30 ms apart
  scroll V [H]                   wheel, in detents (fractions allowed): V>0 is up/away,
                                 H>0 is right; sends hi-res (v120) and detent events
  touchpad click|tap N [--at U,V]
                                 N fingers (1-3) on the virtual touchpad, side by side
                                 18 mm apart around (U,V) of its surface (0..1; default
                                 the centre). click: land, press and release the pad's
                                 button, lift. tap: land, lift 60 ms later. The guest
                                 decides what it is (its click method; a tap does
                                 nothing with tap-to-click off). Not while a real
                                 finger is on the trackpad
  ev DEV TYPE CODE VALUE         raw event, no SYN (DEV: kbd|ptr|rel|touchpad)
  syn [DEV]                      a SYN_REPORT on DEV, or on every device
  release                        release what this connection holds, and what
                                 keep-held connections left pressed
  keep-held                      keep what this connection holds pressed after it closes
  info                           the devices, and one line per display: its scanout,
                                 its guest connector, its rect in the guest's layout,
                                 and its share of the absolute pointer's range
  sleep MS                       (client side) pause between verbs
  keys                           (client side) every KEY name `key` accepts and its
                                 code, one `NAME CODE` per line: exactly the keys the
                                 virtual keyboard advertises; needs no VM and no lever

Every verb but `ev` ends each of its frames with a SYN_REPORT. The absolute device spans
the guest's WHOLE desktop: without --display, abs-norm/abs-px are that whole range, which
is one display's coordinates only while the guest has one. --display N (the scanout slot,
0-based: the guest's connector Virtual-(N+1)) maps a point on display N exactly as the
window showing it would: through the layout the guest's agent reports, else the mapping
the window learned from the guest's cursor echo, else -- with one display only -- the
whole range. A display that does not exist, or whose place in a multi-display desktop
is not known, is refused; `info` says which displays exist and how each maps.

Keys are evdev codes, not macOS keys: no modifier normalization applies (KEY_LEFTMETA is
Super, KEY_LEFTALT is Alt).
Keys and buttons a connection leaves pressed are released when it closes, unless
`keep-held` (or --keep-held) was sent.

On stdin, verbs run in order and the FIRST verb that fails stops the script: the verbs
before it have already reached the guest, none after it is sent, the error names its
line, and `limina input` exits non-zero. A single verb on the command line exits
non-zero when it fails, too. A refusal (the lever is off, an unknown key) is a failure.

Off by default: the VM refuses every verb until its input-inject lever is on. Start
it with LIMINA_INPUT_INJECT=1, tick input-inject in the window's Debug menu, or run
`limina debug <vm> lever input-inject on`. Turning the lever off releases every key
and button injection holds pressed.";

/// The current worker's input endpoints, once a session has any (`attach`/`publish`).
static CONN: Mutex<Option<Arc<WorkerConn>>> = Mutex::new(None);

/// Bumped before every injected event that moves the guest's pointer. The window's echo
/// sampler pairs the guest's cursor with the window's *own* last send; a pointer an injection
/// moved answers nothing the window sent, so the window skips any send this has moved past
/// (`InputState::verify_guest_echo`).
static POINTER_EPOCH: AtomicU64 = AtomicU64::new(0);

/// A headless run's display mode, for `abs-px` without an explicit size.
static HEADLESS_MODE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// Keys and buttons a `keep-held` connection left pressed, for a later `release`.
static ORPHANS: Mutex<Held> = Mutex::new(Held::new());

/// Every live connection's held set, so that turning the lever off can release them all — a
/// harness idling on an open connection with Ctrl down must not keep it down.
static LIVE: Mutex<Vec<Weak<Mutex<Held>>>> = Mutex::new(Vec::new());

/// The answer to a verb while the `input-inject` lever is off.
pub fn refusal() -> String {
    format!(
        "input injection is off for this VM; to allow it, {}",
        crate::debug_ctl::how_to_enable(&crate::debug_ctl::INPUT_INJECT)
    )
}

fn allowed() -> bool {
    crate::debug_ctl::INPUT_INJECT.on()
}

/// The `input-inject` lever was just turned off: release what every connection holds and what
/// `keep-held` connections left behind. Called after the lever is off, so a verb still sending
/// sees it off after its last event and releases what it pressed itself (`Session::run`).
pub fn lever_off() {
    let mut all = std::mem::take(&mut *lock(&ORPHANS));
    let live: Vec<_> = lock(&LIVE).iter().filter_map(Weak::upgrade).collect();
    for held in live {
        all.absorb(std::mem::take(&mut *lock(&held)));
    }
    if all.is_empty() {
        return;
    }
    log::warn!(
        "input: injection turned off; releasing keys {:?} and buttons {:?}",
        all.keys,
        all.buttons
    );
    let frames = all.release_frames();
    if let Err((sent, why)) = send_frames(&frames, &Mutex::new(Held::new())) {
        log::warn!("input: could not release what injection held ({why}); `release` will retry");
        lock(&ORPHANS).absorb(Held::unreleased(&frames[sent..]));
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Make this connection the injection target. It is swapped on every worker relaunch by its
/// owner, so injection follows the current worker for free.
pub fn attach(conn: Arc<WorkerConn>) {
    *lock(&CONN) = Some(conn);
}

/// Publish a freshly spawned headless worker's endpoints.
pub fn publish(io: WorkerIo) {
    let mut conn = lock(&CONN);
    match conn.as_ref() {
        Some(c) => c.swap(io),
        None => *conn = Some(WorkerConn::new(io)),
    }
}

/// See [`POINTER_EPOCH`].
pub fn pointer_epoch() -> u64 {
    POINTER_EPOCH.load(Ordering::Acquire)
}

/// Record a headless run's display mode (its `--display-size`).
pub fn set_headless_mode(mode: (u32, u32)) {
    *lock(&HEADLESS_MODE) = Some(mode);
}

/// One of the worker's input devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    Kbd,
    Ptr,
    Rel,
    Touchpad,
}

impl Device {
    const ALL: [Device; 4] = [Device::Kbd, Device::Ptr, Device::Rel, Device::Touchpad];

    fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "kbd" => Device::Kbd,
            "ptr" => Device::Ptr,
            "rel" => Device::Rel,
            "touchpad" => Device::Touchpad,
            _ => return Err(format!("unknown device {s:?} (kbd|ptr|rel|touchpad)")),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Device::Kbd => "kbd",
            Device::Ptr => "ptr",
            Device::Rel => "rel",
            Device::Touchpad => "touchpad",
        }
    }

    fn frame_lock(self) -> InputDev {
        match self {
            Device::Kbd => InputDev::Kbd,
            Device::Ptr => InputDev::Ptr,
            Device::Rel => InputDev::Rel,
            Device::Touchpad => InputDev::Touchpad,
        }
    }

    fn fd(self, io: &WorkerIo) -> RawFd {
        match self {
            Device::Kbd => io.kbd_fd(),
            Device::Ptr => io.ptr_fd(),
            Device::Rel => io.rel_ptr_fd(),
            Device::Touchpad => io.touchpad_fd(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    Tap,
}

/// One parsed verb.
#[derive(Clone, Debug, PartialEq)]
pub enum Verb {
    Key {
        action: Action,
        keys: Vec<u16>,
    },
    Type(Vec<(u16, bool)>),
    Abs {
        x: i32,
        y: i32,
    },
    AbsNorm {
        u: f64,
        v: f64,
        /// `--display N`: a fraction of that display, not of the whole device range.
        display: Option<usize>,
    },
    AbsPx {
        x: f64,
        y: f64,
        mode: Option<(u32, u32)>,
        /// `--display N`: a pixel of that display, wherever the guest put it in its desktop.
        display: Option<usize>,
    },
    Rel {
        dx: i32,
        dy: i32,
    },
    Button {
        action: Action,
        button: u16,
        count: u32,
    },
    Scroll {
        v: f64,
        h: f64,
    },
    /// Fingers on the virtual touchpad: a physical click, or a tap.
    Touchpad {
        tap: bool,
        fingers: usize,
        /// Where on the surface, as fractions of it (top-left origin).
        at: (f64, f64),
    },
    Ev {
        dev: Device,
        ev: InputEvent,
    },
    Syn(Option<Device>),
    Release,
    KeepHeld,
    Info,
}

fn num<T: std::str::FromStr>(s: Option<&str>, what: &str) -> Result<T, String> {
    let s = s.ok_or_else(|| format!("missing {what}"))?;
    s.parse()
        .map_err(|_| format!("{s:?} is not a valid {what}"))
}

fn unit(s: Option<&str>, what: &str) -> Result<f64, String> {
    let v: f64 = num(s, what)?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("{what} {v} is outside 0..1"))
    }
}

fn action(s: Option<&str>, click: &str) -> Result<Action, String> {
    match s {
        Some("down") => Ok(Action::Down),
        Some("up") => Ok(Action::Up),
        Some(w) if w == click => Ok(Action::Tap),
        other => Err(format!("expected down|up|{click}, got {other:?}")),
    }
}

/// The rest of a verb's words with `--display N` (or `--display=N`) taken out of them, wherever
/// it stands, and N: a scanout slot, 0-based — the `display_id` the display control speaks and
/// the `slot N` the logs name, one less than the guest's `Virtual-N` connector.
fn take_display<'a>(
    words: &mut impl Iterator<Item = &'a str>,
) -> Result<(Option<usize>, Vec<&'a str>), String> {
    let mut display = None;
    let mut rest = Vec::new();
    while let Some(word) = words.next() {
        let value = match word.strip_prefix("--display") {
            Some("") => Some(words.next().ok_or("--display needs a display number")?),
            Some(v) if v.starts_with('=') => Some(&v[1..]),
            _ => None,
        };
        let Some(value) = value else {
            rest.push(word);
            continue;
        };
        if display.is_some() {
            return Err("--display given twice".into());
        }
        let n = value
            .parse::<usize>()
            .ok()
            .filter(|n| *n < crate::window::echo::MAX_SCANOUTS)
            .ok_or_else(|| {
                format!(
                    "--display {value:?} is not a display number 0..{}",
                    crate::window::echo::MAX_SCANOUTS - 1
                )
            })?;
        display = Some(n);
    }
    Ok((display, rest))
}

/// `WxH`.
pub fn parse_mode(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("{s:?} is not a WIDTHxHEIGHT mode"))?;
    match (w.parse::<u32>(), h.parse::<u32>()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(format!("{s:?} is not a WIDTHxHEIGHT mode")),
    }
}

/// `type`'s text, with `\n`, `\t` and `\\` unescaped, as key presses on a US layout. All of it
/// is checked before anything is sent: a string that cannot be typed is refused whole.
fn type_keys(text: &str) -> Result<Vec<(u16, bool)>, String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        let c = if c == '\\' {
            match chars.next() {
                Some('n') => '\n',
                Some('t') => '\t',
                Some('\\') => '\\',
                other => return Err(format!("unknown escape \\{}", other.unwrap_or(' '))),
            }
        } else {
            c
        };
        out.push(names::us_ascii(c).ok_or_else(|| format!("no US key types {c:?}"))?);
    }
    if out.is_empty() {
        return Err("type needs some text".into());
    }
    Ok(out)
}

impl Verb {
    /// Parse one verb (the text after `input `).
    pub fn parse(line: &str) -> Result<Verb, String> {
        let line = line.trim_start();
        // `type` keeps the rest of the line verbatim, spaces and all.
        if let Some(text) = line.strip_prefix("type ") {
            return Ok(Verb::Type(type_keys(text)?));
        }
        let mut w = line.split_whitespace();
        let verb = match w.next().ok_or("empty input verb")? {
            "key" => {
                let action = action(w.next(), "tap")?;
                let spec = w.next().ok_or("key needs KEY[+KEY...]")?;
                let keys = spec
                    .split('+')
                    .map(names::key)
                    .collect::<Result<Vec<_>, _>>()?;
                Verb::Key { action, keys }
            }
            "type" => return Err("type needs some text".into()),
            "abs" => {
                let x: i32 = num(w.next(), "x")?;
                let y: i32 = num(w.next(), "y")?;
                let range = 0..=ABS_MAX as i32;
                if !range.contains(&x) || !range.contains(&y) {
                    return Err(format!("abs takes device units 0..={ABS_MAX}"));
                }
                Verb::Abs { x, y }
            }
            "abs-norm" => {
                let (display, rest) = take_display(&mut w)?;
                let mut w = rest.into_iter();
                let verb = Verb::AbsNorm {
                    u: unit(w.next(), "u")?,
                    v: unit(w.next(), "v")?,
                    display,
                };
                if let Some(extra) = w.next() {
                    return Err(format!("unexpected {extra:?} after the verb"));
                }
                verb
            }
            "abs-px" => {
                let (display, rest) = take_display(&mut w)?;
                let mut w = rest.into_iter();
                let x: f64 = num(w.next(), "x")?;
                let y: f64 = num(w.next(), "y")?;
                let mode = w.next().map(parse_mode).transpose()?;
                if x < 0.0 || y < 0.0 || !x.is_finite() || !y.is_finite() {
                    return Err("abs-px takes non-negative pixels".into());
                }
                if let Some(extra) = w.next() {
                    return Err(format!("unexpected {extra:?} after the verb"));
                }
                Verb::AbsPx {
                    x,
                    y,
                    mode,
                    display,
                }
            }
            "rel" => Verb::Rel {
                dx: num(w.next(), "dx")?,
                dy: num(w.next(), "dy")?,
            },
            "button" => {
                let action = action(w.next(), "click")?;
                let button = names::button(w.next().ok_or("button needs a button")?)?;
                let count = match (action, w.next()) {
                    (Action::Tap, Some(n)) => n
                        .parse::<u32>()
                        .ok()
                        .filter(|n| (1..=10).contains(n))
                        .ok_or_else(|| format!("click count {n:?} is not 1..10"))?,
                    (_, Some(extra)) => return Err(format!("unexpected {extra:?}")),
                    (_, None) => 1,
                };
                Verb::Button {
                    action,
                    button,
                    count,
                }
            }
            "scroll" => {
                let v: f64 = num(w.next(), "vertical detents")?;
                let h: f64 = match w.next() {
                    Some(h) => num(Some(h), "horizontal detents")?,
                    None => 0.0,
                };
                if !v.is_finite() || !h.is_finite() || v.abs() > 1000.0 || h.abs() > 1000.0 {
                    return Err("scroll takes detents in -1000..1000".into());
                }
                Verb::Scroll { v, h }
            }
            "touchpad" => {
                let tap = match w.next() {
                    Some("click") => false,
                    Some("tap") => true,
                    other => return Err(format!("expected touchpad click|tap, got {other:?}")),
                };
                let fingers = w
                    .next()
                    .ok_or("touchpad needs a finger count")?
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=MAX_SLOTS).contains(n))
                    .ok_or_else(|| format!("touchpad takes 1..={MAX_SLOTS} fingers"))?;
                let at = match w.next() {
                    None => (0.5, 0.5),
                    Some(word) => {
                        let spot = match word.strip_prefix("--at") {
                            Some("") => w.next().ok_or("--at needs U,V")?,
                            Some(v) if v.starts_with('=') => &v[1..],
                            _ => return Err(format!("unexpected {word:?} after the verb")),
                        };
                        let (u, v) = spot
                            .split_once(',')
                            .ok_or_else(|| format!("--at {spot:?} is not U,V"))?;
                        (unit(Some(u), "u")?, unit(Some(v), "v")?)
                    }
                };
                Verb::Touchpad { tap, fingers, at }
            }
            "ev" => {
                let dev = Device::parse(w.next().ok_or("ev needs DEV TYPE CODE VALUE")?)?;
                let type_ = names::event_type(w.next().ok_or("ev needs TYPE")?)?;
                let code = names::event_code(w.next().ok_or("ev needs CODE")?)?;
                let value: i32 = num(w.next(), "value")?;
                Verb::Ev {
                    dev,
                    ev: InputEvent::new(type_, code, value),
                }
            }
            "syn" => Verb::Syn(w.next().map(Device::parse).transpose()?),
            "release" => Verb::Release,
            "keep-held" => Verb::KeepHeld,
            "info" => Verb::Info,
            "sleep" => return Err("sleep is the client's (limina input), not the VM's".into()),
            "keys" => return Err("keys is the client's (limina input), not the VM's".into()),
            other => return Err(format!("unknown input verb {other:?}")),
        };
        if let Some(extra) = w.next() {
            return Err(format!("unexpected {extra:?} after the verb"));
        }
        Ok(verb)
    }
}

/// `round(u · ABS_MAX)`: the device value for a fraction of the range.
pub fn norm_to_device(u: f64) -> i32 {
    (u * f64::from(ABS_MAX)).round() as i32
}

/// The device value libinput maps back onto scanout pixel `p` of a `size`-pixel axis.
///
/// libinput scales an absolute value as `v · size / (max − min + 1)`, and a compositor may
/// truncate or round the result to a pixel. Aiming at `p + ¼` — `floor((p + ¼) · 32768 / size)`
/// — lands the image in `(p + ¼ − size/32768, p + ¼]`, which is pixel `p` under both rules for
/// any `size ≤ 8192`. (Aiming at the centre, `p + ½`, rounds up to `p + 1` whenever the
/// division is exact.) With one display at any scale this holds: the range spreads over the
/// logical desktop and the scale cancels on the way back to pixels.
pub fn px_to_device(p: f64, size: u32) -> i32 {
    px_in_share(p, size, 0.0, 1.0)
}

/// Keys and buttons pressed and not yet released, each in the order it was pressed — so a
/// release can undo them in reverse, as a hand would (Super-Tab lets go of Tab first) — and
/// the touchpad's button and fingers (`BTN_LEFT`, `BTN_TOUCH`), should a `touchpad` verb stop
/// halfway.
#[derive(Debug, Default)]
pub struct Held {
    keys: Vec<u16>,
    buttons: Vec<u16>,
    pad: Vec<u16>,
}

fn press(set: &mut Vec<u16>, code: u16) {
    if !set.contains(&code) {
        set.push(code);
    }
}

impl Held {
    const fn new() -> Self {
        Held {
            keys: Vec::new(),
            buttons: Vec::new(),
            pad: Vec::new(),
        }
    }

    fn note(&mut self, dev: Device, ev: InputEvent) {
        if ev.type_ != EV_KEY {
            return;
        }
        let set = match dev {
            Device::Kbd => &mut self.keys,
            Device::Ptr => &mut self.buttons,
            Device::Touchpad if matches!(ev.code, BTN_LEFT | BTN_TOUCH) => &mut self.pad,
            _ => return,
        };
        if ev.value == 0 {
            set.retain(|&c| c != ev.code);
        } else {
            press(set, ev.code);
        }
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty() && self.pad.is_empty()
    }

    fn absorb(&mut self, other: Held) {
        for k in other.keys {
            press(&mut self.keys, k);
        }
        for b in other.buttons {
            press(&mut self.buttons, b);
        }
        for p in other.pad {
            press(&mut self.pad, p);
        }
    }

    /// The frames that release all of it: keys last-pressed first, then buttons likewise, then
    /// the touchpad's button and its fingers.
    fn release_frames(&self) -> Vec<Frame> {
        let keys = self.keys.iter().rev().map(|&k| (Device::Kbd, k));
        let buttons = self.buttons.iter().rev().map(|&b| (Device::Ptr, b));
        let pad_button = self
            .pad
            .contains(&BTN_LEFT)
            .then_some((Device::Touchpad, BTN_LEFT));
        let mut frames: Vec<Frame> = keys
            .chain(buttons)
            .chain(pad_button)
            .map(|(dev, code)| Frame::one(dev, InputEvent::new(EV_KEY, code, 0)))
            .collect();
        if self.pad.contains(&BTN_TOUCH)
            && let Some(pad) = lock(&TOUCHPAD).as_mut()
        {
            // The encoder's lift ends in its own SYN_REPORT.
            frames.push(Frame {
                dev: Device::Touchpad,
                events: pad.release_all(),
                syn: false,
                pause: Duration::ZERO,
            });
        }
        frames
    }

    /// What release frames that were never sent still hold, in press order again.
    fn unreleased(frames: &[Frame]) -> Held {
        let mut held = Held::new();
        for f in frames.iter().rev() {
            for &ev in &f.events {
                held.note(f.dev, InputEvent::new(ev.type_, ev.code, 1));
            }
        }
        held
    }
}

/// Events for one device, sent as one frame (a SYN_REPORT follows unless `syn` is false), after
/// waiting `pause`.
#[derive(Clone, Debug, PartialEq)]
struct Frame {
    dev: Device,
    events: Vec<InputEvent>,
    syn: bool,
    pause: Duration,
}

impl Frame {
    fn new(dev: Device, events: Vec<InputEvent>) -> Self {
        Frame {
            dev,
            events,
            syn: true,
            pause: Duration::ZERO,
        }
    }

    fn one(dev: Device, ev: InputEvent) -> Self {
        Self::new(dev, vec![ev])
    }

    fn after(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }
}

/// How far apart a click's transitions go: libinput debounces a tablet's buttons, and a
/// release→press→release inside 25 ms reads as contact bounce (`window/button_pace.rs`).
const BUTTON_PACE: Duration = Duration::from_millis(30);

fn key(code: u16, down: bool) -> Frame {
    Frame::one(Device::Kbd, InputEvent::new(EV_KEY, code, i32::from(down)))
}

/// How far apart `touchpad`'s fingers land, side by side: about where two fingertips rest.
const FINGER_SPACING_MM: i32 = 18;

/// How long `touchpad tap`'s fingers stay down: well inside libinput's 180 ms tap timeout.
const TAP_HOLD: Duration = Duration::from_millis(60);

/// How long `touchpad click`'s fingers rest before the press, and after the release.
const CLICK_SETTLE: Duration = Duration::from_millis(20);

/// The touchpad encoder injection writes with: its tracking ids start halfway up the range, away
/// from the ones the window's touches take from zero.
static TOUCHPAD: Mutex<Option<Touchpad>> = Mutex::new(None);

/// `touchpad click|tap`'s frames: `fingers` contacts landing side by side around `at`, then
/// either the pad's button pressed and released, or nothing, and the lift.
fn touchpad_frames(
    pad: &mut Touchpad,
    geometry: TouchpadGeometry,
    tap: bool,
    fingers: usize,
    at: (f64, f64),
) -> Vec<Frame> {
    let (cx, cy) = geometry.to_units(at.0, at.1);
    let spacing = FINGER_SPACING_MM * TOUCHPAD_RES as i32;
    let contacts: Vec<Contact> = (0..fingers)
        .map(|i| {
            let offset = (2 * i as i32 - (fingers as i32 - 1)) * spacing / 2;
            Contact {
                id: i as u64,
                x: (cx + offset).clamp(0, geometry.width as i32),
                y: cy,
            }
        })
        .collect();
    // The encoder's frames end in their own SYN_REPORT.
    let raw = |events: Vec<InputEvent>| Frame {
        dev: Device::Touchpad,
        events,
        syn: false,
        pause: Duration::ZERO,
    };
    let button = |down: bool| {
        Frame::one(
            Device::Touchpad,
            InputEvent::new(EV_KEY, BTN_LEFT, i32::from(down)),
        )
    };
    let mut frames = vec![raw(pad.frame(&contacts))];
    if tap {
        frames.push(raw(pad.release_all()).after(TAP_HOLD));
    } else {
        frames.push(button(true).after(CLICK_SETTLE));
        frames.push(button(false).after(BUTTON_PACE));
        frames.push(raw(pad.release_all()).after(CLICK_SETTLE));
    }
    frames
}

/// One connection's injection state: what it holds, and the scroll remainders.
#[derive(Debug)]
pub struct Session {
    /// Shared with [`LIVE`], so [`lever_off`] can release it from another thread.
    held: Arc<Mutex<Held>>,
    keep: bool,
    /// Sub-detent wheel motion carried between `scroll`s, in v120 units, so two half detents
    /// make one detent event — the dual-rate rule `window/input.rs` follows.
    scroll_v: i32,
    scroll_h: i32,
}

/// The current guest mode `abs-px` maps against when none is given.
fn current_mode() -> Result<(u32, u32), String> {
    let sizes = crate::window::echo::scanout_sizes();
    let live: Vec<_> = sizes.iter().filter(|(w, h)| *w > 0 && *h > 0).collect();
    match live.as_slice() {
        [one] => return Ok(**one),
        [] => {}
        many => {
            return Err(format!(
                "the guest has {} displays and the absolute device spans all of them; \
                 name one: abs-px --display N X Y (see `limina input <vm> info`)",
                many.len()
            ));
        }
    }
    lock(&HEADLESS_MODE)
        .ok_or_else(|| "the guest's mode is not known yet; give it: abs-px X Y WxH".into())
}

/// The device value that lands on pixel `p` of a `size`-pixel axis whose content spans
/// `f0..f1` of the device range (fractions of it): [`px_to_device`]'s ¼-pixel aim, placed in
/// that share. With the whole range (`0..1`) it *is* `px_to_device`.
///
/// The guest maps a value back as `v · desktop / 32768` and then onto the display's pixels at
/// its scale, so the image lands in `(p + ¼ − desktop·scale/32768, p + ¼]`: pixel `p` under
/// truncation and rounding alike while the desktop is under 8192 pixels wide at that scale.
pub fn px_in_share(p: f64, size: u32, f0: f64, f1: f64) -> i32 {
    let at = f0 + (p + 0.25) / f64::from(size) * (f1 - f0);
    (at * f64::from(ABS_MAX + 1))
        .floor()
        .clamp(0.0, f64::from(ABS_MAX)) as i32
}

/// One guest display as the supervisor knows it, for `--display` and `info`.
#[derive(Clone, Debug, PartialEq)]
struct Display {
    /// The scanout slot: `--display`'s number, the guest's `Virtual-{slot + 1}`.
    slot: usize,
    /// Its mode in guest pixels, where the supervisor knows it: from the worker's scanout
    /// announcements in a windowed run, from `--display-size` for a headless run's slot 0.
    scanout: Option<(u32, u32)>,
    /// Its rect in the guest's logical desktop, where the guest's agent reported one.
    logical: Option<crate::window::arrangement::GuestRect>,
    /// How the window maps a point on it into the device range (`absfit::Mapping`).
    mapping: crate::window::absfit::Mapping,
}

/// Every display the supervisor knows of, taken once per verb.
#[derive(Clone, Debug, Default, PartialEq)]
struct Displays(Vec<Display>);

impl Displays {
    /// What the supervisor knows now: the live scanouts (windowed), the `--display-size` slot
    /// (headless), and every slot the guest's layout report names.
    fn now() -> Self {
        let sizes = crate::window::echo::scanout_sizes();
        let report = crate::window::arrangement::reported_layout();
        let mut slots: std::collections::BTreeMap<usize, Option<(u32, u32)>> = sizes
            .iter()
            .enumerate()
            .filter(|(_, (w, h))| *w > 0 && *h > 0)
            .map(|(s, m)| (s, Some(*m)))
            .collect();
        if let Some(mode) = *lock(&HEADLESS_MODE) {
            slots.entry(0).or_insert(Some(mode));
        }
        for (s, _) in report.iter().flatten() {
            slots.entry(*s).or_insert(None);
        }
        Displays(
            slots
                .into_iter()
                .map(|(slot, scanout)| Display {
                    slot,
                    scanout,
                    logical: report
                        .iter()
                        .flatten()
                        .find(|(s, _)| *s == slot)
                        .map(|(_, r)| *r),
                    mapping: crate::window::absfit::mapping(slot),
                })
                .collect(),
        )
    }

    fn known(&self) -> String {
        if self.0.is_empty() {
            return "this VM knows of no display".into();
        }
        let names: Vec<String> = self.0.iter().map(|d| d.slot.to_string()).collect();
        format!("the displays are {}", names.join(", "))
    }

    /// The display `--display n` names, if a point on it can be mapped exactly the way the
    /// window maps one: refused when it does not exist, and when the window's only answer for
    /// it is the identity floor (several displays and nothing says where this one is).
    fn target(&self, n: usize) -> Result<&Display, String> {
        let d = self.0.iter().find(|d| d.slot == n).ok_or_else(|| {
            format!(
                "display {n} does not exist or has no scanout ({}; see `limina input <vm> info`)",
                self.known()
            )
        })?;
        if let Some(why) = self.unmapped(d) {
            return Err(format!("display {n} cannot be pointed at: {why}"));
        }
        Ok(d)
    }

    /// Why a point on `d` has no exact place in the device range, or `None` when it has one.
    fn unmapped(&self, d: &Display) -> Option<String> {
        use crate::window::absfit::Mapping;
        match &d.mapping {
            Mapping::Report(_) if d.logical.is_none() => Some(format!(
                "the layout the guest reported does not place Virtual-{}",
                d.slot + 1
            )),
            Mapping::Identity if self.0.len() > 1 => Some(format!(
                "the guest has {} displays and the absolute device spans all of them, but the \
                 guest has reported no layout (limina-agent-session) and none has been learned \
                 from its cursor echo, so where this one sits is unknown",
                self.0.len()
            )),
            _ => None,
        }
    }

    /// `info`'s line for each display.
    fn report(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|d| {
                let scanout = d
                    .scanout
                    .map_or("unknown".to_string(), |(w, h)| format!("{w}x{h}"));
                let logical = d.logical.map_or("unreported".to_string(), |r| {
                    format!("{}x{}+{}+{}", r.w, r.h, r.x, r.y)
                });
                let mapping = match self.unmapped(d) {
                    Some(why) => format!("none ({why})"),
                    None => {
                        let range = f64::from(ABS_MAX);
                        let at = |f: f64| (f * range).round() as i32;
                        match d.mapping.share(d.slot) {
                            Some(s) => format!(
                                "{} x {}..{} y {}..{}",
                                d.mapping.name(),
                                at(s.x0),
                                at(s.x1),
                                at(s.y0),
                                at(s.y1)
                            ),
                            None => d.mapping.name().to_string(),
                        }
                    }
                };
                format!(
                    "display {} connector Virtual-{} scanout {scanout} logical {logical} \
                     device ptr mapping {mapping}",
                    d.slot,
                    d.slot + 1
                )
            })
            .collect()
    }
}

impl Default for Session {
    fn default() -> Self {
        let held = Arc::new(Mutex::new(Held::new()));
        let mut live = lock(&LIVE);
        live.retain(|w| w.strong_count() > 0);
        live.push(Arc::downgrade(&held));
        Session {
            held,
            keep: false,
            scroll_v: 0,
            scroll_h: 0,
        }
    }
}

impl Session {
    /// The frames a verb sends, and the report lines it answers with, against the displays as
    /// the supervisor knows them now.
    fn plan(&mut self, verb: Verb) -> Result<(Vec<Frame>, Vec<String>), String> {
        self.plan_in(verb, Displays::now)
    }

    /// [`Self::plan`] against the displays `displays` describes (read only by the verbs that
    /// need them). Pure but for the mode lookup and the held-set bookkeeping, so it is what the
    /// tests drive.
    fn plan_in(
        &mut self,
        verb: Verb,
        displays: impl FnOnce() -> Displays,
    ) -> Result<(Vec<Frame>, Vec<String>), String> {
        let abs = |x: i32, y: i32| {
            vec![Frame::new(
                Device::Ptr,
                vec![
                    InputEvent::new(EV_ABS, ABS_X, x),
                    InputEvent::new(EV_ABS, ABS_Y, y),
                ],
            )]
        };
        let frames = match verb {
            Verb::Key { action, keys } => {
                let downs = keys.iter().map(|&k| key(k, true));
                let ups = keys.iter().rev().map(|&k| key(k, false));
                match action {
                    Action::Down => downs.collect(),
                    Action::Up => ups.collect(),
                    Action::Tap => downs.chain(ups).collect(),
                }
            }
            Verb::Type(keys) => keys
                .into_iter()
                .flat_map(|(k, shift)| {
                    let mut f = Vec::with_capacity(4);
                    if shift {
                        f.push(key(KEY_LEFTSHIFT, true));
                    }
                    f.push(key(k, true));
                    f.push(key(k, false));
                    if shift {
                        f.push(key(KEY_LEFTSHIFT, false));
                    }
                    f
                })
                .collect(),
            Verb::Abs { x, y } => abs(x, y),
            Verb::AbsNorm {
                u,
                v,
                display: None,
            } => abs(norm_to_device(u), norm_to_device(v)),
            Verb::AbsNorm {
                u,
                v,
                display: Some(n),
            } => {
                let displays = displays();
                let d = displays.target(n)?;
                let (x, y) = d
                    .mapping
                    .place(n, (u, v), ABS_MAX as i32)
                    .ok_or_else(|| format!("display {n} has no place in the device range"))?;
                abs(x, y)
            }
            Verb::AbsPx {
                x,
                y,
                mode,
                display: Some(n),
            } => {
                let displays = displays();
                let d = displays.target(n)?;
                let (w, h) = mode.or(d.scanout).ok_or_else(|| {
                    format!(
                        "display {n}'s mode is not known to this VM (a headless run knows only \
                         display 0's --display-size); give it: abs-px --display {n} X Y WxH"
                    )
                })?;
                if x >= f64::from(w) || y >= f64::from(h) {
                    return Err(format!("({x},{y}) is outside display {n}'s {w}x{h} mode"));
                }
                let s = d
                    .mapping
                    .share(n)
                    .ok_or_else(|| format!("display {n} has no place in the device range"))?;
                abs(px_in_share(x, w, s.x0, s.x1), px_in_share(y, h, s.y0, s.y1))
            }
            Verb::AbsPx {
                x,
                y,
                mode,
                display: None,
            } => {
                let (w, h) = match mode {
                    Some(m) => m,
                    None => current_mode()?,
                };
                if x >= f64::from(w) || y >= f64::from(h) {
                    return Err(format!("({x},{y}) is outside a {w}x{h} mode"));
                }
                abs(px_to_device(x, w), px_to_device(y, h))
            }
            Verb::Rel { dx, dy } => {
                let events: Vec<_> = [(REL_X, dx), (REL_Y, dy)]
                    .into_iter()
                    .filter(|&(_, d)| d != 0)
                    .map(|(c, d)| InputEvent::new(EV_REL, c, d))
                    .collect();
                if events.is_empty() {
                    return Err("rel 0 0 moves nothing".into());
                }
                vec![Frame::new(Device::Rel, events)]
            }
            Verb::Button {
                action,
                button,
                count,
            } => {
                let ev = |down: bool| InputEvent::new(EV_KEY, button, i32::from(down));
                match action {
                    Action::Down => vec![Frame::one(Device::Ptr, ev(true))],
                    Action::Up => vec![Frame::one(Device::Ptr, ev(false))],
                    Action::Tap => (0..count * 2)
                        .map(|i| {
                            let f = Frame::one(Device::Ptr, ev(i % 2 == 0));
                            if i == 0 { f } else { f.after(BUTTON_PACE) }
                        })
                        .collect(),
                }
            }
            Verb::Scroll { v, h } => {
                let mut events = Vec::new();
                for (delta, acc, hi, detent) in [
                    (v, &mut self.scroll_v, REL_WHEEL_HI_RES, REL_WHEEL),
                    (h, &mut self.scroll_h, REL_HWHEEL_HI_RES, REL_HWHEEL),
                ] {
                    let v120 = (delta * 120.0).round() as i32;
                    *acc += v120;
                    let detents = *acc / 120;
                    *acc -= detents * 120;
                    if v120 != 0 {
                        events.push(InputEvent::new(EV_REL, hi, v120));
                    }
                    if detents != 0 {
                        events.push(InputEvent::new(EV_REL, detent, detents));
                    }
                }
                if events.is_empty() {
                    return Err("scroll 0 moves nothing".into());
                }
                vec![Frame::new(Device::Ptr, events)]
            }
            Verb::Touchpad { tap, fingers, at } => {
                let mut pad = lock(&TOUCHPAD);
                let pad = pad.get_or_insert_with(|| {
                    Touchpad::with_tracking_ids_from(MAX_TRACKING_ID / 2 + 1)
                });
                touchpad_frames(pad, crate::hosttrackpad::geometry(), tap, fingers, at)
            }
            Verb::Ev { dev, ev } => vec![Frame {
                dev,
                events: vec![ev],
                syn: false,
                pause: Duration::ZERO,
            }],
            Verb::Syn(dev) => match dev {
                Some(d) => vec![Frame::new(d, Vec::new())],
                None => Device::ALL
                    .iter()
                    .map(|&d| Frame::new(d, Vec::new()))
                    .collect(),
            },
            Verb::Release => {
                let mut all = std::mem::take(&mut *lock(&ORPHANS));
                all.absorb(std::mem::take(&mut *lock(&self.held)));
                return Ok((all.release_frames(), Vec::new()));
            }
            Verb::KeepHeld => {
                self.keep = true;
                return Ok((Vec::new(), Vec::new()));
            }
            Verb::Info => {
                let attached = lock(&CONN).is_some();
                let mode = match current_mode() {
                    Ok((w, h)) => format!("{w}x{h}"),
                    Err(_) => "unknown".into(),
                };
                let mut report = vec![
                    format!(
                        "devices {}",
                        if attached {
                            "kbd ptr rel touchpad"
                        } else {
                            "none"
                        }
                    ),
                    format!("abs-max {ABS_MAX}"),
                    format!("mode {mode}"),
                ];
                report.extend(displays().report());
                let (keys, buttons) = {
                    let held = lock(&self.held);
                    (held.keys.clone(), held.buttons.clone())
                };
                let h = lock(&ORPHANS);
                if !keys.is_empty() || !buttons.is_empty() || !h.is_empty() {
                    report.push(format!(
                        "held keys {keys:?} buttons {buttons:?} (kept {:?} {:?})",
                        h.keys, h.buttons
                    ));
                }
                return Ok((Vec::new(), report));
            }
        };
        Ok((frames, Vec::new()))
    }

    /// Run one verb (the text after `input `) against the current worker — or refuse it, when
    /// the `input-inject` lever is off.
    pub fn run(&mut self, line: &str) -> Result<Vec<String>, String> {
        if !allowed() {
            // Whatever this connection still holds was pressed while injection was allowed;
            // `lever_off` normally took it already, and this catches what raced that sweep.
            self.release_own();
            return Err(refusal());
        }
        let verb = Verb::parse(line)?;
        let release = verb == Verb::Release;
        let (frames, report) = self.plan(verb)?;
        let sent = if frames.is_empty() {
            Ok(())
        } else {
            send_frames(&frames, &self.held)
        };
        // The lever went off while this verb was sending: `lever_off` may have swept before the
        // last press was noted. Reading the lever under the held set's lock — the lock the sweep
        // takes after turning it off — means one of the two sees every press.
        let stray = {
            let mut held = lock(&self.held);
            (!allowed()).then(|| std::mem::take(&mut *held))
        };
        if let Some(stray) = stray {
            release_held(stray, &self.held);
        }
        if let Err((sent, why)) = sent {
            // The release took everything it was asked to undo out of the ledgers; what did
            // not go out is still pressed in the guest, and a later `release` must find it.
            if release {
                lock(&ORPHANS).absorb(Held::unreleased(&frames[sent..]));
            }
            return Err(why);
        }
        Ok(report)
    }

    /// Release what this connection holds now.
    fn release_own(&mut self) {
        let held = std::mem::take(&mut *lock(&self.held));
        release_held(held, &self.held);
    }
}

/// Send the frames that release `held`; what cannot be sent goes to [`ORPHANS`] for a later
/// `release` (or the next [`lever_off`]).
fn release_held(held: Held, ledger: &Mutex<Held>) {
    if held.is_empty() {
        return;
    }
    let frames = held.release_frames();
    if let Err((sent, why)) = send_frames(&frames, ledger) {
        log::warn!(
            "input: could not release keys {:?} and buttons {:?} ({why}); a later `release` \
             will retry",
            held.keys,
            held.buttons
        );
        lock(&ORPHANS).absorb(Held::unreleased(&frames[sent..]));
    }
}

/// Send `frames`, each under its device's frame lock (taken after the frame's pause, never
/// across it), noting presses and releases in `held`. On failure, how many frames went out
/// whole, and why the next did not.
fn send_frames(frames: &[Frame], held: &Mutex<Held>) -> Result<(), (usize, String)> {
    let conn = lock(&CONN).clone().ok_or((
        0,
        "this VM has no input devices to inject into (a headless run needs --input; a managed \
         VM, [hardware] input = true or `limina start <vm> --input`)"
            .to_string(),
    ))?;
    // Snapshot rule as everywhere: hold the Arc so the fds stay open across the sends, and
    // a relaunch mid-verb leaves this verb on the worker it started on.
    let io = conn.io();
    let trace = crate::debug_ctl::POINTER_WIRE_TRACE.on();
    for (i, f) in frames.iter().enumerate() {
        if !f.pause.is_zero() {
            std::thread::sleep(f.pause);
        }
        let _frame = io.frame(f.dev.frame_lock());
        if matches!(f.dev, Device::Ptr | Device::Rel) && !f.events.is_empty() {
            POINTER_EPOCH.fetch_add(1, Ordering::AcqRel);
        }
        let fd = f.dev.fd(&io);
        let syn = f.syn.then(InputEvent::syn);
        for ev in f.events.iter().copied().chain(syn) {
            if trace {
                eprintln!(
                    "[WIRE] t={} dev=inject-{} type={} code={} value={}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_micros()),
                    f.dev.name(),
                    ev.type_,
                    ev.code,
                    ev.value
                );
            }
            try_send(fd, ev).map_err(|why| (i, why))?;
            lock(held).note(f.dev, ev);
        }
    }
    Ok(())
}

impl Drop for Session {
    /// The connection is gone: release what it holds, unless it asked to keep it.
    fn drop(&mut self) {
        let held = std::mem::take(&mut *lock(&self.held));
        if held.is_empty() {
            return;
        }
        if self.keep {
            lock(&ORPHANS).absorb(held);
            return;
        }
        log::info!(
            "input: a client left keys {:?} and buttons {:?} pressed; releasing them",
            held.keys,
            held.buttons
        );
        release_held(held, &self.held);
    }
}

/// One event, one datagram, on a non-blocking socket — and unlike the window's sender, a
/// failure is the caller's answer, not a log line: a harness has to know its event was lost.
fn try_send(fd: RawFd, ev: InputEvent) -> Result<(), String> {
    let bytes = ev.to_bytes();
    let n = unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), 0) };
    if n >= 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    Err(match err.raw_os_error() {
        Some(libc::EAGAIN) | Some(libc::ENOBUFS) => {
            "the worker's input queue is full (is it stalled?)".to_string()
        }
        _ => format!("the worker is not taking input ({err}); is the guest rebooting?"),
    })
}

/// A headless worker's four input socketpairs: what the windowed session makes per spawn
/// (`session::spawn_windowed_worker`), for a run with no window to make them.
pub struct WorkerPipes {
    sup: [OwnedFd; 4],
    worker: [OwnedFd; 4],
    /// The [`WorkerIo`]'s shown-ack slot: `/dev/null`, since a headless run has no window to
    /// ack. Opened here, before the spawn, so nothing after the spawn can fail.
    ack: OwnedFd,
}

impl WorkerPipes {
    pub fn new() -> Result<Self> {
        let mut sup = Vec::with_capacity(4);
        let mut worker = Vec::with_capacity(4);
        for _ in 0..4 {
            let (s, w) = crate::supervisor::socketpair(libc::SOCK_DGRAM)?;
            for fd in [&s, &w] {
                crate::session::set_socket_buffer(fd.as_raw_fd(), 256 * 1024);
            }
            crate::session::set_nonblocking(s.as_raw_fd());
            sup.push(s);
            worker.push(w);
        }
        let arr = |v: Vec<OwnedFd>| -> [OwnedFd; 4] { v.try_into().expect("four") };
        let ack = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .context("opening /dev/null for a headless worker's ack slot")?
            .into();
        Ok(Self {
            sup: arr(sup),
            worker: arr(worker),
            ack,
        })
    }

    /// The worker flags naming the inherited ends.
    pub fn args(&self) -> Vec<String> {
        let [kbd, ptr, rel, tpd] = &self.worker;
        vec![
            "--input-kbd-fd".into(),
            kbd.as_raw_fd().to_string(),
            "--input-ptr-fd".into(),
            ptr.as_raw_fd().to_string(),
            "--input-rel-ptr-fd".into(),
            rel.as_raw_fd().to_string(),
            "--input-touchpad-fd".into(),
            tpd.as_raw_fd().to_string(),
            "--input-touchpad-size".into(),
            crate::hosttrackpad::geometry().to_arg(),
        ]
    }

    pub fn worker_fds(&self) -> Vec<RawFd> {
        self.worker.iter().map(AsRawFd::as_raw_fd).collect()
    }

    /// The spawned worker holds its own copies: drop ours, and keep the supervisor ends as a
    /// [`WorkerIo`]. Infallible, so a spawned worker is always published and monitored.
    pub fn into_io(self, pid: i32) -> WorkerIo {
        let Self { sup, worker, ack } = self;
        drop(worker);
        let [kbd, ptr, rel, tpd] = sup;
        WorkerIo::new(pid, kbd, ptr, rel, tpd, ack)
    }
}

// ---------------------------------------------------------------------------
// The client: `limina input`.
// ---------------------------------------------------------------------------

/// What the runtime socket strips before handing a line here.
pub const PREFIX: &str = "input";

/// `keys`: every key name `key` accepts, with its code, one `NAME CODE` per line. Exactly the
/// keys the virtual keyboard advertises (`names::KEY_NAMES`); answered by the client, so it
/// needs neither a running VM nor the lever.
pub fn key_list() -> Vec<String> {
    names::KEY_NAMES
        .iter()
        .map(|(name, code)| format!("{name} {code}"))
        .collect()
}

/// Whether a verb line is `keys`, which the client answers itself.
pub fn is_keys(verb: &[String]) -> bool {
    verb == ["keys"]
}

/// Send verbs to the supervisor with this pid. `verb` empty or `["-"]` reads them from stdin,
/// one per line (`#` comments, blank lines and client-side `sleep MS` and `keys` allowed). Report
/// lines are printed; the first refusal is an error, and no later verb is sent.
pub fn client(pid: u32, keep_held: bool, verb: &[String]) -> Result<()> {
    let path = crate::runtime_ctl::socket_path(pid);
    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "reaching supervisor {pid} at {} (is the VM running?)",
            path.display()
        )
    })?;
    // A click is paced, and a long `type` is many sends; neither takes anywhere near this.
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    run_verbs(
        &stream,
        keep_held,
        verb,
        std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
}

/// [`client`] over a connected stream, with stdin and stdout passed in so a test can drive it.
/// Stops at the first verb the VM refuses (or that fails to send): the verbs before it have
/// already run, none after it is sent, and the error is what `limina input` exits non-zero with.
fn run_verbs(
    stream: &UnixStream,
    keep_held: bool,
    verb: &[String],
    input: impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut ask = |line: &str, out: &mut dyn Write| -> Result<()> {
        // One verb is one protocol line: a newline inside it would be a second request.
        anyhow::ensure!(
            !line.contains(['\n', '\r']),
            "a verb cannot contain a line break: {line:?}"
        );
        let mut w = stream;
        w.write_all(format!("{PREFIX} {line}\n").as_bytes())
            .context("sending to the supervisor")?;
        let report =
            wire::read_answer(&mut reader).map_err(|why| anyhow::anyhow!("{line}: {why}"))?;
        for l in report {
            writeln!(out, "{l}")?;
        }
        Ok(())
    };
    if keep_held {
        ask("keep-held", out)?;
    }
    if !(verb.is_empty() || verb == ["-"]) {
        if is_keys(verb) {
            return print_keys(out);
        }
        return ask(&verb.join(" "), out);
    }
    for (n, line) in input.lines().enumerate() {
        let line = line.context("reading verbs from stdin")?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(ms) = line.strip_prefix("sleep ") {
            let ms: u64 = ms
                .trim()
                .parse()
                .with_context(|| format!("line {}: sleep {ms:?}: not milliseconds", n + 1))?;
            std::thread::sleep(Duration::from_millis(ms));
            continue;
        }
        if line == "keys" {
            print_keys(out)?;
            continue;
        }
        ask(line, out).with_context(|| {
            format!(
                "line {} of the script failed; the verbs before it ran, none after it was sent",
                n + 1
            )
        })?;
    }
    Ok(())
}

fn print_keys(out: &mut dyn Write) -> Result<()> {
    for l in key_list() {
        writeln!(out, "{l}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(line: &str) -> Vec<Frame> {
        Session::default()
            .plan(Verb::parse(line).unwrap())
            .unwrap()
            .0
    }

    fn evs(frames: &[Frame]) -> Vec<(Device, u16, u16, i32)> {
        frames
            .iter()
            .flat_map(|f| {
                let syn = f.syn.then(InputEvent::syn);
                f.events
                    .iter()
                    .copied()
                    .chain(syn)
                    .map(move |e| (f.dev, e.type_, e.code, e.value))
            })
            .collect()
    }

    #[test]
    fn touchpad_verbs_parse() {
        let tp = |l: &str| Verb::parse(l);
        assert_eq!(
            tp("touchpad click 3").unwrap(),
            Verb::Touchpad {
                tap: false,
                fingers: 3,
                at: (0.5, 0.5)
            }
        );
        assert_eq!(
            tp("touchpad tap 2 --at 0.25,0.75").unwrap(),
            Verb::Touchpad {
                tap: true,
                fingers: 2,
                at: (0.25, 0.75)
            }
        );
        assert_eq!(
            tp("touchpad tap 1 --at=0,1").unwrap(),
            Verb::Touchpad {
                tap: true,
                fingers: 1,
                at: (0.0, 1.0)
            }
        );
        for bad in [
            "touchpad",
            "touchpad press 2",
            "touchpad click",
            "touchpad click 0",
            "touchpad click 4",
            "touchpad click 2 --at",
            "touchpad click 2 --at 0.5",
            "touchpad click 2 --at 1.5,0.5",
            "touchpad click 2 0.5,0.5",
            "touchpad click 2 --at 0.5,0.5 extra",
        ] {
            assert!(tp(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// 100 x 50 mm.
    const PAD: TouchpadGeometry = TouchpadGeometry {
        width: 10_000,
        height: 5_000,
    };

    #[test]
    fn a_touchpad_click_lands_presses_releases_and_lifts_on_the_touchpad() {
        let mut pad = Touchpad::with_tracking_ids_from(100);
        let frames = touchpad_frames(&mut pad, PAD, false, 3, (0.5, 0.5));
        assert!(frames.iter().all(|f| f.dev == Device::Touchpad));
        let ev = evs(&frames);
        let x = |v| (Device::Touchpad, EV_ABS, ABS_MT_POSITION_X, v);
        // Three fingers 18 mm apart around the centre, all at the centre's height.
        for v in [3200, 5000, 6800] {
            assert!(ev.contains(&x(v)), "a finger at x={v}: {ev:?}");
        }
        assert!(ev.contains(&(Device::Touchpad, EV_ABS, ABS_MT_TRACKING_ID, 100)));
        let at = |e: (Device, u16, u16, i32)| ev.iter().position(|&x| x == e).unwrap();
        let tool = at((Device::Touchpad, EV_KEY, BTN_TOOL_TRIPLETAP, 1));
        let press = at((Device::Touchpad, EV_KEY, BTN_LEFT, 1));
        let release = at((Device::Touchpad, EV_KEY, BTN_LEFT, 0));
        let lift = at((Device::Touchpad, EV_KEY, BTN_TOUCH, 0));
        assert!(tool < press && press < release && release < lift, "{ev:?}");
        assert_eq!(pad.active(), 0, "every finger lifted");
        assert!(frames[1].pause > Duration::ZERO && frames[2].pause >= BUTTON_PACE);
    }

    #[test]
    fn a_touchpad_click_stopped_halfway_is_released_with_its_fingers() {
        let mut held = Held::new();
        let ev = |code, value| InputEvent::new(EV_KEY, code, value);
        held.note(Device::Touchpad, ev(BTN_TOOL_DOUBLETAP, 1));
        held.note(Device::Touchpad, ev(BTN_TOUCH, 1));
        held.note(Device::Touchpad, ev(BTN_LEFT, 1));
        assert!(!held.is_empty());
        let frames = held.release_frames();
        assert_eq!(
            frames[0],
            Frame::one(Device::Touchpad, ev(BTN_LEFT, 0)),
            "the button first"
        );
        held.note(Device::Touchpad, ev(BTN_LEFT, 0));
        held.note(Device::Touchpad, ev(BTN_TOUCH, 0));
        assert!(held.is_empty(), "a lifted, released touchpad holds nothing");
    }

    #[test]
    fn a_touchpad_tap_lifts_inside_the_tap_timeout_and_presses_nothing() {
        let mut pad = Touchpad::new();
        let frames = touchpad_frames(&mut pad, PAD, true, 2, (0.0, 0.0));
        let ev = evs(&frames);
        assert!(!ev.iter().any(|e| e.2 == BTN_LEFT), "{ev:?}");
        assert!(ev.contains(&(Device::Touchpad, EV_KEY, BTN_TOOL_DOUBLETAP, 1)));
        // At the corner, the left finger is clamped onto the surface.
        assert!(ev.contains(&(Device::Touchpad, EV_ABS, ABS_MT_POSITION_X, 0)));
        assert!(ev.contains(&(Device::Touchpad, EV_ABS, ABS_MT_POSITION_X, 900)));
        assert_eq!(frames.len(), 2);
        assert!(frames[1].pause < Duration::from_millis(180));
        assert_eq!(pad.active(), 0);
    }

    const SYN_K: (Device, u16, u16, i32) = (Device::Kbd, EV_SYN, SYN_REPORT, 0);
    const SYN_P: (Device, u16, u16, i32) = (Device::Ptr, EV_SYN, SYN_REPORT, 0);

    #[test]
    fn a_chord_presses_in_order_and_releases_in_reverse_each_in_its_own_frame() {
        let k = |c, v| (Device::Kbd, EV_KEY, c, v);
        assert_eq!(
            evs(&plan("key tap KEY_LEFTCTRL+leftalt+f2")),
            vec![
                k(KEY_LEFTCTRL, 1),
                SYN_K,
                k(KEY_LEFTALT, 1),
                SYN_K,
                k(KEY_F2, 1),
                SYN_K,
                k(KEY_F2, 0),
                SYN_K,
                k(KEY_LEFTALT, 0),
                SYN_K,
                k(KEY_LEFTCTRL, 0),
                SYN_K,
            ]
        );
        assert_eq!(evs(&plan("key down 29")), vec![k(KEY_LEFTCTRL, 1), SYN_K]);
        assert_eq!(evs(&plan("key up 0x1d")), vec![k(KEY_LEFTCTRL, 0), SYN_K]);
    }

    #[test]
    fn type_wraps_each_shifted_character_in_its_own_shift() {
        let k = |c, v| (Device::Kbd, EV_KEY, c, v);
        assert_eq!(
            evs(&plan("type a B")),
            vec![
                k(KEY_A, 1),
                SYN_K,
                k(KEY_A, 0),
                SYN_K,
                k(KEY_SPACE, 1),
                SYN_K,
                k(KEY_SPACE, 0),
                SYN_K,
                k(KEY_LEFTSHIFT, 1),
                SYN_K,
                k(KEY_B, 1),
                SYN_K,
                k(KEY_B, 0),
                SYN_K,
                k(KEY_LEFTSHIFT, 0),
                SYN_K,
            ]
        );
        // The rest of the line is the text, spaces included; escapes for what a line can't hold.
        assert_eq!(
            Verb::parse("type  x\\n"),
            Ok(Verb::Type(vec![
                (KEY_SPACE, false),
                (KEY_X, false),
                (KEY_ENTER, false)
            ]))
        );
        assert!(Verb::parse("type héllo").is_err());
        assert!(Verb::parse("type \\q").is_err());
        assert!(Verb::parse("type").is_err());
    }

    #[test]
    fn absolute_positions_in_each_space() {
        let p = |c, v| (Device::Ptr, EV_ABS, c, v);
        assert_eq!(
            evs(&plan("abs 16384 8192")),
            vec![p(ABS_X, 16384), p(ABS_Y, 8192), SYN_P]
        );
        assert_eq!(
            evs(&plan("abs-norm 0.25 0.75")),
            vec![p(ABS_X, 8192), p(ABS_Y, 24575), SYN_P]
        );
        assert_eq!(
            evs(&plan("abs-px 640 400 1280x800")),
            vec![p(ABS_X, 16390), p(ABS_Y, 16394), SYN_P]
        );
        assert!(Verb::parse("abs 32768 0").is_err());
        assert!(Verb::parse("abs -1 0").is_err());
        assert!(Verb::parse("abs-norm 1.5 0").is_err());
        assert!(
            Session::default()
                .plan(Verb::parse("abs-px 1280 0 1280x800").unwrap())
                .is_err()
        );
    }

    use crate::window::absfit::{Line, Mapping};
    use crate::window::arrangement::GuestRect;

    fn rect(x: u32, y: u32, w: u32, h: u32) -> GuestRect {
        GuestRect { x, y, w, h }
    }

    /// Two displays side by side as a guest's agent reports them: slot 0 a 2000x1250 scanout
    /// at scale 2 (logical 1000x625), slot 1 a 1280x800 scanout at scale 1 to its right. The
    /// desktop's bounding box is 2280x800 logical.
    fn side_by_side() -> (Vec<(usize, GuestRect)>, Displays) {
        let rects = vec![(0, rect(0, 0, 1000, 625)), (1, rect(1000, 0, 1280, 800))];
        let displays = Displays(
            [(0, (2000, 1250)), (1, (1280, 800))]
                .into_iter()
                .map(|(slot, scanout)| Display {
                    slot,
                    scanout: Some(scanout),
                    logical: rects.iter().find(|(s, _)| *s == slot).map(|(_, r)| *r),
                    mapping: Mapping::Report(rects.clone()),
                })
                .collect(),
        );
        (rects, displays)
    }

    fn plan_on(displays: &Displays, line: &str) -> Result<Vec<Frame>, String> {
        Session::default()
            .plan_in(Verb::parse(line)?, || displays.clone())
            .map(|(f, _)| f)
    }

    /// The `(ABS_X, ABS_Y)` one planned position frame carries.
    fn xy(frames: &[Frame]) -> (i32, i32) {
        let [f] = frames else {
            panic!("expected one frame, got {frames:?}")
        };
        assert_eq!(f.dev, Device::Ptr);
        let get = |code| f.events.iter().find(|e| e.code == code).unwrap().value;
        (get(ABS_X), get(ABS_Y))
    }

    #[test]
    fn display_is_selected_by_slot_anywhere_in_the_verb() {
        let px = |line: &str| match Verb::parse(line).unwrap() {
            Verb::AbsPx {
                x,
                y,
                mode,
                display,
            } => (x, y, mode, display),
            other => panic!("{other:?}"),
        };
        assert_eq!(px("abs-px --display 1 10 20"), (10.0, 20.0, None, Some(1)));
        assert_eq!(
            px("abs-px 10 20 640x480 --display=2"),
            (10.0, 20.0, Some((640, 480)), Some(2))
        );
        assert_eq!(px("abs-px 10 20"), (10.0, 20.0, None, None));
        assert_eq!(
            Verb::parse("abs-norm 0.5 --display 0 0.25"),
            Ok(Verb::AbsNorm {
                u: 0.5,
                v: 0.25,
                display: Some(0)
            })
        );
        for bad in [
            "abs-px --display",
            "abs-px --display x 1 1",
            "abs-px --display 16 1 1",
            "abs-px --display 1 --display 1 1 1",
            "abs-norm --display 0 0.5",
            "abs-norm --display 0 0.5 0.5 0.5",
            "abs --display 1 0 0",
        ] {
            assert!(Verb::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    /// `abs-norm --display N` is the window's own mapping for a unit point on slot N: through
    /// the guest's reported layout, the display's share of the bounding box.
    #[test]
    fn a_unit_point_on_a_reported_display_lands_where_the_window_puts_it() {
        let (rects, displays) = side_by_side();
        // Slot 1 starts 1000 of 2280 logical units in; slot 0 is 625 of 800 tall.
        let seam = (1000.0 / 2280.0 * f64::from(ABS_MAX)).round() as i32;
        let foot = (625.0 / 800.0 * f64::from(ABS_MAX)).round() as i32;
        assert_eq!(
            xy(&plan_on(&displays, "abs-norm --display 1 0 0").unwrap()),
            (seam, 0)
        );
        assert_eq!(
            xy(&plan_on(&displays, "abs-norm --display 0 1 1").unwrap()),
            (seam, foot)
        );
        assert_eq!(
            xy(&plan_on(&displays, "abs-norm --display 1 1 1").unwrap()),
            (ABS_MAX as i32, ABS_MAX as i32)
        );
        // Exactly what the window sends for the same point on the same slot.
        for (slot, u, v) in [(0, 0.3, 0.6), (1, 0.7, 0.2)] {
            let line = format!("abs-norm --display {slot} {u} {v}");
            assert_eq!(
                Some(xy(&plan_on(&displays, &line).unwrap())),
                crate::window::arrangement::abs_in_layout(&rects, slot, u, v, ABS_MAX as i32)
            );
        }
    }

    /// The guest maps a value back as libinput and mutter do: `v · desktop / 32768` logical
    /// units across the bounding box, then onto the display's pixels at its scale. A pixel of
    /// display N sent with `abs-px --display N` must come back as that pixel, whether the
    /// compositor truncates or rounds — on both displays, at their different scales.
    #[test]
    fn a_pixel_on_a_reported_display_comes_back_as_that_pixel() {
        let (rects, displays) = side_by_side();
        let (dw, dh) = (2280.0, 800.0);
        for (slot, (sw, sh)) in [(0usize, (2000u32, 1250u32)), (1, (1280, 800))] {
            let r = rects[slot].1;
            let (sx, sy) = (
                f64::from(sw) / f64::from(r.w),
                f64::from(sh) / f64::from(r.h),
            );
            let back = |v: i32, d: f64, origin: u32, scale: f64| {
                (f64::from(v) * d / f64::from(ABS_MAX + 1) - f64::from(origin)) * scale
            };
            for (px, py) in (0..sw)
                .step_by(37)
                .chain([sw - 1])
                .zip((0..sh).step_by(23).chain([sh - 1]).cycle())
            {
                let line = format!("abs-px --display {slot} {px} {py}");
                let (x, y) = xy(&plan_on(&displays, &line).unwrap());
                let (ix, iy) = (back(x, dw, r.x, sx), back(y, dh, r.y, sy));
                for (image, p) in [(ix, px), (iy, py)] {
                    assert_eq!(image.floor(), f64::from(p), "{line}: floor of {image}");
                    assert_eq!(image.round(), f64::from(p), "{line}: round of {image}");
                }
                // And it is the same point `abs-norm` names, to within the ¼-pixel aim.
                let u = (f64::from(px) + 0.25) / f64::from(sw);
                let v = (f64::from(py) + 0.25) / f64::from(sh);
                let norm =
                    xy(&plan_on(&displays, &format!("abs-norm --display {slot} {u} {v}")).unwrap());
                assert!(
                    (norm.0 - x).abs() <= 2 && (norm.1 - y).abs() <= 2,
                    "{line}: {x},{y} vs abs-norm {norm:?}"
                );
            }
        }
    }

    /// With one display its share is the whole range, and `--display 0` is today's verb.
    #[test]
    fn the_only_display_is_the_whole_range() {
        let one = Displays(vec![Display {
            slot: 0,
            scanout: Some((1280, 800)),
            logical: None,
            mapping: Mapping::Identity,
        }]);
        assert_eq!(
            plan_on(&one, "abs-px --display 0 640 400").unwrap(),
            plan_on(&one, "abs-px 640 400 1280x800").unwrap()
        );
        assert_eq!(
            plan_on(&one, "abs-norm --display 0 0.25 0.75").unwrap(),
            plan_on(&one, "abs-norm 0.25 0.75").unwrap()
        );
        assert_eq!(
            xy(&plan_on(&one, "abs-px --display 0 640 400").unwrap()),
            (16390, 16394)
        );
    }

    /// No report, two live scanouts, and the window's echo fitted slot 1's share: a pixel lands
    /// on itself under the fitted lines (`pixel = a·u + b`, `u = value / ABS_MAX`).
    #[test]
    fn a_pixel_on_a_fitted_display_lands_on_its_line() {
        let fitted = Mapping::Fitted {
            x: Line {
                a: 2560.0,
                b: -1280.0,
            },
            y: Line { a: 800.0, b: 0.0 },
            scanout: (1280, 800),
        };
        let displays = Displays(vec![
            Display {
                slot: 0,
                scanout: Some((1280, 800)),
                logical: None,
                mapping: Mapping::Fitted {
                    x: Line { a: 2560.0, b: 0.0 },
                    y: Line { a: 800.0, b: 0.0 },
                    scanout: (1280, 800),
                },
            },
            Display {
                slot: 1,
                scanout: Some((1280, 800)),
                logical: None,
                mapping: fitted.clone(),
            },
        ]);
        for p in (0..1280u32).step_by(31).chain([1279]) {
            let q = p.min(799);
            let (x, y) = xy(&plan_on(&displays, &format!("abs-px --display 1 {p} {q}")).unwrap());
            let lx = 2560.0 * f64::from(x) / f64::from(ABS_MAX) - 1280.0;
            assert_eq!(lx.floor(), f64::from(p), "x {p}: {lx}");
            let ly = 800.0 * f64::from(y) / f64::from(ABS_MAX);
            assert_eq!(ly.floor(), f64::from(q), "y {q}: {ly}");
        }
        // abs-norm is the window's fitted placement exactly.
        assert_eq!(
            Some(xy(
                &plan_on(&displays, "abs-norm --display 1 0.5 0.5").unwrap()
            )),
            fitted.place(1, (0.5, 0.5), ABS_MAX as i32)
        );
    }

    #[test]
    fn a_display_that_cannot_be_pointed_at_is_refused_and_says_why() {
        let (_, two) = side_by_side();
        let err = plan_on(&two, "abs-px --display 3 1 1").unwrap_err();
        assert!(err.contains("display 3 does not exist"), "{err}");
        assert!(err.contains("the displays are 0, 1"), "{err}");
        assert!(err.contains("limina input <vm> info"), "{err}");
        let err = plan_on(&two, "abs-px --display 1 1280 0").unwrap_err();
        assert!(err.contains("outside display 1's 1280x800 mode"), "{err}");

        // Two live scanouts, no report, nothing learned: the window's identity floor is no
        // place at all for either display.
        let unknown = Displays(
            [0, 1]
                .into_iter()
                .map(|slot| Display {
                    slot,
                    scanout: Some((1280, 800)),
                    logical: None,
                    mapping: Mapping::Identity,
                })
                .collect(),
        );
        for line in ["abs-px --display 1 1 1", "abs-norm --display 0 0.5 0.5"] {
            let err = plan_on(&unknown, line).unwrap_err();
            assert!(err.contains("cannot be pointed at"), "{line}: {err}");
            assert!(err.contains("reported no layout"), "{line}: {err}");
        }
        // The plain verbs keep their meaning: a fraction of the whole range.
        assert!(plan_on(&unknown, "abs-norm 0.5 0.5").is_ok());

        // A report that leaves out a live scanout does not place it.
        let mut partial = two.clone();
        partial.0[1].logical = None;
        let err = plan_on(&partial, "abs-norm --display 1 0.5 0.5").unwrap_err();
        assert!(err.contains("does not place Virtual-2"), "{err}");

        // Headless: a reported display whose mode the supervisor was never told.
        let mut headless = two;
        headless.0[1].scanout = None;
        let err = plan_on(&headless, "abs-px --display 1 10 10").unwrap_err();
        assert!(err.contains("abs-px --display 1 X Y WxH"), "{err}");
        assert!(plan_on(&headless, "abs-px --display 1 10 10 1280x800").is_ok());
        assert!(plan_on(&headless, "abs-norm --display 1 0.5 0.5").is_ok());
    }

    #[test]
    fn info_lists_each_display_with_its_device_and_share() {
        let (_, mut displays) = side_by_side();
        displays.0.push(Display {
            slot: 2,
            scanout: Some((640, 480)),
            logical: None,
            mapping: Mapping::Report(Vec::new()),
        });
        let (_, report) = Session::default()
            .plan_in(Verb::Info, || displays.clone())
            .unwrap();
        let line = |n: usize| {
            report
                .iter()
                .find(|l| l.starts_with(&format!("display {n} ")))
                .unwrap_or_else(|| panic!("no display {n} in {report:?}"))
                .clone()
        };
        assert_eq!(
            line(0),
            "display 0 connector Virtual-1 scanout 2000x1250 logical 1000x625+0+0 device ptr \
             mapping report x 0..14371 y 0..25599"
        );
        assert_eq!(
            line(1),
            "display 1 connector Virtual-2 scanout 1280x800 logical 1280x800+1000+0 device ptr \
             mapping report x 14371..32767 y 0..32767"
        );
        assert!(
            line(2)
                .contains("mapping none (the layout the guest reported does not place Virtual-3)"),
            "{}",
            line(2)
        );
    }

    #[test]
    fn px_to_device_lands_on_the_pixel_libinput_maps_it_back_to() {
        // Power-of-two sizes are where aiming at the centre broke under rounding.
        for size in [512u32, 800, 1024, 1080, 1280, 2048, 2560, 3024, 4096, 8192] {
            for p in (0..size).step_by(7).chain([size / 2, size - 1]) {
                let v = px_to_device(f64::from(p), size);
                // libinput: v · size / (max − min + 1); the compositor truncates or rounds.
                let image = f64::from(v) * f64::from(size) / f64::from(ABS_MAX + 1);
                assert_eq!(image.floor(), f64::from(p), "floor: size {size} pixel {p}");
                assert_eq!(image.round(), f64::from(p), "round: size {size} pixel {p}");
            }
        }
    }

    #[test]
    fn buttons_clicks_are_paced_and_scroll_emits_both_rates() {
        let frames = plan("button click right 2");
        let b = |v| (Device::Ptr, EV_KEY, BTN_RIGHT, v);
        assert_eq!(
            evs(&frames),
            vec![b(1), SYN_P, b(0), SYN_P, b(1), SYN_P, b(0), SYN_P]
        );
        assert_eq!(frames[0].pause, Duration::ZERO);
        assert!(frames[1..].iter().all(|f| f.pause == BUTTON_PACE));

        let r = |c, v| (Device::Ptr, EV_REL, c, v);
        assert_eq!(
            evs(&plan("scroll 1")),
            vec![r(REL_WHEEL_HI_RES, 120), r(REL_WHEEL, 1), SYN_P]
        );
        assert_eq!(
            evs(&plan("scroll -2 1")),
            vec![
                r(REL_WHEEL_HI_RES, -240),
                r(REL_WHEEL, -2),
                r(REL_HWHEEL_HI_RES, 120),
                r(REL_HWHEEL, 1),
                SYN_P
            ]
        );
        // Half detents accumulate into a whole one on the second.
        let mut s = Session::default();
        let half = || Verb::parse("scroll 0.5").unwrap();
        assert_eq!(
            evs(&s.plan(half()).unwrap().0),
            vec![r(REL_WHEEL_HI_RES, 60), SYN_P]
        );
        assert_eq!(
            evs(&s.plan(half()).unwrap().0),
            vec![r(REL_WHEEL_HI_RES, 60), r(REL_WHEEL, 1), SYN_P]
        );
    }

    #[test]
    fn relative_raw_and_syn() {
        let rel = Device::Rel;
        assert_eq!(
            evs(&plan("rel 7 -3")),
            vec![
                (rel, EV_REL, REL_X, 7),
                (rel, EV_REL, REL_Y, -3),
                (rel, EV_SYN, SYN_REPORT, 0)
            ]
        );
        assert_eq!(evs(&plan("rel 0 5")).len(), 2);
        assert!(
            Session::default()
                .plan(Verb::parse("rel 0 0").unwrap())
                .is_err()
        );
        // The escape hatch sends exactly the event, no SYN.
        assert_eq!(
            evs(&plan("ev touchpad EV_ABS ABS_MT_SLOT 1")),
            vec![(Device::Touchpad, EV_ABS, ABS_MT_SLOT, 1)]
        );
        assert_eq!(evs(&plan("syn rel")), vec![(rel, EV_SYN, SYN_REPORT, 0)]);
        assert_eq!(evs(&plan("syn")).len(), 4);
    }

    #[test]
    fn malformed_verbs_say_what_is_wrong() {
        for bad in [
            "",
            "key",
            "key press a",
            "key tap",
            "key tap KEY_NOPE",
            "key tap a b",
            "abs 1",
            "abs x y",
            "rel 1",
            "button click",
            "button click left 0",
            "button down left 2",
            "scroll",
            "scroll nan",
            "ev mouse EV_KEY BTN_LEFT 1",
            "ev ptr EV_NOPE 0 0",
            "syn mouse",
            "sleep 10",
            "reboot",
        ] {
            assert!(Verb::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn release_undoes_presses_in_reverse_press_order_not_code_order() {
        // KEY_LEFTMETA (125) then KEY_TAB (15): code order would let go of Super first.
        let mut held = Held::new();
        held.note(Device::Kbd, InputEvent::new(EV_KEY, KEY_LEFTMETA, 1));
        held.note(Device::Kbd, InputEvent::new(EV_KEY, KEY_TAB, 1));
        held.note(Device::Kbd, InputEvent::new(EV_KEY, KEY_LEFTMETA, 1)); // no duplicate
        assert_eq!(
            evs(&held.release_frames()),
            vec![
                (Device::Kbd, EV_KEY, KEY_TAB, 0),
                SYN_K,
                (Device::Kbd, EV_KEY, KEY_LEFTMETA, 0),
                SYN_K,
            ]
        );
        // What a failed release did not send comes back in press order.
        let frames = held.release_frames();
        let rest = Held::unreleased(&frames[1..]);
        assert_eq!(rest.keys, vec![KEY_LEFTMETA]);
        assert_eq!(Held::unreleased(&frames).keys, vec![KEY_LEFTMETA, KEY_TAB]);
    }

    #[test]
    fn held_keys_are_tracked_and_released_in_reverse() {
        let mut held = Held::new();
        held.note(Device::Kbd, InputEvent::new(EV_KEY, KEY_LEFTCTRL, 1));
        held.note(Device::Kbd, InputEvent::new(EV_KEY, KEY_LEFTALT, 1));
        held.note(Device::Ptr, InputEvent::new(EV_KEY, BTN_LEFT, 1));
        held.note(Device::Kbd, InputEvent::new(EV_KEY, KEY_A, 1));
        held.note(Device::Kbd, InputEvent::new(EV_KEY, KEY_A, 0));
        // Non-key events and non-key devices hold nothing.
        held.note(Device::Ptr, InputEvent::new(EV_ABS, ABS_X, 1));
        held.note(Device::Rel, InputEvent::new(EV_KEY, BTN_LEFT, 1));
        assert_eq!(
            evs(&held.release_frames()),
            vec![
                (Device::Kbd, EV_KEY, KEY_LEFTALT, 0),
                SYN_K,
                (Device::Kbd, EV_KEY, KEY_LEFTCTRL, 0),
                SYN_K,
                (Device::Ptr, EV_KEY, BTN_LEFT, 0),
                SYN_P,
            ]
        );
    }

    fn pair() -> (OwnedFd, OwnedFd) {
        crate::supervisor::socketpair(libc::SOCK_DGRAM).unwrap()
    }

    fn read_all(fd: &OwnedFd) -> Vec<InputEvent> {
        let mut out = Vec::new();
        loop {
            let mut b = [0u8; limina_input::WIRE_LEN];
            let n = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    b.as_mut_ptr().cast(),
                    b.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n != b.len() as isize {
                return out;
            }
            out.push(InputEvent::from_bytes(&b));
        }
    }

    /// A worker's input endpoints: the supervisor's [`WorkerIo`] and the worker's four ends.
    fn worker() -> (WorkerIo, [OwnedFd; 4]) {
        let (k, kw) = pair();
        let (p, pw) = pair();
        let (r, rw) = pair();
        let (t, tw) = pair();
        let ack: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        (WorkerIo::new(1, k, p, r, t, ack), [kw, pw, rw, tw])
    }

    /// Serialize on the process-global lever and connection, with the lever set to `on`; it is
    /// off again when the guard drops.
    fn lever(on: bool) -> impl Drop {
        struct Guard {
            _levers: std::sync::MutexGuard<'static, ()>,
            _serial: std::sync::MutexGuard<'static, ()>,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                crate::debug_ctl::INPUT_INJECT.set(false);
                *lock(&CONN) = None;
            }
        }
        let levers = lock(&crate::debug_ctl::ACCESS_LEVER_TESTS);
        let serial = lock(&TEST_SERIAL);
        crate::debug_ctl::INPUT_INJECT.set(on);
        Guard {
            _levers: levers,
            _serial: serial,
        }
    }

    /// The whole loop below the verbs: a session's events reach a worker's socket, a dropped
    /// session releases what it held, and a swapped worker gets the next verb.
    #[test]
    fn events_reach_the_current_worker_and_a_closed_session_releases() {
        let _lever = lever(true);

        let (io1, ends1) = worker();
        *lock(&CONN) = None;
        publish(io1);
        let mut s = Session::default();
        s.run("key down KEY_LEFTCTRL").unwrap();
        s.run("button down left").unwrap();
        let epoch = pointer_epoch();
        s.run("rel 1 1").unwrap();
        assert!(pointer_epoch() > epoch, "a pointer move bumps the epoch");
        assert_eq!(
            read_all(&ends1[0]),
            vec![InputEvent::new(EV_KEY, KEY_LEFTCTRL, 1), InputEvent::syn()]
        );
        assert_eq!(read_all(&ends1[2]).len(), 3);

        // A relaunch swaps the endpoints; the next verb goes to the new worker.
        let (io2, ends2) = worker();
        publish(io2);
        s.run("key tap a").unwrap();
        assert_eq!(read_all(&ends2[0]).len(), 4);
        assert!(read_all(&ends1[0]).is_empty());

        // The client goes away: what it held is released, on the current worker.
        drop(s);
        assert_eq!(
            read_all(&ends2[0]),
            vec![InputEvent::new(EV_KEY, KEY_LEFTCTRL, 0), InputEvent::syn()]
        );
        assert_eq!(
            read_all(&ends2[1]),
            vec![InputEvent::new(EV_KEY, BTN_LEFT, 0), InputEvent::syn()]
        );

        // keep-held survives the connection, until someone releases it.
        let mut s = Session::default();
        s.run("keep-held").unwrap();
        s.run("key down KEY_LEFTMETA").unwrap();
        drop(s);
        assert_eq!(read_all(&ends2[0]).len(), 2, "the press, and no release");
        let mut s = Session::default();
        s.run("release").unwrap();
        assert_eq!(
            read_all(&ends2[0]),
            vec![InputEvent::new(EV_KEY, KEY_LEFTMETA, 0), InputEvent::syn()]
        );
        drop(s);

        // A dead worker is an answer, not silence.
        drop(ends2);
        let err = Session::default().run("key tap a").unwrap_err();
        assert!(err.contains("not taking input"), "{err}");
    }

    /// Off, every verb is refused and nothing reaches the worker; turned off, everything held
    /// — by a live connection and by a `keep-held` one that left — is released at once.
    #[test]
    fn the_lever_refuses_every_verb_and_turning_it_off_releases_what_is_held() {
        let _lever = lever(false);
        let (io, ends) = worker();
        publish(io);
        let ctrl = |v| InputEvent::new(EV_KEY, KEY_LEFTCTRL, v);

        let mut s = Session::default();
        for verb in ["key tap a", "info", "release", "rel 1 1", "bogus"] {
            let err = s.run(verb).unwrap_err();
            assert_eq!(err, refusal(), "{verb}");
        }
        assert!(ends.iter().all(|e| read_all(e).is_empty()), "nothing sent");
        assert!(refusal().contains("LIMINA_INPUT_INJECT=1"));
        assert!(refusal().contains("Debug menu"));
        assert!(refusal().contains("limina debug <vm> lever input-inject on"));

        // On at runtime: the next verb goes through.
        crate::debug_ctl::handle(&wire::Request::Lever {
            name: "input-inject".into(),
            on: true,
        })
        .unwrap();
        s.run("key down KEY_LEFTCTRL").unwrap();
        s.run("button down left").unwrap();
        let mut kept = Session::default();
        kept.run("keep-held").unwrap();
        kept.run("key down KEY_LEFTMETA").unwrap();
        drop(kept);
        assert_eq!(read_all(&ends[0]).len(), 4, "two presses, two SYNs");
        assert_eq!(read_all(&ends[1]).len(), 2);

        // Off at runtime, with `s` still open and idle: all of it comes up, newest first.
        crate::debug_ctl::handle(&wire::Request::Lever {
            name: "input-inject".into(),
            on: false,
        })
        .unwrap();
        assert_eq!(
            read_all(&ends[0]),
            vec![
                ctrl(0),
                InputEvent::syn(),
                InputEvent::new(EV_KEY, KEY_LEFTMETA, 0),
                InputEvent::syn()
            ]
        );
        assert_eq!(
            read_all(&ends[1]),
            vec![InputEvent::new(EV_KEY, BTN_LEFT, 0), InputEvent::syn()]
        );
        assert_eq!(s.run("key tap a").unwrap_err(), refusal());
        drop(s);
        assert!(ends.iter().all(|e| read_all(e).is_empty()), "nothing after");
    }

    /// A press the sweep missed (noted after it ran) is released by the connection's next verb,
    /// which is refused.
    #[test]
    fn a_refused_verb_releases_what_its_connection_still_holds() {
        let _lever = lever(true);
        let (io, ends) = worker();
        publish(io);
        let mut s = Session::default();
        // A press sent and noted after the lever went off and the sweep ran (finding nothing).
        let (frames, _) = s
            .plan(Verb::parse("key down KEY_LEFTCTRL").unwrap())
            .unwrap();
        send_frames(&frames, &s.held).unwrap();
        crate::debug_ctl::INPUT_INJECT.set(false);
        assert_eq!(s.run("key tap a").unwrap_err(), refusal());
        assert_eq!(
            read_all(&ends[0]),
            vec![
                InputEvent::new(EV_KEY, KEY_LEFTCTRL, 1),
                InputEvent::syn(),
                InputEvent::new(EV_KEY, KEY_LEFTCTRL, 0),
                InputEvent::syn()
            ]
        );
    }

    /// A harness must fail loudly: a script stops at its first failing verb, after the verbs
    /// before it reached the guest and before any after it is sent, and the error says which
    /// line and why; a refusal for the lever being off is such a failure too. (`limina input`
    /// exits non-zero on the error: `main` returns it.)
    #[test]
    fn a_script_stops_at_its_first_failing_verb_and_says_so() {
        let _lever = lever(true);
        let (io, ends) = worker();
        publish(io);
        let serve = || {
            let (client, server) = UnixStream::pair().unwrap();
            let t = std::thread::spawn(move || crate::runtime_ctl::serve_client(server));
            (client, t)
        };

        let (client, server) = serve();
        let script = "key tap KEY_F15\nkeys\nkey tap escape\nkey tap KEY_F16\n";
        let mut out = Vec::new();
        let err = run_verbs(&client, false, &[], script.as_bytes(), &mut out).unwrap_err();
        let err = format!("{err:#}");
        for want in [
            "line 3",
            "none after it was sent",
            "unknown key \"escape\"",
            "did you mean KEY_ESC?",
            "limina input <vm> keys",
        ] {
            assert!(err.contains(want), "{want:?} missing from: {err}");
        }
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.lines().any(|l| l == "KEY_ESC 1"), "{printed}");
        assert_eq!(printed.lines().count(), names::KEY_NAMES.len());
        drop(client);
        server.join().unwrap();
        let f15 = |v| InputEvent::new(EV_KEY, KEY_F15, v);
        assert_eq!(
            read_all(&ends[0]),
            vec![f15(1), InputEvent::syn(), f15(0), InputEvent::syn()],
            "the verb before the failure ran, and nothing after it"
        );

        // A refusal because the lever is off is a failure, with the refusal as the error.
        crate::debug_ctl::INPUT_INJECT.set(false);
        let (client, server) = serve();
        let verb = ["key".to_string(), "tap".into(), "KEY_F13".into()];
        let err = run_verbs(&client, false, &verb, std::io::empty(), &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains(&refusal()), "{err}");
        drop(client);
        server.join().unwrap();
        assert!(ends.iter().all(|e| read_all(e).is_empty()), "nothing sent");
        // `keys` is the client's: the VM refuses it as a verb.
        assert!(Verb::parse("keys").unwrap_err().contains("client"));
    }

    /// The tests that touch the process-global connection.
    static TEST_SERIAL: Mutex<()> = Mutex::new(());
}
