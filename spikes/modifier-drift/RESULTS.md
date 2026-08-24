# Modifier drift across a Space return — diagnosis

**Reported 2026-08-09.** "When switching workspaces back and forth, sometimes I come back to the
limina one and hit Super while still holding Control; the Super key seems to trigger the overview
on press, and hitting Super again also gets the GNOME overview to toggle. I *think* the held Ctrl
is not being seen when coming from another workspace."

The user's hunch was right, and the trace found a **second, independent fault** stacked on top of
it that explains the "it takes 2 Super presses" part.

## How it was captured

`LIMINA_INPUT_TRACE=1` (added for this, `crates/limina/src/window/input.rs`) prints, for every
keyboard event, what the **host** modifier bitmask says every modifier is doing, what we **believe**
we have told the guest, and the **drift** between them. Drift is the whole diagnosis: a non-empty
drift set at the moment a key is pressed means the guest is about to receive that key wearing the
wrong modifiers.

Repro vehicle: `LIMINA_INPUT_TRACE=1 LIMINA_BIN=target/Limina.app/Contents/MacOS/limina
LIMINA_DISK=…/modkey-repro.raw spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`, fullscreen (own
Space), gesture performed by the user. The **signed** bundle is required, not `target/debug/limina`:
the capture tap is Accessibility-gated on the code hash, and without it the soft keyboard grab —
half of fault B — never engages. Trace: `trace-2026-08-09.log`.

## Fault A — nothing re-announces a modifier held across a Space return

Verbatim, one cycle (t in ms):

```
t=192467.4 space-RETURN (no modifier resync happens here)
t=193476.2 MON-flags kc=lopt flags=0x40101 host=[lctrl] guest=[]  DRIFT[lctrl:host=DOWN]
t=194030.1 MON-flags kc=lopt flags=0xc0121 host=[lopt,lctrl] guest=[]  DRIFT[lopt:host=DOWN lctrl:host=DOWN]
t=194030.8   -> guest mod lopt evdev=125 DOWN      <-- bare Super. No Ctrl was ever sent.
t=194179.9   -> guest mod lopt evdev=125 UP
```

`host=[lctrl]` is continuous from the return through the whole Super press — the Ctrl dev bit
(`0x1`) and class bit (`0x40000`) are both in the raw flags the whole time. `guest=[]` throughout.
The guest received `KEY_LEFTMETA` down/up with nothing else held, which is precisely GNOME's
"open the overview" gesture.

The mechanism, in order:

1. On Space-leave, `release_all_held("space-leave")` (`window/mod.rs`) correctly releases Ctrl in
   the guest — that part already works and is deliberate.
2. On Space-return, **nothing** re-announces it. macOS sends no reconciling `flagsChanged` for a
   modifier that never moved, so the monitor has no event to learn from.
3. `emit_modifier` is keyed on the *event's own keycode*. The next `flagsChanged` carries the full
   bitmask — Ctrl's bit is right there in `0xc0121` — but the function only ever asks about the one
   key the event named. The Ctrl bit is read and discarded on every event.

`sync_capslock` already solves exactly this problem for the lock key, on the same reasoning
("macOS sends no reconciling flagsChanged on refocus, so the next event here re-syncs"). Held
modifiers never got the equivalent.

## Fault B — the user's "Ctrl + Super" *is* the ungrab chord, physically

Modifier normalization is **on by default** (`normalize_modifiers_enabled()` = `on || !off`, both
false → true), so on a Mac whose modifier
row macOS itself has not remapped, the guest's Super is macOS's **left Option**. The ungrab chord is **Ctrl+Option**.
Holding Ctrl and pressing Super is therefore the literal ungrab gesture as far as the tap is
concerned, and the trace shows it firing on the *first* press of every cycle:

```
t=193471.7 release_all_modifiers(soft-grab-exit) believed=[]
t=193476.2 MON-flags kc=lopt ...        <-- note MON, not TAP: the soft grab is now muted
```

