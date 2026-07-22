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

## Design directions to evaluate later (not yet built)

- **Traffic-rate-driven poll↔trap switch (user idea, 2026-07-22).** Today the poll/park choice is
  keyed on idle *duration* (`idle_timeout` → park). A cleaner hysteresis: **default to trap
  (park + doorbell)** — cheap and correct when traffic is sparse — and **switch to polling only
  once the ring is being *flooded*** ("remove the turnstile"). Sparse desktops pay ~1 doorbell per
  wake; only a genuine flood (vkmark ping-pong, uncapped GL) pays the poll's timer wakeups, and gets
  the low pickup latency it actually needs. Composes directly with the flush-cadence finding below:
  if a frame's work coalesces toward ~1 flush/frame, that IS the "sparse" regime → traps handle it,
  poll-sleeps vanish. This is the same M13 `(visible,power)` machinery but keyed on measured submit
  RATE rather than occlusion/power alone — a third input to the selector. Evaluate after the
  flush-cadence probe (a low submit rate is the precondition that makes trap-by-default cheap).

## Flush-cadence probe (host-only instrumentation)

Probe #1: extend `LIMINA_WAKE_TRACE` with per-drain batch counters (`batches/s`, `batch_avg`,
`batch_max`) in `vkr_ring_thread`, to learn how many command-batches the host drains per frame and
their sizes — i.e. whether the ring stays warm because of many small spread-out flushes (coalescing
headroom → structural win) or a few large forced ones (no headroom → M13 tuning is the ceiling).
Instrumentation saved as `vkr-flush-cadence-probe.patch` (spike-only; third_party is gitignored).

### Result (2026-07-22) — clean-fullscreen blobs, F44 enhanced (7.1.4-limina16k, mesa 26.1.4-3, virgl 0043 + probe), 6 vCPU, present=60/s, user-confirmed clean fullscreen

| metric | clean fullscreen | overview-composited (artifact) |
|---|---|---|
| host wakeups/s (procwake) | **~8,050** (matches the re-baseline 8.1k ✓) | ~9,050 |
| vkr_ring poll_sleeps/s | **~5,900** (matches 5.9k ✓) | ~6,800 |
| poll_resumes/s | ~555 | ~800 |
| parks/s | ~182 (~3/frame) | ~210 |
| **batches/s (flushes host drains)** | **~1,600 ≈ 26–27 per frame** | ~1,950 |
| batch_avg | **~575 B** | ~590 B |
| batch_max | ~7–10 KB | ~12–21 KB |

**FINDING — the ring is fed ~26 small flushes per frame (~575 B each), not ~7.** My earlier
~7/frame estimate (from `poll_resumes` ~440/s) undercounted badly: `poll_resumes` only counts
pickups *after a timed sleep*; ~870 of the ~1,600 batches/s are picked up in the cheap
`thrd_yield` window (relax_iter < 16) and never counted. Derived: ~3.7 poll-sleeps per
inter-flush gap; average inter-flush gap ≈ 16.6 ms / 26 ≈ **640 µs** — squarely inside the
40 µs warm plateau and *below* the 1 ms `idle_timeout`, so the ring rarely parks (~3/frame) and
spends the frame poll-climbing the plateau between the 26 flushes. That IS the 5.9k poll-sleeps.

The flushes are **small** (~575 B; ring buffer is far larger) → they are NOT capacity-forced.
Something triggers ~26 flushes/frame at ~640 µs spacing. **Coalescing headroom looks real** —
if a frame's work collapsed toward a handful of flushes, inter-flush gaps would exceed
`idle_timeout` and the ring would park (doorbell) instead of poll → 5.9k → hundreds/s.

