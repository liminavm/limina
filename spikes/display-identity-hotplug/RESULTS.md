# Which display-change event makes a guest compositor re-read the EDID?

An in-place EDID swap is enough for **mutter**. It is **not** enough for **synoik**, which re-reads
the connector only on a genuine disconnect→reconnect. Measured 2026-08-17 on the F44 enhanced
images (`enhanced` = mutter, `enhanced.synoik` = synoik), both booted EFI+venus on the same host,
identities pushed over the display-control socket.

| compositor | in-place EDID swap | disconnect → new EDID → reconnect |
|---|---|---|
| mutter 50.0 | **re-reads**: new identity, new scale, and the previous display's remembered scale restored on return | re-reads, same outcome |
| synoik | **stale**: re-picks a scale for the new *mode* but keeps reporting the OLD identity | **re-reads**: new identity, and the remembered scale restored on return |

So the two tiers need different events, and only the cycle satisfies both.

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

## The cycle needs two messages, not a delay

Sweeping the disconnect window on synoik (`gap-floor.sh`, every step a genuine identity change so
no step can pass by already being on the target):

| disconnect window | result |
|---|---|
| all three commands in **one socket write** | **STALE** (3/3) |
| separate writes, no added delay | RE-READ (3/3) |
| 50 ms | RE-READ (3/3) |
| 100 ms – 2 s | RE-READ |

No wall-clock gap is needed — though the "no added delay" runs still had the few milliseconds of a
process spawn between writes, so treat single-digit ms as the demonstrated floor rather than zero.

The single-write failure is **not** our device layer coalescing. `DisplayUpdate::can_merge` refuses
to fold any update carrying a connection change, and the GPU worker takes exactly one update per
wake, re-kicking its own eventfd while any remain — so all three arrived as distinct, ordered
config-change events. Something on the guest side coalesced its own re-probe. Not chased further,
because the two-message form is both reliable and structurally better.

What the *supervisor* owes is only **order**, and that is a real hazard: `send_display_command`
spawns a thread per call, so two independent calls can invert. Losing that race tells the guest to
reconnect before it is told to disconnect and leaves the connector down with nothing queued to
raise it — hence `send_display_commands`, one thread, blocking writes, with a forced reconnect if
the pair fails half-way.

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
