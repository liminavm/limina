# List-restart unroll: aquarium A/B

**Question.** KosmicKrisp skips the primitive-restart unroll for list topologies by default
(`requires_unroll_restart` in `kk_cmd_draw.c`; `LIMINA_KK_NOLISTRESTART=0` turns the skip off). The
skip is not conformant: list draws that contain a restart index misdraw. Does the conformant unroll
cost anything on a real WebGL workload?

**Answer: yes, about 40x.** With the unroll on, aquarium drops from ~38-42 fps to **1 fps** at both
fish counts, on every run of both unroll points. The frames are correct, only slow: the worker shows
global-ring fences 2 s old and still unsignalled. The skip cannot simply be turned off.

With the unrolls run ahead of the pass and batched per pass (branch `limina-kk-pregfx`, below), the
unroll arm reaches 22-26 fps against the skip arm's 35-46: still a cost, from unrolling ~24k draws
a frame, but no longer a cliff.

**Unrolls avoided at the source, the conformant arm runs at skip speed.** With host zink leaving
list restart out of the supported modes on KK and the GL frontend dropping restart from draws whose
indices hold no restart index (below), the unroll arm reaches 36-47 fps against the skip arm's
35-44, at the same memory.

## Setup

- Remote Mac: M1 Mac mini, 16 GB, macOS 26.6.2, with its own VM suspended and nothing else running.
- One packaged `Limina.app`, identical for every point (CDHash `ccfb06cd3f81badc5f6705255566ab7eb2158e5f`),
  built at limina `ebefed85`. Its bundled KosmicKrisp is limina-kk `152321a1876`.
- Guest: the F44 enhanced image, a fresh APFS clone per point. 4 vCPU, 4 GiB, display pinned to
  1280x800@60.
- Workload: aquarium on the vrend tier (Firefox kiosk). Each point measured 25000 and 30000 fish,
  twice (r1, r2), after the guest settled (load < 0.3, uptime ≥ 200 s).
- Points alternated skip / unroll / skip / unroll / skip.
- Each point proves its arm from the worker's environment (`arm-env.txt`, via `ps -E` on the worker).
- Driver: `point-remote.sh`. `point.sh` is the same procedure for this host.

## Results (fps, read from the in-page counter)

| point | arm    | r1 25k | r1 30k | r2 25k | r2 30k |
|-------|--------|--------|--------|--------|--------|
| s0    | skip   | 42     | 39     | 42     | 38     |
| u0    | unroll | —      | 1      | 1      | 1      |
| s1    | skip   | —      | 38     | 42     | 38     |
| u1    | unroll | —      | 1      | 1      | 1      |
| s2    | skip   | —      | 39     | 42     | 38     |

— : the counter is under Firefox's first-run banner on the first capture of a boot. The full
frames, which show the counter where the crop does not, are gitignored.

The skip arm is stable across its three points to ±1 fps, so the unroll arm's 1 fps is not drift.

## Where the cost goes

KK's encoder guard logs a running total of encoders (`[LIMINA-KK-GUARD] ... encoders=`). Its rate
during aquarium at 30000 fish, divided by the measured fps:

| arm    | encoders/s | fps | encoders per frame |
|--------|-----------:|----:|-------------------:|
| skip   | ~330       | ~40 | ~8                 |
| unroll | ~5000      | 1   | ~5000              |

