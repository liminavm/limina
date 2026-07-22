# venus-ring-doorbell — wakeup-suppression handshake for the vkr ring

**Date:** 2026-07-22. **Status: SPIKE OPEN (de-risking).** Task #42. Lever 4 in
`docs/perf/overhead-inventory.md`.

## The question

The dominant remaining host-wakeup term under a *visible* 60 fps venus workload is the
**vkr_ring poll-sleeps: ~5,900/s** (clean-fullscreen blobs, virgl 0043 adaptive plateau —
see `spikes/wakeup-probe/RESULTS.md` §60fps re-baseline). The host ring thread sits on the
responsive 40 µs warm plateau because blobs feeds the ring in many small per-frame submits,
so `relax_iter` keeps resetting and the ring never falls to the deep-idle backoff. Those are
cheap nanosleep-class wakeups (not CPU), but they are the single biggest chunk left of the
8.1k/s budget (was 18.6k before round 2).

**Can we eliminate the poll-sleeps with a doorbell handshake WITHOUT a net loss?**

The naive form — "host blocks on notify, guest kicks every submit" — is a KNOWN net loss:
it trades cheap host nanosleep wakeups for **guest vmexits** (each `vkNotifyRingMESA` is an
MMIO trap host-ward), and mesa deliberately does *not* notify while actively submitting
because it assumes the host polls. The poll window also catches ~440 resumes/s that would
each otherwise need a doorbell.

The winning form (hypothesis) is the **EVENT_IDX pattern** — already shipped for the
*virtio-gpu queue* in libkrun 0091, but the vkr ring is guest **shared memory**, not a virtio
queue, so it does NOT get EVENT_IDX for free. Adapted:

