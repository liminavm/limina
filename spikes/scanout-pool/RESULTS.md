# An idle scanout pool is free, and a spare slot really can become a monitor at runtime

**Question.** M15 wave 1 needs more than one virtual display, and `num_scanouts` is virtio-gpu
config-space state a driver reads once — so every display a VM may *ever* show has to exist from
boot, as a disconnected scanout. That is only a viable design if an idle pool costs nothing and
a spare slot can be connected later. Both were assumptions. This measures them.

**Verdict: build on the pool.** Slots above 0 are inert until connected — no modes, no EDID, no
framebuffer, no compositor monitor, no new kernel error — and a spare slot connects at runtime
into a real second monitor with the identity we push it. Default the pool to **4**; 8 measured
identically, so the number is a policy choice, not a limit.

## Measured

Host: M1 Max, macOS 26.5. Guest: `Fedora-Workstation-44.enhanced.raw` clone, kernel
`7.1.8-limina16k`, mutter 50. Boot path: the default EFI+venus one
(`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`), `--net --window`, 6 vCPU / 8 GiB.
Pool selected with `LIMINA_DISPLAY_POOL` → the worker's `--display-pool`.

| | pool = 1 (baseline) | pool = 4 | pool = 8 |
|---|---|---|---|
| EFI/GOP → GRUB → installed kernel | boots | boots | boots |
| DRM connectors | 1 | 4 | 8 |
| connected | `Virtual-1`, EDID 128 B, 9 modes | same | same |
| the rest | — | disconnected, EDID 0 B, 0 modes | disconnected, EDID 0 B, 0 modes |
| mutter monitors | 1 (`Virtual-1`, `LMN 23"`) | 1, identical | 1, identical |
| `monitors.xml` stanzas | 1 | 1 | 1 |
| virtio-gpu ctrl errors in dmesg | 10 × `0x1200 (cmd 0x207)` | **same 10** | **same 10** |

**The dmesg errors are not the pool's.** `response 0x1200 (command 0x207)` is `ERR_UNSPEC` on
`SUBMIT_3D`, and the baseline at pool = 1 produces exactly the same ten. Recorded because it is
the one line that *looks* like a pool cost, and only the baseline says otherwise.

The EFI/GOP result is the one that had to be checked first: the firmware picks scanout 0 and is
indifferent to how many others exist, so the cold-boot wedge class is not in play here.

## A spare slot becomes a monitor at runtime — the load-bearing result

With pool = 8, one line to the worker's display-control socket (`hotplug-slot.sh`, the same wire
the supervisor's migration path already uses):

```
display id=1 size=1920x1080 connected=1 refresh=60 dpi=109 vendor=LMN product=20481 \
        serial=1342177281 name=pool%20slot%201
```

- `card0-Virtual-2`: `disconnected, 0 modes` → `connected, EDID 128 B, 9 modes`.
- The pushed identity is in the blob: `strings …/card0-Virtual-2/edid` → `pool slot 1`.
- mutter grew a second monitor, `Virtual-2` / `LMN 20"`, with no restart.
- `connected=0` takes it back down and mutter returns to one monitor.

So the connect/disconnect half of multi-display is **already working end to end** on the shipped
device — what is missing is everything that would put pixels on it.

## The two things a second display hits immediately

Both are limina-side and both are already on the Stage 1 list; this pins them to observed
behaviour rather than to reading the code.

1. **The display backend refuses every slot but 0.** The moment mutter drove the new monitor,
   the worker logged, once per frame:
   ```
   Some(SetScanout)    -> ErrInvalidScanoutId
   Some(ResourceFlush) -> ErrInvalidScanoutId
   ```
   and the guest saw `0x1202` (`ERR_INVALID_SCANOUT_ID`) on commands `0x103`/`0x104`. That is
   `limina-display`'s `scanout_id != 0` hardcode (`iosurface.rs`), not the device.

2. **A refused scanout strands the guest's framebuffer.** `set_scanout` calls
   `resource.scanouts.enable(scanout_id)` (`virtio_gpu.rs:1989`) *before* the backend gets a say
   (`:2001`), so a backend refusal returns early with the resource permanently marked as bound
   to that scanout. At disconnect the guest's unref is then refused:
   ```
   resource 277 has associated scanouts, refusing to delete the resource
   [SCANOUT-LEDGER] resource 277 STRANDED
   ```
   Making the backend accept the whole pool removes today's trigger, but the ordering is wrong
   independently of it — any backend failure leaks the same way. Enable after the backend
   accepts. Small and upstreamable.

## Reproducing

```sh
cp -c Fedora-Workstation-44.enhanced.raw Fedora-Workstation-44.pool-spike.raw
cargo xtask build
spikes/scanout-pool/boot-with-pool.sh 8 &        # LIMINA_DISPLAY_POOL -> --display-pool
spikes/scanout-pool/probe-connectors.sh 2222     # port comes from the worker log
spikes/scanout-pool/hotplug-slot.sh <limina-pid> 1 on 1920x1080
spikes/scanout-pool/hotplug-slot.sh <limina-pid> 1 off
```

## The stock tier and synoik behave identically

The pool changes what a *stock* guest sees, so the two-tier guarantee makes a stock run a
precondition for making a pool the default rather than a nice-to-have; and synoik's config store
is the half that needed its own fix this week, so it has to see the pool the way mutter does.
Both were run at pool = 4, and neither differs from the table above in any respect:

| | stock F44 | synoik |
|---|---|---|
| guest | Fedora's own `6.19.10-300.fc44` (4 KiB), stock mesa 26.0.3, **zero limina packages** | enhanced image, `synoik --session` |
| connectors | 4: slot 0 connected (128 B EDID, 9 modes), 3 inert | same |
| compositor at boot | 1 monitor, `LMN 23"` | 1 monitor, `LMN 23″` |
| ctrl errors in dmesg | same 10 × `0x1200 (cmd 0x207)` | — |
| runtime connect of slot 1 | `Virtual-2` connected, 9 modes, `pool slot 1` in the blob, second monitor appears | same; a `monitors.xml` stanza is written for the new arrangement |
| runtime disconnect | back to 1 monitor | back to 1 monitor, session alive |

The stock result is the stronger one: a guest with none of our components, on Fedora's own
kernel, both tolerates the idle pool and does live connector hotplug. Multi-display is therefore
**not** an enhanced-tier feature — it needs no agent, no custom kernel, and no venus.

Both tiers also reproduce the stranding of item 2 at disconnect, confirming it as a property of
the ordering rather than of any one guest.
