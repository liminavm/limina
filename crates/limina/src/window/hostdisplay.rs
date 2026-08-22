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

use super::fit;
use crate::vmlib::schema::DisplayResolution;
use limina_displayctl::{DisplayCommand, DisplayControl, EdidSpec, RangeSpec};
use objc2_app_kit::{NSScreen, NSWindow};
use objc2_foundation::{NSNumber, NSPoint, NSRect, NSString};

// CoreGraphics (already linked): the arrangement queries the pointer assertions are judged
// against. Points are CG global (top-left origin of the main display).
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> NSRect;
    fn CGGetActiveDisplayList(max_displays: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGGetDisplaysWithPoint(
        point: NSPoint,
        max_displays: u32,
        displays: *mut u32,
        matching: *mut u32,
    ) -> i32;
}

/// The most displays any arrangement query asks about.
const MAX_DISPLAYS: usize = 8;

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
    /// The predicted guest-*logical* size of the monitor: the usable frame in points (`size`
    /// before the backing scale). What the arrangement relay lays the desktop out in — see
    /// `super::arrangement` for why logical, and why prediction.
    pub logical: (u32, u32),
    /// The EDID to advertise for it.
    pub edid: EdidSpec,
}

impl HostDisplay {
    /// **Which physical panel this is** — vendor/model/serial, nothing else. Changes exactly when
    /// the window moves to a different display, which is the only event that warrants cycling the
    /// guest's connector.
    ///
    /// Deliberately *excludes* the refresh rate. Refresh is what a panel is currently doing, not
    /// which panel it is: a ProMotion display dropping 120 → 60 is the same monitor, and treating
    /// it as a swap would black the guest out for the reconnect settle on a display that never
    /// left. Size is excluded for the same reason — two displays of the same size are still two
    /// displays, and one display resized is still one display.
    ///
    /// Also persisted, as `window.fullscreen_display` in `vm.toml`, to remember which panel a VM
    /// was fullscreen on (`vmlib::state`). A key written by a build whose layout included the
    /// refresh simply matches no attached screen and falls through to the frame/main-display
    /// path — the same graceful route as a display that has been unplugged — then re-saves in
    /// this layout on the next fullscreen. It cannot *mis*-match: those keys carry a non-zero
    /// refresh in the low bits where this layout has zeros.
    pub fn identity_key(&self) -> u64 {
        (u64::from(self.edid.serial) << 32) | (u64::from(self.edid.product_id) << 16)
    }

    /// **What that panel is currently doing** — the properties that belong to the mode rather
    /// than the monitor. A change here is an *adjustment*: the guest still needs it (the refresh
    /// and the VRR range ride in the EDID, and variable refresh is unreachable without them),
    /// but it travels in place, with the connector left up.
    pub fn mode_key(&self) -> u64 {
        u64::from(self.edid.refresh_hz)
    }
}

/// Which event the guest gets when the window migrates to another host display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotplugPolicy {
    /// Take the connector down and bring it back up carrying the new EDID. The default,
    /// because it is the only event **both** tiers act on.
    Cycle,
    /// Rewrite the EDID with the connector left connected. Cheaper — the guest never passes
    /// through zero monitors — and enough for a compositor that re-reads an in-place change.
    InPlace,
}

/// `LIMINA_DISPLAY_HOTPLUG`: `cycle` (default) or `inplace`.
const HOTPLUG_ENV: &str = "LIMINA_DISPLAY_HOTPLUG";

impl HotplugPolicy {
    /// The policy for this process, read once.
    pub fn from_env() -> Self {
        use std::sync::OnceLock;
        static POLICY: OnceLock<HotplugPolicy> = OnceLock::new();
        *POLICY.get_or_init(|| Self::parse(std::env::var(HOTPLUG_ENV).ok().as_deref()))
    }

    /// Unset or unrecognized means the default. A typo must not silently select the other
    /// behavior, so it warns and cycles rather than guessing.
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") => Self::Cycle,
            Some(v) if v.eq_ignore_ascii_case("cycle") => Self::Cycle,
            Some(v) if v.eq_ignore_ascii_case("inplace") || v.eq_ignore_ascii_case("in-place") => {
                Self::InPlace
            }
            Some(other) => {
                log::warn!("{HOTPLUG_ENV}={other:?} is not `cycle` or `inplace`; using `cycle`");
                Self::Cycle
            }
        }
    }
}

