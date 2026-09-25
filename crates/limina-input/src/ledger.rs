// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The keyboard's held-key bookkeeping: what the guest has been told is down, and the edges that
//! keep it in step with the host.
//!
//! The supervisor's input path (`crates/limina/src/window/input.rs`) owns a [`KeyLedger`] and
//! sends the [`Edge`]s it returns. Every rule about *what* to send lives here and is pure: which
//! modifiers to heal before a key goes out ([`crate::keymap::reconcile_modifiers`]), when a
//! `flagsChanged` is an edge ([`crate::keymap::modifier_emit`]), when Caps Lock needs a tap
//! ([`crate::keymap::CapsLockSync`]), and what a focus loss, a grab or a normalization flip must
//! release. The input path adds only what needs AppKit: which events reach here at all, the
//! ungrab chord, and the socket.
//!
//! The invariant the rest rests on: **the guest holds exactly the evdev codes of the keys the
//! ledger believes held**, so releasing everything the ledger holds leaves the guest holding
//! nothing. `every_sequence` below checks it against a model keyboard and a model guest.

use std::collections::HashSet;

use crate::keymap::{
    CapsLockSync, KeyRemap, MACOS_KC_CAPSLOCK, MODIFIER_KEYCODES, ModEmit, capslock_on,
    macos_keycode_to_linux_remapped, modifier_emit, reconcile_modifiers,
};

/// Why an [`Edge`] was sent, for the input path's trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// A non-modifier key the host pressed or released.
    Key,
    /// A modifier edge the event named.
    Modifier,
    /// A modifier edge healed from the event's flags (a modifier that moved while we were not
    /// looking).
    Resync,
    /// Half of a Caps Lock tap, aligning the guest's lock with the host LED.
    Caps,
    /// A media or volume key (already an evdev code; no keycode, no remap).
    Aux,
    /// A release forced by a focus loss, a grab change or a normalization flip.
    Flush,
}

/// One key edge for the guest: `code` down or up. The input path sends it as an `EV_KEY` and a
/// `SYN_REPORT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeKind,
    /// The macOS keycode behind it, where there is one (not for [`EdgeKind::Aux`]).
    pub macos_keycode: Option<u16>,
    pub code: u16,
    pub down: bool,
}

/// What the guest has been told is held, and the remap it was told through.
#[derive(Clone, Debug, Default)]
pub struct KeyLedger {
    /// macOS keycodes of held modifiers believed down.
    mods: HashSet<u16>,
    /// macOS keycodes of held non-modifier keys forwarded as down and not yet up, so a focus
    /// loss mid-press can release them.
    keys: HashSet<u16>,
    /// evdev codes of held aux keys (a different namespace from the keycodes above).
    aux: HashSet<u16>,
    caps: CapsLockSync,
    remap: KeyRemap,
}

impl KeyLedger {
    pub fn new(remap: KeyRemap) -> Self {
        Self {
            remap,
            ..Self::default()
        }
    }

    pub fn remap(&self) -> KeyRemap {
        self.remap
    }

    /// The macOS keycodes of the modifiers believed held.
    pub fn held_modifiers(&self) -> &HashSet<u16> {
        &self.mods
    }

    /// How many modifiers, keys and aux keys are believed held.
    pub fn held_counts(&self) -> (usize, usize, usize) {
        (self.mods.len(), self.keys.len(), self.aux.len())
    }

    pub fn is_aux_pressed(&self, code: u16) -> bool {
        self.aux.contains(&code)
    }

    pub fn any_aux_pressed(&self) -> bool {
        !self.aux.is_empty()
    }

    /// Whether this macOS keycode has any guest equivalent at all.
    pub fn maps_to_guest(&self, macos_keycode: u16) -> bool {
        self.code(macos_keycode).is_some()
    }

    fn code(&self, macos_keycode: u16) -> Option<u16> {
        macos_keycode_to_linux_remapped(macos_keycode, &self.remap)
    }

    /// A non-modifier key crossing into the guest. Tracked while held so a flush can release it.
    pub fn emit_key(&mut self, macos_keycode: u16, down: bool, out: &mut Vec<Edge>) {
        let Some(code) = self.code(macos_keycode) else {
            return;
        };
        out.push(Edge {
            kind: EdgeKind::Key,
            macos_keycode: Some(macos_keycode),
            code,
            down,
        });
        if down {
            self.keys.insert(macos_keycode);
        } else {
            self.keys.remove(&macos_keycode);
        }
    }

