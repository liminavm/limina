// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! macOS **aux keys** (the special/media top row) and the per-bucket policy for who owns them.
//!
//! These keys never reach us as ordinary `keyDown`s with a virtual keycode. The fn translation
//! happens in the HID layer *below* a session event tap, so whichever press macOS decides is
//! "brightness down" arrives as an `NX_SYSDEFINED` event (`CGEventType` 14) with subtype 8
//! (`NX_SUBTYPE_AUX_CONTROL_BUTTONS`) and the key packed into `NSEvent.data1` — a namespace
//! entirely separate from the virtual keycodes [`crate::keymap`] handles. That is why they used
//! to sail past the grab straight to macOS: the tap never asked for the event class.
//!
//! Ownership is decided per **bucket**, not per grab mode alone, because these keys are not
//! interchangeable. Eating Cmd-Tab while the VM is focused is expected VM behavior; eating
//! *brightness* is not — there is no backlight in the guest, so forwarding it is pure loss and
//! reads as a broken Mac. The starting policy ([`AuxBucket::owner`]):
//!
//! | bucket       | keys                          | goes to the guest        |
//! |--------------|-------------------------------|--------------------------|
//! | `Media`      | play/pause, next, prev, ff, rw | full grab only           |
//! | `Volume`     | volume up/down, mute          | full grab only           |
//! | `Brightness` | screen brightness up/down     | never (host hardware)    |
//! | `Other`      | eject, illumination, Launchpad, … | never (for now)      |
//!
//! Volume and media both stop at the full grab, for related but distinct reasons.
//!
//! Volume: our audio leaves through CoreAudio, so under a merely-focused window the physical knob
//! the user expects to move is the *host* device volume (with the host HUD). Under an explicit full
//! grab the VM owns the keyboard outright and the guest's own mixer + OSD is the right target.
//!
//! Media: focus is the wrong question for a transport key. "Who is focused" is a keyboard
//! question; "who should receive a transport command" is a media-session question macOS already
//! arbitrates system-wide — and routing on focus gets the common case backwards, denying the keys
//! exactly when music plays in the guest while the user works in a host app. So a merely-focused
//! window does not take them. limina instead announces the VM to macOS as a player while the guest
//! holds its audio device open, and the remote commands come back as these same evdev keys
//! (`docs/design/media-keys-now-playing.md`). That path also reaches the Control Center transport,
//! Siri and the headset gestures, which no bucket rule can: they never enter the keyboard event
//! stream at all.
//!
//! The full-grab entry stays, and does not double-deliver: with the window captured the tap consumes
//! the `NX_SYSDEFINED` event and macOS mints no remote command for the session path to duplicate
//! (measured — `spikes/now-playing-media-keys/RESULTS.md`).
//!
//! Known trap in that choice: under a full grab the volume keys move only the guest mixer,
//! while loudness stays capped by whatever the host volume was at grab time. Grab at host 20%
//! and the guest pins to 100% sounding like a whisper, with no in-grab way to fix it (ungrab,
//! or Ctrl-Opt, then use the host keys). Worth a hint in the UI eventually.
//!
//! The `Other` bucket is the parking lot for keys worth revisiting one at a time (Mission
//! Control → GNOME overview is a plausible future win; suspend/eject probably stay host-side
//! forever). Note this is unverified for fn+F3–F6 specifically: `spikes/fn-key-probe` has only
//! confirmed F1/F2 and F7–F12 arrive in this event class at all, and some Apple top-row keys
//! reach the system by other channels entirely — so promoting one of those is a *probe* first,
//! and only then a table edit.
//!
//! When per-key settings land, shape the config as `nx_key -> Option<GrabMode>` with the
//! buckets supplying the defaults, rather than as per-bucket overrides. Otherwise the first
//! "promote Mission Control but not Launchpad" (both `Other`) forces a refactor at the worst
//! moment. Buckets stay the UI grouping and the default source; the key is the decision unit.

use crate::constants::{
    KEY_MUTE, KEY_NEXTSONG, KEY_PLAYPAUSE, KEY_PREVIOUSSONG, KEY_VOLUMEDOWN, KEY_VOLUMEUP,
};