So every unrolled draw ends the render encoder, runs its compute unroll, and opens a new render
encoder that reloads the attachments (the TODO above the draw loop in `kk_cmd_draw.c`: "Remove this
once unroll, tess and any compute does not split render pass"). The unroll kernel itself is one
parallel 1024-thread workgroup per draw.

The unroll arm also costs memory, as transient churn rather than a leak. Point `kw` sampled the
worker once a second (`rss-watch.sh`, `evidence/kw/rss-kw.tsv`, `evidence/kw/comp-guard.log`):

- The worker holds a steady 4 GB until aquarium starts. Within 6 s of the launch it gains ~2 GB.
- From then on its RSS swings by 1.5-2 GB every few seconds, while host free memory jumps between
  60 MB and 3.5 GB: ~2 GB of per-frame allocations, released as frames retire.
- Its footprint (`top`'s MEM, compressed pages included) peaks at 11 GB and holds at 7.5-10 GB for
  the rest of the run, without trending up. Only a few frames are in flight at once.

So the memory follows the pass splits: ~5000 encoders a frame, each holding its allocations until
the GPU retires it.

The margin is thin, though. One earlier unroll point on the 16 GB remote Mac reached 18.5 GB
resident and hard-hung it (WindowServer watchdog, forced reboot). **Do not run the unroll arm on a
shared host.** Cap any rerun with `rss-watch.sh` (driven by `run-points.sh`, which will not start a
point without its watcher and stops the series when one fires). It kills the VM on the worker's
footprint or the host compressor's growth. RSS alone falls as the worker's pages move into the
compressor.

## With a pre-graphics stream and batched unrolls

KK branch `limina-kk-pregfx` makes two changes, measured one after the other:

1. **Pre-graphics stream** (KK `da9c3527240`): draw-time compute, the unroll included, is recorded
   into a command buffer committed just before the render pass, instead of ending the pass.
2. **Batched unrolls** (KK `0f17f9cc1a0`): a pass's unrolls are queued and run as one dispatch, a
   workgroup per draw, when the pass closes, instead of one dispatch per draw.
   `LIMINA_KK_NO_UNROLL_BATCH=1` restores one dispatch per draw.

| point | build             | arm    | r1 25k | r1 30k | r2 25k | r2 30k |
|-------|-------------------|--------|--------|--------|--------|--------|
| ps0   | pre-graphics      | skip   | 43     | 35     | 39     | 38     |
| pu0   | pre-graphics      | unroll | 4      | 2      | ·      | ·      |
| bs0   | + batched unrolls | skip   | 45     | 40     | 46     | 37     |
| bu0   | + batched unrolls | unroll | 26     | 22     | 26     | 22     |
| bs1   | + batched unrolls | skip   | 41     | 38     | 41     | 37     |
| bs2   | + batched unrolls | skip   | 40     | 37     | 39     | 35     |
| bd0   | + batched, counters (`3bcede94624`) | unroll | 26 | 22 | 26 | 22 |

· : not measured. pu0's watcher stopped it after r1 on a fixed compressor cap; the remote Mac held
~5.4 GB compressed at rest, left by earlier killed workers, so pu0 ran with ~100 MB free and its
numbers are a lower bound. A reboot cleared that before the batched points. A sixth point (bu1) was
killed during settle by bs0's watcher, which had followed each later point's worker until its own
time limit; `rss-watch.sh` now ends with the supervisor it first saw.

- **Correctness holds** on both builds:
  - piglit's two list-restart unroll tests pass;
  - the tessellation list shows no new failures;
  - the virglrs direct and indirect restart tests draw the right pixels, with one and with three
    restart draws in a pass.
- **The pre-graphics stream alone removes the splits but not the cost.** The unroll point ends with
  under 5k compute encoders where the split path had 770k, yet it reaches only 4 fps. Every unroll
  is still its own dispatch, bracketed by two dispatch-to-dispatch barriers, so they run one after
  another.
- **Batching takes the unroll arm from 4 to 22-26 fps**, about 60% of the skip arm. bd0's counters
  show no remaining splits and no seals (in-pass barriers that would push unrolls back to
  splitting). Over a 30 s window there are ~24k pre-graphics streams, each flushing one batch, and
  ~17M unrolls: **about 24k unrolled draws a frame, one per fish, in ~33 batches.**
- **What is left is the unroll work itself.** Each of those draws still costs a 1024-thread
  workgroup, a heap allocation and an indirect draw, where the skip arm draws straight from the
  application's index buffer.
- **Memory**: the worker's footprint steps from 4.8 to ~7 GB when aquarium starts and holds there.
  The host compressor grew ~120 MB. The split path held 7.5-10 GB.
- Captures are copied while the VM rewrites them once a second, so a copy can be truncated. A crop
  of one shows garbage below the counter (bu0 r2 30k); the counter line above it is intact.

## With needless unrolls avoided

Two Mesa commits on branch `limina-kk-restartscan` (off `limina-kk` `88b1341efe8`), with KK's
unroll path unchanged:

1. **GL frontend** (`79c39a54c88`, an upstream candidate): the index min/max cache records whether
   a scan saw the restart index, and before drawing a topology the driver cannot restart, restart is
   dropped from draws whose ranges hold none. A static index buffer is scanned once.
2. **zink on KK** (`6295aedf934`, limina-only): list topologies are left out of
   `supported_prim_modes_with_restart`, so the frontend check runs on them. Only draws that really
   restart reach emulation.

The `rr` points run that host Mesa, bundled, with `LIMINA_KK_NOLISTRESTART=0`. On this stack the
variable has no effect on vrend's draws: zink no longer hands KK a list restart, so KK's skip and its
unroll are both bypassed. The `os` points run the shipping bundle with the skip, interleaved with them.

| point | build                 | arm    | r1 25k | r1 30k | r2 25k | r2 30k | peak footprint |
|-------|-----------------------|--------|--------|--------|--------|--------|----------------|
| os0   | shipping              | skip   | 43     | 38     | 44     | 40     | 6655 MiB       |
| os1   | shipping              | skip   | 41     | 38     | 43     | 37     | 6649 MiB       |
| os2   | shipping              | skip   | 42     | 35     | 40     | 38     | 6676 MiB       |
| rr0   | restart scan          | unroll | 47     | 36     | 41     | 39     | 6650 MiB       |
| os3   | shipping              | skip   | 41     | 40     | 42     | 38     | 6683 MiB       |
| rr1   | restart scan          | unroll | 42     | 37     | 42     | 39     | 6677 MiB       |

- **The two arms are indistinguishable**, in fps and in memory, as expected if aquarium's draws hold
  no restart index and none reaches the unroll (this build carries no counters to show it).
- **Correctness**: `spikes/list-restart-probe` draws a triangle list with a restart index mid-list
  on the host GL stack vrend uses. This stack draws it correctly; the shipping stack's skip draws the
  wrong triangle. piglit's primitive-restart and provoking-vertex list (245 tests) gives the same
  results under the guest's virgl driver on both stacks: its list-restart tests do not tell the skip
  from conformant restart.
- **Venus is untouched.** Both commits are in host GL; a venus guest's draws reach KK without them,
  so on that tier list restart still takes KK's skip, or its unroll with the skip off.
- The first two restart-scan points (dropped) never booted: one dylib in that bundle kept an
  ad-hoc signature, which dyld refuses next to the team-signed binaries.

## Reading it

- WebGL 2 always has primitive restart enabled, and zink forwards that as `primitiveRestartEnable`
  on every indexed draw. Every indexed triangle-list draw in the scene therefore takes the GPU
  unroll, whether or not its indices contain a restart index.
- So a conformant default needs the unroll to cost almost nothing on draws that have no restart
  index. Turning on the existing path is not that.