    /// A `flagsChanged` for a held modifier: an edge only when the flags say the key's state
    /// differs from what the guest believes (see [`modifier_emit`]).
    pub fn emit_modifier(&mut self, macos_keycode: u16, flags: u64, out: &mut Vec<Edge>) {
        self.modifier_edge(macos_keycode, flags, EdgeKind::Modifier, out);
    }

    fn modifier_edge(
        &mut self,
        macos_keycode: u16,
        flags: u64,
        kind: EdgeKind,
        out: &mut Vec<Edge>,
    ) {
        let Some(code) = self.code(macos_keycode) else {
            return;
        };
        let was = self.mods.contains(&macos_keycode);
        let Some(ModEmit::Edge(down)) = modifier_emit(macos_keycode, flags, was) else {
            return;
        };
        if down {
            self.mods.insert(macos_keycode);
        } else {
            self.mods.remove(&macos_keycode);
        }
        out.push(Edge {
            kind,
            macos_keycode: Some(macos_keycode),
            code,
            down,
        });
    }

    /// Align the guest's **held** modifiers with the host bitmask, emitting whatever edges the
    /// two disagree about. The held-modifier twin of [`Self::sync_capslock`], and it heals the
    /// same blind spot: a modifier that goes down (or up) while the window isn't receiving events
    /// is never mentioned again, because macOS sends no reconciling `flagsChanged` on refocus and
    /// the key does not move until it is released.
    ///
    /// The case that motivated it (2026-08-09, `spikes/modifier-drift/`): Control held through a
    /// Space switch. Leaving the Space correctly releases it in the guest; coming back restored
    /// nothing, so the next key arrived unmodified — a bare Super, which GNOME reads as "open the
    /// overview". Every bitmask in between carried the Control bit; nothing read it.
    ///
    /// `except` is the modifier the caller is about to emit itself — see
    /// [`reconcile_modifiers`], where excluding it is what keeps Control ahead of Super.
    ///
    /// The input path calls it from the `flagsChanged` and key-down paths only, both of which
    /// carry device-dependent bits. Pointer events are left out: they would heal at class
    /// granularity at best, and [`reconcile_modifiers`] refuses to press on that evidence anyway.
    pub fn sync_modifiers(&mut self, flags: u64, except: Option<u16>, out: &mut Vec<Edge>) {
        for (macos_keycode, _) in reconcile_modifiers(flags, &self.mods, except) {
            self.modifier_edge(macos_keycode, flags, EdgeKind::Resync, out);
        }
    }

    /// Align the guest's Caps Lock with the host LED: a press and a release when they differ.
    pub fn sync_capslock(&mut self, flags: u64, out: &mut Vec<Edge>) {
        if self.caps.observe(capslock_on(flags))
            && let Some(code) = self.code(MACOS_KC_CAPSLOCK)
        {
            for down in [true, false] {
                out.push(Edge {
                    kind: EdgeKind::Caps,
                    macos_keycode: Some(MACOS_KC_CAPSLOCK),
                    code,
                    down,
                });
            }
        }
    }

    /// A key-down or key-up, as the monitor and the tap both forward it: the modifiers healed
    /// before a press, Caps Lock aligned, then the key, so the key (and a Caps Lock tap) lands
    /// wearing what the user holds.
    pub fn key(&mut self, macos_keycode: u16, down: bool, flags: u64, out: &mut Vec<Edge>) {
        if down {
            self.sync_modifiers(flags, None, out);
        }
        self.sync_capslock(flags, out);
        self.emit_key(macos_keycode, down, out);
    }

    /// A `flagsChanged` the caller forwards: the other modifiers healed, Caps Lock aligned, then
    /// this one's edge. Healing comes first so a Caps Lock tap never goes out wearing a modifier
    /// the user let go of while the window was not looking.
    pub fn flags_changed(&mut self, macos_keycode: u16, flags: u64, out: &mut Vec<Edge>) {
        self.sync_modifiers(flags, Some(macos_keycode), out);
        self.sync_capslock(flags, out);
        self.emit_modifier(macos_keycode, flags, out);
    }

