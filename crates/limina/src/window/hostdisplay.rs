// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Describe the host display a window is on, as the EDID the guest should see.
//!
//! In match-host mode the guest is driven to the point size of the screen the window sits on.
//! This module supplies the rest of that story: the display's *identity* (so a guest compositor
//! can recognize it and key its remembered per-monitor configuration on it), its real refresh
//! rate and VRR range, and a physical size that makes the guest choose the right scale factor.
//!
//! The identity follows the **host display**, not the virtual one: dragging the window to
//! another monitor hands the guest that monitor's identity, which is what makes GNOME apply
//! that monitor's remembered configuration — the Parallels behavior. See
//! `docs/design/stable-edid-hotplug.md`.
//!
//! Everything here that is arithmetic lives in free functions so it can be tested without a
//! screen; the AppKit/CoreGraphics reads are the thin part.

use limina_displayctl::{EdidSpec, RangeSpec};
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSNumber, NSString};

/// Vendor id limina stamps on the displays it synthesizes.
const LIMINA_VENDOR: [u8; 3] = *b"LMN";

/// Fallback DPI when the host won't tell us a physical size (projectors and some capture
/// devices report 0×0 mm). This is the value limina advertised unconditionally before it
/// derived one, so falling back to it cannot change behavior on displays that used to work.
const FALLBACK_DPI: u32 = 300;

/// The generator's fixed blanking, needed to convert a refresh rate into the horizontal rate
/// and pixel clock the range descriptor is expressed in. Mirrors `edid.rs`'s defaults; if those
/// ever become dynamic, this has to follow.
const BLANKING_HORIZONTAL: u32 = 560;
const BLANKING_VERTICAL: u32 = 50;

/// What the supervisor needs to know about the screen the window is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDisplay {
    /// The guest resolution this display implies (screen frame, in points).
    pub size: (u32, u32),
    /// The EDID to advertise for it.
    pub edid: EdidSpec,
}

impl HostDisplay {
    /// A cheap value that changes exactly when the *identity* does. Used to decide whether the
    /// window has actually migrated, rather than merely been resized: two displays of the same
    /// size are a migration, and today's size-only check misses that.
    pub fn identity_key(&self) -> u64 {
        (u64::from(self.edid.serial) << 32)
            | (u64::from(self.edid.product_id) << 16)
            | u64::from(self.edid.refresh_hz.min(u32::from(u16::MAX)) as u16)
    }
}

/// Describe the screen a window currently sits on. `None` when AppKit has no screen for it
/// (which happens mid-transition — the caller simply skips that tick).
pub fn describe(screen: &NSScreen) -> HostDisplay {
    let frame = screen.frame().size;
    let size = (frame.width.round() as u32, frame.height.round() as u32);
    let backing = screen.backingScaleFactor();
    let display_id = display_id_of(screen);
    let (millimeters_wide, _) = screen_size_millimeters(display_id);
    let refresh_hz = refresh_of(screen);
    let name = screen.localizedName().to_string();
    let (vendor_number, model_number, serial_number) = display_numbers(display_id);

    let (product_id, serial) = identity_from(vendor_number, model_number, serial_number, &name);

    HostDisplay {
        edid: EdidSpec {
            refresh_hz,
            dpi: dpi_from(size.0, millimeters_wide, backing),
            vendor: LIMINA_VENDOR,
            product_id,
            serial,
            name,
            serial_string: None,
            range: range_from(screen, size, refresh_hz),
            // The advertised extra modes are left to the generator's built-in list for now:
            // the standard-timing encoding can't express most real Mac point sizes (widths
            // must be a multiple of 8 below 2288, in four aspect ratios), so a "real" mode
            // list needs the CTA extension block tracked in the design doc.
            modes: Vec::new(),
            alt_mode: alt_mode_for(size, refresh_hz),
        },
        size,
    }
}

/// Derive the EDID product code and serial number from what CoreGraphics knows about the
/// panel. Vendor/model/serial numbers come from the display's own EDID, so they are stable
/// across reboots and re-plugs — which is the whole point.
///
/// Some displays report zeros for all three (notably virtual and capture displays). Those fall
/// back to hashing the localized name, which is stable enough to be useful and never zero.
fn identity_from(vendor: u32, model: u32, serial: u32, name: &str) -> (u16, u32) {
    if vendor == 0 && model == 0 && serial == 0 {
        let hashed = fnv1a(name.as_bytes());
        return ((hashed >> 32) as u16, (hashed as u32).max(1));
    }
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&vendor.to_le_bytes());
    bytes.extend_from_slice(&model.to_le_bytes());
    bytes.extend_from_slice(&serial.to_le_bytes());
    let hashed = fnv1a(&bytes);
    // The model number is meaningful on its own, so keep it as the product code and let the
    // hash (which folds in the serial) disambiguate two of the same model.
    let product_id = if model == 0 {
        (hashed >> 32) as u16
    } else {
        model as u16
    };
    (product_id, (hashed as u32).max(1))
}

