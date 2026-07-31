# fn-key-probe — which event class does each top-row key land in?

**Question.** limina forwarded F1–F19 fine (`keymap.rs` function row) but the special/media top
row never reached the guest at all. Why, and which physical press is affected?

**Answer (source-verified, 2026-07-31).** Two different event classes, and we only asked for one:

- Presses that macOS resolves to a **virtual keycode** arrive as ordinary `keyDown`/`keyUp`
  (CGEventType 10/11) — already forwarded by the tap and the local monitor.
- Presses macOS resolves to a **special/aux key** (brightness, volume, media transport,
  keyboard illumination, eject) arrive as **`NX_SYSDEFINED`** (CGEventType **14**), subtype 8
  (`NX_SUBTYPE_AUX_CONTROL_BUTTONS`), with the key packed into `NSEvent.data1` — a namespace
  disjoint from virtual keycodes. Neither `capture_tap.rs`'s tap mask nor the local monitor's
  `NSEventMask` asked for type 14, so these went straight to macOS in *every* mode.

Which physical press lands in which class depends on the *"Use F1, F2, etc. as standard function
keys"* setting — the fn translation happens in the **HID layer below a session event tap**, so by
the time we see the event the decision is already made. We can observe it, not intercept it
earlier. That's the whole reason `fn` is not a modifier we can key off:
`kCGEventFlagMaskSecondaryFn` (1 << 23) *does* ride along on real F-key and arrow keyDowns, but
it is **not** how fn+top-row brightness/media arrive.

`NX_KEYTYPE_*` values were read from the SDK header, not recalled:
`…/MacOSX.sdk/System/Library/Frameworks/IOKit.framework/Versions/A/Headers/hidsystem/ev_keymap.h`
— `SOUND_UP 0`, `SOUND_DOWN 1`, `BRIGHTNESS_UP 2`, `BRIGHTNESS_DOWN 3`, `MUTE 7`, `PLAY 16`,
`NEXT 17`, `PREVIOUS 18`, `FAST 19`, `REWIND 20`, `ILLUMINATION_* 21-23`. evdev codes came from
`linux/input-event-codes.h` (`KEY_NEXTSONG 163`, `KEY_PLAYPAUSE 164`, `KEY_PREVIOUSSONG 165`,
`KEY_REWIND 168`, `KEY_FASTFORWARD 208`).

## The probe

`fnprobe.swift` is a **listen-only** session tap that dumps keyDown/keyUp/flagsChanged and
decoded `NX_SYSDEFINED` aux keys. Listen-only means it consumes nothing — the keyboard behaves
normally while it runs, so it's safe to leave open during a session.

```sh
swiftc -O spikes/fn-key-probe/fnprobe.swift -o spikes/fn-key-probe/fnprobe
spikes/fn-key-probe/fnprobe          # Ctrl-C to stop
```

Needs Accessibility for whatever app runs it (Terminal/iTerm), else `CGEventTapCreate` returns
nil and it says so. Use it to confirm a key's class before adding it to a bucket — the mapping
from *physical key* to *event class* varies with the keyboard and the F-keys setting, so it is
not something to assume.

## Observed run (M1 Max MacBook internal keyboard, 2026-07-31 — `output.txt`)

This Mac has *"Use F1, F2 as standard function keys"* **on**, and the split is exactly as
predicted — bare top row is keycodes, fn+top row is the aux class:

| press | arrives as | who handled it before |
|---|---|---|
| F1, F2, F10, F11, F12 **bare** | `keyDown keycode=0x7A/0x78/0x6D/0x67/0x6F` | guest (keymap function row) ✓ |
| **fn**+F1/F2 | `SYSDEF NX=3/2 BRIGHTNESS_DOWN/UP` | host only — never seen by limina |
| **fn**+F10/F11/F12 | `SYSDEF NX=7/1/0 MUTE/SOUND_DOWN/SOUND_UP` | host only |
| **fn**+F8/F9/F7 | `SYSDEF NX=16/19/20 PLAY/FAST/REWIND` | host only |
| **fn** alone | `flagsChg keycode=0x3F` + `keyDown keycode=0xB3` | neither (see below) |

Two things the run taught us that reasoning alone did not:

1. **The transport keys report `FAST`/`REWIND`, never `NEXT`/`PREVIOUS`.** The ⏭/⏮ keys emit
   `NX_KEYTYPE_FAST` (19) and `NX_KEYTYPE_REWIND` (20) — macOS's names for what the same
   hardware reports over USB HID as Scan Next/Previous Track. Mapping them literally to
   `KEY_FASTFORWARD`/`KEY_REWIND` would hand the guest codes GNOME leaves unbound: dead keys
   that would read as "the forwarding is broken". They map to `KEY_NEXTSONG`/`KEY_PREVIOUSSONG`
   instead (`auxkey.rs`, locked in by `apple_transport_keys_are_track_skip_not_seek`).