/// The commands to push, **in order**, when the window has migrated to this display.
///
/// `drives_size` is whether the *display mode* lets the host screen decide the guest's
/// resolution — true only for match-host. The identity half is pushed in **every** mode:
/// which physical panel the window is on is not a sizing policy, and a dynamic or fixed VM
/// that never learns it keeps a flat 300 DPI on every display it is dragged to, so an ordinary
/// external monitor reads as Retina and the guest picks the wrong scale.
///
/// A `Cycle` is two commands and the order is load-bearing, so the caller must send them
/// sequentially over the same socket rather than firing them off independently — see
/// [`super::send_display_commands`].
///
/// **Why the cycle is the default.** An in-place EDID swap is enough for mutter, which re-reads
/// the connector and re-applies the arriving display's remembered configuration. It is *not*
/// enough for a compositor that refreshes a monitor's identity only on a reconnect: it keeps
/// reporting the previous display's identity, so the layout the user then sets is saved under the
/// wrong monitor and per-display memory is lost. Both tiers act on the cycle, and it costs
/// milliseconds — the guest is at zero monitors only between two socket writes, which is why the
/// original "that is a real session state" objection to it does not hold. Measured both ways in
/// `spikes/display-identity-hotplug/`.
/// The command for a change to the display the guest is **already on** — a new refresh rate, or a
/// new size in the modes that drive one. In place, connector untouched.
///
/// This is the counterpart to [`migration_commands`], and the split is the whole point: a swap
/// gets the connector cycle, an adjustment must not. Cycling here would black the guest out for
/// the settle every time a panel changed its own refresh — a ProMotion display switching rates,
/// or a host mode change — on a monitor the user never touched.
///
/// It still has to be *sent*: the EDID is where the refresh and the VRR range descriptor live, so
/// a guest that never receives this can never drive variable refresh.
pub fn adjustment_command(
    host: &HostDisplay,
    drives_size: bool,
    display_id: u32,
) -> DisplayCommand {
    DisplayCommand::Display(DisplayControl {
        display_id,
        size: drives_size.then_some(host.size),
        position: None,
        connected: None,
        edid: Some(host.edid.clone()),
    })
}

/// Plug a connector in, carrying this display's whole identity.
///
/// The arriving half of a connector cycle, and the whole of a slot handover: when a panel's own
/// slot comes up it comes up already knowing what it is, so the guest's compositor recognizes the
/// monitor and applies that monitor's remembered configuration on the first probe. The EDID rides
/// the *connect* and never the disconnect — a connector that comes back still advertising the old
/// identity is exactly the staleness the cycle exists to avoid.
/// Whether a push to this slot should carry a size — i.e. whether limina picks the guest's
/// resolution for that connector, or leaves the guest to keep whatever it has.
///
/// For the **window's own panel** this is the display mode: `host` drives the guest to the
/// panel; `dynamic` and a fixed size are the *window's* to decide, and the resize path pushes
/// them from the window's content size instead.
///
/// For **every other panel** it is unconditionally true. Those connectors are shown by a
/// borderless window that fills its panel and that the user cannot resize, so nothing else will
/// ever set their size — and a connect that carries none leaves the guest advertising a stale
/// preferred mode. Measured on the two-panel rig 2026-08-18, dynamic mode: the built-in's
/// connector kept the 1800×1176 the *window* had been before fullscreen, so its EDID described a
/// 1800 px panel at the built-in's real 254 DPI and GNOME named the monitor `LMN 8"`. mutter
/// still picked the native 3024×1960 from the mode list, so the picture was right — but a guest
/// with no saved configuration takes the preferred mode, and that one was wrong.
pub fn drives_size(mode: DisplayResolution, is_window_panel: bool) -> bool {
    !is_window_panel || mode == DisplayResolution::Host
}

/// `position` is the arrangement relay's guest-desktop suggestion for this connector
/// (`super::arrangement`): riding the connect keeps the hotplug the guest sees atomic — the
/// probe that discovers the monitor already finds its suggested offset in place, instead of
/// racing a second config-change against mutter building the new layout.
pub fn connect_command(
    host: &HostDisplay,
    drives_size: bool,
    display_id: u32,
    position: Option<(u32, u32)>,
) -> DisplayCommand {
    DisplayCommand::Display(DisplayControl {
        display_id,
        size: drives_size.then_some(host.size),
        position,
        connected: Some(true),
        edid: Some(host.edid.clone()),
    })
}