/// Pixels-per-inch to advertise.
///
/// The guest's framebuffer is sized in *points*, and each point covers `backing` device pixels,
/// so the density the guest should reason about is the panel's point density scaled by the
/// backing factor. That keeps a Retina panel at the 2× guest scale it already gets, while an
/// ordinary external monitor now correctly reports ~110 DPI instead of the flat 300 that made
/// every display look Retina to the guest.
fn dpi_from(points_wide: u32, millimeters_wide: f64, backing: f64) -> u32 {
    if millimeters_wide <= 0.0 || points_wide == 0 || backing <= 0.0 {
        return FALLBACK_DPI;
    }
    let inches = millimeters_wide / 25.4;
    let dpi = (f64::from(points_wide) / inches) * backing;
    if dpi.is_finite() && dpi >= 1.0 {
        dpi.round() as u32
    } else {
        FALLBACK_DPI
    }
}

/// The monitor range limits, when the panel genuinely has a *range* — i.e. it supports variable
/// refresh. A fixed-rate display gets `None`: emitting a range for it would also declare the
/// EDID continuous-frequency, which invites the guest to infer modes the panel never had.
fn range_from(screen: &NSScreen, size: (u32, u32), refresh_hz: u32) -> Option<RangeSpec> {
    let min_interval = screen.minimumRefreshInterval();
    let max_interval = screen.maximumRefreshInterval();
    range_from_intervals(min_interval, max_interval, size, refresh_hz)
}

/// The arithmetic half of [`range_from`], split out to be testable without a screen. Intervals
/// are seconds per frame: the *minimum* interval is the *maximum* refresh rate.
fn range_from_intervals(
    min_interval: f64,
    max_interval: f64,
    size: (u32, u32),
    refresh_hz: u32,
) -> Option<RangeSpec> {
    let height = size.1;
    if !(min_interval.is_finite() && max_interval.is_finite()) {
        return None;
    }
    if min_interval <= 0.0 || max_interval <= 0.0 || max_interval <= min_interval {
        return None;
    }
    let max_hz = (1.0 / min_interval).round() as u32;
    let min_hz = (1.0 / max_interval).round() as u32;
    if max_hz <= min_hz || max_hz > 255 || min_hz == 0 {
        return None;
    }
    // The descriptor states horizontal rate and pixel clock, not just vertical rate, and the
    // guest checks candidate modes against them — so they have to bound the modes we actually
    // advertise, computed with the same blanking the generator uses.
    let vertical_total = u64::from(height + BLANKING_VERTICAL);
    let horizontal_khz = |hz: u32| -> u32 {
        let hz = u64::from(hz);
        ((hz * vertical_total).div_ceil(1000)) as u32
    };
    // The range must cover every mode we advertise, and the preferred one is the display's
    // current refresh — which can exceed what the VRR window reports.
    let max_advertised_hz = max_hz.max(refresh_hz);
    Some(RangeSpec {
        min_vertical_hz: min_hz.min(255) as u8,
        max_vertical_hz: max_advertised_hz.min(255) as u8,
        min_horizontal_khz: horizontal_khz(min_hz).clamp(1, 255) as u8,
        max_horizontal_khz: horizontal_khz(max_advertised_hz).min(255) as u8,
        max_pixel_clock_mhz: max_pixel_clock_mhz(size, max_advertised_hz),
    })
}

/// Maximum pixel clock in MHz for the fastest mode we advertise, using the same blanking the
/// generator does. This only needs to be an upper bound — understating it would have the guest
/// prune a mode we also advertise — so it rounds up.
fn max_pixel_clock_mhz(size: (u32, u32), max_hz: u32) -> u32 {
    let (width, height) = size;
    let horizontal_total = u64::from(width + BLANKING_HORIZONTAL);
    let vertical_total = u64::from(height + BLANKING_VERTICAL);
    let clock = u64::from(max_hz) * horizontal_total * vertical_total;
    (clock.div_ceil(1_000_000)) as u32
}

