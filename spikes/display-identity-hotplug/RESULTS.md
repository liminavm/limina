# Which display-change event makes a guest compositor re-read the EDID?

An in-place EDID swap is enough for **mutter**. It is **not** enough for **synoik**, which updates a
monitor's *mode list* from a new EDID but refreshes the *identity* it reports and keys on only across
a connector cycle with a real gap in it. So only the cycle satisfies both tiers, and it is limina's
default.

Measured 2026-08-17 on the F44 enhanced images (`enhanced` = mutter 50.0, `enhanced.synoik` =
synoik), both booted EFI+venus on the same host — first with identities pushed over the
display-control socket, then end-to-end by dragging the window between two physical displays.

| compositor | in-place EDID swap | cycle: disconnect → settle → reconnect + EDID |
|---|---|---|
| mutter | **re-reads**: new identity, new scale, previous display's remembered scale restored on return | re-reads, same outcome |
| synoik | **stale identity** (mode list does update) | **re-reads** the identity |

Two things the cycle does *not* fix, both synoik's and both filed:

- a reconnect carrying **no size** leaves the identity stale (the shape `dynamic`/`fixed` send);
- synoik's config store keeps exactly **one** `<configuration>`, so per-display memory fails even
  with a correct identity. Unreachable from limina at any EDID or event.

## What "stale" looks like

synoik, pushed `DELL P2723QE / 0x370D1790` in place while sitting on `BenQ LCD / 0x6C42FAE5`:

```
before:  (0, 0, 1.25, ... [('Virtual-1', 'PNP(LMN)', 'BenQ LCD', '0x6C42FAE5')
after:   (0, 0, 1.75, ... [('Virtual-1', 'PNP(LMN)', 'BenQ LCD', '0x6C42FAE5')
```

The scale moved (1.25 → 1.75), so the new *mode* was seen; the identity did not change. A save
while in this state lands under the wrong monitor key, which is how per-display layout memory is
lost even though each individual save is internally consistent.

Under a cycle the same push yields `('Virtual-1', 'PNP(LMN)', 'DELL P2723QE', '0x370D1790')`,
scale 1.5, and cycling back restores BenQ at its remembered 1.25.

## Ship the two-message form, and it needs a size

The reconnect must carry the new EDID itself — `connected=0`, then
`connected=1` + EDID (+ size). Verified 3/3 on synoik and 2/2 on mutter. It is also the
structurally right shape: the new EDID and the reconnect are one atomic update, so a reconnected
connector can never be probed while still advertising the old EDID.

**A reconnect with no `size` does not make synoik re-read** (mutter does): pushed a fresh identity
with no size field, synoik stayed on the previous one. So `host` mode — which folds the screen size
in — works on both, while `dynamic` and `fixed`, which must not dictate the guest's resolution, are
stale on synoik. The host side is not the problem: the worker applies `size`, `edid` and `connected`
independently and mutter picks up the sizeless EDID, so the EDID *did* change
(`third_party/libkrun/src/devices/src/virtio/gpu/worker.rs`).

## The cycle needs a real delay, ~50 ms, and a `nc`-driven test hides that

The guest must *observe* the connector down, and that takes wall-clock time. Sweeping the
disconnect window on synoik (`gap-floor.sh`, every step a genuine identity change so no step can
pass by already being on the target):

| disconnect window | result |
|---|---|
| all three commands in **one socket write** | **STALE** (3/3) |
| two writes from the shipped sender, back to back | **STALE** — modes update, identity does not |
| separate `nc` invocations, no added `sleep` | RE-READ (3/3) |
| 50 ms | RE-READ (3/3) |
| 100 ms – 2 s | RE-READ |

**The `nc` row is a trap and it fooled us once.** One process spawn per command silently supplies
milliseconds, so "no added delay" measured as a pass and the first version of this document
concluded no delay was needed. The shipped sender writes both commands from one thread microseconds
apart and reproduced the stale identity through the real window-migration path. An experiment whose
harness contributes the very quantity under test cannot measure it: the passing configuration and
the shipped configuration were not the same experiment. Hence `CONNECTOR_DOWN_SETTLE` = 60 ms in
`window/mod.rs`, on the sender's own thread so the window never stalls for it.