    /// A media or volume key, already resolved to its evdev code.
    pub fn aux(&mut self, code: u16, down: bool, out: &mut Vec<Edge>) {
        out.push(Edge {
            kind: EdgeKind::Aux,
            macos_keycode: None,
            code,
            down,
        });
        if down {
            self.aux.insert(code);
        } else {
            self.aux.remove(&code);
        }
    }

    /// Release every key believed held, through the map that pressed it, and forget them: a
    /// focus loss, a park, a grab. State is re-learned from the next events.
    pub fn release_all_held(&mut self, out: &mut Vec<Edge>) {
        for &macos_keycode in self.mods.iter().chain(self.keys.iter()) {
            if let Some(code) = self.code(macos_keycode) {
                out.push(Edge {
                    kind: EdgeKind::Flush,
                    macos_keycode: Some(macos_keycode),
                    code,
                    down: false,
                });
            }
        }
        self.flush_aux(out);
        self.mods.clear();
        self.keys.clear();
    }

    /// Release every held-modifier key, believed held or not, and every aux key: the end of a
    /// capture, when the ungrab chord is itself mid-press. Held non-modifier keys stay.
    pub fn release_all_modifiers(&mut self, out: &mut Vec<Edge>) {
        for macos_keycode in MODIFIER_KEYCODES {
            if let Some(code) = self.code(macos_keycode) {
                out.push(Edge {
                    kind: EdgeKind::Flush,
                    macos_keycode: Some(macos_keycode),
                    code,
                    down: false,
                });
            }
        }
        self.mods.clear();
        self.flush_aux(out);
    }

    fn flush_aux(&mut self, out: &mut Vec<Edge>) {
        for &code in &self.aux {
            out.push(Edge {
                kind: EdgeKind::Flush,
                macos_keycode: None,
                code,
                down: false,
            });
        }
        self.aux.clear();
    }

    /// Turn modifier normalization on or off, draining everything held through the old map
    /// first: a press and its release are two mappings of one keycode, so flipping between them
    /// would press one evdev code and release another. Returns whether anything changed.
    pub fn set_normalize(&mut self, on: bool, out: &mut Vec<Edge>) -> bool {
        if self.remap.normalize == on {
            return false;
        }
        self.release_all_held(out);
        self.remap.normalize = on;
        true
    }
}

/// Every sequence of keyboard events to depth four, from a model keyboard through macOS's
/// modifier remap into the ledger, and the ledger's edges into a model guest.
///
/// The keyboard holds left Control, Option and Command, right Command, Shift, `A`, Caps Lock and a
/// volume key (an aux key). macOS turns a physical modifier into the one that *arrives* through
/// its Modifier Keys setting: none, Control↔Command, or Option↔Command. An event's flags carry
/// the arrived modifiers' left/right bits, their class bits and the Caps Lock LED, as macOS's do.
/// Besides the events the ledger sees, a key can change while the window is not looking (the
/// press or release is never delivered), focus can be lost, a capture can end, and
/// normalization can be flipped from the Input menu. The guest applies each edge as evdev does:
/// a press of a held key and a release of an unheld one change nothing, and Caps Lock toggles
/// its lock on a press.
///
/// After every step:
/// - the guest holds exactly the codes of what the ledger believes held, so a flush leaves it
///   holding nothing;
/// - the press an event names lands with every other modifier the user holds already down in the
///   guest, and after any event carrying flags that heals, the guest's modifiers are exactly the ones
///   physically held, as the guest should see them: by position when normalization is on (Option
///   is Super, Command is Alt, whatever macOS has swapped), macOS's own meaning when it is off;
/// - after any event carrying flags, the guest's Caps Lock matches the LED;
/// - after a focus loss the guest holds nothing, and after a capture ends no modifier or aux key.
#[cfg(test)]
mod every_sequence {
    use std::collections::BTreeSet;

    use super::KeyLedger;
    use crate::constants::*;
    use crate::keymap::*;

    const DEPTH: usize = 4;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Key {
        LCtrl,
        LOpt,
        LCmd,
        RCmd,
        LShift,
        A,
        Caps,
        VolUp,
    }
    use Key::*;
    const KEYS: [Key; 8] = [LCtrl, LOpt, LCmd, RCmd, LShift, A, Caps, VolUp];
    const MODIFIERS: [Key; 5] = [LCtrl, LOpt, LCmd, RCmd, LShift];

