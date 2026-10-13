# Where a transport frame waits

**Question:** why glmark2-wayland ran ~45% behind venus on the native-context transport while
vkmark did not and the aquarium won.

**Answer:** every frame blocked the client on the CPU until the GPU had finished it. There were
two independent causes, each about a full Metal round trip (~200 µs):

1. **Ours, fixed:** a sync-file export of a signal pushed behind a queue wait fell back to
   `vk_sync_wait(INFINITE)`. WSI's present submit is exactly that: the render semaphore's wait,
   then a signal, with no command buffer. Fixed in the guest KK (`dce6d5542a8`, `metal-native-context`
   branch of the mesa fork) by pushing an empty commit behind such a signal and tying the signal
   to its seqno.
2. **Stock zink's, not ours:** zink's `implicit_sync` workaround makes kopper submit the present's
   wait and `vkWaitForFences` on it before presenting, for every driver not on its list. Upstream
   took KosmicKrisp off the workaround in `e9a87f80b2d` ("zink: do not use implicit sync on
   kosmickrisp", 2026-08-27, `main` only). Fedora's 26.1.8 and our enhanced-tier 26.2.3 both
   predate it. venus has always been exempt, which is the whole of the gap that remained.

Measured 2026-10-13: `metal.test` clone, 1280x800, gnome-shell and clients all on zink over the
arm's stack, a fresh boot per arm; glmark2-es2-wayland `build`, 512x512. "New zink" is the mesa
fork at `dce6d5542a8`, which carries `e9a87f80b2d`, built as a prefix in the guest.

| | venus | transport |
|---|---|---|
| stock zink 26.1.8, before the export fix | 2340 2356 2442 | 1312 1307 1293 |
| stock zink 26.1.8 | 2411 2375 2423 | 1757 1771 1785 |
| new zink | 2366 2266 2261 | 3377 3346 3338 |
| new zink, its per-present wait forced back on (A/B toggle) | — | 1749 1790 |

The new zink is neutral on venus, so its row compares like with like: +45%.

Checked by eye, from the scanout capture: glmark2 frames at 3000+ fps composited at 60 Hz show
no seams or partial buffers; an X11 glmark2-es2 through Xwayland (itself on the transport) renders
cleanly and logs nothing; Firefox's aquarium renders at 60 fps.

The export fix also moved headless vk-replay from 1569 to 2110 fps (venus ~1820) and vkmark cube
from 2684 to 4922.

## The anatomy of one commit

From the GPU host's stage timings, per commit, all contexts:

| stage | µs |
|---|---|
| `commit` → GPU starts | ~73 |
| GPU executes | ~34 |
| GPU ends → feedback handler | ~72 |
| signal encoded → shared-event listener raises the guest's mirror | ~195 |
| doorbell → first command after a park | ~1 |

So a CPU wait on a just-submitted frame costs ~200 µs no matter how small the frame, and almost
all of it is Metal's submission and notification latency, not the transport. The rule that follows:
**the transport must never make a client wait on its newest frame**; waits on older frames are
pipelined away. The guest's own polling adds ~50 µs a wait on top (its backoff sleeps in 20 µs
`nanosleep`s, which a guest vCPU pays as timer exits); spinning instead buys +14% while the
per-frame waits exist and nothing once they are gone.

## Instruments

Apply to the trees named, rebuild, and turn on with the env var. Each prints one line a second.

- `guest-kk-wait-stats.patch` (mesa fork, `metal-native-context`, on `dce6d5542a8`):
  `LIMINA_LMN_STATS=1` prints `[LIMINA-LMN-STATS]` from the client process. It shows commits,
  CPU fence waits with the backoff tier that caught them, commit→completion latency against GPU
  time, sync-file exports and their CPU-wait fallbacks, imports, and synchronous calls per op.
  `LIMINA_LMN_SPIN=1` makes the backoff never sleep.
- `gpu-host-stage-stats.patch` (limina, `crates/limina-gpu-host`): `LIMINA_GPU_HOST_STATS=1` in
  the worker's environment prints `[GPU-HOST-STATS]` into the worker log with the stage table above.
- `zink-implicit-sync-toggle.patch` (mesa fork): `LIMINA_ZINK_IMPLICIT_SYNC=1` forces zink's
  per-present CPU wait back on, for an A/B within one build.

Which wait blocks: gdb in the guest with a breakpoint just past the "already signalled" return of
`mtl_shared_event_wait_until_signaled_value` (a breakpoint at entry also counts the waits that
return at once, four a frame here).
