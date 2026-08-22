// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The display-control socket wire format — the supervisor's half of the runtime display path.
//!
//! The worker (`limina-vmm`) binds a UNIX socket (`--display-control-socket`) and applies
//! newline-delimited commands to the live virtio-gpu. The supervisor's window code connects and
//! writes them when the window is resized or moves to another host display; the test harness
//! uses the same socket.
//!
//! There are two commands:
//!
//! ```text
//! resize <width> <height>
//! display id=0 size=1512x982 connected=1 refresh=120 dpi=226 vendor=LMN product=48879 \
//!         serial=19088743 name=Built-in%20Display serialstr=ABC123 \
//!         range=48-120/30-200/675 modes=1920x1080@60,1280x800@60 alt=1512x982@60
//! ```
//!
//! `resize` is the original, shorter form and stays supported forever — it is what the harness
//! and any external scripting use. `display` is the general one: every field after `id` is
//! optional, so a caller can push a size, an identity, a connection change, or all of them at
//! once (which is what moving the window to another display does).
//!
//! **Unknown keys are ignored, not rejected.** A newer supervisor talking to an older worker
//! degrades to whatever that worker understands instead of having its whole command dropped.
//!
//! This crate is deliberately dependency-free and holds no libkrun types: the supervisor has no
//! libkrun dependencies by design (it spawns the signed worker rather than calling `hv_vm_*`),
//! so the shared format has to be plain data. The worker converts it into libkrun's
//! `DisplayUpdate` on arrival. See `docs/design/stable-edid-hotplug.md`.

/// One command on the display-control socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayCommand {
    /// `resize <w> <h>` — the historical short form, equivalent to a `display` command that
    /// sets only `size` on display 0.
    Resize { width: u32, height: u32 },
    /// The general form.
    Display(DisplayControl),
}

/// A runtime change to one virtual display. Every field beyond the id is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayControl {
    pub display_id: u32,
    /// New preferred mode, in guest pixels.
    pub size: Option<(u32, u32)>,
    /// New position in the guest's desktop, in the coordinates its compositor lays monitors
    /// out in (logical, not pixels — see `docs/design/arrangement-relay.md`). The arrangement
    /// relay's suggested-offset source; only meaningful as part of a full-set emission.
    pub position: Option<(u32, u32)>,
    /// New connection state; `false` is a genuine unplug as far as the guest is concerned.
    pub connected: Option<bool>,
    /// New EDID — identity, physical size, mode list and refresh range.
    pub edid: Option<EdidSpec>,
}

/// The EDID the guest should see for this display. Mirrors what the host display actually is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdidSpec {
    /// Refresh rate of the preferred (current) mode.
    pub refresh_hz: u32,
    /// Pixels per inch, from which the advertised physical size is derived. Held constant
    /// across a resize on purpose, so the guest never flips its scale factor mid-drag.
    pub dpi: u32,
    /// Three-letter PNP-style vendor id.
    pub vendor: [u8; 3],
    pub product_id: u16,
    /// Stable, unique per physical host display, and the same across reboots.
    pub serial: u32,
    /// Human-readable display name, as the host reports it.
    pub name: String,
    /// Optional serial *string* descriptor (we put the host display's persistent UUID here).
    pub serial_string: Option<String>,
    /// The panel's real refresh range — the prerequisite for the guest ever seeing VRR.
    pub range: Option<RangeSpec>,
    /// Extra advertised modes, `(width, height, refresh)`. Never preferred.
    pub modes: Vec<(u16, u16, u16)>,
    /// An additional detailed timing, typically the current size at the panel's other
    /// refresh rate.
    pub alt_mode: Option<(u32, u32, u32)>,
}

impl Default for EdidSpec {
    fn default() -> Self {
        Self {
            refresh_hz: 60,
            dpi: 300,
            vendor: *b"LMN",
            product_id: 1,
            serial: 1,
            name: String::new(),
            serial_string: None,
            range: None,
            modes: Vec::new(),
            alt_mode: None,
        }
    }
}

