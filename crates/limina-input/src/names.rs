// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Evdev codes by name, and US-layout ASCII to key presses — the vocabulary of host-side
//! input injection (`limina input`).
//!
//! The names are the kernel's own (`<linux/input-event-codes.h>`), so a harness written
//! against evtest output or a compositor's keybinding table reads the same here. Only the keys
//! the virtual keyboard advertises ([`SUPPORTED_KEYBOARD_KEYS`]) have names: anything else
//! would be dropped by the guest's capability bitmap, and a name that silently does nothing is
//! worse than an error.

use crate::constants::*;

macro_rules! named {
    ($($k:ident),* $(,)?) => {
        &[$((stringify!($k), $k)),*]
    };
}

/// Every key the virtual keyboard advertises, by its kernel name.
pub const KEY_NAMES: &[(&str, u16)] = named![
    KEY_ESC,
    KEY_1,
    KEY_2,
    KEY_3,
    KEY_4,
    KEY_5,
    KEY_6,
    KEY_7,
    KEY_8,
    KEY_9,
    KEY_0,
    KEY_MINUS,
    KEY_EQUAL,
    KEY_BACKSPACE,
    KEY_TAB,
    KEY_Q,
    KEY_W,
    KEY_E,
    KEY_R,
    KEY_T,
    KEY_Y,
    KEY_U,
    KEY_I,
    KEY_O,
    KEY_P,
    KEY_LEFTBRACE,
    KEY_RIGHTBRACE,
    KEY_ENTER,
    KEY_LEFTCTRL,
    KEY_A,
    KEY_S,
    KEY_D,
    KEY_F,
    KEY_G,
    KEY_H,
    KEY_J,
    KEY_K,
    KEY_L,
    KEY_SEMICOLON,
    KEY_APOSTROPHE,
    KEY_GRAVE,
    KEY_LEFTSHIFT,
    KEY_BACKSLASH,
    KEY_Z,
    KEY_X,
    KEY_C,
    KEY_V,
    KEY_B,
    KEY_N,
    KEY_M,
    KEY_COMMA,
    KEY_DOT,
    KEY_SLASH,
    KEY_RIGHTSHIFT,
    KEY_KPASTERISK,
    KEY_LEFTALT,
    KEY_SPACE,
    KEY_CAPSLOCK,
    KEY_F1,
    KEY_F2,
    KEY_F3,
    KEY_F4,
    KEY_F5,
    KEY_F6,
    KEY_F7,
    KEY_F8,
    KEY_F9,
    KEY_F10,
    KEY_NUMLOCK,
    KEY_SCROLLLOCK,
    KEY_KP7,
    KEY_KP8,
    KEY_KP9,
    KEY_KPMINUS,
    KEY_KP4,
    KEY_KP5,
    KEY_KP6,
    KEY_KPPLUS,
    KEY_KP1,
    KEY_KP2,
    KEY_KP3,
    KEY_KP0,
    KEY_KPDOT,
    KEY_F11,
    KEY_F12,
    KEY_KPENTER,
    KEY_RIGHTCTRL,
    KEY_KPSLASH,
    KEY_RIGHTALT,
    KEY_HOME,
    KEY_UP,
    KEY_PAGEUP,
    KEY_LEFT,
    KEY_RIGHT,
    KEY_END,
    KEY_DOWN,
    KEY_PAGEDOWN,
    KEY_INSERT,
    KEY_DELETE,
    KEY_MUTE,
    KEY_VOLUMEDOWN,
    KEY_VOLUMEUP,
    KEY_KPEQUAL,
    KEY_LEFTMETA,
    KEY_RIGHTMETA,
    KEY_F13,
    KEY_F14,
    KEY_F15,
    KEY_F16,
    KEY_F17,
    KEY_F18,
    KEY_F19,
    KEY_NEXTSONG,
    KEY_PLAYPAUSE,
    KEY_PREVIOUSSONG,
];

/// The pointer buttons, by kernel name.
pub const BUTTON_NAMES: &[(&str, u16)] = named![BTN_LEFT, BTN_RIGHT, BTN_MIDDLE];

/// Event types, by kernel name (for the raw escape hatch).
pub const TYPE_NAMES: &[(&str, u16)] = named![EV_SYN, EV_KEY, EV_REL, EV_ABS];