    fn usage(k: Key) -> Option<u32> {
        Some(match k {
            LCtrl => HID_LEFT_CONTROL,
            LOpt => HID_LEFT_OPTION,
            LCmd => HID_LEFT_COMMAND,
            RCmd => HID_RIGHT_COMMAND,
            _ => return None,
        })
    }

    fn keycode_of_usage(u: u32) -> u16 {
        match u {
            HID_LEFT_CONTROL => 0x3B,
            HID_LEFT_OPTION => 0x3A,
            HID_LEFT_COMMAND => 0x37,
            HID_RIGHT_CONTROL => 0x3E,
            HID_RIGHT_OPTION => 0x3D,
            HID_RIGHT_COMMAND => 0x36,
            _ => unreachable!(),
        }
    }

    /// Each keycode's left/right flag bit and class bit (IOLLEvent.h, `NSEventModifierFlag*`).
    fn flag_bits(kc: u16) -> u64 {
        const SHIFT: u64 = 1 << 17;
        const CONTROL: u64 = 1 << 18;
        const OPTION: u64 = 1 << 19;
        const COMMAND: u64 = 1 << 20;
        match kc {
            0x37 => 0x08 | COMMAND,
            0x36 => 0x10 | COMMAND,
            0x38 => 0x02 | SHIFT,
            0x3A => 0x20 | OPTION,
            0x3D => 0x40 | OPTION,
            0x3B => 0x01 | CONTROL,
            0x3E => 0x2000 | CONTROL,
            _ => unreachable!(),
        }
    }

    /// macOS's Modifier Keys setting, as the `(src, dst)` pairs it stores: pressing `src` acts
    /// as `dst`.
    #[derive(Clone, Copy, Debug)]
    enum Setting {
        Identity,
        CtrlCmd,
        OptCmd,
    }
    const SETTINGS: [Setting; 3] = [Setting::Identity, Setting::CtrlCmd, Setting::OptCmd];

    fn pairs(s: Setting) -> Vec<(u32, u32)> {
        let swap = |a, b, c, d| vec![(a, b), (b, a), (c, d), (d, c)];
        match s {
            Setting::Identity => vec![],
            Setting::CtrlCmd => swap(
                HID_LEFT_CONTROL,
                HID_LEFT_COMMAND,
                HID_RIGHT_CONTROL,
                HID_RIGHT_COMMAND,
            ),
            Setting::OptCmd => swap(
                HID_LEFT_OPTION,
                HID_LEFT_COMMAND,
                HID_RIGHT_OPTION,
                HID_RIGHT_COMMAND,
            ),
        }
    }

    /// The keycode a physical key arrives as under the setting.
    fn arrival(s: Setting, k: Key) -> u16 {
        match usage(k) {
            Some(u) => {
                let to = pairs(s)
                    .into_iter()
                    .find(|&(src, _)| src == u)
                    .map_or(u, |(_, dst)| dst);
                keycode_of_usage(to)
            }
            None => match k {
                LShift => 0x38,
                A => 0x00,
                Caps => 0x39,
                _ => unreachable!(),
            },
        }
    }

    /// The code the guest should see for a held physical modifier, written from the rule rather
    /// than from `KeyRemap`: by position when normalizing, macOS's meaning otherwise.
    fn expected_code(s: Setting, normalize: bool, k: Key) -> u16 {
        if normalize {
            match k {
                LCtrl => KEY_LEFTCTRL,
                LOpt => KEY_LEFTMETA,
                LCmd => KEY_LEFTALT,
                RCmd => KEY_RIGHTALT,
                LShift => KEY_LEFTSHIFT,
                _ => unreachable!(),
            }
        } else {
            match arrival(s, k) {
                0x3B => KEY_LEFTCTRL,
                0x3E => KEY_RIGHTCTRL,
                0x3A => KEY_LEFTALT,
                0x3D => KEY_RIGHTALT,
                0x37 => KEY_LEFTMETA,
                0x36 => KEY_RIGHTMETA,
                0x38 => KEY_LEFTSHIFT,
                _ => unreachable!(),
            }
        }
    }

