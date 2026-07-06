# venus blob-exhaustion — empirical results (2026-07-06)

## TL;DR (the surprise)

The `venus-blob-repro` crate (from `dogfood-guest:~/Projects/gnome-shell-rs/venus-blob-repro`,
copied to `~/Projects/gnome-shell-rs/venus-blob-repro`) reproduces host blob-pool exhaustion by
**leaking exportable dmabuf device memory in a loop**. On **dogfood-guest (production)** it aborts
the guest GPU context (SIGABRT / ring FATAL). On **my local dev-Mac stack it degrades
GRACEFULLY and recovers** — same guest, opposite outcome. So the abort is **not** an inherent
guest-venus-26.1.3 bug; it is triggered by a **host-stack (or exhaustion-type) difference**.

## What the crate actually does (pins the churn path)

Allocates `VkImage` (LINEAR RGBA8 1280×720, COLOR_ATTACHMENT|TRANSFER_SRC) + **dedicated,
exportable** `VkDeviceMemory` (`ExternalMemoryImageCreateInfo` + `ExportMemoryAllocateInfo`,
`DMA_BUF_EXT`), calls `vkGetMemoryFdKHR` to realize the host blob, and **never frees**. Each
such allocation → one host `virgl_renderer_resource_create_blob` with **`blob_id != 0`**.
This is exactly the path our fault-injection hook targets — my earlier flat measurements were a
**workload** miss (plain host-visible `blob-churn` / vkcube swapchain never allocate *exportable*
memory in a loop), not an instrumentation miss.

## Empirical run (local `Fedora-Workstation-44.enhanced.test.raw`, 8 GiB, my rebuilt virgl)

- Host device-memory `create_blob(blob_id!=0)` heartbeats climbed 0 → **2048** as the crate ran
  (confirms the path + instrumentation).
- Crate hit `vkAllocateMemory: ERROR_OUT_OF_DEVICE_MEMORY` at **allocation #2271 (~7984 MB ≈ the
  VM's 8 GiB RAM)** → printed `GRACEFUL`, freed everything, **exit 0. No abort.**
- Guest `dmesg`: `virtio_gpu_dequeue_ctrl_func *ERROR* response 0x1200 (command 0x10c)`
  (ERR_UNSPEC on RESOURCE_CREATE_BLOB) then `0x1203 (command 0x102)` (ERR_INVALID_RESOURCE_ID on
  the follow-up UNREF of the never-created resource). **Clean virtio-gpu errors — the venus ring
  was NOT poisoned.**
- **Post-exhaustion the VM is healthy:** gnome-shell still alive (pid unchanged, never
  restarted), `graphical.target` active, **no coredumps**, no ring-fatal in the journal, and
  `vulkaninfo` still enumerates `Virtio-GPU Venus`. The pool recovered (crate freed on graceful
  exit).

## Environment diff (local vs dogfood-guest)

| | local dev-Mac VM | dogfood-guest (dogfood) |
|---|---|---|
| guest kernel | 7.1.2-limina16k | 7.1.2-limina16k |
| guest mesa | 26.1.3-3.limina.fc44 | 26.1.3-3.limina.fc44 (**identical**) |
| venus | 26.1.3 | 26.1.3 (**identical**) |
| VM RAM | 8 GiB | 24 GiB |
| host stack | my fresh `virgl-prefix` build + this libkrun | **dogfood-mac's deployed limina (compositor session's build)** |

Guest is byte-identical → the guest's reaction to a given host response is fixed → the graceful
-vs-abort divergence is **host-side**. Two live hypotheses:
1. **Host version.** dogfood-mac's libkrun/virglrenderer poisons the venus ring on blob-create OOM (or
   on the follow-up invalid-resource-id reference via a *ring* command), where my current fork
   returns a clean `ERR_UNSPEC` on the *control* queue → graceful. If so, **our current host fork
   may already be well-behaved**, and the bug lives in dogfood-mac's (older/WIP) host.
2. **Exhaustion type (RAM-dependent).** My 8 GiB VM exhausts **guest RAM** (clean ERR_UNSPEC).
   dogfood-guest's 24 GiB may exhaust a *different* resource first (host-visible mapping / `hv_vm_map`
   / a per-resource count limit) whose failure poisons the ring — the reviewer's map-time
   hypothesis. Discriminate by re-running locally with more RAM (16–24 GiB).

## Instrumentation state
- `virgl_renderer_resource_create_blob`: env-gated fault injection
  (`LIMINA_BLOB_CREATE_FAIL_AFTER`/`_FOR`, `blob_id!=0` filtered) + `LIMINA_BLOB_HEARTBEAT`
  device-mem create counter. **Confirmed correctly aimed** at the crate's path.
- `LIMINA_TICK` counters on `resource_create` / `import_blob` / `blob_shm` (blob_id==0) — all flat
  under vkcube present (no per-frame host-allocation churn in the shipped zero-copy path).
- Patch: `inject-create-blob.patch` (applied to the gitignored `third_party/virglrenderer` tree).

