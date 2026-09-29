// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The guest multitouch touchpad: its geometry, and the encoder that turns the host's
//! per-frame contact list into Linux MT protocol B events.
//!
//! The guest's libinput classifies the device as a clickpad and does its own gesture
//! processing (scroll, pinch, swipes) from raw contacts, so the host only reports where each
//! finger is. It carries two or three fingers, never one or four: a single finger stays the
//! host tablet's and four are the host's gestures (`docs/design/trackpad-gestures.md`).
//!
//! The encoder is pure and host-side; the worker only advertises the device
//! ([`crate::backends`]) and forwards the events the supervisor writes.

use crate::InputEvent;
use crate::constants::*;

/// Contact slots the device advertises (`ABS_MT_SLOT` 0..=2). Three is the most fingers the
/// guest ever owns; the device advertises only what can actually arrive.
pub const MAX_SLOTS: usize = 3;

/// Device units per millimetre on both axes. Positions are reported in 0.01 mm, which is the
/// unit `MTDeviceGetSensorSurfaceDimensions` answers in.
pub const TOUCHPAD_RES: u32 = 100;

/// Largest tracking id the device advertises; ids wrap below it.
pub const MAX_TRACKING_ID: i32 = 0xffff;

/// The physical surface the guest is told about, in device units (0.01 mm). libinput reads it
/// once, at probe, and derives scroll distances, palm zones and gesture thresholds from it, so
/// it should be the real trackpad's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchpadGeometry {
    pub width: u32,
    pub height: u32,
}

impl TouchpadGeometry {
    /// The built-in trackpad of a 14"/16" MacBook Pro (M1 Max): 124.8 × 76.8 mm.
    pub const BUILT_IN: Self = Self {
        width: 12480,
        height: 7680,
    };

    /// Parse the worker's `WxH` flag value (device units).
    pub fn parse(s: &str) -> Option<Self> {
        let (w, h) = s.split_once('x')?;
        let geom = Self {
            width: w.trim().parse().ok()?,
            height: h.trim().parse().ok()?,
        };
        (geom.width > 0 && geom.height > 0).then_some(geom)
    }

    /// The flag value [`parse`](Self::parse) reads.
    pub fn to_arg(self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    /// Map a normalized position (`0.0..=1.0`, top-left origin) to device units.
    pub fn to_units(self, nx: f64, ny: f64) -> (i32, i32) {
        let scale = |n: f64, extent: u32| (n.clamp(0.0, 1.0) * f64::from(extent)).round() as i32;
        (scale(nx, self.width), scale(ny, self.height))
    }
}

impl Default for TouchpadGeometry {
    fn default() -> Self {
        Self::BUILT_IN
    }
}

/// One finger on the host surface, in device units. `id` is the host's identity for the
/// finger, stable for as long as it stays down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contact {
    pub id: u64,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    host_id: u64,
    tracking_id: i32,
    x: i32,
    y: i32,
}

/// The guest device's contact state, as the guest last saw it.
#[derive(Debug, Default)]
pub struct Touchpad {
    slots: [Option<Slot>; MAX_SLOTS],
    /// The slot this frame has addressed so far. `ABS_MT_SLOT` is stateful in the kernel, so
    /// within a frame it is only re-sent when it changes; each frame starts by sending it,
    /// because a guest that reset the device since has forgotten it.
    current_slot: Option<usize>,
    next_tracking_id: i32,
    /// The legacy single-pointer position last sent (`ABS_X`/`ABS_Y`).
    pointer: Option<(i32, i32)>,
}