pub fn migration_commands(
    host: &HostDisplay,
    drives_size: bool,
    policy: HotplugPolicy,
    display_id: u32,
) -> Vec<DisplayCommand> {
    let arrive = DisplayControl {
        display_id,
        size: drives_size.then_some(host.size),
        position: None,
        connected: matches!(policy, HotplugPolicy::Cycle).then_some(true),
        edid: Some(host.edid.clone()),
    };
    match policy {
        HotplugPolicy::InPlace => vec![DisplayCommand::Display(arrive)],
        // The EDID rides with the replug, not with the unplug: a connector that comes back
        // still advertising the old EDID is the stale case we are trying to avoid.
        HotplugPolicy::Cycle => vec![
            DisplayCommand::Display(DisplayControl {
                display_id,
                connected: Some(false),
                ..DisplayControl::default()
            }),
            DisplayCommand::Display(arrive),
        ],
    }
}

/// The camera-housing ("notch") height of a screen in points, or 0 on a screen without one.
///
/// **Cached per display, because the live read is not always available.** With the bundle's
/// `NSPrefersDisplaySafeAreaCompatibilityMode` key — the key that gets a fullscreen window the
/// whole panel — AppKit reports the housing only while the window is *not* fullscreen. Measured
/// on a built-in Retina display (`spikes/notch-fullscreen/`): windowed gives
/// `safeAreaInsets.top = 32` and a 32 pt-tall `auxiliaryTopLeftArea`; fullscreen zeroes the
/// insets and empties the auxiliary rect. Fullscreen is exactly when the policy needs the
/// number, so learn it whenever it *is* readable and remember it against the display id.
///
/// A screen we have never seen unfullscreened reports 0 — i.e. the guest gets the whole panel.
/// That is the safe way to be wrong: it wastes no rows, and the window is on a notched built-in
/// display for at least one non-fullscreen tick in every path that can reach fullscreen.
pub fn notch_inset(screen: &NSScreen) -> f64 {
    use std::cell::RefCell;
    thread_local! {
        /// (panel key, housing height). A handful of screens at most, so a Vec beats a map.
        static SEEN: RefCell<Vec<(u64, f64)>> = const { RefCell::new(Vec::new()) };
    }

    // Keyed on the panel, NOT the display id — see [`panel_key`]. A hotplug renumbers the ids.
    let id = panel_key(screen);
    // `safeAreaInsets` is the direct answer; `auxiliaryTopLeftArea` (the menu-bar region beside
    // the housing) is the corroborating read, and non-empty only on a notched panel.
    let live = screen.safeAreaInsets().top.max({
        let aux = screen.auxiliaryTopLeftArea();
        if aux.size.width > 0.0 {
            aux.size.height
        } else {
            0.0
        }
    });

    SEEN.with(|seen| {
        let mut seen = seen.borrow_mut();
        let known = seen.iter_mut().find(|(seen_id, _)| *seen_id == id);
        if live > 0.0 && live.is_finite() {
            match known {
                Some(entry) => entry.1 = live,
                None => seen.push((id, live)),
            }
            return live;
        }
        known.map_or(0.0, |(_, height)| *height)
    })
}

/// Remember how much shorter than the panel AppKit actually made a NATIVE fullscreen window on
/// this display, and hand that number back later.
///
/// [`notch_inset`] is *nearly* the answer for the `avoid` policy but not exactly: the housing is
/// 43 pt on one measured Mac and 32 pt on another, while the fullscreen window comes up 44 and 33 pt shorter
/// than the panel respectively. One point, consistently, on both machines — plausibly a
/// separator AppKit draws, but a constant fitted to two data points is a guess. Observing the
/// real figure costs a subtraction on a tick we already run, and being exact matters here: the
/// guest is driven to this size, so a point of error means every frame is rescaled by 0.08%
/// instead of landing 1:1.
///
/// Until a display has been seen fullscreen once, [`fullscreen_inset`] falls back to the housing
/// height — off by that one point for exactly one modeset.
pub fn learn_fullscreen_inset(screen: &NSScreen, observed: f64) {
    // Bounds: a mid-transition tick can report nonsense, and a *negative* or huge value would
    // poison the cache for the session.
    if !observed.is_finite() || observed <= 0.0 || observed > 200.0 {
        return;
    }
    FULLSCREEN_INSETS.with(|seen| {
        let id = panel_key(screen);
        let mut seen = seen.borrow_mut();
        match seen.iter_mut().find(|(seen_id, _)| *seen_id == id) {
            Some(entry) => entry.1 = observed,
            None => seen.push((id, observed)),
        }
    });
}

