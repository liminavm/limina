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

The Command/Option swap is **on by default** (`swap_cmd_opt_enabled()` = `swap || !no_swap`, both
false → true), so the guest's Super is macOS's **left Option**. The ungrab chord is **Ctrl+Option**.
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

## Consequences for the fix

- Fault A wants a whole-bitmask reconcile of held modifiers, the `sync_capslock` analogue: pure
  function in `limina-input/src/keymap.rs`, RED-first, next to `modifier_emit`. Two traps found
  while reading: the reconcile must emit the *other* modifiers **before** the event's own keycode
  (so Ctrl-down precedes Super-down), and it must not run while the ungrab chord is in `Withhold`,
  or it leaks exactly the edges the chord is hiding.
- Non-`flagsChanged` events carry only class bits, no left/right dev bits, so a reconcile driven
  from *every* event cannot tell left Ctrl from right. `flagsChanged` carries both. Whether the
  first `flagsChanged` is a sufficient heal point, or key/pointer events should heal too at class
  granularity, is a real design fork — decide it against the trace, not from assumption.
- Fault A alone does not fully fix the reported symptom, and fault B alone does not either. Fixing
  only A leaves press 1 eaten by the chord; fixing only B makes press 1 arrive bare instead of
  press 2. The report needs both.
- Fault B is a policy collision, not a coding error: with the default swap, the guest's Super and
  the ungrab chord's Option are one physical key. Any fix is a decision about which gesture owns
  Ctrl+Option, and that is the user's call.
