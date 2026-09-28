# Seated venus replay stall: a lost ring doorbell

**Cause: the host venus ring parked inside the guest's notify throttle window.** Fixed in virglrs
`c3f6e4b` (`venus: keep a ring up after a doorbell that found it awake`), host-side, for every guest
Mesa.

## The stall, as read off two held guests

`venus_shell_replay_matches_llvmpipe_reference` ran into its 300 s deadline with the renderer
refusing nothing. `loop.sh` (three lanes of the `venus_replay` binary, the failing VM held by
`LIMINA_TEST_HOLD_ON_FAIL`) reproduced it twice in seven iterations, measured 2026-09-27. Both
held guests showed the same thing:

- `eglretrace`'s main thread in `vn_ring_wait_seqno` ← `vn_call_vkGetSemaphoreCounterValue` ←
  `vn_WaitSemaphores` ← `zink_image_map` ← `st_ReadPixels`, the snapshot readback at a frame
  boundary. Polling, not blocked: `clock_nanosleep` in `vn_relax`.
- The worker's venus ring threads in `park_if_quiet` (`ring_thread.rs:809`, via a dSYM of the
  running binary), waiting for `notified`. The gpu worker idle in `kevent`: nothing queued.
- The ring's shared words (`ringpeek.py`, below): tail equal to the guest's own `ring->cur`,
  head 68 bytes behind it, status `IDLE|ALIVE`. The guest's `last_notify` stamped at the moment the
  wait began (658 s and 449 s before the read).

So a command sat in a parked ring and the guest never rang for it.

## Why the guest did not ring

Mesa rings only when it reads IDLE after storing the tail, and then not again for one idle timeout
(`VN_RING_IDLE_TIMEOUT_NS`, 1 ms) after ringing (`vn_ring_submit_internal`, `next_notify`). That is
safe only if the host cannot park again within one idle timeout of the doorbell. The host broke it:

1. `park_if_quiet` sets IDLE, and its tail check finds the write the guest just made, so it never
   sleeps; the ring runs the write and its idle clock starts.
2. The guest had read that IDLE, stamps `last_notify` (after the host already started its clock)
   and rings.
3. The doorbell arrives while the ring is awake and sets `notified`; the next `park_if_quiet` cleared
   it on entry and parked one idle timeout after the write, before the guest's window closed.
4. The next write (`vn_WaitSemaphores` polls on a ~1 ms cadence) reads IDLE inside that window, is not
   allowed to ring, and the ring sleeps with it unread.

The fix: a doorbell already rung on entry to `park_if_quiet` is activity, so the ring consumes it and
does not park; the idle clock restarts after the doorbell, which is after the guest's stamp. The
virglrs ring-thread test `a_doorbell_rung_while_awake_holds_the_ring_through_the_guests_throttle`
stages the interleaving with a 200 ms idle timeout. The C renderer's `vkr_ring` clears
`pending_notify` the same way.

## Tools here

- `loop.sh` — the reproduction vehicle: runs `venus_replay` in parallel lanes until a replay fails,
  holds that VM, and stops the rest. The forensics each failure writes (`Guest::forensics`) are what
  named the two stacks above.
- `ringpeek.py` — reads a guest venus ring's words from inside the guest: the `vn_ring` struct from
  `/proc/<pid>/mem`, and the shared head/tail/status through the ring's GEM object, mapped from a
  `pidfd_getfd` copy of the process's render-node fd (the process's own mapping is a PFN map
  `/proc/<pid>/mem` cannot read). `sudo python3 ringpeek.py <pid> <vn_ring address>`; the address is
  the ring id, which the worker names its ring threads after (`virglrs-ring-<id>`, in decimal).
