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

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use limina_debug::wire;
use limina_input::InputEvent;
use limina_input::constants::*;
use limina_input::names;

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
  abs-norm U V                   absolute pointer, 0..1 of the device range
  abs-px X Y [WxH]               absolute pointer at scanout pixel (X,Y) of a WxH mode;
                                 WxH defaults to the guest's current mode (windowed) or
                                 the boot's --display-size (headless)
  rel DX DY                      relative motion on the virtual mouse
  button down|up|click BTN [N]   BTN_LEFT|BTN_RIGHT|BTN_MIDDLE (or left/right/middle);
                                 click N times, transitions 30 ms apart
  scroll V [H]                   wheel, in detents (fractions allowed): V>0 is up/away,
                                 H>0 is right; sends hi-res (v120) and detent events
  ev DEV TYPE CODE VALUE         raw event, no SYN (DEV: kbd|ptr|rel|touchpad)
  syn [DEV]                      a SYN_REPORT on DEV, or on every device
  release                        release what this connection holds, and what
                                 keep-held connections left pressed
  keep-held                      keep what this connection holds pressed after it closes
  info                           the devices and the absolute pointer's mapping
  sleep MS                       (client side) pause between verbs

Every verb but `ev` ends each of its frames with a SYN_REPORT. The absolute device spans
the guest's WHOLE desktop: with one display, abs-norm/abs-px are that display's
coordinates; with several, only `abs` is well defined. Keys are evdev codes, not macOS
keys: no modifier normalization applies (KEY_LEFTMETA is Super, KEY_LEFTALT is Alt).
Keys and buttons a connection leaves pressed are released when it closes, unless
`keep-held` (or --keep-held) was sent.";

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
    },
    AbsPx {
        x: f64,
        y: f64,
        mode: Option<(u32, u32)>,
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
            "abs-norm" => Verb::AbsNorm {
                u: unit(w.next(), "u")?,
                v: unit(w.next(), "v")?,
            },
            "abs-px" => {
                let x: f64 = num(w.next(), "x")?;
                let y: f64 = num(w.next(), "y")?;
                let mode = w.next().map(parse_mode).transpose()?;
                if x < 0.0 || y < 0.0 || !x.is_finite() || !y.is_finite() {
                    return Err("abs-px takes non-negative pixels".into());
                }
                Verb::AbsPx { x, y, mode }
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
    let v = ((p + 0.25) * f64::from(ABS_MAX + 1) / f64::from(size)).floor();
    v.clamp(0.0, f64::from(ABS_MAX)) as i32
}

/// Keys and buttons pressed and not yet released, each in the order it was pressed — so a
/// release can undo them in reverse, as a hand would (Super-Tab lets go of Tab first).
#[derive(Debug, Default)]
pub struct Held {
    keys: Vec<u16>,
    buttons: Vec<u16>,
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
        }
    }

    fn note(&mut self, dev: Device, ev: InputEvent) {
        if ev.type_ != EV_KEY {
            return;
        }
        let set = match dev {
            Device::Kbd => &mut self.keys,
            Device::Ptr => &mut self.buttons,
            _ => return,
        };
        if ev.value == 0 {
            set.retain(|&c| c != ev.code);
        } else {
            press(set, ev.code);
        }
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    fn absorb(&mut self, other: Held) {
        for k in other.keys {
            press(&mut self.keys, k);
        }
        for b in other.buttons {
            press(&mut self.buttons, b);
        }
    }

    /// The frames that release all of it: keys last-pressed first, then buttons likewise.
    fn release_frames(&self) -> Vec<Frame> {
        let keys = self.keys.iter().rev().map(|&k| (Device::Kbd, k));
        let buttons = self.buttons.iter().rev().map(|&b| (Device::Ptr, b));
        keys.chain(buttons)
            .map(|(dev, code)| Frame::one(dev, InputEvent::new(EV_KEY, code, 0)))
            .collect()
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

/// One connection's injection state: what it holds, and the scroll remainders.
#[derive(Debug, Default)]
pub struct Session {
    held: Held,
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
                 abs-px is only defined for one (use `abs` in device units)",
                many.len()
            ));
        }
    }
    lock(&HEADLESS_MODE)
        .ok_or_else(|| "the guest's mode is not known yet; give it: abs-px X Y WxH".into())
}