/// `NSEventSubtype` of an aux-key system-defined event (`NX_SUBTYPE_AUX_CONTROL_BUTTONS`).
/// Every other subtype on `NX_SYSDEFINED` is window-server bookkeeping (screen changed, app
/// activated, …) and carries no key at all.
pub const NX_SUBTYPE_AUX_CONTROL_BUTTONS: i16 = 8;

// NX_KEYTYPE_* from IOKit's `hidsystem/ev_keymap.h` (read from the SDK header, not recalled).
const NX_KEYTYPE_SOUND_UP: u16 = 0;
const NX_KEYTYPE_SOUND_DOWN: u16 = 1;
const NX_KEYTYPE_BRIGHTNESS_UP: u16 = 2;
const NX_KEYTYPE_BRIGHTNESS_DOWN: u16 = 3;
const NX_KEYTYPE_MUTE: u16 = 7;
const NX_KEYTYPE_PLAY: u16 = 16;
const NX_KEYTYPE_NEXT: u16 = 17;
const NX_KEYTYPE_PREVIOUS: u16 = 18;
const NX_KEYTYPE_FAST: u16 = 19;
const NX_KEYTYPE_REWIND: u16 = 20;

/// How much of the keyboard the VM currently owns — the gate the bucket policy reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrabMode {
    /// Neither grab is engaged: every aux key belongs to the host.
    None,
    /// Soft keyboard grab (window is key, no explicit grab gesture).
    Soft,
    /// Full pointer+keyboard capture (Cmd-Ctrl-G).
    Full,
}

impl GrabMode {
    /// How much the VM owns, as an ordering — `None` < `Soft` < `Full`. Lets a bucket state
    /// its threshold once ([`AuxBucket::min_grab`]) instead of enumerating mode pairs.
    fn rank(self) -> u8 {
        match self {
            GrabMode::None => 0,
            GrabMode::Soft => 1,
            GrabMode::Full => 2,
        }
    }
}

/// The class an aux key belongs to. Ownership is decided per bucket, so a future settings UI
/// can flip one class without disturbing the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuxBucket {
    /// Transport controls — the guest may well be running the media player.
    Media,
    /// Volume / mute — guest mixer vs. host CoreAudio device volume.
    Volume,
    /// Screen brightness — host display hardware; the guest has no backlight.
    Brightness,
    /// Everything else we haven't ruled on yet (eject, keyboard illumination, Launchpad, …).
    Other,
}

impl AuxBucket {
    /// The weakest grab at which this bucket's keys go to the guest, or `None` when the host
    /// keeps them in every mode. This *is* the policy table.
    pub fn min_grab(self) -> Option<GrabMode> {
        match self {
            // Full only. Focus is the wrong question for a transport key, so a merely-focused
            // window no longer eats them: they go to macOS, which routes them to whoever holds
            // the media session — us, while the guest is playing. See the header.
            AuxBucket::Media => Some(GrabMode::Full),
            AuxBucket::Volume => Some(GrabMode::Full),
            AuxBucket::Brightness | AuxBucket::Other => None,
        }
    }

    /// Whether this bucket's keys go to the guest under `mode`. A threshold comparison rather
    /// than a match over the pairs: when per-key config lands, a threshold is a value a user
    /// can set, and every combination has to mean something — a match with an `unreachable!`
    /// arm for a "nonsense" pairing turns a future config value into a panic.
    pub fn goes_to_guest(self, mode: GrabMode) -> bool {
        if mode == GrabMode::None {
            return false; // nothing grabbed — the host keeps every key
        }
        self.min_grab().is_some_and(|min| mode.rank() >= min.rank())
    }
}

/// One decoded aux-key press/release, as packed into `NSEvent.data1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuxKeyEvent {
    /// The `NX_KEYTYPE_*` code.
    pub nx_key: u16,
    /// `true` for a press, `false` for a release (`data1` carries the direction explicitly,
    /// unlike `flagsChanged`).
    pub down: bool,
    /// macOS-generated auto-repeat — dropped like any other repeat, since the guest kernel
    /// repeats from the held-down state itself.
    pub repeat: bool,
}