**CAVEAT (drives probe #2): `batches` is summed across ALL active venus rings** (the wake-trace
uses process-wide statics; one ring thread per venus context). The ~26/frame is the *aggregate*
of every process feeding venus — plausibly mutter + firefox-main + firefox-content, each with its
own ring thread independently polling on the 40 µs plateau. So the poll-sleep budget scales with
the *number of concurrently-active rings*, not just one app's submit rate. In clean unredirected
fullscreen mutter *should* be direct-scanning firefox's buffer (not compositing) → its ring should
be parked, but that's unverified. Per-ring attribution is required before claiming firefox alone
does 26 flushes/frame or that coalescing within one app is the lever.

### Probe #2 RESULT (2026-07-22) — per-ring gap histogram, clean fullscreen, decides lever 2

Extended the probe with a per-ring inter-flush gap histogram (`vkr_ring_wake_trace_gap`, keyed by
`ctx_id`; buckets <40µs/<160µs/<640µs/<1ms/<4ms/<16ms/≥16ms; "parkable" = fraction of wall time in
gaps ≥1ms). Same F44 enhanced / blobs / clean-fullscreen (user-confirmed) / present=60/s setup.

**Per-ring, clean fullscreen (representative samples):**

| ring | flushes/s | parkable | dominant gaps/s |
|---|---|---|---|
| **ctx=3 mutter** | **120 (2/frame)** | **100%** | ~110 in 4–16ms (nothing < 1ms) |
| **ctx=6 firefox** | ~1,440 (24/frame) | ~89% | <40µs ~650, <160µs ~375, <640µs ~200, **1–4ms ~110**, 4–16ms ~65 |

(overview state for contrast: mutter jumps to ~515 flushes/s — direct scanout quiets it ~4×.)

**THE key subtlety — "parkable" ≠ "no poll-sleeps".** Every gap that reaches the 1 ms
`idle_timeout` first **walks the full warm plateau** (16 warm rungs ≈ 610 µs + one 640 µs deep
sleep ≈ **~17 poll-sleeps**) *before* the ring parks. So a ring can be 100% parkable and STILL
burn thousands of poll-sleeps/s just walking to the park point:
- **mutter**: 2 flushes/frame, all gaps ≥4ms, yet ~110 gaps/s × ~17 = **~1,900 poll-sleeps/s**
  spent walking a plateau it never needed (it was always going to park).
- **firefox**: the expensive gaps are the **~110/s in 1–4ms** (~2–3/frame, the sync-separated
  flushes) — each also walks the full ~17-sleep plateau → ~1,900/s — plus the intra-burst
  <640µs gaps (~200/s × several sleeps). The <40µs gaps (~650/s, ~11/frame) are FREE (caught in
  the 16-`thrd_yield` window, no timed sleep).

So of the ~7.3k poll-sleeps this run, **~3.5–4k are "plateau-walk to an inevitable park"** on gaps
that are ≥1ms — where parking earlier is SAFE (the guest's 1 ms notify rate-limit isn't tripped,
because consecutive flushes are ≥1 ms apart) and adds **zero** extra doorbells (the gap was going
to park+doorbell anyway). The remaining ~3k are intra-burst <640µs gaps where the ring is genuinely
active and polling is the right call.

### Lever 2 VERDICT — validated, but reshaped: history-adaptive plateau depth (not a binary poll/trap switch)

The rings are **already** parked 89–100% of the time (idle_timeout does the "trap when idle" job).
A binary poll↔trap switch adds nothing there. The waste is that the warm plateau — whose *purpose*
is low-latency pickup for an **actively-fed** ring — is walked **unconditionally on every gap**,
including the long idle gaps of a mostly-parked ring, where it's ~17 pure-waste sleeps to reach a
park that was inevitable.

**The win = make the plateau depth adaptive to recent traffic (the user's traffic-rate idea, made
concrete):** a ring whose recent gaps have been *long* (sparse regime — mutter always; firefox's
post-burst sync gaps) should park after a **short probe** (~4 rungs) instead of walking all 16;
a ring in a *tight burst* (recent gaps < ~160µs) keeps the full responsive plateau. Estimated
safe win: both rings' ≥1ms gaps (~260/s) parking after ~4 rungs instead of ~17 saves ~13×260 ≈
**~3,400 poll-sleeps/s — roughly halving the total — with no extra doorbells, no mutter patch, no
guest change.** The hard safety boundary is the guest's 1 ms notify rate-limit: never park-early
on a gap you can't be confident is ≥1 ms, or the guest may rate-limit-suppress its wake → stall.
Recent-gap history is exactly the signal that says "we're in a sparse regime, early-park is safe."

