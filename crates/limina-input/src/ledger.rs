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
//! nothing.

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

    /// A key-down or key-up, as the monitor and the tap both forward it: Caps Lock aligned, the
    /// modifiers healed before a press so it lands wearing what the user holds, then the key.
    pub fn key(&mut self, macos_keycode: u16, down: bool, flags: u64, out: &mut Vec<Edge>) {
        self.sync_capslock(flags, out);
        if down {
            self.sync_modifiers(flags, None, out);
        }
        self.emit_key(macos_keycode, down, out);
    }

    /// A `flagsChanged` the caller forwards: Caps Lock aligned, the other modifiers healed, then
    /// this one's edge.
    pub fn flags_changed(&mut self, macos_keycode: u16, flags: u64, out: &mut Vec<Edge>) {
        self.sync_capslock(flags, out);
        self.sync_modifiers(flags, Some(macos_keycode), out);
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