/// On a ProMotion panel, also advertise the current size at 60 Hz, so the guest has a fixed-rate
/// mode to fall back to. `None` when the display only has the one rate.
fn alt_mode_for(size: (u32, u32), refresh_hz: u32) -> Option<(u32, u32, u32)> {
    (refresh_hz > 60).then_some((size.0, size.1, 60))
}

fn refresh_of(screen: &NSScreen) -> u32 {
    let reported = screen.maximumFramesPerSecond();
    if reported > 0 {
        reported as u32
    } else {
        60
    }
}

/// The `CGDirectDisplayID` behind an `NSScreen`, from its device description. Zero when the key
/// is missing, which the CoreGraphics queries below all tolerate by returning zeros.
fn display_id_of(screen: &NSScreen) -> u32 {
    let description = screen.deviceDescription();
    let key = NSString::from_str("NSScreenNumber");
    let Some(value) = description.objectForKey(&key) else {
        return 0;
    };
    value
        .downcast_ref::<NSNumber>()
        .map(|n| n.unsignedIntValue())
        .unwrap_or(0)
}

fn screen_size_millimeters(display_id: u32) -> (f64, f64) {
    // SAFETY: `CGDisplayScreenSize` takes a display id by value and returns a POD struct; an
    // unknown id yields (0, 0), which the caller treats as "no physical size".
    let size = unsafe { CGDisplayScreenSize(display_id) };
    (size.width, size.height)
}

fn display_numbers(display_id: u32) -> (u32, u32, u32) {
    // SAFETY: all three take a display id by value and return a plain u32 (0 for an unknown
    // display or one that doesn't report the field).
    unsafe {
        (
            CGDisplayVendorNumber(display_id),
            CGDisplayModelNumber(display_id),
            CGDisplaySerialNumber(display_id),
        )
    }
}

#[repr(C)]
struct CGSize {
    width: f64,
    height: f64,
}