The failure is **not** our device layer coalescing. `DisplayUpdate::can_merge` refuses to fold any
update carrying a connection change, and the GPU worker takes exactly one update per wake, re-kicking
its own eventfd while any remain — so the commands do arrive as distinct, ordered config-change
events. The coalescing is the guest's own re-probe.

The supervisor also owes **order**, and that is a real hazard: `send_display_command` spawns a
thread per call, so two independent calls can invert. Losing that race tells the guest to reconnect
before it is told to disconnect and leaves the connector down with nothing queued to raise it —
hence `send_display_commands`, one thread, blocking writes, with a forced reconnect if the pair
fails half-way.

## End to end, through the real window-migration path, on two physical displays

Everything above pushes identities at the socket by hand. This is the shipped path — window
position → `hostdisplay::describe` → `migration_commands` → socket → libkrun → guest — with the
window dragged between a 2560x1440 external panel and a 3024x1964 built-in Retina
(`migrate-window.sh`, `per-display-memory.sh`).

| | mutter | synoik |
|---|---|---|
| identity follows the window both ways | yes | yes (**needs the 60 ms settle**; without it the mode list updates and the identity stays stale) |
| each display's own scale re-applied on arrival | yes — 1.333 external / 2.0 built-in, every switch | no |
| `monitors.xml` stanzas | **2**, one per display, both retained | **1** — configuring a display *replaces* the other |

So limina's half is done: both compositors now learn which physical display they are on. Per-display
memory works on mutter and still does not on synoik, for a **second and independent** reason — its
config store keeps exactly one `<configuration>`. With the built-in configured to 1.75, the external
panel's stanza is gone and its remembered 1.333-equivalent never returns. That one is not reachable
from limina at any EDID or event: one stanza cannot hold two displays.

A useful property of the sequence: the boot-time identity push is itself a migration, so it cycles
too. A compositor that starts before the push lands would otherwise latch the anonymous boot
identity (`RHT / krun-display`) permanently — visible as the first stanza in the mutter arm's file.

This matters for the cost of a cycle: the zero-monitor interval is milliseconds, not the visible
outage the original design assumed when it chose the in-place swap.

## EDID and DRM state are identical across the two tiers

Same connector, same status, same mode list, same 128-byte EDID blob, same kernel
(`7.1.8-limina16k`). Two differences are the compositors' own, not ours:

- **Scale ladders differ.** For 2560x1440 mutter offers `1.0 1.25 1.333 1.667 2.0 2.5 2.667` —
  it keeps only scales that divide the mode into an integer logical size. synoik offers a plain
  quarter-step ladder `1.0 1.25 1.5 1.75 … 3.0`. So 133% exists in mutter's Settings and 150% in
  synoik's, from one identical EDID.
- **The vendor field is rendered differently.** mutter prints the raw PNP id `LMN` and lowercases
  the serial (`0x6c42fae5`); synoik prints `PNP(LMN)` and uppercases it (`0x6C42FAE5`). The same
  physical display therefore gets a different `<monitorspec>`, so a `monitors.xml` is not portable
  between the two compositors.

`/sys/class/drm/card0-Virtual-1/edid` reads **0 bytes** on both while both compositors have the
full identity and mode list — they read the DRM connector property, so nothing is broken, but
anything reading EDID from sysfs sees nothing.

## Reproducing

Both VMs boot from CoW clones (`cp -c`) of the stock images; the disk boots in place.

```
cargo xtask run --disk Fedora-Workstation-44.enhanced.synoik.<clone>.raw   # synoik arm
cargo xtask run --disk Fedora-Workstation-44.enhanced.<clone>.raw          # mutter arm
```

Read the auto-allocated SSH port and the socket from the worker log
(`/tmp/limina-worker-<disk>.log`, `$TMPDIR/limina-resize-<supervisor-pid>.sock`), then:

```
bash probe-drm.sh                       # piped to `ssh … bash -s`: DRM + EDID + DisplayConfig state
bash swap-experiment.sh <port> <sock> <label> [inplace|cycle]
bash gap-floor.sh <port> <sock> <gap-seconds> [repeats]
```

`gap-sweep.sh` walks a range of gaps in one run, but it alternates targets by step index, so a
step can land on the identity it was already on and read as a pass. `gap-floor.sh` picks its
target from the live state instead — prefer it, and treat any sweep row whose target equals the
previous state as no evidence.
