# venus_replay wedge 2026-07-12: zink unflushed-batch wait deadlock (guest-side)

**ROOT-CAUSED AND EMPIRICALLY CONFIRMED — a classic lost-wakeup bug in zink,
UNFIXED UPSTREAM (mesa main as of 2026-07-12 has the identical code).**

The short version: `zink_batch_usage_unflushed_wait()`'s multi-context branch checks
`u->unflushed` OUTSIDE `u->mtx`, then locks and `cnd_wait`s with **no predicate
re-check and no wait loop** (zink_batch.c:1205-1216). The signal side (`submit_queue`)
clears `u->unflushed` (line 657) and `cnd_broadcast`s (line 846) **without holding
`u->mtx`**. If the flush thread clears+broadcasts in the window between the waiter's
check and its `cnd_wait`, the waiter sleeps forever on a signal that already fired.

Live confession (read out of `/proc/2046/mem` via the blocked futex uaddr in
`/proc/2046/syscall`): the waited-on `struct zink_batch_usage` at `0x5555a65c2df0`
had `usage=0x30dd`, `submit_count=0x1057`, and **`unflushed == 0`** — the batch was
long flushed while the waiter slept on `u->flush`.

Resurrection proof: installing gdb in the (disposable) test guest and hand-delivering
the missed signal — `call pthread_cond_broadcast((void*)0x5555a65c2df8)` from an idle
thread — woke eglretrace instantly; it completed the remaining ~7.5k trace calls,
wrote snapshots 43-47, and exited cleanly. One lost wakeup was the entire wedge.

Bonus bug found while reading: the `trywait` path passes `{0, 10000}` to
`cnd_timedwait`, which takes an ABSOLUTE timespec — epoch+10µs is always in the past,
so try-waits return immediately (busy-wait, not a hang).

Fix: `patches/mesa/0014-zink-fix-unflushed-batch-wait-lost-wakeup.diff` (predicate
loop under the mutex on the wait side; clear+broadcast under the mutex on the submit
side). Applies to BOTH mesa builds: the guest RPM (`build-mesa-rpm.sh`) and the host
zink-on-KK tree (`/Volumes/mesa-cs`, branch `limina/kosmickrisp`) — the host GL tier
runs this same code. Upstreaming candidate (obvious-fix bucket).

## Incident

`scripts/test-boot.sh` hung on `venus_replay::venus_replay_matches_llvmpipe_reference`
for ~100 minutes (normal: ~90 s). First observed occurrence — the same test was green on
recent runs (e.g. the 105261b fd-census validation).

- Suite started 08:05, seated guest booted 08:18, sshd banner 08:19.
- `eglretrace` ran at normal pace 08:19 → 08:20:20 (43 snapshots written, last
  `0000064003.png` at 08:20:20.88), then froze taking the NEXT snapshot.
- Guest otherwise healthy: gnome-shell rendering live on venus (scanout.png shows the
  overview with a ticking clock 100 min later), load average 0.01, sshd fine.
- Host worker idle (~4% avg CPU). gvproxy.log grew to 920 MB (debug packet log of the
  1.0 GB fixture upload + 100 min of keepalives) — the upload itself completed at 08:21.
- Around 08:20–08:21 packagekitd did a metadata refresh and gnome-shell popped a
  "Software Upgrade Available" notification — i.e. the shell was actively rendering on
  venus concurrently with the replay when it wedged (possible race ingredient: two
  venus clients).

## Where it's stuck (the decisive stacks)

Guest `mesa-dri-drivers-26.2.0-1.limina.fc43.aarch64`, `eglretrace` pid 2046,
all 10 threads parked in futex waits (kernel stacks all `futex_wait_queue`; userspace
via `eu-stack -p 2046`):

Main thread — mapping a snapshot readback, waiting for a batch flush:

```
#3  pthread_cond_wait
#4  cnd_wait
#5  zink_batch_usage_unflushed_wait
#6  zink_batch_usage_wait
#7  zink_image_map
#8  st_ReadPixels
#9  _mesa_ReadnPixelsARB
#10 GLDumper::getSnapshot(int, bool)
#11 retrace::takeSnapshot
#13 glretrace::frame_complete
#14 retrace_eglSwapBuffers
```

Every thread that could perform that flush is parked IDLE waiting for work:

- `eglretrac:zfq0` (2052), `eglretr:disk$0` (2054), `eglretrac:zcq0` (2055),
  `eglretra:zcfq0` (2056), `eglretra:gdrv0` (2062), `eglretra:zcfq1` (2063):
  all `cnd_wait ← util_queue_thread_func` (empty util_queue).
- `WSI swapchain q` (2060): `u_cnd_monotonic_timedwait ← x11_manage_present_queue`.
- `WSI swapchain e` (2061): `u_cnd_monotonic_wait ← x11_manage_event_queue`.
- `vn_wsi[0,0]` (2064): `cnd_wait ← vn_wsi_present_thread`.

## Reading

`zink_batch_usage_unflushed_wait` blocks on the batch-state condvar until the tracked
batch is flushed/submitted. Nobody holds a pending flush for it — the driver thread and
zink flush queues are all empty and idle. So the resource's `zink_batch_usage` records a
usage for a batch that will never be (or already was) flushed, and the signal was lost:
a stale-usage / lost-wakeup race in zink's unflushed-batch tracking, likely between the
threaded-context flush path and the map path.

Key scoping facts:

- This wait is PRE-venus: it never reaches the vn ring or the host. The host worker and
  virglrenderer are innocent (and gnome-shell's venus contexts kept rendering fine).
  It is NOT one of the fixed venus ring classes (0026–0032).
- Trigger is probabilistic (same test, same image was green before); concurrent shell
  rendering + snapshot readback every 100 calls is the aggravating workload.

## Next steps (when it bites again / if we chase it)

- Reproduce with `ZINK_DEBUG=flushsync` (if available) or with the threaded context
  disabled (`GALLIUM_THREAD=0`... zink uses `zink_flush_queue`; check the right knob)
  to see whether the race lives in the tc interplay.
- Check upstream mesa for `zink_batch_usage_unflushed_wait` hang reports/fixes newer
  than our 26.2.0 base; check whether our patches/mesa 0009/0010 (venus WSI) or 0012
  touch the flush path (unlikely — they're WSI/vn side).
- The L2 harness should get a per-ssh-step timeout so a wedged replay fails the test in
  minutes instead of hanging the suite (the ssh_poll steps have Durations; the replay
  exec apparently doesn't).

Raw capture (stacks, ls, journal excerpt) taken live during the incident is inlined
above; nothing else was extracted before teardown.
