# venus blob-exhaustion → graceful degrade (RED-first design)

> ## ⚠️ SUPERSEDED — the fix is HOST-side, not guest-venus (2026-07-06)
> The vulkan-compositor session corrected the framing (see `RESULTS.md` §CORRECTION and
> `~/Projects/gnome-shell-rs/DEVMAC-SESSION-confusion.remote.md`). **The guest-venus
> "degrade instead of abort" plan below (abort inventory, `patches/mesa/0014`) is ABANDONED:**
> once the *host* sets `VK_RING_STATUS_FATAL_BIT_MESA`, mesa aborts *by design* — the guest can't
> overrule it. The real defect is the **host** venus ring-command mem-alloc handler setting the
> ring FATAL on a *recoverable* blob-create OOM (fatal should be reserved for genuine protocol
> corruption). Our current host fork **appears already correct** (returns clean OOM, ring stays
> alive); dogfood-mac's deployed host is the likely culprit. The abort inventory + source analysis below
> is kept as **still-accurate reference for the guest abort path** (what aborts, where), but the
> action moved to: pin dogfood-mac's deployed host commit → diff its blob-alloc-failure handling vs our
> fork → deploy the graceful path. The fault-injection hook + instrumentation remain useful.

**Problem.** Twice now, host GPU-blob-pool exhaustion has **SIGABRT'd the guest GPU
context**, taking the compositor + every guest GPU client down and wedging the VM until
reboot. The proximate trigger (per-frame blob churn — present-blit shadow + per-frame
scanout dmabuf re-import) is fixed by caching (9abcacce/5b32c903). This spike is the
**robustness** fix: a future exhaustion, or any in-guest Vulkan client, must degrade to a
recoverable error, not abort the process.

## Verified layering (2026-07-06)

The fix is **guest venus** (the Linux-side Mesa Vulkan driver), *not* host KosmicKrisp and
*not* host virglrenderer. The host is already largely well-behaved:

- `vkr_dispatch_vkAllocateMemory` returns recoverable `args->ret = VK_ERROR_OUT_OF_DEVICE_MEMORY`
  on allocation failure (encoded back to the guest) — `vkr_device_memory.c`.
- `virgl_renderer_resource_create_blob` returns a clean `-ENOMEM` on the control queue when the
  pool is exhausted — it does **not** poison the venus ring — `virglrenderer.c:1145`.
- The host sets `VK_RING_STATUS_FATAL_BIT_MESA` **only** on genuine protocol corruption:
  dangling resource id (`vkr_context_set_fatal`, `vkr_device_memory.c:25/680`) or a command that
  returns `ret < 0` (`vkr_ring.c:436`). That is the *correct* contract.

> ⚠️ The venus source was first read out of `/Volumes/mesa-cs` (the **host** KK/zink checkout,
> mesa 26.2.0-devel). That tree does **not** build venus. The **guest** venus ships from Fedora
> F44 mesa **26.1.3** + `patches/mesa/` 0009–0013. See `docs/codebases.md`.
>
> **Step 0 DONE (2026-07-06):** sparse-cloned upstream `mesa-26.1.3`
> (`third_party/mesa-guest-2613`, gitignored; VERSION-confirmed 26.1.3) and re-verified every
> site below — **identical line-for-line** to what was traced: `vn_ring.c:455-457` (abort on
> `VK_RING_STATUS_FATAL_BIT_MESA`), `vn_cs.h:124-129` (`set_fatal` = flag only, comment says
> "should be VK_ERROR_DEVICE_LOST or even abort()"), `vn_cs.h:165-169` (`reserve` → `set_fatal`),
> `vn_cs.c:224` (`reserve_internal`) → `vn_cs.c:264` (`vn_renderer_shmem_create`),
> `vn_command_buffer.c:1100-1101` (existing graceful `get_fatal` → `STATE_INVALID`). Line refs
> here are valid against the shipped guest. (TODO if paranoid: diff Fedora's mesa spec patch list
> — venus internals are ~never distro-patched, so upstream parity is taken as sufficient.)

## Two candidate guest abort chains (the repro must pin which fires)

**Chain A — encoder shmem-grow OOM (`vn_cs`).**
`vn_cs_encoder_reserve` → `vn_cs_encoder_reserve_internal` (`vn_cs.c:224`) →
`vn_renderer_shmem_create` returns NULL (host `-ENOMEM`) → `vn_cs_encoder_set_fatal` sets
`enc->fatal_error` (`vn_cs.h:124`). Mesa's own comment: *"should be treated as
VK_ERROR_DEVICE_LOST or even abort()."* Some paths already handle it gracefully — the ring
*upload* path returns `VK_ERROR_OUT_OF_HOST_MEMORY` (`vn_ring.c`), and the command-buffer path
checks `vn_cs_encoder_get_fatal` and marks the CB `INVALID` (`vn_command_buffer.c:1100`). Other
encode call sites ignore the `reserve()` bool → the flag never surfaces as a VkResult.