## Next (decision pending — see the host-diff question)
- If **host version**: diff dogfood-mac's deployed libkrun/virglrenderer vs our fork; the fix may be
  host-side (return recoverable OOM, don't poison the ring) — matching the earlier source finding
  that our current host already does this.
- If **exhaustion type**: re-run locally at 16–24 GiB to trip the map-time path, then aim the fix
  there (libkrun `resource_map_blob`/`hv_vm_map` + the guest venus map-failure reaction).
- Either way the guest-venus degrade-instead-of-abort work (abort inventory in `DESIGN.md`)
  remains the belt-and-suspenders fix for the stock tier.

## CORRECTION from the vulkan-compositor session (2026-07-06)

The compositor session (dogfood-mac/dogfood-guest — its owned-Vulkan renderer is what churned) reviewed
this and corrected the framing. Full exchange:
`~/Projects/gnome-shell-rs/DEVMAC-SESSION-confusion.remote.md`. Key points:

- **There are THREE failure modes; the crate (leak-into-`Vec`) only exercises #3, the wrong one:**
  1. *Guest over-allocation (trigger)* — the WIP compositor churned a blob **per frame**
     (present-blit shadow `new_color_target` + scanout re-import on every `bind`). 100% WIP, not
     shipped, already fixed (`5b32c903`/`9abcacce`).
  2. *Host reaction to a blob-alloc failure (**THE defect to scope**)* — on failure, does the host
     return recoverable OOM and keep the ring alive, or set `VK_RING_STATUS_FATAL_BIT_MESA`? A
     **VMM-level** decision.
  3. *Guest-RAM exhaustion* — leaking into a `Vec` hits the guest kernel RAM ceiling (~8 GiB) →
     graceful **by construction**. **This is what my run hit** — not the compositor's failure mode.
- **The guest-venus "degrade instead of abort" patch is the WRONG LAYER.** Once the *host* sets the
  ring FATAL bit, mesa's guest driver `abort()`s **by design** — there is no guest recovery path and
  not meant to be one; the guest doesn't *choose* fatal, the host *tells* it. The only guest-visible
  alternative is `VK_ERROR_DEVICE_LOST` instead of `abort()` — still a dead device, still an upstream
  change, doesn't fix the wedge. **So the `DESIGN.md` abort-inventory / guest-venus plan is
  abandoned.**
- **The fix is HOST-side:** the host venus ring-command mem-alloc handler must, on blob-create
  failure, return OOM in the ring reply and leave the ring alive — never set fatal on a *recoverable*
  failure (fatal is only for genuine protocol corruption). Their guest-side ground truth: on
  dogfood-guest the **compositor itself** aborts mid-session (7–39 s, flat small footprint, nowhere near
  24 GiB) at an *unrelated* later ring op (`vkAllocateCommandBuffers`/`vkGetMemoryFdPropertiesKHR` in
  `vn_ring.c:456` / `vn_common.c:272`) — the messenger, not the culprit; the host set the ring FATAL
  asynchronously. Same `0x1200`/`0x1203` control-queue errors as mine — **but on dogfood-guest the ring
  goes FATAL and mine doesn't.** Identical guest ⇒ host-side divergence, confirmed.
- **Conclusion: my current host fork appears already correct; dogfood-mac's deployed host is the culprit**
  (older/different build that treats blob-create OOM — or the dangling-resource-id follow-up — as
  fatal protocol corruption). Action: (a) pin dogfood-mac's deployed host commit, (b) diff its
  blob-alloc-failure handling vs our fork, (c) deploy the graceful path to dogfood-mac/dogfood-guest. The
  persistent-leak-until-reboot symptom is **downstream** of the abort (a hard abort skips orderly
  context teardown → host never reclaims) — fix the abort and it disappears.

### Verifying the load-bearing claim (my host is graceful on a *forced* create_blob failure)
The natural-exhaustion run confounded the host-reaction (#2) with the guest-RAM trigger (#3). To
isolate #2, re-run with the deterministic injection (`LIMINA_BLOB_CREATE_FAIL_AFTER`), which forces
`virgl_renderer_resource_create_blob` to fail *without* touching guest RAM — a host-pool-style
failure like dogfood-mac's.

**Result (2026-07-06, `LIMINA_BLOB_CREATE_FAIL_AFTER=50`, worker env confirmed):** injection
fired at create **#50** and **kept failing every subsequent `blob_id!=0` create through #2272**
(2200+ forced `-ENOMEM`s). The guest venus **absorbed all of them gracefully** — the crate still
allocated 2275 and only stopped at the guest-RAM ceiling (`ERROR_OUT_OF_DEVICE_MEMORY`), exit 0;
**gnome-shell survived (pid unchanged), venus still enumerated, NO `VK_RING_STATUS_FATAL`, no
abort, no coredump.** So on our host, a sustained stream of `create_blob` OOM failures produces
**clean control-queue errors and never poisons the venus ring.** This is the exact behavior
dogfood-mac's host apparently lacks. **Load-bearing claim confirmed: the defect is host-side and NOT in
our current fork.** (The forced failures did not even stop the crate early — venus tolerates/falls
back on each failed export — underscoring how robustly graceful this stack is.)

## Net conclusion & next step
- The bug is a **host** VMM defect (ring FATAL on recoverable blob-create OOM); our current fork is
  clean; **dogfood-mac's deployed host is the culprit.**
- **Next: pin dogfood-mac's deployed limina host commit** (read-only) and diff its blob-alloc-failure /
  `vkr_context_set_fatal` path against our fork, then deploy our fork's graceful path to
  dogfood-mac/dogfood-guest. The guest-venus patch is abandoned.
- Repro-crate note (for the compositor session): the leak-into-`Vec` design can only ever hit
  guest RAM (mode #3); to stress the host path it'd need alloc+free-bounded — but our injection
  hook already proves the host path is graceful here, so the crate isn't needed for our stack.
