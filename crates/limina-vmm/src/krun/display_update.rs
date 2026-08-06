// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Turn a display-control command off the socket into the libkrun display update that applies
//! it to the live virtio-gpu.
//!
//! This is the seam between the supervisor's plain-data wire format (`limina-displayctl`, which
//! carries no libkrun types because the supervisor has no libkrun dependencies) and libkrun's
//! `DisplayUpdate` / `EdidParams`. Everything lossy about that translation is here — string
//! names into fixed 13-byte EDID descriptors, an unbounded mode list into the eight standard
//! timing slots — so it can be tested without booting a VM.
//!
//! See `docs/design/stable-edid-hotplug.md`.

use devices::display::{
    DetailedMode, EdidIdentity, EdidParams, PhysicalSize, RefreshRange, StandardTiming,
    StandardTimings,
};
use devices::virtio::DisplayUpdate;
use limina_displayctl::{DisplayControl, EdidSpec};

/// Number of standard-timing slots an EDID has.
const STANDARD_TIMING_SLOTS: usize = 8;

pub fn display_update_from(control: DisplayControl) -> DisplayUpdate {
    DisplayUpdate {
        display_id: control.display_id,
        size: control.size,
        edid: control.edid.map(edid_params_from),
        connected: control.connected,
    }
}

fn edid_params_from(spec: EdidSpec) -> EdidParams {
    EdidParams {
        refresh_rate: spec.refresh_hz,
        // DPI rather than fixed millimetres: our window is a viewport, not a panel, so the
        // advertised physical size tracks the resolution and the DPI the guest computes stays
        // put across a resize (which keeps its scale factor from flipping mid-drag).
        physical_size: PhysicalSize::Dpi(spec.dpi.max(1)),
        identity: Some(EdidIdentity {
            manufacturer: spec.vendor,
            product_id: spec.product_id,
            serial: spec.serial,
            product_name: descriptor_text(&spec.name),
            serial_string: spec.serial_string.as_deref().map(descriptor_text),
        }),
        range: spec.range.map(|r| RefreshRange {
            min_vertical_hz: r.min_vertical_hz,
            max_vertical_hz: r.max_vertical_hz,
            min_horizontal_khz: r.min_horizontal_khz,
            max_horizontal_khz: r.max_horizontal_khz,
            max_pixel_clock_mhz: r.max_pixel_clock_mhz,
        }),
        standard_timings: standard_timings_from(&spec.modes),
        alt_mode: spec
            .alt_mode
            .map(|(width, height, refresh_hz)| DetailedMode {
                width,
                height,
                refresh_hz,
            }),
    }
}

/// Pack a name into an EDID string descriptor: 13 bytes, `0x0A`-terminated when it fits, space
/// padded. Non-ASCII is dropped rather than truncated mid-codepoint — the field is defined as
/// ASCII, and a lone continuation byte would render as garbage in the guest's monitor list.
///
/// Over-long names are cut at a **word boundary** where one exists in the second half of the
/// field: macOS's "Built-in Retina Display" hard-truncated to `Built-in Reti`, which is how it
/// then appeared in the guest's monitor list. `Built-in` is both shorter and legible.
fn descriptor_text(text: &str) -> [u8; 13] {
    let mut out = [0x20u8; 13];
    let ascii: String = text
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .collect();
    let ascii = ascii.trim();

    let kept: &str = if ascii.len() <= 13 {
        ascii
    } else {
        // Prefer the last word boundary at or before the field width, as long as it doesn't
        // throw away more than half the field; otherwise a hard cut is the lesser evil.
        match ascii[..13].rfind(' ') {
            Some(boundary) if boundary >= 7 => ascii[..boundary].trim_end(),
            _ => &ascii[..13],
        }
    };

    let bytes = kept.as_bytes();
    let len = bytes.len();
    out[..len].copy_from_slice(bytes);
    // The terminator is only required when there's room for it; a full 13-byte name has none,
    // which the spec allows.
    if len < 13 {
        out[len] = 0x0A;
    }
    out
}

