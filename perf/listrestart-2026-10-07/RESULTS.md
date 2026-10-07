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

The unroll arm also grows the worker without bound: on the 16 GB remote Mac it reached 18.5 GB
resident with a 4 GiB guest and hard-hung the host (WindowServer watchdog, forced reboot).
**Do not run the unroll arm on a shared host**, and cap any rerun with a timeout and an RSS watchdog.

## Reading it

- WebGL 2 always has primitive restart enabled, and zink forwards that as `primitiveRestartEnable`
  on every indexed draw. Every indexed triangle-list draw in the scene therefore takes the GPU
  unroll, whether or not its indices contain a restart index.
- So a conformant default needs the unroll to cost almost nothing on draws that have no restart
  index. Turning on the existing path is not that.