impl Session {
    /// The frames a verb sends, and the report lines it answers with. Pure but for the mode
    /// lookup and the held-set bookkeeping, so it is what the tests drive.
    fn plan(&mut self, verb: Verb) -> Result<(Vec<Frame>, Vec<String>), String> {
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
            Verb::AbsNorm { u, v } => abs(norm_to_device(u), norm_to_device(v)),
            Verb::AbsPx { x, y, mode } => {
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
                all.absorb(std::mem::take(&mut self.held));
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
                let h = lock(&ORPHANS);
                if !self.held.is_empty() || !h.is_empty() {
                    report.push(format!(
                        "held keys {:?} buttons {:?} (kept {:?} {:?})",
                        self.held.keys, self.held.buttons, h.keys, h.buttons
                    ));
                }
                return Ok((Vec::new(), report));
            }
        };
        Ok((frames, Vec::new()))
    }

    /// Run one verb (the text after `input `) against the current worker.
    pub fn run(&mut self, line: &str) -> Result<Vec<String>, String> {
        let verb = Verb::parse(line)?;
        let release = verb == Verb::Release;
        let (frames, report) = self.plan(verb)?;
        if !frames.is_empty()
            && let Err((sent, why)) = self.send(&frames)
        {
            // The release took everything it was asked to undo out of the ledgers; what did
            // not go out is still pressed in the guest, and a later `release` must find it.
            if release {
                lock(&ORPHANS).absorb(Held::unreleased(&frames[sent..]));
            }
            return Err(why);
        }
        Ok(report)
    }

    /// Send `frames`, each under its device's frame lock (taken after the frame's pause, never
    /// across it). On failure, how many frames went out whole, and why the next did not.
    fn send(&mut self, frames: &[Frame]) -> Result<(), (usize, String)> {
        let conn = lock(&CONN).clone().ok_or((
            0,
            "this VM has no input devices to inject into (a headless run needs --input)"
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
                self.held.note(f.dev, ev);
            }
        }
        Ok(())
    }
}

impl Drop for Session {
    /// The connection is gone: release what it holds, unless it asked to keep it.
    fn drop(&mut self) {
        let held = std::mem::take(&mut self.held);
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
        let frames = held.release_frames();
        if let Err((sent, why)) = self.send(&frames) {
            log::warn!(
                "input: could not release what a client left pressed ({why}); a later \
                 `release` will retry"
            );
            lock(&ORPHANS).absorb(Held::unreleased(&frames[sent..]));
        }
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

/// Send verbs to the supervisor with this pid. `verb` empty or `["-"]` reads them from stdin,
/// one per line (`#` comments, blank lines and client-side `sleep MS` allowed). Report lines are
/// printed; the first refusal is an error.
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
    let mut reader = BufReader::new(&stream);
    let mut ask = |line: &str| -> Result<()> {
        // One verb is one protocol line: a newline inside it would be a second request.
        anyhow::ensure!(
            !line.contains(['\n', '\r']),
            "a verb cannot contain a line break: {line:?}"
        );
        (&stream)
            .write_all(format!("{PREFIX} {line}\n").as_bytes())
            .context("sending to the supervisor")?;
        let report =
            wire::read_answer(&mut reader).map_err(|why| anyhow::anyhow!("{line}: {why}"))?;
        for l in report {
            println!("{l}");
        }
        Ok(())
    };
    if keep_held {
        ask("keep-held")?;
    }
    if !(verb.is_empty() || verb == ["-"]) {
        return ask(&verb.join(" "));
    }
    for line in std::io::stdin().lock().lines() {
        let line = line.context("reading verbs from stdin")?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(ms) = line.strip_prefix("sleep ") {
            let ms: u64 = ms
                .trim()
                .parse()
                .with_context(|| format!("sleep {ms:?}: not milliseconds"))?;
            std::thread::sleep(Duration::from_millis(ms));
            continue;
        }
        ask(line)?;
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

    /// The whole loop below the verbs: a session's events reach a worker's socket, a dropped
    /// session releases what it held, and a swapped worker gets the next verb.
    #[test]
    fn events_reach_the_current_worker_and_a_closed_session_releases() {
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
        fn worker() -> (WorkerIo, [OwnedFd; 4]) {
            let (k, kw) = pair();
            let (p, pw) = pair();
            let (r, rw) = pair();
            let (t, tw) = pair();
            let ack: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
            (WorkerIo::new(1, k, p, r, t, ack), [kw, pw, rw, tw])
        }
        let _serial = lock(&TEST_SERIAL);

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
        *lock(&CONN) = None;
    }

    /// The tests that touch the process-global connection.
    static TEST_SERIAL: Mutex<()> = Mutex::new(());
}