/// Fill the eight standard-timing slots from the supplied mode list. Returns `None` for an
/// empty list so the generator keeps its built-in modes rather than advertising nothing.
/// Modes past the eighth are dropped — the format has no more room.
fn standard_timings_from(modes: &[(u16, u16, u16)]) -> Option<StandardTimings> {
    if modes.is_empty() {
        return None;
    }
    let mut slots: StandardTimings = [None; STANDARD_TIMING_SLOTS];
    for (slot, (width, height, refresh_hz)) in slots.iter_mut().zip(modes.iter().copied()) {
        *slot = Some(StandardTiming {
            width,
            height,
            refresh_hz,
        });
    }
    if modes.len() > STANDARD_TIMING_SLOTS {
        log::warn!(
            "display-control: {} modes supplied, only the first {STANDARD_TIMING_SLOTS} fit \
             the EDID standard-timing list",
            modes.len()
        );
    }
    Some(slots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use limina_displayctl::RangeSpec;

    fn spec() -> EdidSpec {
        EdidSpec {
            refresh_hz: 120,
            dpi: 226,
            vendor: *b"LMN",
            product_id: 7,
            serial: 0xDEAD_BEEF,
            name: "Built-in Display".into(),
            serial_string: Some("UUID-1".into()),
            range: Some(RangeSpec {
                min_vertical_hz: 48,
                max_vertical_hz: 120,
                min_horizontal_khz: 30,
                max_horizontal_khz: 200,
                max_pixel_clock_mhz: 675,
            }),
            modes: vec![(1920, 1080, 60)],
            alt_mode: Some((1512, 982, 60)),
        }
    }

    #[test]
    fn a_full_control_maps_every_field() {
        let update = display_update_from(DisplayControl {
            display_id: 1,
            size: Some((1512, 982)),
            connected: Some(true),
            edid: Some(spec()),
        });

        assert_eq!(update.display_id, 1);
        assert_eq!(update.size, Some((1512, 982)));
        assert_eq!(update.connected, Some(true));
        let params = update.edid.expect("edid");
        assert_eq!(params.refresh_rate, 120);
        assert_eq!(params.physical_size, PhysicalSize::Dpi(226));
        let identity = params.identity.expect("identity");
        assert_eq!(identity.manufacturer, *b"LMN");
        assert_eq!(identity.serial, 0xDEAD_BEEF);
        assert_eq!(params.range.expect("range").max_vertical_hz, 120);
        assert_eq!(
            params.alt_mode,
            Some(DetailedMode {
                width: 1512,
                height: 982,
                refresh_hz: 60
            })
        );
    }

    /// A size-only command must not fabricate an EDID: the display keeps the identity it has.
    #[test]
    fn a_size_only_control_leaves_the_edid_alone() {
        let update = display_update_from(DisplayControl {
            display_id: 0,
            size: Some((1920, 1080)),
            ..Default::default()
        });
        assert!(update.edid.is_none());
        assert!(update.connected.is_none());
    }

    #[test]
    fn names_are_padded_terminated_and_truncated() {
        assert_eq!(&descriptor_text("Built-in")[..8], b"Built-in");
        assert_eq!(descriptor_text("Built-in")[8], 0x0A);
        assert_eq!(descriptor_text("Built-in")[9..], [0x20; 4]);

        // Exactly 13 characters: no room for a terminator, and none is required.
        let full = descriptor_text("ABCDEFGHIJKLM");
        assert_eq!(&full[..], b"ABCDEFGHIJKLM");

        // A single over-long word has no boundary to cut at, so it truncates hard.
        assert_eq!(
            &descriptor_text("ABCDEFGHIJKLMNOPQRS")[..],
            b"ABCDEFGHIJKLM"
        );
    }

    /// macOS's real display names are longer than the field, and a hard cut produced
    /// "Built-in Reti" in the guest's monitor list. Cut at a word boundary instead.
    #[test]
    fn over_long_names_are_cut_at_a_word_boundary() {
        let packed = descriptor_text("Built-in Retina Display");
        assert_eq!(&packed[..8], b"Built-in");
        assert_eq!(packed[8], 0x0A, "the shortened name is terminated");

        // Names that already fit are untouched.
        assert_eq!(&descriptor_text("BenQ LCD")[..8], b"BenQ LCD");

        // A boundary too early in the field would throw away most of the name, so the hard
        // cut wins there.
        let packed = descriptor_text("DELL UltraSharp U2720Q");
        assert_eq!(&packed[..13], b"DELL UltraSha");
    }

    /// A localized display name must not put a half-codepoint in the guest's monitor list.
    #[test]
    fn non_ascii_names_are_stripped_not_split() {
        let packed = descriptor_text("Écran Principal");
        assert!(packed.is_ascii());
        assert_eq!(&packed[..5], b"cran ");
    }

    #[test]
    fn an_empty_mode_list_keeps_the_builtin_timings() {
        assert_eq!(standard_timings_from(&[]), None);
    }

    #[test]
    fn modes_fill_the_slots_in_order_and_excess_is_dropped() {
        let modes: Vec<(u16, u16, u16)> = (0..10).map(|i| (800 + i * 8, 600, 60)).collect();
        let slots = standard_timings_from(&modes).expect("slots");
        assert_eq!(slots[0].expect("first").width, 800);
        assert_eq!(slots[7].expect("eighth").width, 800 + 7 * 8);
        assert_eq!(slots.len(), STANDARD_TIMING_SLOTS);
    }
}