- Host publishes **"parked at ring position X"** into a ring control word *before* it blocks.
- Guest kicks (notify → vmexit) **ONLY** when it advances the ring past a parked host.
- Whenever the host keeps up by polling (never parks), the guest stays **silent** — no vmexit.
- Close the lost-wakeup race with the standard publish-then-recheck barrier (write parked
  position, memory barrier, re-read the ring tail; if the guest already advanced, don't park).

Eliminates the poll-sleeps without a notify-per-submit.

## Why this is a spike, not a straight build

Venus ring wakeup protocols are limina's **worst historical bug class**:
`limina-zink-lost-wakeup` (unflushed-wait lost-wakeup deadlock, a 100-min wedge) and the whole
`limina-venus-ring-poison` saga both lived exactly here. This is a **mesa + virglrenderer
co-design** across two of our forks in the hot path. So the spike is timeboxed to:

1. **Prototype** the handshake in the virgl (host) + mesa (guest) forks behind a flag.
2. **Measure both axes** — it must cut wakeups AND not regress throughput:
   - wakeups: `LIMINA_WAKE_TRACE` decomposition + `procwake` under clean-fullscreen blobs
     (target: poll_sleeps 5.9k → near-zero; total 8.1k → ~2-3k; guest vmexit/notify rate must
     NOT balloon — watch the guest-side notify count).
   - throughput: `spikes/wakeup-probe/ab-vkmark.sh` (venus, submit-latency-bound — the most
     sensitive probe; must stay ≈ the 0043 ~2360 Score, not regress toward the block-on-notify
     cliff).
3. **Assess the lost-wakeup correctness surface** — enumerate every park↔advance interleaving
   and show the recheck-after-publish barrier closes each. Also: does it stay correct when the
   guest is stock mesa (no handshake support) → must fall back to today's poll cleanly
   (two-tier guarantee).
4. **Gate ship on the numbers**, same discipline as the #35 gate. Drop or defer if the trade
   isn't clearly positive.

## Method / oracles

- Wakeups: `spikes/wakeup-probe/procwake <worker-pid> 5 5`; `LIMINA_WAKE_TRACE=1` (worker log
  `[WAKETRACE vkr_ring]` poll_sleeps/poll_resumes/parks, `[WAKETRACE gpu_worker]`,
  `[WAKETRACE fence]`). **Clean fullscreen only** (kiosk; overview composites +2.7k artifact).
- Throughput: `spikes/wakeup-probe/ab-vkmark.sh <label>` (fresh clone → venus boot → idle
  sample → vkmark ×N with per-run wakeup sampling → teardown).
- Guest vmexit/notify rate: NEW — need a counter on the guest notify call site AND/OR host-side
  count of received doorbells while parked, to prove we didn't just move the cost guest-ward.
- Correctness: reason through interleavings; stress with vkmark (tight ping-pong, worst case for
  the park race) + niri vulkan suite (the fd-census workload) for a soak.

## Code map (verified against live source, 3 independent reads)

Host ring = `third_party/virglrenderer/src/venus/vkr_ring.{c,h}`. Guest ring = the venus
Vulkan driver `vn_ring.{c,h}` (checkout under `spikes/venus-261-source/virtio-vulkan/`).
libkrun EVENT_IDX = `third_party/libkrun/src/devices/src/virtio/queue.rs` (+ gpu, patch 0091).

**Shared-memory ring control region** (guest+host both map it; NOT a virtqueue) —
`vkr_ring.h:38-49` / `vn_ring.c:254-260`: three cache-line-isolated 32-bit atomics —
`head` (host-write, consume cursor), `tail` (guest-write, produce cursor), `status`
(host-write bitmask) — plus the power-of-two `buffer`. Status bits (`vn_protocol_renderer_defines.h:488-493`):
`IDLE=0x1`, `FATAL=0x2`, `ALIVE=0x4`. **The only host→guest published word is the binary
`status`; there is no position/threshold field.**

**Host park path** (`vkr_ring.c:458-497`): when `vkr_ring_now() >= last_submit + idle_timeout`,
set `IDLE`, **seq_cst reload tail**, and if `buffer.cur == tail` (nothing new) `cnd_wait`
**indefinitely (0 host wakeups)**. Woken only by `vkr_ring_notify` (`vkr_ring.c:584`:
`pending_notify=true; cnd_signal`). Otherwise (ring being fed) it polls via `vkr_ring_relax`
(`vkr_ring.c:268-317`): 16 `thrd_yield` spins, then the **40 µs warm plateau (16 rungs)**,
then **640 µs deep-idle**; one sleep per rung; `relax_iter` resets on every processed command.

**Host drains in bulk** (`vkr_ring.c:499-519`): one `vkr_ring_read_buffer(cmd_size = tail - cur)`
per wake consumes *everything* pending. So a single notify already coalesces all queued commands.

**Guest notify path** (`vn_ring.c:439-483`, `vn_ring_submit_internal`): write buffer → advance
`cur` → `vn_ring_store_tail` (**seq_cst**) → `vn_ring_load_status` (**seq_cst**). Emit a doorbell
**iff `status & IDLE` AND `os_time_timeout(last_notify, next_notify=last_notify+1ms, now)`** — the
1-per-ms rate-limit. While `IDLE` is clear (host polling) the guest is **silent**. The doorbell
itself (`vn_ring_submit_locked`, `vn_ring.c:620-658`) is a standalone tiny
`vkNotifyRingMESA` → `vn_renderer_submit_simple` → `DRM_IOCTL_VIRTGPU_EXECBUFFER`
(`vn_renderer_virtgpu.c:535`) = **an extra host-ward vmexit**, NOT piggybacked on frame work.

**The 1 ms is shared** — `VN_RING_IDLE_TIMEOUT_NS` (`vn_ring.c:18`) is BOTH the guest notify
rate-limit AND (via `VkRingCreateInfoMESA.idleTimeout`, `vn_ring.c:356` → `vkr_transport.c:213`
→ host `ring->idle_timeout`, `vkr_ring.c:458`) the host park threshold.

**Race close** — identical to libkrun EVENT_IDX (`queue.rs:640-690`, publish-then-`fence(SeqCst)`-then-recheck):
here the host stores `IDLE` then seq_cst-loads tail; the guest seq_cst-stores tail then loads
`status`; SC order guarantees at least one of {guest sees IDLE → notifies, host sees new tail →
doesn't park}. Already airtight; `pending_notify` is the predicate.

## Analysis — the spike's premise is FALSIFIED

1. **The wakeup-suppression handshake already exists and is already correct.** The
   IDLE-bit + seq_cst SB-litmus IS the EVENT_IDX pattern (host publishes "I'm parked", guest
   kicks only then, race closed by the SC fence). Lever 4's premise — "build an EVENT_IDX
   handshake for the vkr ring" — was written from the general EVENT_IDX concept without
   re-reading this code. Verify-premises-before-deep-dive paid off again.

2. **Full EVENT_IDX (a position-threshold field) buys nothing here.** Its only advantage over
   a binary flag is coalescing many per-descriptor kicks into one by firing only when the
   producer crosses the threshold. The host **already bulk-drains the entire buffer per wake**,
   so one notify already coalesces everything — there is no per-descriptor over-notification to
   suppress. Adding a host-published index (new control-region word + mesa/virgl protocol
   plumbing) would add complexity for zero suppression gain.

3. **The 5.9k poll-sleeps are intrinsic to a shared-memory ring under active sub-idle-timeout
   feeding.** Normal Vulkan submits are shared-memory-only (no ioctl/doorbell — the ring's whole
   purpose). The host can only learn of them by polling tail OR by parking + waiting for a
   notify. There is **no cheap middle**. The poll window deliberately covers sub-`idle_timeout`
   micro-gaps so the host doesn't pay park+doorbell+latency on every gap. blobs feeds many
   sub-1ms micro-gaps/frame → the ring never reaches the 1ms park threshold → it polls the whole
   time on the 40 µs plateau → ~5,900 nanosleeps/s.

4. **You cannot "just park sooner."** Lowering `idle_timeout` below the guest's notify
   rate-limit (both 1ms, deliberately coupled) creates a **stall**: the host parks, but the
   guest's next submit can be rate-limit-suppressed (it notified <1ms ago) → host sleeps up to
   1ms with pending work → frame hitch. To park sooner you must ALSO shorten the guest
   rate-limit — a coordinated mesa+virgl change that *increases* guest vmexits. Converting the
   ~5,900 cheap host nanosleeps into park/doorbell cycles trades them for guest vmexits (each an
   execbuffer trap, far costlier than a nanosleep that immediately re-sleeps) **plus** injected
   park-wake latency on every micro-gap. This is the same net-loss the 0b′ work already rejected
   ("the poll window is USEFUL … don't replace it with immediate parking", `wakeup-probe/RESULTS.md`).

5. **The only real levers are latency-trading tuning knobs = M13.** Reducing poll-sleeps means
   either a **deeper relax plateau** (longer sleeps, more pickup latency — the plateau retune) or
   a **shorter `idle_timeout`+rate-limit** (park sooner, more vmexits + park-wake latency). Both
   trade latency for wakeups, and both are exactly what M13's `(visible, power)` policy is meant
   to set (focused+AC → responsive/poll; occluded/battery/60fps-capped → deep backoff/park). The
   doorbell-handshake and the plateau retune are **literally the same lever**, as suspected.

## Verdict

**DROP the doorbell-handshake as a separate mechanism.** There is nothing to build: the
handshake exists, is race-free, and is already optimal for the ring's bulk-drain structure; a
position-threshold refinement is pure cost. The 5.9k poll-sleeps are a deliberate,
correct pre-park poll window whose only reduction levers are latency-trading tuning
(plateau depth + `idle_timeout`), which **fold into the M13 `(visible, power)` knob** already
on the roadmap (M13 task 4) — not a new spike, not a new protocol.

Same discipline as the #35 gate: the spike's value is preventing a redundant mechanism build.

**Optional empirical nail (not run):** clamp `idle_timeout` host-side in `vkr_ring_create`
(ignore the guest's 1ms, use e.g. 100 µs) and measure — expected to surface the stall (§4) as
frame hitches and/or no net wakeup win. Reasoned + backed by the 0b′ early-park rejection;
not run to avoid a build/boot/eyeball cycle re-confirming a known result. Left here as the
decisive experiment if the conclusion is ever doubted.
