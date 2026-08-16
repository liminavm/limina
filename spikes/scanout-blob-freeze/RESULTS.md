# The "SET_SCANOUT_BLOB ACKed but never applied" freeze — diagnosed 2026-08-16

The guest compositor (synoik) reported a display freeze in which it keeps rendering correct frames
at 60 Hz, every `SET_SCANOUT_BLOB` is consumed and ACKed `RESP_OK_NODATA`, nothing errors, and the
screen holds a stale frame. Only a **genuinely new** resource restores a participant, and creating
new resources can knock another participant out. Their writeup:
`limina-issue-scanout-blob-not-applied.md` in the synoik tree.

**It is ours, and it is in the supervisor — not in the worker, not in virglrenderer, not in the
guest.** That is why five successive theories (four theirs, one mine) all fitted the evidence and
all died: every instrument either side had was pointed at a healthy component.

**ANSWER (run 2, instrumented): the supervisor's surface store EVICTS the compositor's scanout
buffers at its 32-entry cap while they are still being presented.** Jump to §"CONFIRMED" — the
sections before it are the first run's reasoning, which reached the right subsystem and the wrong
mechanism, and are kept for the two hypotheses they kill.

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
- **Surface-store eviction.** ~~`SurfaceStore` is capped at 32 with FIFO eviction… only **20**
  distinct IOSurface ids exist in the whole session. Not eviction.~~ **WRONG, and this was the
  answer.** The count came from `[LIMINA-VKR-MTLTEX]` lines, which cover only venus *scanout
  imports*; the store receives every published surface, and run 2 published 41. A ruling-out is
  only as good as the population it counted.

## The mechanism, as reasoned in run 1 — SUPERSEDED by §CONFIRMED

Right subsystem, right failure shape, wrong exit path: the store does drop surfaces the guest still
presents, but by eviction rather than by release. The release hazard below is nonetheless **real
and latent** — a guest that presents a released id fails identically — so this stays on the record.

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

## CONFIRMED 2026-08-16, run 2: it is EVICTION, not release

The instrument (`299cdad`) makes the failed resolve name its own cause. Second reproduction, same
image, same workload — and the answer is unambiguous:

```
window: surface 142 unresolved; skipping frame — the store EVICTED this surface at its cap
and the guest is still presenting it. Permanent for this id: nothing re-publishes it.
```

**833 skipped frames, every one an eviction. Zero of them a release.** The inferred step in the
section below was wrong, and the guest session is who forced the check: they showed nothing in
their compositor can release those buffers mid-session, which is why the instrument was widened to
cover both exits instead of only the one I expected.

The numbers:

| | |
|---|---|
| surfaces published this session | 103 publishes, **41 distinct** |
| `SURFACE_STORE_CAP` | **32** |
| evictions | 43 |
| releases | 19 |
| scanout-bound surfaces | IOSurface **142** (`res 7`), **145** (`res 12`) |
| ids that went unresolved | **142 and 145** — exactly the scanout pair |

**The policy is the bug, not the cap.** `SurfaceStore` evicts FIFO on *insertion order*
(`order.push_back` at insert, `pop_front` at eviction). A compositor publishes its permanent
scanout ring **first**, at startup, and then churns transient client buffers — so the oldest-inserted
surfaces are precisely the ones in continuous use, and the churn pushes them out. Eviction is
ordered by age when the thing that matters is use.

This also explains the trigger, which nothing had pinned down: it is **surface publication volume,
not time under scan-out**. Run 2 reproduced in seconds rather than minutes — launch Firefox,
fullscreen it, hit Super — because that mints a burst of new surfaces and crosses the cap
immediately. The earlier correlation with "time under client pass-through scan-out" was
publication volume wearing a disguise.

My earlier "eviction is ruled out" was wrong on its own terms: it counted 20 distinct ids from
`[LIMINA-VKR-MTLTEX]` lines, which cover only **venus scanout imports**, while the store receives
**every** published surface. This run published 41.

## Superseded: the release hypothesis

Kept because the reasoning below is still correct about `note_released` — a guest that presents a
released id would fail identically, and that latent hazard is real even though it is not what fired
here.

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

## The fix, shipped 2026-08-16 — two shapes, verified in that order

**Shape 1 — never evict an id the guest is presenting.** `pin_presented(id)` runs on every frame
apply (unconditionally, *not* only on a cache miss: the frame cache in front means a hot id would
never touch the store and would look idle to any eviction policy — which is exactly how the
compositor's ring got evicted while in continuous use). Pinned ids are skipped by `evict_to_cap`,
bounded at `PINNED_CAP = 6` so a guest that mints a fresh id every frame cannot pin the store open
and re-create the retention leak the cap exists to stop.

Verified under the same workload: **0 unresolved frames against 833 before**, over a 30-minute
session with 365 evictions — the cap keeps doing its job, it just no longer aims at the ring.

**Shape 2 — ask for it back on a resolve miss.** The pin is a first line of defence, not a proof:
an id the guest stops presenting falls out of the LRU pin and can be evicted, and the store cannot
recover a non-global surface by itself. So a failed resolve now sends `resurface <id>` to the
worker, which looks the id up in the registry every publish already populates
(`virgl_renderer_republish_iosurface`) and publishes it again. Throttled to one request per id per
250 ms — the guest presents a missing id at 60 Hz.

**The answer must ride the surface Mach port, never the control socket.** IOSurface ids recycle
(39 distinct ids across 301 guest buffers in `spikes/venus-churn-retention/`), and the ordering
hazard that creates was closed by putting publishes and releases on one FIFO port. The *request*
may go out of band; the *answer* is an ordinary publish.

### Shape 2, verified end-to-end 2026-08-16

Shape 1 masks shape 2 by design — with the pin on, no workload evicts a presented surface, so
nothing exercises the recovery. `LIMINA_PIN_PRESENTED=0` disables the pin for exactly this reason
and is kept as a permanent test lever. Same image, same repro (launch Firefox → fullscreen →
Super), pin off:

```
13:02:08 DEBUG surface store: evicted surface 139 at cap 8 …
13:02:08 WARN  window: surface 139 unresolved; skipping frame — the store EVICTED this surface …
13:02:08 WARN  window: asking the worker to re-publish surface 139 …
13:02:08.928 DEBUG gpu: re-published surface 139 on request
13:02:08 DEBUG surface store: published surface 139 (33 held)
```

Three evictions of presented ids, three requests, three re-publishes, each round trip inside the
same second — and **one skipped frame per event** rather than a permanent freeze. The user, driving
the window, could not see the hitch at all.

**A side observation this run explains**, previously unexplained: the "held" counter appears to run
two interleaved sequences (`33 held` and `9 held` for the same id, one second apart). There are two
`SurfaceStore`s — the Mach store at `SURFACE_STORE_CAP = 32` and the main-thread frame-apply cache
at `FRAME_CACHE_CAP = 8`. Both log through the same lines. Not a bug; the eviction that produced
the miss above was the *cache's*, immediately after the store's.

## Still open

**The publish/release asymmetry**: 103 publishes against 19 releases in the run-2 session. Two
candidates, neither asserted: (a) benign — those surfaces really are still alive; (b) structural —
the guest creates two exportable images per buffer and vkr publishes per `IOSurfaceCreate` while a
release carries one id per resource. If (b), *any* cap is eventually reached, which changes how
generous 32 really is. Worth measuring before trusting the cap as a memory bound.

**The per-frame warn for a genuinely unrecoverable id** still fires at 60 Hz (a surface the worker
no longer has registered). Pre-existing; the request itself is throttled, the log line is not.