2. **`fn` alone also emits a plain `keyDown` with keycode `0xB3`** (the Globe key's own action —
   emoji picker / input-source switch, depending on the Keyboard setting), on top of the
   `flagsChanged` for keycode `0x3F`. Neither is in our keymap, so nothing reaches the guest —
   but while a grab is engaged the tap *consumes* both, which is the behavior we want: tapping
   fn over a grabbed VM won't pop the host emoji picker. Nothing to do; noted so a future
   `KEY_FN` mapping doesn't come as a surprise.

This run is also a single config (*F-keys as standard*
**on**, which is not the macOS default); with the setting off, the bare top row is the aux class
and the fn+F-keys are the plain keycodes. The bucket policy behaves the same either way, but any
claim of the form "bare F-keys reach the guest" is config-dependent.

Also worth remembering: **every** F-key press on this keyboard carries the `FN` flag
(`kCGEventFlagMaskSecondaryFn`) whether or not fn is held — bare F1 shows `flags=[FN]` too. So
that flag can never be used to tell "fn was held"; only the event *class* distinguishes them.

## Second run: fn+F3–F6 are a THIRD mechanism (`output-f3f6.txt`, 2026-07-31)

The interesting keys turned out not to be aux keys at all:

| press | arrives as |
|---|---|
| bare F3/F4/F5/F6 | `keyDown keycode=0x63/0x76/0x60/0x61` (the ordinary function row) |
| **fn**+F3 Mission Control | `keyDown keycode=0xA0` |
| **fn**+F4 Spotlight | `keyDown keycode=0xB1` |
| **fn**+F5 Dictation | `keyDown keycode=0xB0` |
| **fn**+F6 Do Not Disturb | `keyDown keycode=0xB2` |
| **fn** alone (Globe) | `keyDown keycode=0xB3` |

(The moon key is **Do Not Disturb / Focus**, not suspend — worth knowing, because a Focus toggle
has a plausible guest counterpart while suspend would be permanently host-only.)

So there are **three** classes, not two: virtual keycodes we map, NX_SYSDEFINED aux keys, and
these special-action keycodes that are ordinary keyDowns carrying codes no keymap claims. That
kills the assumption behind the earlier backlog note: promoting Mission Control → GNOME overview
is a `keymap.rs` entry, **not** an `auxkey` bucket edit.

All six are **inert while the VM window is focused**: the grab drops any keycode with no guest
mapping, so there's no GNOME overview (we send nothing) and no Mission Control either (we consume
the event before the system hotkey handler sees it). That is deliberate — see the decision below.
Ctrl-Opt (mute the soft grab) is the way to reach them meanwhile.
`macos_special_action_keycodes_have_no_guest_mapping` pins the premise, so mapping one of these
later fails a test rather than silently changing behavior.

### Decision: a grab DROPS unmapped keys; it does not hand them to the host

The obvious-looking alternative — "we can't use it, so let macOS have it" — was implemented and
then **rejected** (2026-07-31), because it is fail-dangerous. We cannot enumerate what an unknown
key does on an arbitrary keyboard, and some are destructive: a keyboard with a reboot/sleep/eject
key, pressed by a user aiming it at the *guest*, would act on the host — and a grabbed user
cannot ungrab fast enough to cancel. A dropped key costs a keystroke; a host reboot costs the
session. Nor does anything safety-critical depend on pass-through: the real "recapture control"
combos (force-quit, lock, power) run through secure-input paths a session tap never sees.

This is consistent with the aux buckets rather than in tension with them. Those hand macOS only
keys we have *identified and deliberately classified* — brightness stays host because we know
it's brightness. The rule is knowledge-based: **classify a key and route it on purpose, or drop
it. Never forward blind.**

### Trap this run cost us

The first attempt wrote an **empty** `output-f3f6.txt` — which reads exactly like the most
important result this probe can produce ("that key generates no events at all"). Cause: Swift's
`print` is block-buffered when stdout is a file rather than a terminal, so `> out.txt` + Ctrl-C
discards the buffer. The probe now calls `setvbuf(stdout, nil, _IOLBF, 0)`, so a missing line is
trustworthy. If you see an empty capture from an older build, that is lost output, not a finding.

## What shipped from it

`crates/limina-input/src/auxkey.rs` — bucket policy (`Media` / `Volume` / `Brightness` / `Other`)
plus the `data1` decode, and `capture_tap.rs` routes type 14 through it. Ownership is per bucket
rather than per grab mode, because these keys are not interchangeable: eating Cmd-Tab while the
VM is focused is expected, eating *brightness* is not (there is no backlight in the guest).

| bucket | keys | goes to the guest |
|---|---|---|
| `Media` | play/pause, next/prev track (incl. the FAST/REWIND codes) | soft **or** full grab |
| `Volume` | volume up/down, mute | full grab only |
| `Brightness` | brightness up/down | never |
| `Other` | eject, illumination, Launchpad, unknown codes | never (revisit one at a time) |

Note this is a **tap-only** capability: aux keys are never delivered to a local `NSEvent`
monitor, so without Accessibility (no tap) the buckets are inert and every key stays with the
host — a graceful degradation, but worth knowing when a media key "doesn't work" on a machine
where the grant is missing.
