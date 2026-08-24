# Does macOS remap modifier keys before an application sees them?

**Yes — in the HID layer, before any app.** Both the keycode and the modifier flags arrive
already rewritten, so an `NSEvent` cannot tell you which key the user physically pressed.

## Measurement (2026-08-24, macOS 26.5, M1 Max, built-in keyboard `1452-834-0`)

With System Settings ▸ Keyboard ▸ Modifier Keys set to swap **Control ↔ Command**, both sides:

| physical key pressed | `NSEvent.keyCode` delivered | flags |
| --- | --- | --- |
| left Control | `0x37` `kVK_Command` | `0x00100108` (Command class + left-Command device bit) |
| left Command | `0x3B` `kVK_Control` | `0x00040101` (Control class + left-Control device bit) |
| left Option | `0x3A` `kVK_Option` | `0x00080120` (unremapped, so unchanged) |

Run `swiftc -O probe.swift -o probe && ./probe` to repeat it; the probe prints the configuration
it found and then a line per `flagsChanged`.

## Why it matters

limina's modifier normalization is **positional** — the key in the Option position becomes
Super, the key in the Command position becomes Alt — so it has to be applied to the physical
key. Because macOS has already moved the row by the time we see it, limina's own mapping used to
*compose* with the user's: with the swap above, the key labelled Control reached the guest as
Alt. Normalization therefore reads the configuration and inverts it first
(`crates/limina/src/hostmods.rs`), which is only possible because the mapping is readable.

## Where the configuration lives

The **ByHost** global domain — `NSUserDefaults` cannot see it, only `CFPreferences` — under one
key per keyboard, `com.apple.keyboard.modifiermapping.<vendor>-<product>-<n>`, holding an array
of `{HIDKeyboardModifierMappingSrc, HIDKeyboardModifierMappingDst}`. Values are HID usages plus a
`0x7_0000_0000` usage-page prefix; `-1` means "No Action".

```
$ defaults -currentHost read -g          # com.apple.keyboard.modifiermapping.1452-834-0
    HIDKeyboardModifierMappingSrc = 30064771299;   # 0xE3 left Command
    HIDKeyboardModifierMappingDst = 30064771296;   # 0xE0 left Control
```

`cargo test -p limina --bins -- --ignored --nocapture dump_this_macs_modifier_map` prints what
the machine in front of you has configured, through limina's own reader.

## Known limitation

The mapping is **per keyboard** and an `NSEvent` does not say which keyboard produced it, so
limina cannot invert per device. When the configured keyboards disagree it warns once and
inverts nothing. One case is genuinely undecidable this way: a PC keyboard whose owner swapped
Cmd/Opt in macOS to restore the positional feel gets that swap undone again inside the guest.