/// Monitor range limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSpec {
    pub min_vertical_hz: u8,
    pub max_vertical_hz: u8,
    /// Horizontal rates are u16: a 4K panel at 120 Hz needs 265 kHz, past what the EDID
    /// range descriptor's byte holds on its own (the generator emits the +255 offset flags).
    pub min_horizontal_khz: u16,
    pub max_horizontal_khz: u16,
    pub max_pixel_clock_mhz: u32,
}

impl DisplayCommand {
    /// Render the command as the single line that goes over the socket (no trailing newline).
    pub fn to_wire(&self) -> String {
        match self {
            DisplayCommand::Resize { width, height } => format!("resize {width} {height}"),
            DisplayCommand::Display(control) => control.to_wire(),
        }
    }

    /// Parse one line. Returns `None` for anything unrecognized — the worker logs and ignores
    /// it rather than treating a stray line as fatal.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        match parts.next()? {
            "resize" => {
                let width = parts.next()?.parse().ok()?;
                let height = parts.next()?.parse().ok()?;
                Some(DisplayCommand::Resize { width, height })
            }
            "display" => DisplayControl::parse_fields(parts).map(DisplayCommand::Display),
            _ => None,
        }
    }
}

impl DisplayControl {
    pub fn to_wire(&self) -> String {
        let mut out = format!("display id={}", self.display_id);
        if let Some((width, height)) = self.size {
            out.push_str(&format!(" size={width}x{height}"));
        }
        if let Some((x, y)) = self.position {
            out.push_str(&format!(" pos={x},{y}"));
        }
        if let Some(connected) = self.connected {
            out.push_str(&format!(" connected={}", u8::from(connected)));
        }
        if let Some(edid) = &self.edid {
            out.push_str(&format!(" refresh={}", edid.refresh_hz));
            out.push_str(&format!(" dpi={}", edid.dpi));
            out.push_str(&format!(
                " vendor={}",
                std::str::from_utf8(&edid.vendor).unwrap_or("LMN")
            ));
            out.push_str(&format!(" product={}", edid.product_id));
            out.push_str(&format!(" serial={}", edid.serial));
            out.push_str(&format!(" name={}", percent_encode(&edid.name)));
            if let Some(serial_string) = &edid.serial_string {
                out.push_str(&format!(" serialstr={}", percent_encode(serial_string)));
            }
            if let Some(r) = &edid.range {
                out.push_str(&format!(
                    " range={}-{}/{}-{}/{}",
                    r.min_vertical_hz,
                    r.max_vertical_hz,
                    r.min_horizontal_khz,
                    r.max_horizontal_khz,
                    r.max_pixel_clock_mhz
                ));
            }
            if !edid.modes.is_empty() {
                let modes: Vec<String> = edid
                    .modes
                    .iter()
                    .map(|(w, h, hz)| format!("{w}x{h}@{hz}"))
                    .collect();
                out.push_str(&format!(" modes={}", modes.join(",")));
            }
            if let Some((w, h, hz)) = edid.alt_mode {
                out.push_str(&format!(" alt={w}x{h}@{hz}"));
            }
        }
        out
    }