/// A non-negative number in decimal or `0x` hex.
pub fn parse_number(s: &str) -> Option<u32> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn lookup(table: &[(&str, u16)], prefix: &str, s: &str) -> Option<u16> {
    let upper = s.to_ascii_uppercase();
    let full = if upper.starts_with(prefix) {
        upper
    } else {
        format!("{prefix}{upper}")
    };
    table.iter().find(|(n, _)| *n == full).map(|&(_, c)| c)
}

/// A keyboard key: its kernel name (`KEY_LEFTCTRL`, case-insensitive, the `KEY_` prefix
/// optional — `leftctrl`, `f2`, `a`) or its code (`29`, `0x1d`). Either way it must be one the
/// virtual keyboard advertises, or the guest would drop it.
pub fn key(s: &str) -> Result<u16, String> {
    let code = match parse_number(s) {
        Some(n) => u16::try_from(n).map_err(|_| format!("{s} is not a key code"))?,
        None => lookup(KEY_NAMES, "KEY_", s).ok_or_else(|| {
            let guess = match nearest_key(s) {
                Some(name) => format!("; did you mean {name}?"),
                None => String::new(),
            };
            format!("unknown key {s:?}{guess} (`limina input <vm> keys` lists every key name)")
        })?,
    };
    if SUPPORTED_KEYBOARD_KEYS.contains(&code) {
        Ok(code)
    } else {
        Err(format!(
            "key code {code} is not advertised by the virtual keyboard, so the guest would \
             drop it (`ev kbd` sends it anyway)"
        ))
    }
}

/// The key name closest to an unknown `s`, for the error to suggest — never accepted in its
/// place. A name that the input starts with, or that starts with the input, wins (`escape` →
/// `KEY_ESC`, `pageup2` → `KEY_PAGEUP`), the longest such; otherwise the nearest by edit
/// distance, if it is close enough to be a typo (`backspce` → `KEY_BACKSPACE`).
pub fn nearest_key(s: &str) -> Option<&'static str> {
    let upper = s.to_ascii_uppercase();
    let want = upper.strip_prefix("KEY_").unwrap_or(&upper);
    if want.is_empty() {
        return None;
    }
    let bare = |n: &'static str| n.strip_prefix("KEY_").unwrap_or(n);
    // One name inside the other: a shared prefix first (`escape` → `ESC`), then anywhere
    // (`ctrl` → `LEFTCTRL`), the longest overlap, then the shorter name. Overlaps shorter than
    // three letters say nothing (`E` starts `ESC`, `END`, `ENTER`, …).
    let inside = KEY_NAMES
        .iter()
        .map(|&(n, _)| n)
        .filter_map(|n| {
            let b = bare(n);
            let overlap = b.len().min(want.len());
            let prefix = want.starts_with(b) || b.starts_with(want);
            (overlap >= 3 && (prefix || want.contains(b) || b.contains(want))).then_some((
                prefix,
                overlap,
                std::cmp::Reverse(b.len()),
                n,
            ))
        })
        .max_by_key(|&(prefix, overlap, shorter, _)| (prefix, overlap, shorter))
        .map(|(.., n)| n);
    if inside.is_some() {
        return inside;
    }
    let (name, d) = KEY_NAMES
        .iter()
        .map(|&(n, _)| (n, edit_distance(want, bare(n))))
        .min_by_key(|&(_, d)| d)?;
    (d <= (want.len() / 3).max(1)).then_some(name)
}

/// Levenshtein distance over bytes (key names are ASCII).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let next = (prev + usize::from(ca != cb))
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// A pointer button: `BTN_LEFT`/`left`/`1`-style names (`left`, `right`, `middle`, with or
/// without `BTN_`) or a code.
pub fn button(s: &str) -> Result<u16, String> {
    if let Some(n) = parse_number(s) {
        return u16::try_from(n)
            .ok()
            .filter(|c| BUTTON_NAMES.iter().any(|(_, b)| b == c))
            .ok_or_else(|| format!("{s} is not a pointer button the device advertises"));
    }
    lookup(BUTTON_NAMES, "BTN_", s).ok_or_else(|| format!("unknown button {s:?}"))
}

/// An event type for the raw escape hatch: `EV_KEY`/`key` or a number.
pub fn event_type(s: &str) -> Result<u16, String> {
    match parse_number(s) {
        Some(n) => u16::try_from(n).map_err(|_| format!("{s} is not an event type")),
        None => lookup(TYPE_NAMES, "EV_", s).ok_or_else(|| format!("unknown event type {s:?}")),
    }
}