/// Unpack an aux-key `NSEvent.data1`: key in bits 16..32, key state in bits 8..16
/// (`0x0A` = down, `0x0B` = up), auto-repeat in bit 0. Returns `None` when the state nibble is
/// neither down nor up — a shape we don't understand and must not guess at.
pub fn decode_aux_data1(data1: i64) -> Option<AuxKeyEvent> {
    let nx_key = ((data1 & 0xFFFF_0000) >> 16) as u16;
    let down = match (data1 & 0xFF00) >> 8 {
        0x0A => true,
        0x0B => false,
        _ => return None,
    };
    Some(AuxKeyEvent {
        nx_key,
        down,
        repeat: data1 & 0x1 != 0,
    })
}

/// The bucket an `NX_KEYTYPE_*` code belongs to. Unknown codes are [`AuxBucket::Other`], so a
/// key we've never seen stays with the host rather than reaching the guest as nothing.
pub fn nx_key_bucket(nx_key: u16) -> AuxBucket {
    match nx_key {
        NX_KEYTYPE_PLAY | NX_KEYTYPE_NEXT | NX_KEYTYPE_PREVIOUS | NX_KEYTYPE_FAST
        | NX_KEYTYPE_REWIND => AuxBucket::Media,
        NX_KEYTYPE_SOUND_UP | NX_KEYTYPE_SOUND_DOWN | NX_KEYTYPE_MUTE => AuxBucket::Volume,
        NX_KEYTYPE_BRIGHTNESS_UP | NX_KEYTYPE_BRIGHTNESS_DOWN => AuxBucket::Brightness,
        _ => AuxBucket::Other,
    }
}

/// The evdev code for an `NX_KEYTYPE_*`, or `None` if we have no guest equivalent. Every code
/// returned here must be in `SUPPORTED_KEYBOARD_KEYS` (asserted by a test), or the guest kernel
/// drops it: the capability bitmap is read once at device init.
pub fn nx_key_to_linux(nx_key: u16) -> Option<u16> {
    Some(match nx_key {
        NX_KEYTYPE_SOUND_UP => KEY_VOLUMEUP,
        NX_KEYTYPE_SOUND_DOWN => KEY_VOLUMEDOWN,
        NX_KEYTYPE_MUTE => KEY_MUTE,
        NX_KEYTYPE_PLAY => KEY_PLAYPAUSE,
        // FAST/REWIND deliberately map to next/previous *track*, not to seek. On Apple
        // keyboards the ⏭/⏮ keys are what emit `NX_KEYTYPE_FAST`/`REWIND` — verified on this
        // Mac by `spikes/fn-key-probe` (fn+F9/F7 → NX=19/20, and NX_KEYTYPE_NEXT/PREVIOUS
        // never appear at all). The same physical keys over USB HID report Scan Next/Previous
        // Track, which Linux maps to KEY_NEXTSONG/KEY_PREVIOUSSONG — so the literal
        // FAST→KEY_FASTFORWARD reading would send the guest a code GNOME leaves unbound, i.e.
        // a dead key that looks like the forwarding is broken.
        NX_KEYTYPE_NEXT | NX_KEYTYPE_FAST => KEY_NEXTSONG,
        NX_KEYTYPE_PREVIOUS | NX_KEYTYPE_REWIND => KEY_PREVIOUSSONG,
        _ => return None,
    })
}

/// The **policy** decision for one aux key: the evdev code to send the guest, or `None` to let
/// the event through to macOS (wrong bucket for this grab mode, or no guest equivalent). Use
/// [`route_aux_event_key`] for a real event — policy alone can strand a held key.
pub fn route_aux_key(nx_key: u16, mode: GrabMode) -> Option<u16> {
    if !nx_key_bucket(nx_key).goes_to_guest(mode) {
        return None;
    }
    nx_key_to_linux(nx_key)
}