`release_all_modifiers("soft-grab-exit")` has exactly one caller, `flush_modifiers()`, which has
exactly one caller: the chord's `Fire` branch. So on the first Super press the tap arms the chord,
**withholds** the press, and on release fires — dropping the withheld press entirely and setting
`soft_muted = true`. The guest sees nothing at all.

The mute is why every later line in the cycle is `MON-` rather than `TAP-`: with the soft grab
muted, the tap passes events through and the local NSEvent monitor forwards them. The **second**
Super press takes that path and reaches the guest — bare, per fault A — and the overview opens.

**That is the "2 presses".** Press 1 is eaten by the chord; press 2 arrives without its Ctrl.

Worth noting the chord arms off `ungrab_chord_step`, which reads the CONTROL class bit straight
from the raw flags — including a Ctrl the guest has never been told about. The chord and the guest
disagree about what is held, and the chord wins.

## Fixed, 2026-08-09

**A — `reconcile_modifiers`** (`crates/limina-input/src/keymap.rs`), the held-modifier twin of
`CapsLockSync`: given the host bitmask and the believed pressed-set, it returns the edges that
close the gap. Fed from the `flagsChanged` and key-down paths, after the chord has had its say so
withheld edges stay withheld. Two details are load-bearing, and both are unit-tested against
bitmasks lifted verbatim from the trace above:

- The event's own modifier is **excluded**. The walk order puts Option before Control, so
  reconciling it here would emit Super *before* the Ctrl it is meant to be modified by.
- A press is **never** inferred from a device-independent class bit. Asked about all eight
  keycodes, a lone CONTROL bit answers "down" for both Controls and would press a key the user is
  not holding. Releases need no such gate — a clear class bit means both sides are genuinely up.

**B — `chord_survives_mute`** (`window/grab_policy.rs`): the chord no longer goes deaf once it
fires. A Control still held re-arms it, so repeated Ctrl+Super releases and never reaches the
guest — the reporter's stated expectation. Gated on the mute itself, so `--no-soft-kbd-grab` keeps
Ctrl+Option as an ordinary guest combo.

Verified live on the same vehicle, five consecutive chords, every one `Withhold → Withhold →
Fire` with no `evdev=125` in sight, and the guest's Control restored by `RESYNC` after each:

```
chord(ungrabbed) kc=lopt flags=0xc0121 armed false->true action=Withhold
chord(ungrabbed) kc=lopt flags=0x40101 armed true->false action=Fire
MON-flags kc=lopt flags=0x40101 host=[lctrl] guest=[]  DRIFT[lctrl:host=DOWN]
  RESYNC lctrl DOWN
```

The first cut of B also called `flush_modifiers()` on the ungrabbed fire, and the trace caught
what that costs: the flush released Control in the guest and the very next edge resynced it back,
one spurious release/press pair per chord, five in five. Removed — this path never forwards a
chord edge, so the only modifiers it holds are ones the resync put there because the user really
is holding them.

A side effect worth knowing: synthetic `flagsChanged` (osascript) carry keycode 0, so
`emit_modifier` used to drop them entirely and no synthetic modifier ever reached the guest. The
reconcile does not depend on the event naming a key, so scripted key+modifier combos now land.

## Still open

- **Pointer events do not heal.** The reconcile runs on `flagsChanged` and key-down only. A
  Ctrl+click straight after a Space return still reaches the guest as a plain click. Pointer
  events carry class bits at best, and `reconcile_modifiers` refuses to press on that evidence
  (see above), so healing there needs a different answer than "call it from one more place".
- **Why the tap sees each `flagsChanged` twice.** Every chord edge in the trace appears as two
  identical calls ~1 ms apart (`armed false->true`, then `armed true->true`). The dedup in
  `modifier_emit` absorbs it and it predates this work, but it is unexplained.