/// An event code for the raw escape hatch: a number, or any key/button name above, or one of
/// the axis names the devices use (`REL_X`, `ABS_Y`, `REL_WHEEL_HI_RES`, `SYN_REPORT`, …).
/// Axis names are shared between types (`REL_X` and `ABS_X` are both 0), so the name is taken
/// at its word and not checked against the type.
pub fn event_code(s: &str) -> Result<u16, String> {
    if let Some(n) = parse_number(s) {
        return u16::try_from(n).map_err(|_| format!("{s} is not an event code"));
    }
    const AXES: &[(&str, u16)] = named![
        SYN_REPORT,
        REL_X,
        REL_Y,
        REL_HWHEEL,
        REL_WHEEL,
        REL_WHEEL_HI_RES,
        REL_HWHEEL_HI_RES,
        ABS_X,
        ABS_Y,
        ABS_MT_SLOT,
        ABS_MT_POSITION_X,
        ABS_MT_POSITION_Y,
        ABS_MT_TRACKING_ID,
        BTN_TOOL_FINGER,
        BTN_TOUCH,
        BTN_TOOL_DOUBLETAP,
        BTN_TOOL_TRIPLETAP,
    ];
    let upper = s.to_ascii_uppercase();
    [AXES, KEY_NAMES, BUTTON_NAMES]
        .iter()
        .flat_map(|t| t.iter())
        .find(|(n, _)| *n == upper)
        .map(|&(_, c)| c)
        .ok_or_else(|| format!("unknown event code {s:?}"))
}