    fn parse_fields<'a>(parts: impl Iterator<Item = &'a str>) -> Option<Self> {
        let mut control = DisplayControl::default();
        let mut edid = EdidSpec::default();
        let mut saw_edid_field = false;

        for field in parts {
            let (key, value) = field.split_once('=')?;
            match key {
                "id" => control.display_id = value.parse().ok()?,
                "size" => control.size = Some(parse_dimensions(value)?),
                "pos" => control.position = Some(parse_position(value)?),
                "connected" => control.connected = Some(parse_bool(value)?),
                "refresh" => {
                    edid.refresh_hz = value.parse().ok()?;
                    saw_edid_field = true;
                }
                "dpi" => {
                    edid.dpi = value.parse().ok()?;
                    saw_edid_field = true;
                }
                "vendor" => {
                    let bytes = value.as_bytes();
                    if bytes.len() != 3 {
                        return None;
                    }
                    edid.vendor = [bytes[0], bytes[1], bytes[2]];
                    saw_edid_field = true;
                }
                "product" => {
                    edid.product_id = value.parse().ok()?;
                    saw_edid_field = true;
                }
                "serial" => {
                    edid.serial = value.parse().ok()?;
                    saw_edid_field = true;
                }
                "name" => {
                    edid.name = percent_decode(value);
                    saw_edid_field = true;
                }
                "serialstr" => {
                    edid.serial_string = Some(percent_decode(value));
                    saw_edid_field = true;
                }
                "range" => {
                    edid.range = Some(parse_range(value)?);
                    saw_edid_field = true;
                }
                "modes" => {
                    edid.modes = value
                        .split(',')
                        .filter(|m| !m.is_empty())
                        .map(parse_mode)
                        .collect::<Option<Vec<_>>>()?;
                    saw_edid_field = true;
                }
                "alt" => {
                    let (w, h, hz) = parse_mode(value)?;
                    edid.alt_mode = Some((u32::from(w), u32::from(h), u32::from(hz)));
                    saw_edid_field = true;
                }
                // Forward compatibility: a key this build doesn't know is ignored, so a newer
                // supervisor's command still applies everything an older worker understands.
                _ => {}
            }
        }

        if saw_edid_field {
            control.edid = Some(edid);
        }
        Some(control)
    }
}

fn parse_position(value: &str) -> Option<(u32, u32)> {
    let (x, y) = value.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn parse_dimensions(value: &str) -> Option<(u32, u32)> {
    let (w, h) = value.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

fn parse_mode(value: &str) -> Option<(u16, u16, u16)> {
    let (size, refresh) = value.split_once('@')?;
    let (w, h) = size.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, refresh.parse().ok()?))
}

fn parse_range(value: &str) -> Option<RangeSpec> {
    let mut sections = value.split('/');
    let (min_v, max_v) = sections.next()?.split_once('-')?;
    let (min_h, max_h) = sections.next()?.split_once('-')?;
    let clock = sections.next()?;
    Some(RangeSpec {
        min_vertical_hz: min_v.parse().ok()?,
        max_vertical_hz: max_v.parse().ok()?,
        min_horizontal_khz: min_h.parse().ok()?,
        max_horizontal_khz: max_h.parse().ok()?,
        max_pixel_clock_mhz: clock.parse().ok()?,
    })
}