    const MODIFIER_CODES: [u16; 9] = [
        KEY_LEFTCTRL,
        KEY_RIGHTCTRL,
        KEY_LEFTALT,
        KEY_RIGHTALT,
        KEY_LEFTMETA,
        KEY_RIGHTMETA,
        KEY_LEFTSHIFT,
        KEY_RIGHTSHIFT,
        KEY_CAPSLOCK,
    ];

    #[derive(Clone, Copy, Debug)]
    enum Op {
        /// A physical press or release the ledger is told about.
        Press(Key),
        Release(Key),
        /// A physical press or release while the window is not looking.
        Blind(Key, bool),
        /// A pointer event, which carries flags but heals only Caps Lock.
        Pointer,
        FocusLoss,
        CaptureEnds,
        ToggleNormalize,
    }

    fn alphabet() -> Vec<Op> {
        let mut ops = Vec::new();
        for k in KEYS {
            if k == Caps {
                // Caps Lock toggles its LED on a press; macOS reports nothing on its release.
                ops.push(Op::Press(Caps));
                ops.push(Op::Blind(Caps, true));
                continue;
            }
            ops.push(Op::Press(k));
            ops.push(Op::Release(k));
            if k != VolUp {
                ops.push(Op::Blind(k, true));
                ops.push(Op::Blind(k, false));
            }
        }
        ops.extend([
            Op::Pointer,
            Op::FocusLoss,
            Op::CaptureEnds,
            Op::ToggleNormalize,
        ]);
        ops
    }

    struct World {
        setting: Setting,
        normalize: bool,
        down: [bool; KEYS.len()],
        led: bool,
        ledger: KeyLedger,
        guest: BTreeSet<u16>,
        guest_caps: bool,
    }

    fn idx(k: Key) -> usize {
        KEYS.iter().position(|&x| x == k).unwrap()
    }

    impl World {
        fn new(setting: Setting) -> Self {
            let remap = KeyRemap {
                normalize: true,
                host: HostModifierMap::from_pairs(pairs(setting)),
            };
            World {
                setting,
                normalize: true,
                down: [false; KEYS.len()],
                led: false,
                ledger: KeyLedger::new(remap),
                guest: BTreeSet::new(),
                guest_caps: false,
            }
        }

        fn flags(&self) -> u64 {
            let mut f = 0x100;
            if self.led {
                f |= 1 << 16;
            }
            for k in MODIFIERS {
                if self.down[idx(k)] {
                    f |= flag_bits(arrival(self.setting, k));
                }
            }
            f
        }

        fn apply(&mut self, edges: &[super::Edge]) {
            for e in edges {
                if e.down {
                    if self.guest.insert(e.code) && e.code == KEY_CAPSLOCK {
                        self.guest_caps = !self.guest_caps;
                    }
                } else {
                    self.guest.remove(&e.code);
                }
            }
        }

        /// What the guest should believe held: the codes of every key the ledger holds.
        fn believed(&self) -> BTreeSet<u16> {
            let l = &self.ledger;
            l.mods
                .iter()
                .chain(l.keys.iter())
                .filter_map(|&kc| l.code(kc))
                .chain(l.aux.iter().copied())
                .collect()
        }

        fn expected_modifiers(&self) -> BTreeSet<u16> {
            MODIFIERS
                .into_iter()
                .filter(|&k| self.down[idx(k)])
                .map(|k| expected_code(self.setting, self.normalize, k))
                .collect()
        }

        fn guest_modifiers(&self) -> BTreeSet<u16> {
            self.guest
                .iter()
                .copied()
                .filter(|c| MODIFIER_CODES.contains(c))
                .collect()
        }