/// The key, and whether Shift must be held, that types `c` on a US layout. `None` for anything
/// a US keyboard has no key for (non-ASCII, control characters other than tab and newline).
pub fn us_ascii(c: char) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I, KEY_J, KEY_K, KEY_L, KEY_M,
        KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R, KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X, KEY_Y, KEY_Z,
    ];
    const DIGITS: [u16; 10] = [
        KEY_0, KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9,
    ];
    // The shifted digit row, in digit order: Shift-0 is ')', Shift-1 is '!', ….
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    Some(match c {
        'a'..='z' => (LETTERS[c as usize - 'a' as usize], false),
        'A'..='Z' => (LETTERS[c as usize - 'A' as usize], true),
        '0'..='9' => (DIGITS[c as usize - '0' as usize], false),
        ' ' => (KEY_SPACE, false),
        '\n' => (KEY_ENTER, false),
        '\t' => (KEY_TAB, false),
        '-' => (KEY_MINUS, false),
        '_' => (KEY_MINUS, true),
        '=' => (KEY_EQUAL, false),
        '+' => (KEY_EQUAL, true),
        '[' => (KEY_LEFTBRACE, false),
        '{' => (KEY_LEFTBRACE, true),
        ']' => (KEY_RIGHTBRACE, false),
        '}' => (KEY_RIGHTBRACE, true),
        '\\' => (KEY_BACKSLASH, false),
        '|' => (KEY_BACKSLASH, true),
        ';' => (KEY_SEMICOLON, false),
        ':' => (KEY_SEMICOLON, true),
        '\'' => (KEY_APOSTROPHE, false),
        '"' => (KEY_APOSTROPHE, true),
        '`' => (KEY_GRAVE, false),
        '~' => (KEY_GRAVE, true),
        ',' => (KEY_COMMA, false),
        '<' => (KEY_COMMA, true),
        '.' => (KEY_DOT, false),
        '>' => (KEY_DOT, true),
        '/' => (KEY_SLASH, false),
        '?' => (KEY_SLASH, true),
        _ => {
            let i = SHIFTED_DIGITS.find(c)?;
            (DIGITS[i], true)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_key_has_exactly_one_name_and_nothing_else_does() {
        assert_eq!(KEY_NAMES.len(), SUPPORTED_KEYBOARD_KEYS.len());
        for code in SUPPORTED_KEYBOARD_KEYS {
            let names: Vec<_> = KEY_NAMES.iter().filter(|(_, c)| c == code).collect();
            assert_eq!(names.len(), 1, "code {code}: {names:?}");
        }
        for (name, code) in KEY_NAMES {
            assert_eq!(key(name), Ok(*code), "{name}");
        }
    }

    #[test]
    fn names_are_forgiving_about_case_and_prefix_and_codes_work_too() {
        assert_eq!(key("KEY_LEFTCTRL"), Ok(KEY_LEFTCTRL));
        assert_eq!(key("leftctrl"), Ok(KEY_LEFTCTRL));
        assert_eq!(key("key_f2"), Ok(KEY_F2));
        assert_eq!(key("a"), Ok(KEY_A));
        assert_eq!(key("30"), Ok(KEY_A));
        assert_eq!(key("0x1e"), Ok(KEY_A));
        assert!(key("KEY_NOPE").is_err());
        // KEY_POWER (116) is a real kernel key the device does not advertise.
        assert!(key("116").unwrap_err().contains("not advertised"));
        assert!(key("70000").is_err());
    }

    #[test]
    fn an_unknown_key_suggests_the_nearest_name_and_where_the_list_is_but_is_not_accepted() {
        for (typed, want) in [
            ("escape", "KEY_ESC"),
            ("ESCAPE", "KEY_ESC"),
            ("KEY_ESCAPE", "KEY_ESC"),
            ("pageup2", "KEY_PAGEUP"),
            ("ctrl", "KEY_LEFTCTRL"),
            ("backspce", "KEY_BACKSPACE"),
            ("entr", "KEY_ENTER"),
        ] {
            assert_eq!(nearest_key(typed), Some(want), "{typed}");
            let err = key(typed).unwrap_err();
            assert!(err.contains(&format!("did you mean {want}?")), "{err}");
            assert!(err.contains("limina input <vm> keys"), "{err}");
        }
        // Nothing close: no guess, but still the pointer to the list.
        assert_eq!(nearest_key("zzzzzzzz"), None);
        assert_eq!(nearest_key(""), None);
        let err = key("zzzzzzzz").unwrap_err();
        assert!(
            !err.contains("did you mean") && err.contains("keys"),
            "{err}"
        );
    }

    #[test]
    fn buttons_and_raw_codes() {
        assert_eq!(button("left"), Ok(BTN_LEFT));
        assert_eq!(button("BTN_RIGHT"), Ok(BTN_RIGHT));
        assert_eq!(button("0x112"), Ok(BTN_MIDDLE));
        assert!(button("BTN_SIDE").is_err());
        assert!(button("0x113").is_err());
        assert_eq!(event_type("EV_REL"), Ok(EV_REL));
        assert_eq!(event_type("abs"), Ok(EV_ABS));
        assert_eq!(event_type("3"), Ok(EV_ABS));
        assert_eq!(event_code("REL_WHEEL_HI_RES"), Ok(REL_WHEEL_HI_RES));
        assert_eq!(event_code("abs_y"), Ok(ABS_Y));
        assert_eq!(event_code("BTN_LEFT"), Ok(BTN_LEFT));
        assert_eq!(event_code("KEY_A"), Ok(KEY_A));
        assert_eq!(event_code("0x110"), Ok(BTN_LEFT));
        assert!(event_code("REL_NOPE").is_err());
    }

    #[test]
    fn us_ascii_covers_every_printable_character() {
        for c in (0x20u8..0x7f).map(char::from) {
            assert!(us_ascii(c).is_some(), "{c:?} has no key");
        }
        assert_eq!(us_ascii('a'), Some((KEY_A, false)));
        assert_eq!(us_ascii('Z'), Some((KEY_Z, true)));
        assert_eq!(us_ascii('0'), Some((KEY_0, false)));
        assert_eq!(us_ascii(')'), Some((KEY_0, true)));
        assert_eq!(us_ascii('!'), Some((KEY_1, true)));
        assert_eq!(us_ascii('@'), Some((KEY_2, true)));
        assert_eq!(us_ascii('('), Some((KEY_9, true)));
        assert_eq!(us_ascii('~'), Some((KEY_GRAVE, true)));
        assert_eq!(us_ascii('\n'), Some((KEY_ENTER, false)));
        assert_eq!(us_ascii('é'), None);
        assert_eq!(us_ascii('\u{7}'), None);
        // Every key typing can press is one the device advertises.
        for c in (0x20u8..0x7f).map(char::from) {
            let (k, _) = us_ascii(c).unwrap();
            assert!(SUPPORTED_KEYBOARD_KEYS.contains(&k), "{c:?}");
        }
    }
}