impl Touchpad {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many contacts the guest believes are down.
    pub fn active(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Report the full set of contacts down now. Contacts no longer present are lifted, new
    /// ones get a free slot and a fresh tracking id, and moved ones report their new position.
    /// Contacts beyond [`MAX_SLOTS`] are ignored. Returns the events for one frame, ending in
    /// `SYN_REPORT`, or nothing if the guest's view is already current.
    pub fn frame(&mut self, contacts: &[Contact]) -> Vec<InputEvent> {
        let contacts = &contacts[..contacts.len().min(MAX_SLOTS)];
        let before = self.active();
        self.current_slot = None;
        let mut out = Vec::new();

        for s in 0..MAX_SLOTS {
            if let Some(slot) = self.slots[s]
                && !contacts.iter().any(|c| c.id == slot.host_id)
            {
                self.lift(s, &mut out);
            }
        }

        for c in contacts {
            match self.slot_of(c.id) {
                Some(s) => {
                    let slot = self.slots[s].expect("slot_of returns live slots");
                    if (slot.x, slot.y) != (c.x, c.y) {
                        self.select(s, &mut out);
                        if slot.x != c.x {
                            out.push(abs(ABS_MT_POSITION_X, c.x));
                        }
                        if slot.y != c.y {
                            out.push(abs(ABS_MT_POSITION_Y, c.y));
                        }
                        self.slots[s] = Some(Slot {
                            x: c.x,
                            y: c.y,
                            ..slot
                        });
                    }
                }
                None => {
                    let Some(s) = self.slots.iter().position(Option::is_none) else {
                        continue;
                    };
                    let tracking_id = self.take_tracking_id();
                    self.select(s, &mut out);
                    out.push(abs(ABS_MT_TRACKING_ID, tracking_id));
                    out.push(abs(ABS_MT_POSITION_X, c.x));
                    out.push(abs(ABS_MT_POSITION_Y, c.y));
                    self.slots[s] = Some(Slot {
                        host_id: c.id,
                        tracking_id,
                        x: c.x,
                        y: c.y,
                    });
                }
            }
        }

        self.finish(before, &mut out);
        out
    }

    /// Lift every contact, so the guest never keeps a stuck finger across a transition (the
    /// pointer leaving the view, capture toggling, the host taking the sequence over).
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        self.frame(&[])
    }

    fn slot_of(&self, host_id: u64) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.is_some_and(|s| s.host_id == host_id))
    }

    fn take_tracking_id(&mut self) -> i32 {
        let id = self.next_tracking_id;
        self.next_tracking_id = if id >= MAX_TRACKING_ID { 0 } else { id + 1 };
        id
    }

    fn select(&mut self, s: usize, out: &mut Vec<InputEvent>) {
        if self.current_slot != Some(s) {
            out.push(abs(ABS_MT_SLOT, s as i32));
            self.current_slot = Some(s);
        }
    }

    fn lift(&mut self, s: usize, out: &mut Vec<InputEvent>) {
        self.select(s, out);
        out.push(abs(ABS_MT_TRACKING_ID, -1));
        self.slots[s] = None;
    }

    /// The per-frame trailer: the legacy single-pointer axes, `BTN_TOUCH`, the one
    /// `BTN_TOOL_*` naming the finger count, and `SYN_REPORT`. virtio-input does no pointer
    /// emulation of its own, so the legacy axes are ours to send.
    fn finish(&mut self, before: usize, out: &mut Vec<InputEvent>) {
        // The oldest contact drives the legacy pointer, as the kernel's own emulation does.
        let oldest = self
            .slots
            .iter()
            .flatten()
            .min_by_key(|s| age_key(s.tracking_id, self.next_tracking_id))
            .map(|s| (s.x, s.y));
        if let Some((x, y)) = oldest {
            let (px, py) = self.pointer.unwrap_or((-1, -1));
            if px != x {
                out.push(abs(ABS_X, x));
            }
            if py != y {
                out.push(abs(ABS_Y, y));
            }
        }
        self.pointer = oldest;

        let after = self.active();
        if before != after {
            if (before == 0) != (after == 0) {
                out.push(key(BTN_TOUCH, after > 0));
            }
            if let Some(tool) = tool_for(before) {
                out.push(key(tool, false));
            }
            if let Some(tool) = tool_for(after) {
                out.push(key(tool, true));
            }
        }

        if !out.is_empty() {
            out.push(InputEvent::syn());
        }
    }
}