extern "C" {
    /// Physical size of the display in millimetres; (0, 0) when unknown.
    fn CGDisplayScreenSize(display: u32) -> CGSize;
    /// The panel's own EDID vendor / model / serial, hence stable across reboots.
    fn CGDisplayVendorNumber(display: u32) -> u32;
    fn CGDisplayModelNumber(display: u32) -> u32;
    fn CGDisplaySerialNumber(display: u32) -> u32;
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Retina panel must keep reporting a Retina-class density, or the guest would drop to
    /// 1× and everything in it would halve in apparent size.
    #[test]
    fn a_retina_panel_reports_a_retina_density() {
        // 14" MacBook Pro: 1512 points wide, ~301 mm, 2× backing.
        let dpi = dpi_from(1512, 301.0, 2.0);
        assert!(
            (240..=270).contains(&dpi),
            "expected a ~2x-class density, got {dpi}"
        );
    }

    /// The flat 300 DPI limina used to advertise made every external monitor look Retina to the
    /// guest. An ordinary 27" 1440p panel must now report its real ~109 DPI.
    #[test]
    fn an_ordinary_external_monitor_reports_its_real_density() {
        // 27" 2560x1440 at 1× : 2560 points over ~597 mm.
        let dpi = dpi_from(2560, 597.0, 1.0);
        assert!((100..=120).contains(&dpi), "expected ~109 dpi, got {dpi}");
    }

    /// A display that reports no physical size must fall back to the value limina advertised
    /// before it derived one — never to a nonsense density.
    #[test]
    fn a_display_without_a_physical_size_falls_back() {
        assert_eq!(dpi_from(1920, 0.0, 2.0), FALLBACK_DPI);
        assert_eq!(dpi_from(1920, -1.0, 2.0), FALLBACK_DPI);
        assert_eq!(dpi_from(0, 300.0, 2.0), FALLBACK_DPI);
        assert_eq!(dpi_from(1920, 300.0, 0.0), FALLBACK_DPI);
        assert_eq!(dpi_from(1920, f64::NAN, 2.0), FALLBACK_DPI);
    }

    /// The same physical monitor must produce the same identity every time — that is what makes
    /// the guest's remembered per-monitor configuration stick across reboots.
    #[test]
    fn identity_is_stable_for_the_same_panel() {
        let first = identity_from(0x0610, 0xa050, 0x1234_5678, "Built-in Display");
        let second = identity_from(0x0610, 0xa050, 0x1234_5678, "Built-in Display");
        assert_eq!(first, second);
        assert_ne!(first.1, 0, "the serial must never be zero");
    }

    /// Two monitors of the same model must not collide, or the guest would apply one's
    /// configuration to the other.
    #[test]
    fn two_panels_of_the_same_model_get_different_identities() {
        let a = identity_from(0x10ac, 0x4090, 0x1111, "DELL U2720Q");
        let b = identity_from(0x10ac, 0x4090, 0x2222, "DELL U2720Q");
        assert_eq!(a.0, b.0, "same model ⇒ same product code");
        assert_ne!(a.1, b.1, "different serial ⇒ different EDID serial");
    }

    /// Displays that report nothing about themselves still need a stable, non-zero identity.
    #[test]
    fn a_display_reporting_no_numbers_falls_back_to_its_name() {
        let (product, serial) = identity_from(0, 0, 0, "Sidecar Display");
        assert_ne!(serial, 0);
        assert_eq!(identity_from(0, 0, 0, "Sidecar Display"), (product, serial));
        assert_ne!(identity_from(0, 0, 0, "Other Display").1, serial);
    }

    /// ProMotion: a 48-120 Hz panel must produce a range the guest can act on.
    #[test]
    fn a_promotion_panel_yields_a_vrr_range() {
        let range =
            range_from_intervals(1.0 / 120.0, 1.0 / 48.0, (1512, 982), 120).expect("a range");
        assert_eq!(range.min_vertical_hz, 48);
        assert_eq!(range.max_vertical_hz, 120);
        // 120 Hz over 982+50 lines ≈ 124 kHz.
        assert!(
            (120..=130).contains(&range.max_horizontal_khz),
            "got {} kHz",
            range.max_horizontal_khz
        );
        assert!(range.min_horizontal_khz < range.max_horizontal_khz);
        assert!(range.max_pixel_clock_mhz > 0);
    }

    /// A fixed-rate display gets no range at all: a range descriptor also declares the EDID
    /// continuous-frequency, which would invite the guest to infer modes the panel never had.
    #[test]
    fn a_fixed_rate_display_yields_no_range() {
        assert_eq!(
            range_from_intervals(1.0 / 60.0, 1.0 / 60.0, (1920, 1080), 60),
            None
        );
        assert_eq!(range_from_intervals(0.0, 0.0, (1920, 1080), 60), None);
        assert_eq!(range_from_intervals(f64::NAN, 1.0, (1920, 1080), 60), None);
        // Inverted intervals (max faster than min) are nonsense, not a range.
        assert_eq!(
            range_from_intervals(1.0 / 48.0, 1.0 / 120.0, (1920, 1080), 60),
            None
        );
    }

    /// The advertised range has to *contain* the preferred mode, or the guest prunes it.
    #[test]
    fn the_range_always_covers_the_advertised_refresh_rate() {
        let range =
            range_from_intervals(1.0 / 90.0, 1.0 / 48.0, (1512, 982), 120).expect("a range");
        assert!(
            range.max_vertical_hz >= 120,
            "range tops out at {} but we advertise 120 Hz",
            range.max_vertical_hz
        );
    }

    #[test]
    fn only_a_high_refresh_panel_gets_a_60hz_alternate() {
        assert_eq!(alt_mode_for((1512, 982), 120), Some((1512, 982, 60)));
        assert_eq!(alt_mode_for((1920, 1080), 60), None);
    }

    /// The identity key must react to a migration between two same-sized displays — the case
    /// the old size-only check silently missed.
    #[test]
    fn the_identity_key_distinguishes_same_sized_displays() {
        let make = |serial: u32| HostDisplay {
            size: (1920, 1080),
            edid: EdidSpec {
                serial,
                ..EdidSpec::default()
            },
        };
        assert_ne!(make(1).identity_key(), make(2).identity_key());
        assert_eq!(make(7).identity_key(), make(7).identity_key());
    }

    /// ...and to a refresh-rate change on the same display (a mode switch on the host).
    #[test]
    fn the_identity_key_reacts_to_a_refresh_change() {
        let make = |refresh_hz: u32| HostDisplay {
            size: (1512, 982),
            edid: EdidSpec {
                serial: 9,
                refresh_hz,
                ..EdidSpec::default()
            },
        };
        assert_ne!(make(60).identity_key(), make(120).identity_key());
    }
}