/// Percent-encode everything outside a conservative unreserved set. Display names contain
/// spaces (and, on a localized system, arbitrary UTF-8), and the format is whitespace-split.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Inverse of [`percent_encode`]. A malformed escape is passed through literally rather than
/// failing the whole command — a mangled display *name* must never cost us the modeset.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_control() -> DisplayControl {
        DisplayControl {
            display_id: 0,
            size: Some((1512, 982)),
            position: Some((2048, 0)),
            connected: Some(true),
            edid: Some(EdidSpec {
                refresh_hz: 120,
                dpi: 226,
                vendor: *b"LMN",
                product_id: 48879,
                serial: 19_088_743,
                name: "Built-in Display".into(),
                serial_string: Some("ABC-123".into()),
                range: Some(RangeSpec {
                    min_vertical_hz: 48,
                    max_vertical_hz: 120,
                    min_horizontal_khz: 30,
                    max_horizontal_khz: 200,
                    max_pixel_clock_mhz: 675,
                }),
                modes: vec![(1920, 1080, 60), (1280, 800, 60)],
                alt_mode: Some((1512, 982, 60)),
            }),
        }
    }

    #[test]
    fn a_full_command_round_trips() {
        let command = DisplayCommand::Display(full_control());
        let wire = command.to_wire();
        assert_eq!(
            DisplayCommand::parse(&wire),
            Some(command),
            "wire was {wire}"
        );
    }

    /// The historical short form must keep working — the test harness and external scripting
    /// speak it, and it is what every existing caller sends.
    #[test]
    fn the_resize_short_form_still_parses() {
        assert_eq!(
            DisplayCommand::parse("resize 900 650"),
            Some(DisplayCommand::Resize {
                width: 900,
                height: 650
            })
        );
        assert_eq!(
            DisplayCommand::Resize {
                width: 900,
                height: 650
            }
            .to_wire(),
            "resize 900 650"
        );
    }

    /// Each part is independently pushable: a bare connect/disconnect carries no EDID and no
    /// size, and must not invent either.
    #[test]
    fn a_connectivity_only_command_carries_nothing_else() {
        let wire = "display id=1 connected=0";
        let parsed = DisplayCommand::parse(wire).expect("should parse");
        let DisplayCommand::Display(control) = parsed else {
            panic!("expected a display command");
        };
        assert_eq!(control.display_id, 1);
        assert_eq!(control.connected, Some(false));
        assert_eq!(control.size, None);
        assert_eq!(control.edid, None);
    }

    /// The arrangement relay's field stands alone and parses back to only itself.
    #[test]
    fn a_position_only_command_carries_nothing_else() {
        let parsed = DisplayCommand::parse("display id=2 pos=2048,0").expect("should parse");
        let DisplayCommand::Display(control) = parsed else {
            panic!("expected a display command");
        };
        assert_eq!(control.display_id, 2);
        assert_eq!(control.position, Some((2048, 0)));
        assert_eq!(control.size, None);
        assert_eq!(control.connected, None);
        assert_eq!(control.edid, None);
    }

    #[test]
    fn a_size_only_command_carries_no_edid() {
        let parsed = DisplayCommand::parse("display id=0 size=1920x1080").expect("should parse");
        let DisplayCommand::Display(control) = parsed else {
            panic!("expected a display command");
        };
        assert_eq!(control.size, Some((1920, 1080)));
        assert_eq!(control.edid, None);
    }

    /// Display names have spaces, and the format is whitespace-split — an unencoded name would
    /// be read as a series of bogus fields.
    #[test]
    fn display_names_survive_spaces_and_unicode() {
        for name in [
            "Built-in Retina Display",
            "DELL U2720Q",
            "Écran principal",
            "モニター",
        ] {
            let control = DisplayControl {
                edid: Some(EdidSpec {
                    name: name.into(),
                    ..EdidSpec::default()
                }),
                ..DisplayControl::default()
            };
            let wire = control.to_wire();
            assert!(
                !wire.contains(' ') || wire.split_whitespace().count() >= 2,
                "name must not break the field split: {wire}"
            );
            let parsed = DisplayCommand::parse(&wire).expect("should parse");
            let DisplayCommand::Display(back) = parsed else {
                panic!("expected a display command");
            };
            assert_eq!(back.edid.expect("edid").name, name);
        }
    }

    /// A newer supervisor may send fields this build has never heard of; that must not cost us
    /// the rest of the command.
    #[test]
    fn unknown_keys_are_ignored_not_fatal() {
        let parsed =
            DisplayCommand::parse("display id=0 size=1280x800 hdr=pq colorspace=bt2020 vrr=1")
                .expect("unknown keys must not fail the parse");
        let DisplayCommand::Display(control) = parsed else {
            panic!("expected a display command");
        };
        assert_eq!(control.size, Some((1280, 800)));
    }

    #[test]
    fn malformed_lines_are_rejected() {
        for line in [
            "",
            "nonsense",
            "resize",
            "resize 900",
            "resize wide tall",
            "display id=0 size=1280",
            "display id=0 size=widexdeep",
            "display id=notanumber",
            "display id=0 vendor=TOOLONG",
            "display id=0 connected=maybe",
            "display id=0 range=48-120/30-200",
            "display bare",
        ] {
            assert_eq!(DisplayCommand::parse(line), None, "should reject {line:?}");
        }
    }

    #[test]
    fn percent_coding_round_trips_and_tolerates_garbage() {
        assert_eq!(percent_decode(&percent_encode("a b%c")), "a b%c");
        // A truncated escape is passed through rather than failing the command.
        assert_eq!(percent_decode("abc%"), "abc%");
        assert_eq!(percent_decode("%ZZfoo"), "%ZZfoo");
    }
}