        fn step(&mut self, op: Op) -> Result<(), String> {
            let mut out = Vec::new();
            let mut heals = false;
            // The keycode the event names, whose press must land last.
            let mut own = None;
            let mut carries_flags = false;
            match op {
                Op::Press(k) | Op::Release(k) => {
                    let press = matches!(op, Op::Press(_));
                    if k != Caps && self.down[idx(k)] == press {
                        return Ok(());
                    }
                    if k == Caps {
                        self.led = !self.led;
                    } else {
                        self.down[idx(k)] = press;
                    }
                    let flags = self.flags();
                    carries_flags = k != VolUp;
                    match k {
                        VolUp => self.ledger.aux(KEY_VOLUMEUP, press, &mut out),
                        A => {
                            let kc = arrival(self.setting, A);
                            self.ledger.key(kc, press, flags, &mut out);
                            heals = press;
                            own = Some(kc);
                        }
                        _ => {
                            let kc = arrival(self.setting, k);
                            self.ledger.flags_changed(kc, flags, &mut out);
                            heals = true;
                            own = Some(kc);
                        }
                    }
                }
                Op::Blind(Caps, _) => self.led = !self.led,
                Op::Blind(k, press) => self.down[idx(k)] = press,
                Op::Pointer => {
                    self.ledger.sync_capslock(self.flags(), &mut out);
                    carries_flags = true;
                }
                Op::FocusLoss => self.ledger.release_all_held(&mut out),
                Op::CaptureEnds => self.ledger.release_all_modifiers(&mut out),
                Op::ToggleNormalize => {
                    self.normalize = !self.normalize;
                    self.ledger.set_normalize(self.normalize, &mut out);
                }
            }
            // The event's own press must land wearing the modifiers the user holds: every heal
            // edge goes out before it, or Super reaches the guest ahead of the Control it was
            // pressed under.
            for e in &out {
                if heals && e.down && e.macos_keycode == own {
                    let mut want = self.expected_modifiers();
                    want.remove(&e.code);
                    if self.guest_modifiers() != want {
                        return Err(format!(
                            "code {} went out wearing {:?}, the user holds {want:?}",
                            e.code,
                            self.guest_modifiers()
                        ));
                    }
                }
                self.apply(std::slice::from_ref(e));
            }

            if self.guest != self.believed() {
                return Err(format!(
                    "the guest holds {:?}, the ledger believes {:?}",
                    self.guest,
                    self.believed()
                ));
            }
            if heals && self.guest_modifiers() != self.expected_modifiers() {
                return Err(format!(
                    "the guest's modifiers are {:?}, the user holds {:?}",
                    self.guest_modifiers(),
                    self.expected_modifiers()
                ));
            }
            if carries_flags && self.guest_caps != self.led {
                return Err(format!(
                    "the guest's Caps Lock is {}, the LED {}",
                    self.guest_caps, self.led
                ));
            }
            match op {
                Op::FocusLoss if !self.guest.is_empty() => {
                    return Err(format!("a focus loss left {:?} held", self.guest));
                }
                Op::CaptureEnds
                    if self
                        .guest
                        .iter()
                        .any(|c| MODIFIER_CODES.contains(c) || *c == KEY_VOLUMEUP) =>
                {
                    return Err(format!("the end of a capture left {:?} held", self.guest));
                }
                _ => {}
            }
            Ok(())
        }
    }

    fn run(setting: Setting, seq: &[Op]) -> Result<(), String> {
        let mut w = World::new(setting);
        for (i, &op) in seq.iter().enumerate() {
            w.step(op)
                .map_err(|m| format!("{setting:?} {:?}, at step {}: {m}", &seq[..=i], i + 1))?;
        }
        Ok(())
    }

    fn walk(setting: Setting, ops: &[Op], seq: &mut Vec<Op>, walked: &mut u64) {
        if seq.len() == DEPTH {
            *walked += 1;
            if let Err(m) = run(setting, seq) {
                panic!("{m}");
            }
            return;
        }
        for &op in ops {
            seq.push(op);
            walk(setting, ops, seq, walked);
            seq.pop();
        }
    }

    /// Control let go while the window was not looking, then Caps Lock: the tap must not go out
    /// while the guest still holds the Control.
    #[test]
    fn a_caps_lock_tap_waits_for_the_modifiers_to_heal() {
        run(
            Setting::Identity,
            &[Op::Press(LCtrl), Op::Blind(LCtrl, false), Op::Press(Caps)],
        )
        .unwrap_or_else(|m| panic!("{m}"));
    }

    #[test]
    fn every_keyboard_sequence_keeps_the_guest_in_step() {
        let ops = alphabet();
        let mut walked = 0;
        for setting in SETTINGS {
            walk(setting, &ops, &mut Vec::with_capacity(DEPTH), &mut walked);
        }
        assert_eq!(walked, 3 * (ops.len() as u64).pow(DEPTH as u32));
    }
}