**Chain B — dangling-resource abort (HYPOTHESIS — NOT yet pinned to the incident).**
A blob/memory create fails, the guest **does not stop**, and submits a *later* ring command
referencing the un-created resource id → host `vkr_context_get_resource` fails
(`failed to import resource: invalid res_id %u`, `vkr_device_memory.c:24`) →
`vkr_context_set_fatal` → host poisons the ring (`vkr_ring.c:436`) → guest sees FATAL in
`vn_relax`/`vn_ring_submit` → **`abort()`**.

> **Correction (adversarial review, 2026-07-06).** I originally labelled Chain B "the observed
> SIGABRT." **That was an assumption, inherited from the other session's assessment, not read
> from an incident log.** I could not find the venus call site where the guest "does not stop":
> every mainline device-memory path already frees and propagates on `bo_create` failure
> (`vn_device_memory.c` `alloc_export` :216-240, `import_dma_buf` :124-165, `alloc_guest_vram`
> :167-213; ioctl maps to a recoverable result at `vn_renderer_virtgpu.c:232,652`). So Chain B
> may not be reachable via create-failure at all — the real incident could be **map-time**
> exhaustion (host-visible BAR / libkrun `resource_map_blob`/`hv_vm_map`), or a mem-id lifetime
> race, whose guest errno site is entirely different from create. **Step 1 is now: pin the site
> from the incident logs FIRST** (per our own discipline — the worker log + guest backtrace are
> the oracle), then aim the injection. Do not treat "which chain fires" as the only open
> question; brace for "neither, under create-injection ⇒ wrong injection site."

## Abort inventory (verified at 26.1.3) — converting one site is NOT enough

There are **≥3** abort families reachable once the host poisons the ring; fixing only
`vn_ring.c:457` turns crash → **permanent hang** (a *worse* outcome):

1. `vn_ring.c:455-457` — `vn_ring_submit_internal`, abort when submit sees FATAL.
2. **`vn_common.c:271-273` — `vn_relax` aborts on FATAL, UNCONDITIONALLY** (the abort right below
   it, the 895s watchdog at :281, *is* gated by `!VN_DEBUG(NO_ABORT)`; the FATAL one is not).
   **Every waiter dies here**, not at submit. `vn_ring_wait_seqno` (`vn_ring.c` ~185-198) is
   `do {} while(true)` with the only exit being seqno-satisfied — it *relies on* `vn_relax` to
   abort. Convert :457 alone and waiters spin to the 895s abort (or forever) = frozen VM that
   can't even crash-restart.
3. `vn_cs.h:205-207` — `vn_cs_decoder_set_fatal` is a bare `abort()`. Any truncated/garbled reply
   decode aborts. Note `vn_ring_set_reply_shmem_locked` (`vn_ring.c:684`) **ignores** its submit
   result — under OOM this can leave a stale reply stream → garbage decode → this abort.

Host side keeps the guest watchdog from tripping: `vkr_context_ring_monitor_thread` keeps
refreshing `ALIVE` (`vkr_context.c:656-664`) and the fatal ring-thread exit never removes the
ring — so removing the guest aborts without an explicit give-up path = silent hang.

**Silver lining (review):** the *submit-path* propagation is nearly free. `vn_ring_submit_locked`
already returns `VkResult`; a NULL reply makes every generated `vn_call_*` return an error with
**zero per-call-site edits**, and async commands become safe no-ops post-FATAL. The genuine work
is (a) the **wait loops** (`vn_relax` FATAL branch sets a sticky device-lost flag; the
`do/while` waiters + fence/sem/query waits in `vn_queue` must give up and return
`VK_ERROR_DEVICE_LOST`) and (b) choosing **reset semantics** for the sticky `fatal_error` on the
shared `ring->upload`/pool encoders (`vn_cs.c` reset deliberately does NOT clear it — leave it
sticky and one transient OOM = permanent wedge; too eager and we mask real corruption).

## RED-first plan (revised)

**Step 0 — DONE.** Sites re-verified at 26.1.3 (see the caveat box above).

**Step 1 — PIN THE SITE EMPIRICALLY (deterministic injection replaces the missing log).**
The two incident worker logs live in the compositor session's checkout (not local; commits
9abcacce/5b32c903 aren't in these forks) and aren't in local session history — so instead of
waiting on the log we **pin the site by construction**: the injection makes the failure
deterministic and we observe which layer/mode actually SIGABRTs the guest. This is empirical,
not assumption.

