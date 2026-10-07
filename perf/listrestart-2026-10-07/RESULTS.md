# List-restart unroll: aquarium A/B

**Question.** KosmicKrisp skips the primitive-restart unroll for list topologies by default
(`requires_unroll_restart` in `kk_cmd_draw.c`; `LIMINA_KK_NOLISTRESTART=0` turns the skip off). The
skip is not conformant: list draws that contain a restart index misdraw. Does the conformant unroll
cost anything on a real WebGL workload?

**Answer: yes, about 40x.** With the unroll on, aquarium drops from ~38-42 fps to **1 fps** at both
fish counts, on every run of both unroll points. The frames are correct, only slow: the worker shows
global-ring fences 2 s old and still unsignalled. The skip cannot simply be turned off.

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

## With a pre-graphics stream

KK branch `limina-kk-pregfx` records draw-time compute (the unroll included) into a command buffer
committed just before the render pass, instead of ending the pass for it. The same A/B with that
build bundled (app CDHash `0382908e6bcc73e98714a64ef897347b21c0f879`, KK `da9c3527240`):

| point | arm    | r1 25k | r1 30k | r2 25k | r2 30k |
|-------|--------|--------|--------|--------|--------|
| ps0   | skip   | 43     | 35     | 39     | 38     |
| pu0   | unroll | 4      | 2      | ·      | ·      |

· : not measured; the watcher stopped the point after r1.

- **The splits are gone.** The unroll point ends with 4844 encoders in total, where the split path
  had 770k. Correctness holds: piglit's two list-restart unroll tests pass, the tessellation list
  shows no new failures, and the virglrs direct and indirect restart tests draw the right pixels.
- **The per-draw dispatch is not gone.** The guard's `checks` counter (one per guarded compute
  operation, ~5 per unroll dispatch) reaches 5.8M, so the dispatch count is unchanged: about 5000
  single-workgroup unroll dispatches a frame, each bracketed by two dispatch-to-dispatch barriers,
  so they run one after another. That is the remaining 10x.
- **Memory**: the worker's footprint steps from 4.5 to 6.4 GB within seconds of aquarium starting
  and then holds, where the split path held 7.5-10 GB. Lower, not gone.
- The unroll numbers are a lower bound: the remote Mac had ~100 MB free during the point, and the
  watcher's once-a-second sampling stalled for 16 s. The watcher's compressor cap (a fixed 6 GB,
  against ~5.4 GB already compressed on that host at rest) is what stopped it.

Getting to the skip arm's speed needs the per-draw dispatches themselves to go: fewer barriers
between the independent unrolls (they allocate output with an atomic bump, and nothing in the
stream reads another's output), or one batched dispatch per pass.

## Reading it

- WebGL 2 always has primitive restart enabled, and zink forwards that as `primitiveRestartEnable`
  on every indexed draw. Every indexed triangle-list draw in the scene therefore takes the GPU
  unroll, whether or not its indices contain a restart index.
- So a conformant default needs the unroll to cost almost nothing on draws that have no restart
  index. Turning on the existing path is not that.