This is a refinement of the 0043 adaptive plateau (which resets `relax_iter` per command): add a
second axis — adapt `warm_rungs` to observed inter-flush history — and let M13 `(visible,power)`
bias it. It is entirely host-side (virglrenderer), fits the two-tier guarantee (stock guests
benefit; the rate-limit coupling already exists in stock mesa), and does not touch mutter.

**Compositor angle (for the user's own compositor):** a compositor that direct-scans the fullscreen
app (mutter already does — 2 flushes/frame) needs to do nothing special; its residual ~1.9k
plateau-walk is a *host* artifact fixed by the adaptive plateau above, not a compositor change.

**App angle:** firefox's ~24 flushes/frame (esp. the ~2–3 sync-separated 1–4ms gaps) drives the
intra-burst remainder; reducing app flushes (guest coalescing) would help but is per-app and out of
scope. The guest-side flush-trigger attribution (mesa `vn_ring` tags) is only worth doing if we
later want to chase that; the host adaptive plateau is the general, one-place lever.

## Implementation attempt — adaptive plateau DEPTH (2026-07-22)

Implemented adaptive `warm_rungs` in `vkr_ring_relax` (saved as
`spikes/venus-ring-doorbell/vkr-adaptive-plateau-depth.patch`, env-tunable
`LIMINA_RELAX_WARM_MAX/MIN/SPARSE_AFTER`): a per-ring counter of consecutive "long" gaps
(≥640µs, i.e. gaps that walked into the deep-idle/park region); after `sparse_after` of them the
ring uses a MINIMAL warm plateau (park-walk in ~4 sleeps instead of ~17). Never parks before
`idle_timeout` → guest notify-rate-limit handshake untouched. Verified loaded (symbol present,
worker maps our prefix).

**Mechanism — VALIDATED (decisive).** Forcing the minimal plateau for *all* rings
(`LIMINA_RELAX_WARM_MAX=LIMINA_RELAX_WARM_MIN=2`), same overview state as the ~6.8k baseline:
**poll_sleeps 6.8k → ~2.2k, present still 60/s.** So coarsening the pre-park poll window delivers
a ~3× cut — the plateau-walk really is most of the budget, and cutting it doesn't stop rendering.

**Detector at default (`sparse_after=2`) — INERT on the interactive workload.** Clean-fullscreen
blobs poll_sleeps stayed ~5.9k (unchanged). Root cause, and it's **fundamental, not a tuning miss**:
the expensive plateau-walk happens on the **inter-frame idle gap**, which **follows a burst**. A
vsync-capped app's frame = [burst of short-gap flushes] → [one long idle gap to the next vsync].
The burst's short gaps keep resetting `consec_long` to 0, so when the ring *enters* the long idle
gap it's at `warm_max` and walks the full ~17-sleep plateau before parking. **No gap-history signal
can predict that the post-burst gap will be long** — history says "we were just bursting."

And you can't fix it by globally shortening the warm phase: vkmark's submit-latency-bound gaps are
~400–640µs — exactly what the 640µs warm plateau was tuned (in 0043) to cover. Shorten it and
vkmark's throughput falls back off the cliff. So **there is no safe static / gap-history default
that wins the vsync-capped case without regressing vkmark.**

## Verdict — mechanism proven, ship it under the M13 signal (not gap-history)

The distinguishing signal between "coarsen freely" and "must stay responsive" is **not recent gap
history — it's whether the ring is latency-bound**: a **vsync-capped / occluded / battery** app can
always coarsen (the added ≤640µs pickup latency is hidden by the frame budget — proven: forced
warm=2 held 60fps), while an **uncapped, submit-latency-bound** app (vkmark, uncapped game) must
keep the responsive plateau. That is precisely the **M13 `(visible, power)` (and vsync-capped)
state**, known directly — whereas gap-history is a proxy that provably can't see the post-burst
idle coming.

So: **the adaptive-plateau-depth mechanism is the right vehicle and is proven (~3× cut, no
early-park, no guest change, no mutter patch), but it must be driven by the M13 signal, not a
standalone gap-history heuristic.** Landing:
- Mechanism saved as `vkr-adaptive-plateau-depth.patch` (env-tunable `warm_rungs`); becomes a
  virglrenderer patch when M13 wires the signal to select `warm_rungs` per ring.
- M13 drives it: vsync-capped/occluded/battery ring → minimal plateau (≈ the forced-warm=2 win,
  ~3× fewer poll-sleeps); focused + uncapped/AC → full responsive plateau (vkmark-safe).
- The gap-history detector is kept in the patch as a *fallback* for genuinely-never-bursting rings
  (idle/background apps) where it's safe and non-inert, but it is NOT the primary signal.
- **Guardrail before shipping under M13: a vkmark A/B** confirming the "uncapped ⇒ full plateau"
  path leaves the ~2360 score intact (by construction it should — uncapped rings never enter the
  coarsen state — but confirm empirically).

This is the same conclusion the earlier analysis reached from the other direction: the doorbell-
handshake / plateau-retune / "lever 2" all converge on **one M13 knob**, and probe-driven work has
now (a) proven the mechanism and its magnitude, (b) built the mechanism, and (c) established that
its driving signal is M13's visibility/vsync/power state, not anything the ring can infer locally.

## UPDATE — longer-period PROFILE detector makes it a STANDALONE win (2026-07-22)

The "needs M13" verdict above was too pessimistic — it assumed the detector had to *predict the
next gap* from immediate history (which can't work). The user's idea: don't predict the gap,
**classify the regime over a longer window**. A vsync-capped app goes genuinely long-idle once per
frame (per-cycle slack); a saturated submit-latency-bound app (uncapped vkmark) never does. In the
capped regime you coarsen EVERY gap — safe *because the regime guarantees slack* (the ≤640µs pickup
latency is hidden). No need to predict which gap is long.

Implemented as `vkr_ring_profile_warm_rungs` (replaces the consec-long detector;
`vkr-adaptive-plateau-depth.patch`, env-tunable): a per-ring signal "has this ring had a **long
(≥2 ms) idle gap within the last 100 ms**?" → yes = capped/slack → minimal plateau; no = saturated →
full responsive plateau. The "longer period" is the 100 ms (~6 frame) slack window, so a burst's
short gaps no longer fool it. Starts responsive; never changes the park time (idle_timeout).

**A/B — both axes win (defaults long_idle=2ms, slack=100ms, warm_min=2, warm_max=16):**

| workload | metric | baseline (0043) | profile build | result |
|---|---|---|---|---|
| **blobs, clean fullscreen 60 fps** (vsync-capped ⇒ coarsen) | vkr_ring poll_sleeps/s | ~5,900 | **~1,750** | **−70% (~3.4×)** |
| | host wakeups/s (procwake) | ~8,100 | **~4,000** | **−51% (~2×)** |
| | present | 60/s | **60/s** | held |
| **vkmark** (saturated ⇒ stay responsive) | Score | ~2,360 | **2,289** (vertex 2212 / texture 2367 FPS) | responsive — **NOT** the cap=640 cliff (1193) |

The profile correctly classified: firefox+mutter (regular per-frame long idles) → coarsen → the ~3×
poll-sleep cut; vkmark (continuous sub-ms gaps, no long idle) → responsive → score intact. This is
the forced-warm=2 magnitude, achieved *selectively and safely*.

## FINAL VERDICT — standalone shippable win (M13 composes, isn't required)

The adaptive plateau depth with the **longer-period profile detector** is a self-tuning, per-ring,
host-side win: **−70% vkr_ring poll-sleeps / −50% host wakeups on the visible 60 fps common case,
vkmark throughput intact, guest-transparent, no mutter patch, no early-park (guest notify handshake
untouched).** It does NOT need the M13 signal — the ring derives "capped vs latency-bound" from its
own traffic profile. M13 **composes** later: `(visible, power)` can bias the thresholds (e.g. on
battery, lower `long_idle`/raise coarseness; occluded → coarsen hardest).

Promote `vkr-adaptive-plateau-depth.patch` to a real `patches/virglrenderer/` patch. Remaining
before ship:
- **Human eyeball on blobs smoothness in coarsen mode** — NOT yet captured (firefox WebGL got flaky
  on re-launch this session, the known scanout-automation flakiness; `present=60/s` held steady
  throughout the measured window as a strong proxy, but a human confirm of no micro-stutter is the
  proper gate).
- **Full vkmark-suite A/B** (this run was 2 scenes, 2289; confirm the whole suite stays ≈2360).
- Tune check: `long_idle=2ms`/`slack=100ms` defaults chosen from the blob profile; confirm they
  hold for 30 fps-capped and ~120 fps-capped apps (the regime test should scale, but verify).

### (Superseded) Probe #2 as originally scoped — guest mesa flush-trigger tags

Attribute the ~26 flushes/frame: (a) **per-ring breakdown** host-side (tag each batch with
`ctx->ctx_id`, count rings + flushes/ring — cheap, host-only, tells us how many *processes* feed
and whether mutter is parked in unredirected fullscreen); (b) **guest-side flush-trigger tags**
in mesa `vn_ring` (fence-wait / queue-submit / present / ring-full) — the decisive one: how many
of a single app's flushes are *forced* by a sync point (irreducible) vs *eager* (batchable). If
most are forced → coalescing lever closes, M13 tuning is the ceiling. If many are eager → the
structural win (park-per-frame) is real. Start with (a) — it's host-only and may already explain
much of the count as "N processes each polling."

### Probe #2a partial answer, gathered live (host `sample` + guest `/proc/*/fd`)

**Two active venus rings, each an independent poller:** `vkr-ring-3` = **gnome-shell (mutter)**,
`vkr-ring-6` = **firefox** (the only two processes holding `/dev/dri/renderD*`). So the aggregate
~26 flushes/frame and ~5.9k poll-sleeps are **split across a compositor ring + an app ring**, both
polling the 40 µs plateau concurrently. Notable: even in *clean unredirected fullscreen*, mutter
keeps an active, feeding ring — it is NOT parked (whether it still composites per-frame or just does
cursor/bookkeeping is unknown without the per-ring flush split).

**This reshapes the levers:**
- The poll-sleep budget scales with the **number of concurrently-active venus apps**, not one app's
  submit rate. A "single fullscreen 3D app" already means ≥2 pollers (compositor + app).
- A concrete new lever: **can mutter's ring park during unredirected fullscreen** (direct scanout,
  no compositing)? If so, that's potentially ~half the poll-sleeps gone for the common
  fullscreen-game case — independent of any per-app coalescing.
- The user's **traffic-rate poll↔trap hybrid maps naturally onto per-ring state**: a lightly-fed
  compositor ring would trap/park while a flooded app ring polls. The switch is per-ring, which is
  exactly where the shared-memory ring already has its own thread + IDLE handshake.

---

**Optional empirical nail (not run):** clamp `idle_timeout` host-side in `vkr_ring_create`
(ignore the guest's 1ms, use e.g. 100 µs) and measure — expected to surface the stall (§4) as
frame hitches and/or no net wakeup win. Reasoned + backed by the 0b′ early-park rejection;
not run to avoid a build/boot/eyeball cycle re-confirming a known result. Left here as the
decisive experiment if the conclusion is ever doubted.