/// The height a native fullscreen window loses to the camera housing on this display: the
/// learned figure if we have one, else the housing height itself. Zero on a notchless display.
pub fn fullscreen_inset(screen: &NSScreen) -> f64 {
    let id = panel_key(screen);
    let learned = FULLSCREEN_INSETS.with(|seen| {
        seen.borrow()
            .iter()
            .find(|(seen_id, _)| *seen_id == id)
            .map(|(_, inset)| *inset)
    });
    match learned {
        Some(inset) => inset,
        // No housing means no inset to learn — don't invent one.
        None => notch_inset(screen),
    }
}

thread_local! {
    /// (panel key, observed native-fullscreen inset). See [`learn_fullscreen_inset`] and
    /// [`panel_key`] — keyed on the panel, because a hotplug renumbers the display ids.
    static FULLSCREEN_INSETS: std::cell::RefCell<Vec<(u64, f64)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Describe the screen a window currently sits on. `None` when AppKit has no screen for it
/// (which happens mid-transition — the caller simply skips that tick).
///
/// `scale` decides whether the reported size is the screen's points or its device pixels; see
/// [`fit::Scale`]. The advertised *density* is the panel's real device density either way — it
/// describes the glass, not the framebuffer — which is what lets the guest pick a 2x scale once
/// the framebuffer is big enough to carry one.
///
/// `notch_inset` is the housing height to withhold from the guest (0 unless the VM's policy is
/// `avoid` and this screen has a housing). It is subtracted here — where the guest's *resolution*
/// is decided — rather than only at fit time, so that entering fullscreen never modesets: the
/// guest is already exactly as tall as the area fullscreen will give it.
/// [`HostDisplay::identity_key`] for a screen, without needing the caller to know the scale or
/// notch policy — neither feeds the identity fields (vendor/model/serial and refresh), only the
/// size ones. Used to remember *which panel* a VM was fullscreen on across restarts, where a
/// `CGDirectDisplayID` would be worthless: those are reassigned on reboot and hotplug.
pub fn identity_key_of(screen: &NSScreen) -> u64 {
    describe(screen, fit::Scale::new(1.0, false), 0.0).identity_key()
}

/// Describe an attached panel by its [`panel_key`], for the displays the *window* is not on.
///
/// Those have no window to read a backing scale from, so they take their own, and they are
/// described in their own device pixels — a display the guest owns outright gets all of it.
///
/// The notch policy still reaches them, because a secondary panel can be the notched built-in.
/// Its window is borderless and above menu-bar level, which is the one thing macOS lets draw
/// beside the camera housing, so `extend` needs no carrier here and the guest is simply given
/// the whole panel. Under `avoid` the housing band is withheld from the *mode*, so the guest is
/// already the height its window will be and entering fullscreen never modesets — the same
/// reason [`describe`] takes the inset at all. `None` if the panel is no longer attached.
pub fn describe_panel(
    panel: u64,
    notch: crate::vmlib::schema::NotchPolicy,
    mtm: objc2::MainThreadMarker,
) -> Option<HostDisplay> {
    NSScreen::screens(mtm)
        .iter()
        .find(|s| panel_key(s) == panel)
        .map(|s| {
            let scale = fit::Scale::new(s.backingScaleFactor(), true);
            // The same figure the primary's own fullscreen uses, deliberately: a panel's guest
            // mode must not change when it switches between being the window's and being a
            // secondary, or the guest would save a second configuration for it and re-arrange
            // the desktop every time the window moved.
            let inset = match notch {
                crate::vmlib::schema::NotchPolicy::Avoid => fullscreen_inset(&s),
                crate::vmlib::schema::NotchPolicy::Extend => 0.0,
            };
            describe(&s, scale, inset)
        })
}

pub fn describe(screen: &NSScreen, scale: fit::Scale, notch_inset: f64) -> HostDisplay {
    let frame = screen.frame().size;
    let (usable_w, usable_h) = fit::usable_content(frame.width, frame.height, notch_inset);
    let points = (usable_w.round() as u32, usable_h.round() as u32);
    // Snapped *here*, where the resolution is decided, so the size we push and the EDID we
    // generate from it can never disagree — the guest kernel prunes a preferred mode that does
    // not match the pushed rect. See [`fit::snap_to_scalable`] for why a panel's exact height is
    // often one its own desktop cannot scale.
    let size = fit::snap_to_scalable(scale.to_guest((usable_w, usable_h)));
    let backing = screen.backingScaleFactor();
    let display_id = display_id_of(screen);
    let (millimeters_wide, _) = screen_size_millimeters(display_id);
    let refresh_hz = refresh_of(screen);
    let name = screen.localizedName().to_string();
    let (vendor_number, model_number, serial_number) = display_numbers(display_id);

    let (product_id, serial) = identity_from(vendor_number, model_number, serial_number, &name);

    HostDisplay {
        logical: points,
        edid: EdidSpec {
            refresh_hz,
            dpi: dpi_from(points.0, millimeters_wide, backing),
            vendor: LIMINA_VENDOR,
            product_id,
            serial,
            name,
            serial_string: None,
            range: range_from(screen, size, refresh_hz),
            // The advertised extra modes are left to the generator's built-in list for now:
            // the standard-timing encoding can't express most real Mac point sizes (widths
            // must be a multiple of 8 below 2288, in four aspect ratios), so a "real" mode
            // list needs a DisplayID extension block tracked in the design doc.
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
/// This is the **panel's** density — points per inch scaled by the backing factor, i.e. device
/// pixels per inch — and it does not depend on the display mode: it describes the glass, not the
/// framebuffer. Under HiDPI the guest's framebuffer is that many pixels across, so the number is
/// literally its own density and GNOME offers the 2× scale. Without HiDPI the framebuffer is half
/// that, and the same number keeps the guest reasoning about the size things appear at rather
/// than the count of pixels it happens to be drawing into.
///
/// Either way it replaced a flat 300 DPI that made every ordinary external monitor look Retina;
/// a 27" 1440p panel now correctly reports ~109.
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
        min_horizontal_khz: horizontal_khz(min_hz).clamp(1, u32::from(u16::MAX)) as u16,
        max_horizontal_khz: horizontal_khz(max_advertised_hz).min(u32::from(u16::MAX)) as u16,
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
pub(crate) fn display_id_of(screen: &NSScreen) -> u32 {
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

/// Every display containing this CG global point. Mirrored displays answer together, so a
/// membership test is always "contains", never "equals". Empty means the point is nowhere the
/// cursor can actually be — `CGWarpMouseCursorPosition` does not fail there, it silently clamps
/// into the display union, which is how a "just past this edge" target lands on a neighbour.
pub(crate) fn displays_at(p: NSPoint) -> Vec<u32> {
    let mut ids = [0u32; MAX_DISPLAYS];
    let mut matching: u32 = 0;
    // SAFETY: plain CoreGraphics query writing at most MAX_DISPLAYS ids into our buffer.
    unsafe {
        CGGetDisplaysWithPoint(p, MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut matching);
    }
    ids[..(matching as usize).min(MAX_DISPLAYS)].to_vec()
}

/// The active displays with their CG global bounds — the arrangement, for assertion dumps.
pub(crate) fn active_displays() -> Vec<(u32, NSRect)> {
    let mut ids = [0u32; MAX_DISPLAYS];
    let mut count: u32 = 0;
    // SAFETY: plain CoreGraphics queries, bounded by our buffer.
    unsafe {
        CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count);
        ids[..(count as usize).min(MAX_DISPLAYS)]
            .iter()
            .map(|&id| (id, CGDisplayBounds(id)))
            .collect()
    }
}

/// The displays a window's frame intersects — the set a point "inside this window" may
/// legitimately land on. A window straddling a seam covers two; `NSWindow::screen` names only
/// the majority one, which is why the assertions take the set and not the screen.
pub(crate) fn displays_under_window(window: &NSWindow) -> Vec<u32> {
    let f = window.frame();
    // NS screen coordinates share the main display's origin with CG but grow upward.
    // SAFETY: plain CoreGraphics query.
    let main_h = unsafe { CGDisplayBounds(CGMainDisplayID()) }.size.height;
    let (fx0, fx1) = (f.origin.x, f.origin.x + f.size.width);
    let (fy0, fy1) = (main_h - (f.origin.y + f.size.height), main_h - f.origin.y);
    active_displays()
        .into_iter()
        .filter(|(_, b)| {
            let (bx0, bx1) = (b.origin.x, b.origin.x + b.size.width);
            let (by0, by1) = (b.origin.y, b.origin.y + b.size.height);
            fx0 < bx1 && bx0 < fx1 && fy0 < by1 && by0 < fy1
        })
        .map(|(id, _)| id)
        .collect()
}

/// The arrangement as one line, for assertion messages: `id@x,y WxH` per active display, the
/// main display first.
pub(crate) fn describe_arrangement() -> String {
    // SAFETY: plain CoreGraphics query.
    let main = unsafe { CGMainDisplayID() };
    active_displays()
        .iter()
        .map(|(id, b)| {
            format!(
                "{id}{}@{:.0},{:.0} {:.0}x{:.0}",
                if *id == main { "(main)" } else { "" },
                b.origin.x,
                b.origin.y,
                b.size.width,
                b.size.height
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// A cache key for a physical panel that **survives a hotplug**.
///
/// `CGDirectDisplayID` does not: macOS reassigns ids when the arrangement changes, so a cache
/// keyed on one hands back a *stranger's* value after an unplug/replug. Measured 2026-08-08 on the
/// notch caches below: after replugging an external monitor the built-in panel's learned housing
/// inset was no longer found under its new id, so the guest was sized as if there were no housing
/// and rendered entirely below it — while the external display, having inherited an id that *did*
/// carry a learned inset, got a 33 pt housing strip placed on a panel that has no housing (the
/// strip's frame was traced at x=0, on the external display). The user saw two GNOME panels, and
/// it healed on anything that forced a re-measure — leaving fullscreen, or the chrome ask.
///
/// Vendor/model/serial come from the panel's own EDID, so they are stable across hotplugs and
/// reboots alike. A display reporting none of them falls back to the transient id, tagged so it
/// can never collide with a real panel's key.
pub(crate) fn panel_key(screen: &NSScreen) -> u64 {
    let id = display_id_of(screen);
    let (vendor, model, serial) = display_numbers(id);
    panel_key_from(vendor, model, serial, id)
}

fn panel_key_from(vendor: u32, model: u32, serial: u32, display_id: u32) -> u64 {
    const UNIDENTIFIED: u64 = 1 << 63;
    if vendor == 0 && model == 0 && serial == 0 {
        return UNIDENTIFIED | u64::from(display_id);
    }
    let mut bytes = [0u8; 12];
    bytes[0..4].copy_from_slice(&vendor.to_le_bytes());
    bytes[4..8].copy_from_slice(&model.to_le_bytes());
    bytes[8..12].copy_from_slice(&serial.to_le_bytes());
    fnv1a(&bytes) & !UNIDENTIFIED
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

    /// The window's own panel follows the display mode: match-host drives the guest, dynamic and
    /// fixed leave the resolution to the window.
    #[test]
    fn the_window_s_panel_is_driven_only_in_match_host_mode() {
        assert!(drives_size(DisplayResolution::Host, true));
        assert!(!drives_size(DisplayResolution::Dynamic, true));
        assert!(!drives_size(DisplayResolution::Fixed(1600, 1000), true));
    }

    /// Any other panel is driven whatever the mode, because its window fills that panel and
    /// nothing else ever sets its size. Without this the connector kept a stale preferred mode —
    /// the size the *window* happened to have — and described the wrong physical panel with it.
    #[test]
    fn a_panel_the_window_is_not_on_is_always_driven_to_its_own_size() {
        for mode in [
            DisplayResolution::Host,
            DisplayResolution::Dynamic,
            DisplayResolution::Fixed(1600, 1000),
        ] {
            assert!(
                drives_size(mode, false),
                "{mode:?} left a secondary sizeless"
            );
        }
    }

    fn host() -> HostDisplay {
        HostDisplay {
            logical: (1512, 982),
            size: (2560, 1440),
            edid: EdidSpec {
                refresh_hz: 60,
                dpi: 109,
                vendor: LIMINA_VENDOR,
                product_id: 7,
                serial: 0xDEAD_BEEF,
                name: "BenQ LCD".into(),
                ..EdidSpec::default()
            },
        }
    }

    fn control(command: &DisplayCommand) -> &DisplayControl {
        match command {
            DisplayCommand::Display(control) => control,
            other => panic!("expected a display command, got {other:?}"),
        }
    }

    /// The migration cycle: the guest has to see the connector go away and come back, or a
    /// compositor that only refreshes a monitor's identity on a reconnect keeps serving the
    /// previous display's identity — and then saves the new display's layout under it.
    /// Measured in `spikes/display-identity-hotplug/`.
    #[test]
    fn a_cycle_takes_the_connector_down_then_back_up_with_the_new_edid() {
        let commands = migration_commands(&host(), true, HotplugPolicy::Cycle, 2);
        assert_eq!(commands.len(), 2, "a cycle is exactly two commands");
        assert!(
            commands.iter().all(|c| control(c).display_id == 2),
            "both halves address the same connector — a cycle that unplugged one slot and \
             plugged another would be two events, not one migration"
        );

        let down = control(&commands[0]);
        assert_eq!(down.connected, Some(false), "the first command unplugs");
        assert!(
            down.edid.is_none() && down.size.is_none(),
            "the unplug carries nothing else: the guest must not learn the new identity \
             until the connector comes back"
        );

        let up = control(&commands[1]);
        assert_eq!(up.connected, Some(true), "the second command replugs");
        assert_eq!(
            up.edid.as_ref().map(|e| e.serial),
            Some(0xDEAD_BEEF),
            "the new EDID rides WITH the replug, atomically — a connector that comes back \
             still carrying the old EDID is exactly the stale case"
        );
        assert_eq!(up.size, Some((2560, 1440)));
    }

    /// `dynamic` and `fixed` may not dictate the guest's resolution, so their reconnect carries
    /// the identity alone. That is enough: both compositors re-read a sizeless reconnect,
    /// verified end-to-end by dragging a dynamic-mode VM between two physical displays.
    #[test]
    fn a_cycle_that_does_not_drive_the_size_still_replugs_with_the_identity() {
        let commands = migration_commands(&host(), false, HotplugPolicy::Cycle, 0);
        assert_eq!(commands.len(), 2);
        let up = control(&commands[1]);
        assert_eq!(up.connected, Some(true));
        assert!(
            up.size.is_none(),
            "dynamic and fixed keep their own resolution"
        );
        assert!(up.edid.is_some(), "but they still hand over the identity");
    }

    /// The escape hatch: one command, no connectivity change — what limina shipped before, and
    /// still the cheaper event for a guest that re-reads an in-place change.
    #[test]
    fn in_place_stays_a_single_command_that_never_touches_connectivity() {
        let commands = migration_commands(&host(), true, HotplugPolicy::InPlace, 0);
        assert_eq!(commands.len(), 1);
        let only = control(&commands[0]);
        assert_eq!(only.connected, None);
        assert_eq!(only.size, Some((2560, 1440)));
        assert!(only.edid.is_some());
    }

    #[test]
    fn the_hotplug_policy_defaults_to_cycle_and_reads_its_opt_out() {
        assert_eq!(HotplugPolicy::parse(None), HotplugPolicy::Cycle);
        assert_eq!(HotplugPolicy::parse(Some("cycle")), HotplugPolicy::Cycle);
        assert_eq!(
            HotplugPolicy::parse(Some("inplace")),
            HotplugPolicy::InPlace
        );
        assert_eq!(
            HotplugPolicy::parse(Some("in-place")),
            HotplugPolicy::InPlace
        );
        assert_eq!(
            HotplugPolicy::parse(Some("InPlace")),
            HotplugPolicy::InPlace
        );
        // Garbage must not silently pick the non-default behavior.
        assert_eq!(HotplugPolicy::parse(Some("nonsense")), HotplugPolicy::Cycle);
        assert_eq!(HotplugPolicy::parse(Some("")), HotplugPolicy::Cycle);
    }

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
            logical: (1920, 1080),
            edid: EdidSpec {
                serial,
                ..EdidSpec::default()
            },
        };
        assert_ne!(make(1).identity_key(), make(2).identity_key());
        assert_eq!(make(7).identity_key(), make(7).identity_key());
    }

    /// The notch caches are keyed on this, and **a hotplug renumbers `CGDirectDisplayID`**. When
    /// they were keyed on the id, replugging an external monitor left the built-in unable to find
    /// its own learned housing inset — so the guest was sized as if the panel had no housing and
    /// rendered wholly below it — while the external display inherited an id that *did* carry a
    /// learned inset and got a 33 pt housing strip placed on it. Two GNOME panels, healing on
    /// anything that forced a re-measure. So: same panel, new id, same key.
    #[test]
    fn the_panel_key_survives_a_hotplug_renumbering_the_display_ids() {
        // One physical panel, seen under two different display ids across an unplug/replug.
        assert_eq!(
            panel_key_from(0x610, 0xa038, 0x31d1_4c41, 1),
            panel_key_from(0x610, 0xa038, 0x31d1_4c41, 724_045_140),
        );
        // Two panels are still two panels, whatever ids they happen to hold.
        assert_ne!(
            panel_key_from(0x610, 0xa038, 0x31d1_4c41, 3),
            panel_key_from(0x9d1, 0x7f5c, 0x0000_0001, 3),
        );
        // A display reporting nothing identifying falls back to its id, and those stay distinct.
        assert_ne!(panel_key_from(0, 0, 0, 1), panel_key_from(0, 0, 0, 2));
        assert_eq!(panel_key_from(0, 0, 0, 9), panel_key_from(0, 0, 0, 9));
        // ...and an unidentified display can never be confused with a real panel.
        assert_ne!(
            panel_key_from(0, 0, 0, 1),
            panel_key_from(0x610, 0xa038, 0x31d1_4c41, 1),
        );
    }

    /// Every mode hands over the identity; only match-host also drives the resolution.
    #[test]
    fn identity_travels_in_every_mode_but_size_only_in_host_mode() {
        let host = HostDisplay {
            size: (1512, 982),
            logical: (1512, 982),
            edid: EdidSpec {
                serial: 42,
                name: "Built-in".into(),
                ..EdidSpec::default()
            },
        };

        // Under either policy the arriving display is described by the LAST command.
        for policy in [HotplugPolicy::Cycle, HotplugPolicy::InPlace] {
            let commands = migration_commands(&host, true, policy, 0);
            let host_mode = control(commands.last().expect("a command"));
            assert_eq!(host_mode.size, Some((1512, 982)), "{policy:?}");
            assert_eq!(
                host_mode.edid.as_ref().expect("edid").serial,
                42,
                "{policy:?}"
            );

            // Dynamic/fixed: the guest's resolution is theirs to decide, but it still must
            // learn which display it is on.
            let commands = migration_commands(&host, false, policy, 0);
            let other_mode = control(commands.last().expect("a command"));
            assert_eq!(
                other_mode.size, None,
                "{policy:?} must not override the mode's own size"
            );
            assert_eq!(
                other_mode.edid.as_ref().expect("edid").serial,
                42,
                "{policy:?}"
            );
        }
    }

    /// ...and to a refresh-rate change on the same display (a mode switch on the host).
    #[test]
    fn a_refresh_change_is_an_adjustment_not_a_different_panel() {
        let make = |refresh_hz: u32| HostDisplay {
            size: (1512, 982),
            logical: (1512, 982),
            edid: EdidSpec {
                serial: 9,
                refresh_hz,
                ..EdidSpec::default()
            },
        };
        // Refresh is what the panel is *doing*, not which panel it is. A ProMotion display
        // dropping 120 → 60 must not read as "the user swapped monitors" and cycle the
        // connector — the guest would black out for the settle on a display that never left.
        assert_eq!(
            make(60).identity_key(),
            make(120).identity_key(),
            "the same panel keeps one identity across a refresh change"
        );
        // ...but the change still has to reach the guest: the EDID carries the refresh and the
        // VRR range descriptor, which is what a guest needs to ever drive variable refresh.
        assert_ne!(
            make(60).mode_key(),
            make(120).mode_key(),
            "a refresh change is a mode change and must still be pushed"
        );
    }

    /// The two keys answer different questions, and a swap is both at once.
    #[test]
    fn a_different_panel_changes_the_identity() {
        let make = |serial: u32| HostDisplay {
            size: (1512, 982),
            logical: (1512, 982),
            edid: EdidSpec {
                serial,
                refresh_hz: 60,
                ..EdidSpec::default()
            },
        };
        assert_ne!(make(9).identity_key(), make(10).identity_key());
    }

    /// An adjustment is the cheap event: same panel, so the connector never goes down.
    #[test]
    fn an_adjustment_carries_the_edid_without_touching_connectivity() {
        let command = adjustment_command(&host(), true, 0);
        let adjusted = control(&command);
        assert_eq!(
            adjusted.connected, None,
            "an adjustment must never unplug the connector"
        );
        assert_eq!(adjusted.size, Some((2560, 1440)));
        assert!(
            adjusted.edid.is_some(),
            "the new refresh/range rides in the EDID"
        );

        // The modes that don't drive the guest's resolution still push the EDID alone.
        let sizeless = adjustment_command(&host(), false, 0);
        assert!(control(&sizeless).size.is_none());
        assert!(control(&sizeless).edid.is_some());
    }
}