- **1a. Injection hook — BUILT (create-time, primary).** `LIMINA_BLOB_CREATE_FAIL_AFTER=N`
  (+ optional `LIMINA_BLOB_CREATE_FAIL_FOR=K`) in `virgl_renderer_resource_create_blob`,
  **filtered to `blob_id != 0`** (guest device-memory — production's churn; ring/reply/CS shmems
  at `blob_id == 0` are exempt, so we don't pin the already-graceful patch-0012 path), windowed,
  env-gated OFF by default. Applied to the working tree; saved as
  `spikes/venus-blob-exhaustion/inject-create-blob.patch`. (Counter is a plain static — fine for
  one VM; make per-context before reuse as backpressure.) Host split confirmed at
  `vkr_context.c` `vkr_context_create_resource` (blob_id==0+MAPPABLE ⇒ shm, else device-memory).
- **1b. Faithful repro = the REAL desktop, not (only) a synthetic probe.** Boot the enhanced
  venus desktop (`boot-enhanced-efi-kk.sh`) with the injection env reaching the **worker**, drive
  frames, and watch whether **gnome-shell/mutter** (the actual production client, doing the actual
  present-blit shadow + scanout re-import churn) SIGABRTs. This reproduces production's exact
  client and path — the injection just makes the create failure happen on demand instead of after
  slow real exhaustion. *(Attended: needs a boot + interaction; the worker log line
  `[limina-inject] fail device-memory blob create #…` confirms the hook fired.)*
- **1c. Decision probe — WRITTEN (`blob-churn.c`).** A synthetic churn oracle that checks every
  `VkResult`. Its job is to answer *does the mainline device-memory path degrade gracefully?*
  Likely **yes** — `vkAllocateMemory` frees+propagates (`vn_device_memory.c:216-240`), so the
  probe may print `graceful: …OUT_OF_DEVICE_MEMORY` and exit 0. **That clean exit is INFORMATION,
  not vindication:** it proves device-memory create-injection alone does NOT reproduce the abort,
  so the production abort is on a path the probe doesn't reach → escalate to 1d.
- **1d. Site sweep (self-sufficient, no user needed — just attended boots).** Run injection at
  each candidate layer until one SIGABRTs the real desktop, pinning the site:
  1. `blob_id != 0` create (this hook) — device-memory create.
  2. `blob_id == 0` create — ring/reply/CS shmem (add a `LIMINA_BLOB_SHMEM_FAIL_*` mode).
  3. **map-time** — hook `resource_map_blob` → `rutabaga.map_ptr()` in libkrun
     (`third_party/libkrun/src/devices/src/virtio/gpu/virtio_gpu.rs:2032`), the host-visible-BAR /
     `hv_vm_map` exhaustion the review flagged. *(Fallback hook located, not yet written.)*
  Whichever wedges gnome-shell is the production site; aim Step 2 there.

> **Working hypothesis:** create-time (`blob_id != 0`) — the other session's own assessment names
> "*When RESOURCE_CREATE_BLOB fails*", and 9abcacce/5b32c903 cache blob *creates*. Map-time is the
> documented fallback if 1b/1c come back clean.

**Step 2 — the fix (guest venus, `patches/mesa/0014-…`).** Scope set by the abort inventory,
not one line:
- Kill Chain B at the source: propagate `VK_ERROR_OUT_OF_DEVICE_MEMORY` at the pinned
  allocate/import/map site and **do not submit** commands referencing the un-created resource.
- Make ring-FATAL survivable, not just non-aborting: sticky device-lost flag set in the
  `vn_relax` FATAL branch; wait loops + submit entry points return `VK_ERROR_DEVICE_LOST`;
  decide the `fatal_error` reset policy. (Leverage the free submit-path propagation.)

**Step 3 — GREEN (scope corrected).** GREEN = **the venus oracle receives a recoverable error
and survives; NO process takes SIGABRT.** Compositor survival is a *measured* follow-on outcome,
NOT the gate — zink/mutter behaviour on mid-frame `DEVICE_LOST` is unverified and may need mutter
work (cf. `patches/mutter/0002`). Even "gnome-shell exits cleanly, GDM restarts it, VM not
wedged" is the win vs SIGABRT.

**Step 4 — graduate to L2.** Boot enhanced image with the (filtered, windowed) injection env
reaching the **worker** process; assert oracle clean-exit + no-SIGABRT, and (separately, softer)
compositor liveness. Assert the worker links `virgl-prefix` before trusting any "no effect"
result (the link trap).

## Two-tier exposure (this fix protects only the ENHANCED tier)
`patches/mesa/0014` ships in the guest mesa RPM → a **stock** Fedora guest (the compatibility
floor) keeps aborting until the fix flows upstream → Fedora → back. The only stock-tier lever is
**host-side**: the soft blob budget the injection hook doubles as, applied with **headroom** —
start failing guest-facing *device-memory* creates while **reserving capacity for ring/reply
shmems**, so exhaustion lands on the paths that already recover (`vn_device_memory.c:216-240`),
not on ring infrastructure. Deferred policy; stated so we don't forget stock stays vulnerable.

## Out of scope / notes
- Caching (already landed) remains the load-bearing mitigation; this makes the *tail* safe.
- A recoverable error still forces a well-written compositor to rebuild its swapchain/device —
  survivable, not free. Goal is "no SIGABRT / no wedge", not "invisible".
- Upstreamable: guest propagation half = clean mesa/venus report (OOM ≠ protocol corruption).
  Don't over-claim "FATAL only on protocol corruption" upstream — `vkr_ring_thread` also exits
  fatal on `cnd_wait` failure + the wait-ring seqno consistency check (`vkr_ring.c:374,418`).
- Layering re-confirmed by review incl. the skipped layer: libkrun propagates `create_blob`
  errors with `?` (no unwrap, `virtio_gpu.rs:1835-1853`) → injection won't crash the worker.