/// The decision for one *event*: policy, plus the rule that **a release always follows its
/// press**. `held` says whether we already forwarded a press for this key's evdev code.
///
/// Without the second rule, a grab mode that changes mid-press strands the key down in the
/// guest: press vol-up under a full grab (forwarded), release the grab while still holding, and
/// the key-up routes under the *new* mode straight to macOS — the guest never sees it, GNOME
/// repeats the held volume key, and it ramps to max with no way to stop it. The exception is
/// releases only: a *press* the current mode doesn't claim is still refused.
/// The rule is symmetric: a **press** goes where the policy says, a **release** goes wherever
/// its press went. Note the release arm keys off `held` alone and not on the policy — the
/// mirror case matters too. Hold a media key while the VM is unfocused (macOS acts on the
/// press), then Cmd-Tab into the VM mid-hold: the key-up now arrives under a grab that claims
/// the bucket, and forwarding it would consume a release macOS is still waiting for while
/// handing the guest an unmatched one.
pub fn route_aux_event_key(nx_key: u16, mode: GrabMode, down: bool, held: bool) -> Option<u16> {
    let code = nx_key_to_linux(nx_key)?;
    let ours = if down {
        nx_key_bucket(nx_key).goes_to_guest(mode)
    } else {
        held
    };
    ours.then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::SUPPORTED_KEYBOARD_KEYS;

    #[test]
    fn media_reaches_the_guest_only_under_an_explicit_capture() {
        assert_eq!(
            route_aux_key(NX_KEYTYPE_PLAY, GrabMode::Full),
            Some(KEY_PLAYPAUSE)
        );
        // A merely-focused window does NOT take the transport keys. They go to macOS, which
        // routes them to whoever holds the media session — limina included, while the guest is
        // playing (docs/design/media-keys-now-playing.md). Eating them here would deny the key
        // to a host player the user is actually listening to, and would take it from the guest
        // in the case that matters most: music in the VM, work in a host app.
        assert_eq!(route_aux_key(NX_KEYTYPE_PLAY, GrabMode::Soft), None);
        assert_eq!(route_aux_key(NX_KEYTYPE_PLAY, GrabMode::None), None);
    }

    #[test]
    fn volume_needs_the_full_grab_so_a_focused_window_doesnt_steal_the_host_knob() {
        assert_eq!(
            route_aux_key(NX_KEYTYPE_SOUND_UP, GrabMode::Full),
            Some(KEY_VOLUMEUP)
        );
        // The load-bearing case: merely focusing the VM must NOT take volume from the host.
        assert_eq!(route_aux_key(NX_KEYTYPE_SOUND_UP, GrabMode::Soft), None);
        assert_eq!(route_aux_key(NX_KEYTYPE_MUTE, GrabMode::Soft), None);
    }

    #[test]
    fn a_release_always_follows_its_press_across_a_mid_press_mode_change() {
        // The press goes to the guest under a full grab...
        assert_eq!(
            route_aux_key(NX_KEYTYPE_SOUND_UP, GrabMode::Full),
            Some(KEY_VOLUMEUP)
        );
        // ...and then the user ungrabs (Cmd-Ctrl-G / Ctrl-Opt) while still holding the key.
        // The policy alone would hand the release to macOS and leave KEY_VOLUMEUP down in the
        // guest forever — GNOME repeats a held volume key, so the guest ramps to max with no
        // way to stop it. Whoever got the press must get the release.
        assert_eq!(
            route_aux_event_key(NX_KEYTYPE_SOUND_UP, GrabMode::Soft, false, true),
            Some(KEY_VOLUMEUP)
        );
        // But a release we never forwarded a press for stays with the host.
        assert_eq!(
            route_aux_event_key(NX_KEYTYPE_SOUND_UP, GrabMode::Soft, false, false),
            None
        );
        // And a *press* under the wrong mode is still refused — the exception is releases only.
        assert_eq!(
            route_aux_event_key(NX_KEYTYPE_SOUND_UP, GrabMode::Soft, true, false),
            None
        );
        // The same holds all the way down to no grab at all: focus can move away mid-press
        // (the key-up then arrives with nothing grabbed), and the guest still needs its release.
        assert_eq!(
            route_aux_event_key(NX_KEYTYPE_PLAY, GrabMode::None, false, true),
            Some(KEY_PLAYPAUSE)
        );
        assert_eq!(
            route_aux_event_key(NX_KEYTYPE_PLAY, GrabMode::None, true, false),
            None
        );
    }

    #[test]
    fn a_claimed_bucket_does_not_steal_a_release_whose_press_macos_handled() {
        // Hold a media key while the VM is unfocused (macOS acted on the press), then Cmd-Tab
        // into the VM still holding it. The key-up arrives under a grab that claims the
        // bucket — but we never forwarded the press, so it must stay with macOS: consuming it
        // would leave the host player waiting for a release it never gets.
        assert_eq!(
            route_aux_event_key(NX_KEYTYPE_PLAY, GrabMode::Soft, false, false),
            None
        );
        assert_eq!(
            route_aux_event_key(NX_KEYTYPE_SOUND_UP, GrabMode::Full, false, false),
            None
        );
    }

    #[test]
    fn apple_transport_keys_are_track_skip_not_seek() {
        // `spikes/fn-key-probe` observed fn+F9/F7 on this MacBook emitting NX_KEYTYPE_FAST(19)
        // and REWIND(20) — NEXT(17)/PREVIOUS(18) never appear. Those physical keys are ⏭/⏮, so
        // they must reach the guest as track skip; KEY_FASTFORWARD/KEY_REWIND would be a dead
        // key in GNOME and read as "forwarding is broken".
        assert_eq!(route_aux_key(19, GrabMode::Full), Some(KEY_NEXTSONG));
        assert_eq!(route_aux_key(20, GrabMode::Full), Some(KEY_PREVIOUSSONG));
        // A keyboard that does emit the NEXT/PREVIOUS codes lands on the same keys.
        assert_eq!(route_aux_key(17, GrabMode::Full), Some(KEY_NEXTSONG));
        assert_eq!(route_aux_key(18, GrabMode::Full), Some(KEY_PREVIOUSSONG));
    }

    #[test]
    fn brightness_is_host_hardware_in_every_mode() {
        for mode in [GrabMode::None, GrabMode::Soft, GrabMode::Full] {
            assert_eq!(route_aux_key(NX_KEYTYPE_BRIGHTNESS_UP, mode), None);
            assert_eq!(route_aux_key(NX_KEYTYPE_BRIGHTNESS_DOWN, mode), None);
        }
    }

    #[test]
    fn unknown_and_other_bucket_keys_stay_with_the_host_even_fully_grabbed() {
        // NX_KEYTYPE_EJECT (14), NX_KEYTYPE_ILLUMINATION_UP (21), and a code that doesn't
        // exist: all `Other`, all host-owned for now.
        for nx in [14, 21, 250] {
            assert_eq!(nx_key_bucket(nx), AuxBucket::Other);
            assert_eq!(route_aux_key(nx, GrabMode::Full), None);
        }
    }

    #[test]
    fn data1_decodes_the_documented_wire_layout() {
        // Literal wire values, written from the layout in the header rather than built with
        // the decoder's own shifts — otherwise the test only proves the code agrees with
        // itself. 0x0010_0A00 = NX_KEYTYPE_PLAY (16) in bits 16..32, state 0x0A (down).
        assert_eq!(
            decode_aux_data1(0x0010_0A00),
            Some(AuxKeyEvent {
                nx_key: 16,
                down: true,
                repeat: false
            })
        );
        // 0x0003_0B00 = BRIGHTNESS_DOWN (3), state 0x0B (up) — as observed in the probe run.
        assert_eq!(
            decode_aux_data1(0x0003_0B00),
            Some(AuxKeyEvent {
                nx_key: 3,
                down: false,
                repeat: false
            })
        );
    }

    #[test]
    fn data1_decodes_key_direction_and_repeat() {
        // NX_KEYTYPE_PLAY (16) down: key in the high half, 0x0A state nibble.
        let d1 = (16 << 16) | 0x0A00;
        assert_eq!(
            decode_aux_data1(d1),
            Some(AuxKeyEvent {
                nx_key: 16,
                down: true,
                repeat: false
            })
        );
        // The same key up (0x0B).
        assert_eq!(
            decode_aux_data1((16 << 16) | 0x0B00),
            Some(AuxKeyEvent {
                nx_key: 16,
                down: false,
                repeat: false
            })
        );
        // Auto-repeat rides in bit 0 of a down.
        assert!(decode_aux_data1(d1 | 1).unwrap().repeat);
        // A state we don't recognize is refused rather than guessed into a press.
        assert_eq!(decode_aux_data1((16 << 16) | 0x0C00), None);
    }

    #[test]
    fn every_forwardable_aux_key_is_advertised_by_the_device() {
        // Same invariant as the virtual-keycode map: a code the capability bitmap doesn't
        // claim is silently dropped by the guest kernel.
        for nx in 0u16..=0xFF {
            if let Some(code) = nx_key_to_linux(nx) {
                assert!(
                    SUPPORTED_KEYBOARD_KEYS.contains(&code),
                    "NX key {nx} -> KEY {code} not in SUPPORTED_KEYBOARD_KEYS"
                );
            }
        }
    }
}
