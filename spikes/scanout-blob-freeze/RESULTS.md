# The "SET_SCANOUT_BLOB ACKed but never applied" freeze — diagnosed 2026-08-16

The guest compositor (synoik) reported a display freeze in which it keeps rendering correct frames
at 60 Hz, every `SET_SCANOUT_BLOB` is consumed and ACKed `RESP_OK_NODATA`, nothing errors, and the
screen holds a stale frame. Only a **genuinely new** resource restores a participant, and creating
new resources can knock another participant out. Their writeup:
`limina-issue-scanout-blob-not-applied.md` in the synoik tree.

**It is ours, and it is in the supervisor — not in the worker, not in virglrenderer, not in the
guest.** That is why five successive theories (four theirs, one mine) all fitted the evidence and
all died: every instrument either side had was pointed at a healthy component.

## Reproduction

`gfxsynoik.raw` (clone of `Fedora-Workstation-44.enhanced.synoik.raw`), M1 Max, 6 vCPU / 8 GiB,
EFI → GRUB → `7.1.8-limina16k`, coexist GPU, windowed, booted with

```sh
RUST_LOG=warn,krun_devices::virtio::gpu::virtio_gpu=debug
```

which is what makes the per-flip `SET_SCANOUT_BLOB` / `[FLUSHDBG]` lines visible — they are
`debug`, and the worker defaults to `warn`, so **every previous investigation had been throwing
this away.**

Workload: GNOME-style overview toggling with two Firefox windows, one fullscreen running the WebGL
aquarium. User reported onset ~11:56:50Z and a fully frozen black screen by ~11:58:35Z.

## What the log shows

**The failing line, first seen 11:56:38Z** — 12 s before the user noticed anything:

```
WARN limina::window] window: surface 139 unresolved; skipping frame
WARN limina::window] window: surface 141 unresolved; skipping frame
```

Skips per minute — `evidence/skip-rate-per-minute.txt`:

| minute | skipped frames |
|---|---|
| 11:56 | 161 |
| 11:57 | 168 |
| 11:58 | 1651 |
| 11:59 | 3013 |

Against ~383 `SET_SCANOUT_BLOB`/min at 11:56 that is ~42% of frames dropped, which is exactly the
user's "the animation is 2 frames, one halfway and one at the end". By 11:58 it is every frame,
which is the freeze.

**The worker was healthy throughout**, which is the part that exonerates everything previously
suspected:

- `SET_SCANOUT_BLOB` only ever bound `res=7 -> IOSurface 139` and `res=12 -> IOSurface 141`, 2863
  and 2861 times, stable from 11:52:49 to 11:59:50. The mapping never went stale.
- `[FLUSHDBG] flush res=7 … iosurface_id=Some(139)` / `res=12 … Some(141)` continuously, including
  through the freeze.
- Zero `not IOSurface-backed` warnings.
- The flip rate went **up** ~9× at the freeze (333/min → 3013/min): the guest was presenting
  harder, correctly, into correctly-resolved surfaces.

## Two hypotheses killed on the way

- **Stale cached IOSurface id (mine).** `RutabagaResource.iosurface_id` really is resolved once at
  create and never refreshed (`virgl_renderer.rs:768`, `:980`), and `set_scanout_blob` really does
  return `Ok(OkNoData)` unconditionally — so the code path is as suspicious as it looked. It is
  simply not what happened: the mapping was correct for the entire session. A suspicious code path
  is a lead until observed, and this one did not survive observation.
- **Surface-store eviction.** `SurfaceStore` is capped at 32 with FIFO eviction, and synoik mints
  fresh scanout resources, so overflow was plausible. Only **20** distinct IOSurface ids exist in
  the whole session (`evidence/distinct-iosurface-ids.txt`). Not eviction.

## The mechanism

`crates/limina/src/window/present.rs`. The supervisor holds guest surface id → retained
`IOSurfaceRef`, and the worker tells it when the guest lets go of a resource:

```rust
Ok(SurfaceMsg::Released(id)) => map.lock().unwrap().note_released(id),
```

`note_released` drops the reference immediately, resting on an assumption its own doc comment
spells out:

> *"Safe to act on immediately even if the surface is still on the layer — Core Animation and
> `last_ca` hold their own references… **And the guest will not present it again, so no later
> resolve can miss because of this.**"*

**That premise does not hold for synoik**, which keeps presenting `res 7` / `res 12` after the
release. The frame-apply path then does:

```rust
surface_map.lock().unwrap().get(id).or_else(|| IOSurfaceLookup(id))
```

and **both halves fail** — the map because we dropped it, and the global lookup because these
surfaces are deliberately **non-global** (capability-scoped, handed over by Mach port so a
cross-process `iosdump` cannot read the user's screen). With no fallback left, every later frame
naming that id is skipped, silently, forever.

Note the interaction: the non-global scanout work removed the fallback that had been quietly
masking this, and the surface-release work (added to fix an unbounded supervisor-side retention
leak) supplied the removal. Each is correct alone.

Every reported observation follows:

| observation | mechanism |
|---|---|
| consumed and ACKed, never applied | the **worker** ACKs; the **supervisor** drops the frame afterwards — different process |
| nothing errors | a `WARN` in a host process the guest cannot see |
| per-resource, not a display-path failure | only released ids are affected |
| only a genuinely new resource heals a participant | a new resource publishes a new surface into the map — the only thing that repopulates it |
| creating new resources can knock another out | releases follow the guest's unref pattern; re-creating one participant's resources unrefs others |
| fullscreen toggle at identical size stayed wedged, resize healed | SDL reusing its swapchain publishes nothing new; a resize forces new buffers |

## Still inferred, not observed

That a `SurfaceMsg::Released` is what removed 139/141. **There is no logging at all on that path** —
`note_released` is silent — which is exactly why this has been invisible from both sides. Confirming
it is a one-line instrument and should happen before anyone writes a fix.

Unexplained and on the guest's side of the line: what changes at ~11:56:38 to make synoik unref
buffers it goes on presenting. The bug is ours regardless — we must not drop a surface the guest can
still name — but the trigger is theirs.

## Evidence

`evidence/` holds a decimated host-log slice (every 40th routine line, every `unresolved`), the
per-minute skip and scanout rates, and the distinct IOSurface ids. The full 262k-line worker log is
not committed; regenerate with the boot command above.