/// How long ago `id` was issued, given the next id to be issued; smaller is newer. Handles the
/// wrap at [`MAX_TRACKING_ID`].
fn age_key(id: i32, next: i32) -> std::cmp::Reverse<i32> {
    std::cmp::Reverse((next - id).rem_euclid(MAX_TRACKING_ID + 1))
}

fn tool_for(count: usize) -> Option<u16> {
    match count {
        1 => Some(BTN_TOOL_FINGER),
        2 => Some(BTN_TOOL_DOUBLETAP),
        3 => Some(BTN_TOOL_TRIPLETAP),
        _ => None,
    }
}

fn abs(code: u16, value: i32) -> InputEvent {
    InputEvent::new(EV_ABS, code, value)
}

fn key(code: u16, down: bool) -> InputEvent {
    InputEvent::new(EV_KEY, code, down as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u64, x: i32, y: i32) -> Contact {
        Contact { id, x, y }
    }

    #[test]
    fn two_fingers_landing_together() {
        let mut tp = Touchpad::new();
        let ev = tp.frame(&[c(10, 100, 200), c(11, 300, 400)]);
        assert_eq!(
            ev,
            vec![
                abs(ABS_MT_SLOT, 0),
                abs(ABS_MT_TRACKING_ID, 0),
                abs(ABS_MT_POSITION_X, 100),
                abs(ABS_MT_POSITION_Y, 200),
                abs(ABS_MT_SLOT, 1),
                abs(ABS_MT_TRACKING_ID, 1),
                abs(ABS_MT_POSITION_X, 300),
                abs(ABS_MT_POSITION_Y, 400),
                abs(ABS_X, 100),
                abs(ABS_Y, 200),
                key(BTN_TOUCH, true),
                key(BTN_TOOL_DOUBLETAP, true),
                InputEvent::syn(),
            ]
        );
        assert_eq!(tp.active(), 2);
    }

    #[test]
    fn motion_sends_only_changed_axes_and_legacy_follows_the_oldest() {
        let mut tp = Touchpad::new();
        tp.frame(&[c(10, 100, 200), c(11, 300, 400)]);
        // Only the second finger moves, and only in x: the legacy pointer stays put.
        let ev = tp.frame(&[c(10, 100, 200), c(11, 350, 400)]);
        assert_eq!(
            ev,
            vec![
                abs(ABS_MT_SLOT, 1),
                abs(ABS_MT_POSITION_X, 350),
                InputEvent::syn()
            ]
        );
        // The first finger moves: the slot is re-selected and the legacy axes follow it.
        let ev = tp.frame(&[c(10, 100, 250), c(11, 350, 400)]);
        assert_eq!(
            ev,
            vec![
                abs(ABS_MT_SLOT, 0),
                abs(ABS_MT_POSITION_Y, 250),
                abs(ABS_Y, 250),
                InputEvent::syn(),
            ]
        );
    }

    #[test]
    fn an_unchanged_frame_sends_nothing() {
        let mut tp = Touchpad::new();
        tp.frame(&[c(1, 5, 5), c(2, 6, 6)]);
        assert!(tp.frame(&[c(2, 6, 6), c(1, 5, 5)]).is_empty());
        assert!(Touchpad::new().release_all().is_empty());
    }

    #[test]
    fn a_third_finger_swaps_the_tool_in_one_frame() {
        let mut tp = Touchpad::new();
        tp.frame(&[c(1, 0, 0), c(2, 10, 10)]);
        let ev = tp.frame(&[c(1, 0, 0), c(2, 10, 10), c(3, 20, 20)]);
        assert_eq!(
            ev,
            vec![
                abs(ABS_MT_SLOT, 2),
                abs(ABS_MT_TRACKING_ID, 2),
                abs(ABS_MT_POSITION_X, 20),
                abs(ABS_MT_POSITION_Y, 20),
                key(BTN_TOOL_DOUBLETAP, false),
                key(BTN_TOOL_TRIPLETAP, true),
                InputEvent::syn(),
            ]
        );
    }

    #[test]
    fn contacts_beyond_the_slots_are_ignored() {
        let mut tp = Touchpad::new();
        tp.frame(&[c(1, 0, 0), c(2, 0, 0), c(3, 0, 0), c(4, 0, 0)]);
        assert_eq!(tp.active(), MAX_SLOTS);
    }

    #[test]
    fn release_all_lifts_every_slot_and_every_key() {
        let mut tp = Touchpad::new();
        tp.frame(&[c(1, 0, 0), c(2, 10, 10), c(3, 20, 20)]);
        let ev = tp.release_all();
        assert_eq!(
            ev,
            vec![
                abs(ABS_MT_SLOT, 0),
                abs(ABS_MT_TRACKING_ID, -1),
                abs(ABS_MT_SLOT, 1),
                abs(ABS_MT_TRACKING_ID, -1),
                abs(ABS_MT_SLOT, 2),
                abs(ABS_MT_TRACKING_ID, -1),
                key(BTN_TOUCH, false),
                key(BTN_TOOL_TRIPLETAP, false),
                InputEvent::syn(),
            ]
        );
        assert_eq!(tp.active(), 0);
        assert!(tp.release_all().is_empty());
    }

    #[test]
    fn a_lifted_slot_is_reused_with_a_fresh_tracking_id() {
        let mut tp = Touchpad::new();
        tp.frame(&[c(1, 0, 0), c(2, 10, 10)]);
        let ev = tp.frame(&[c(2, 10, 10), c(3, 30, 30)]);
        assert_eq!(
            ev,
            vec![
                abs(ABS_MT_SLOT, 0),
                abs(ABS_MT_TRACKING_ID, -1),
                abs(ABS_MT_TRACKING_ID, 2),
                abs(ABS_MT_POSITION_X, 30),
                abs(ABS_MT_POSITION_Y, 30),
                // Finger 2 (slot 1) is now the oldest and drives the legacy pointer.
                abs(ABS_X, 10),
                abs(ABS_Y, 10),
                InputEvent::syn(),
            ]
        );
    }

    #[test]
    fn tracking_ids_wrap_and_age_survives_the_wrap() {
        let mut tp = Touchpad::new();
        tp.next_tracking_id = MAX_TRACKING_ID;
        tp.frame(&[c(1, 1, 1)]);
        tp.frame(&[c(1, 1, 1), c(2, 9, 9)]);
        assert_eq!(tp.slots[0].unwrap().tracking_id, MAX_TRACKING_ID);
        assert_eq!(tp.slots[1].unwrap().tracking_id, 0);
        // Moving the newer finger does not move the legacy pointer off the older one.
        let ev = tp.frame(&[c(1, 1, 1), c(2, 8, 8)]);
        assert!(
            !ev.iter()
                .any(|e| e.type_ == EV_ABS && (e.code == ABS_X || e.code == ABS_Y))
        );
    }

    #[test]
    fn geometry_parses_and_maps() {
        let g = TouchpadGeometry::parse("12480x7680").unwrap();
        assert_eq!(g, TouchpadGeometry::BUILT_IN);
        assert_eq!(TouchpadGeometry::parse(&g.to_arg()), Some(g));
        assert_eq!(TouchpadGeometry::parse("0x7680"), None);
        assert_eq!(TouchpadGeometry::parse("12480"), None);
        assert_eq!(g.to_units(0.5, 1.0), (6240, 7680));
        assert_eq!(g.to_units(-0.1, 1.2), (0, 7680));
    }
}
