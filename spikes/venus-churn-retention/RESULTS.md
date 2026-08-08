# Does the host reclaim venus memory the guest has freed?

**SOLVED 2026-08-07 — and the holder was never in the worker at all.** It is an **unbounded
`HashMap<u32, CFRetained<IOSurfaceRef>>` in the SUPERVISOR's frame-apply path**
(`crates/limina/src/window/mod.rs:1263`), which retains every distinct scanout IOSurface id it
has ever presented and is cleared only on a display-mode change. IOSurface storage bills to the
**creator**, so the leak accumulates in the *worker's* `owned unmapped` while the retaining
reference lives in the *supervisor* — which is why every worker-side ledger we built came back
balanced, and why "kill the worker" appeared to be the only reclaim. **§0.3 is the answer.**
Fixed in limina `8e00d94`; the same storm now plateaus at 423 MB instead of climbing to 8.6 GB.

§0.0–§0.2 are the eliminations that got there, and they remain valid as eliminations; their
*conclusions about the holder* are superseded by §0.3. The rest of the file is the (instructive)
record of four earlier probe runs that measured nothing.

---

## 0.3 THE ANSWER: the supervisor's frame-apply surface cache (2026-08-07, fourth storm)

Two measurements, one storm.

**1. The worker's display path is clean.** A ledger in code we own
(`ScanoutLedger`, `third_party/libkrun/.../virtio_gpu.rs`) counts every resource the display
path binds to a scanout against the ones the guest later unrefs:

```
[SCANOUT-LEDGER] seen 256 (fresh 255) released 251 still-held 3 | presents 251 stranded 0
[SCANOUT-LEDGER] seen 512 (fresh 511) released 507 still-held 3 | presents 507 stranded 0
```

511 fresh scanout resources in ten seconds — a fresh one per frame, as the compositor-side doc
describes — and **507 released**, three still held (the live buffer set), zero stranded. Yet
across the same window `owned unmapped` went 47.4 M → **8.6 G** and kernel `IOSurface` 143 → 764.
The worker lets go of everything and the memory stays.

**2. Killing ONLY the supervisor frees all of it, with the worker still running.** Every earlier
run killed the *worker*, which takes the supervisor down with it — so the reclaim had been
attributed to the wrong task. Split the kill and the answer is unambiguous:

```
                        footprint   owned unmapped   IOSurface   IOSurfaceDeviceCache
before (worker alive)       14.0G             8.6G         763                    729
supervisor killed            5.4G             896K         135                    102
   (worker still alive)
```

So the holder is on the supervisor side. `SurfaceStore` is capped at 32 and exonerated by
arithmetic (32 × 14.1 MiB ≈ 451 MiB), but the frame-apply path **copies out of that bounded
store into an unbounded one**:

```rust
// Cache looked-up surfaces by id (the worker reuses a small fixed set, its double buffer).
let cache: RefCell<HashMap<u32, CFRetained<IOSurfaceRef>>> = ...;
```

The comment states the premise, and the premise is the bug: a compositor that mints a fresh
scanout resource per frame gives a fresh **id** per frame, so every frame adds a permanent
retained entry. Only a display-mode change clears it. 621 retained surfaces × 14.1 MiB
≈ 8.55 GB — the whole observed figure, and 511 of them are exactly the fresh scanout binds the
ledger counted.

This also retires the confusion of §0.0–§0.2 in one stroke: the venus CF census, the KK dealloc
sentinels and the display ledger were all *correct*. The worker really did release. Creator
billing put someone else's leak in the worker's column.

### Acceptance: the same storm against the bounded cache (fifth storm, limina `8e00d94`)

```
                            footprint   owned unmapped   IOSurface
baseline                         3.9G            58.3M         143
mid-storm (t+5s)                 4.3G           413.8M         168
client killed (t+10s)            4.3G           425.9M         167
   + 20 s                        4.3G           422.8M         167
```

8.6 G → **423 M**, and — the property that matters — it **plateaus**: mid-storm, at the kill,
and twenty seconds later it is the same number. Kernel IOSurfaces grew by 24 instead of 621.

The ~423 MB residual is not a leftover leak: it is `SurfaceStore`'s own cap, 32 × 14.1 MiB ≈
451 MiB, which is bounded and behaves (an earlier arm evicted it on demand: resident 580.6 M →
154.7 M after 200 fresh publishes). Nothing survives the bounded cache, so there is no residual
CoreAnimation/WindowServer-side holder — the one thing the split kill could not have separated.

**Open, for the user to decide:** 451 MiB at 1440p is ~1.9 GB at 5K. Two ways to shrink the
tail — a smaller store cap (simple, but the store's slack is what covers publish/apply
reordering), or have the worker tell the supervisor when a scanout resource is unref'd, which
takes the tail to zero and matches "recover once the guest process dies" exactly rather than
approximately. The worker already knows the moment precisely; the ledger counts it.

**What is proven and what is inferred.** The split kill proves the holder is *supervisor-side*.
That the holder is this specific cache is code-reading plus arithmetic — nothing else on that
side can hold more than 451 MiB, and the entry count matches the frame count. The acceptance
test is the post-fix storm, not the unit test: with the cache bounded, `owned unmapped` must
plateau at cap × frame size instead of climbing. Any residual that survives a bounded cache is
CoreAnimation/WindowServer-side — the one thing a split kill cannot separate from the cache.

### The reproducer, reduced to a vehicle we own (`crates/limina-test/guest/kmschurn.py`)

Everything above was measured through synoik on a seated GNOME session — a compositor we do
not control, needing a real GDM login, a seat fight, and a wallpaper to amplify. The bug does
not need any of that. Its trigger is exactly **a fresh scanout resource per flip**, so a ~400
line ctypes presenter reproduces it: gbm allocation under zink (which makes the buffers venus
blobs, the shape synoik has), `drmModeAddFB2WithModifiers`, a page flip, release the previous.
No Wayland, no session, no GDM — `systemctl isolate multi-user.target` frees DRM master and it
takes the card directly.

Validated three ways on `kmsprobe.raw` (2560x1440, LINEAR, 300 flips in 5.0 s), against builds
differing only in whether `SurfaceStore::insert` evicts:

| run | guest workload | worker `owned unmapped` | regions |
|---|---|---|---|
| `churn`, eviction disabled (the bug) | created=301, ledger fresh 253/256 | 127.8 M → **8.4 G** | 15 → **620** |
| `churn`, `8e00d94` in place | created=301, ledger fresh 253/256 | 141.8 M → 407.8 M | 16 → **29** |
| `static`, `8e00d94` in place | created=3, 300 flips | 407.8 M → 407.8 M | 29 → **29** |

8.4 G over 620 regions is 14.1 MiB each — the leak signature exactly, at ~100 GB/min. The
guest side of the first two rows is byte-identical, so the whole difference is host-side
retention. The third row is the control that matters: same flip rate, same pixels, same
everything, and the *only* removed variable is buffer freshness — it does not move a single
region. The differential is churn, not presentation.

**This is why the vehicle is trustworthy: it was proven to FAIL against the bug before any
green from it was believed.** A test vehicle that has never reproduced the failure guards
nothing, and "our new instrument shows no leak" is an expensive thing to believe wrongly.

It is now wired as a standing guard —
`crates/limina-test/tests/scanout_churn_retention.rs`, which boots **windowed**
(`--display-capture` never enters `window::run`, so a captured boot would test none of this)
and asserts on the `owned unmapped` **byte** delta: **2340 MiB with eviction disabled, 98 MiB
with it**, at 1280x800. It asserted on region COUNT until 2026-08-07 — see §0.4 for why that
was blind (regions coalesce; the count moved by 1 across a 20x byte change).

Two traps it documents in place. The env (`GALLIUM_DRIVER=zink`,
`MESA_LOADER_DRIVER_OVERRIDE=zink`, `VK_DRIVER_FILES=…virtio_icd…`) is load-bearing and not
set by the script — without it gbm loads a different gallium driver, the buffers are not venus
blobs, and the run silently exercises a path the bug never lived on; it prints the env it saw
so a log can be audited. And its `handles=` count is **not** an identity count: the kernel
recycles a GEM handle number as soon as its object dies, so a perfect churn run reports
`handles=3`. The host-side `[SCANOUT-LEDGER] … fresh N` line is the identity oracle.

### §0.4 Re-measuring the residual: the cap was right, the ORACLE was not (2026-08-07)

This section originally claimed the residual was not the cap but a second publisher re-sending
surfaces. **That claim was wrong and is corrected below** — it was written from a call-count
without reading the call site. What survives is a genuine oracle bug and two facts worth having.

**The split kill, done properly** (`kill -9` on the supervisor: SIGTERM lets it tear the worker
down on its way out, which is what invalidated the first attempt — the worker died with it and
nothing could be attributed):

```
                                    owned unmapped   regions
baseline                                    19.2 M        39
after 300 churn flips @2560x1440            436.8 M       38
supervisor SIGKILLed, worker ALIVE            896 K         7
```

The supervisor holds it, and 436.8 M is `SURFACE_STORE_CAP` x one framebuffer
(32 x 14.1 MiB = 451 MiB). **§0.3's arithmetic was right.**

**THE REAL BUG HERE — the region count is not an oracle.** Note that column: it moves by ONE
across a 20x change in bytes, because regions coalesce. The L2 guard asserted on region count,
so it was blind to the entire 437 MB; it separated the catastrophic case (620 regions) and
nothing finer. An oracle that reads green while the quantity it guards moves 20x is worse than
none. Now asserted in BYTES, re-proven both ways: 2340 MiB fail / 98 MiB pass, six times the
headroom the region threshold had.

**Two facts from instrumenting both ends of the Mach surface port.**

| | count |
|---|---|
| sends from `limina-display`'s `publish()` | 12 |
| receives in the supervisor's store | 614 |
| distinct ids received | 39 |

1. **vkr publishes scanout surfaces to the supervisor directly**, bypassing `limina-display` —
   602 of the 614. This is structural, not a leak: the venus scanout IOSurfaces are created
   *inside* vkr and are deliberately **non-global**, so `IOSurfaceLookup(id)` fails by design
   and `limina-display` — which only ever sees an `iosurface_id` in `present_surface`, never
   the `IOSurfaceRef` — cannot mint a Mach port for one. Only the creator can. The call sits in
   `vkr_mtl_iosurface_alloc`, **one publish per `IOSurfaceCreate`**, which is correct.
   *(The first version of this section claimed vkr re-sent each surface ~25x with no
   already-sent guard, and filed a task to add one. There is nothing to dedupe — that was
   inferred from 614-vs-12 without reading the call site. Read the call site.)*
2. **602 publishes for 301 guest buffers = exactly 2 IOSurfaces per scanout buffer.** That is
   the same ~2x already noted in the backlog (1207 charges for 611 creates) and is the open
   question worth pulling on: halving it halves the resting cost of every cached surface.
3. **IOSurface ids RECYCLE.** 301 guest buffers produced only **39 distinct ids** — the guest
   holds ~3 at a time and the kernel reuses an id as soon as a surface dies.

**Fact 3 is why the unref-notification work was parked rather than merged.** The mechanism was
verified working — `hits=301, misses=0`, every release found its entry — and it correctly stops
the supervisor holding what the guest freed. But **with ids recycling, `release <id>` can drop a
DIFFERENT live surface that has since been given that id**: a correctness hazard, not merely an
ineffective optimisation.

> **UN-PARKED AND SHIPPED 2026-08-07** (limina `93ff513`, libkrun `d9afca2`; the two
> `wip-release-notify-*.patch` files are gone from this directory, superseded). The hazard needed
> no epoch and no change of key. It existed only because the release rode the **control socket**
> while publishes rode the **Mach surface port** — two queues, no ordering between them. Move the
> release onto the same port, send it *before* the rutabaga unref that drops the last host
> reference, and it closes by causality:
>
> - the release is enqueued while the surface is still alive;
> - the id cannot recycle until the surface dies, which is after that;
> - so a publish of a recycled id is enqueued strictly later;
> - and one Mach port is one FIFO queue, so the supervisor sees them in that order and can never
>   drop the newcomer.
>
> Sender identity does not enter into it — causality orders the enqueues, not the sender. The
> lesson worth keeping is that **the hazard was a property of the CHANNEL, not of the key**; a
> whole epoch-plumbing design was avoided by asking which queue the message was in. What §1
> actually needed measuring, and what this bought, is in `buffer-lifetime-matrix.md` §8 and
> `testcomp/supervisor-retention.sh`.

### §0.5 The 2x IOSurfaces: the GUEST creates two exportable images per buffer (2026-08-07)

§0.4 left "602 publishes for 301 guest buffers = exactly 2 IOSurfaces per scanout buffer" as the
open question. Answered by tagging every allocation site in vkr and counting across one
300-flip churn (`iosurface-site-census.patch` here — re-apply it to repeat this):

| site | what it is | allocations |
|---|---|---|
| A `vkr_image.c` metal_objects | MoltenVK `VkImportMetalIOSurfaceInfoEXT` | **0** (backend retired) |
| B `vkr_image.c` mtltexture | `LIMINA_KK_MTLTEXTURE_SCANOUT` | **0** (gated off) |
| C `vkr_image.c` kk_linear | the KK native-modifier path | **602** |
| D `vrend_renderer.c` plain | classic/EGL-backed scanout | **3** (modeset only) |

So it is not vkr allocating twice for one image — it is **one allocation per image, and the
guest creating two images per buffer**. The tag prints the usage flags, and they alternate:

```
[IOSITE] C kk_linear 2560x1440 fmt=44 usage=0x480017
[IOSITE] C kk_linear 2560x1440 fmt=44 usage=0x80097
```

`0x480017` = HOST_TRANSFER | ATTACHMENT_FEEDBACK_LOOP | COLOR_ATTACHMENT | SAMPLED |
TRANSFER_SRC/DST; `0x80097` = the same minus HOST_TRANSFER, plus **INPUT_ATTACHMENT**. Two
genuinely different images, not one image seen twice.

vkr's behaviour is correct: it allocates for any image carrying
`VkExternalMemoryImageCreateInfo` that is not an import, because it implements the export
contract itself (no macOS driver honours dma-buf handle types). An exportable image the guest
asked for gets a surface. **The 2x is upstream of us.**

**CAVEAT, and it is a big one: this was measured through `kmschurn.py`, which allocates with
gbm under zink — and zink is known to create a separate linear scanout shadow alongside the
main image.** So this 2x may be an artifact of the probe's allocation path rather than a
property of real workloads: synoik has allocated scanout images *directly in Vulkan* since
`95c306bf` and would create one. Do NOT generalize this to the dogfood stack without
re-measuring there. The backlog's "1207 charges for 611 creates" was seen on synoik, which
suggests something similar happens there too — but that is a separate measurement, not this one.

**Next, and it is now the same task as the `churn-vk` arm:** teach `kmschurn.py` to allocate
scanout images directly in Vulkan (the sequence in synoik's `scanout-buffers-via-vulkan.md`),
then re-run this census. One image per buffer would confirm the 2x is zink's shadow and close
this; two would mean it is real and worth chasing into the guest.

### §0.6 CLOSED: the 2x is zink's shadow. A Vulkan-native allocator gets one (2026-08-07)

The caveat above was the whole finding. `kmschurn.py` now has `-vk` modes that allocate the
scanout image in venus directly (synoik's sequence step for step), so the
census could be re-run with the ALLOCATOR as the only variable. Both arms ran **in the same
boot, against the same instrumented build, at the same frame count**:

| arm | buffers created | `[IOSITE] C` allocations | usage flags seen | ledger charges | peak |
|---|---|---|---|---|---|
| `churn` (gbm under zink) | 301 | **602** | `0x480017` x301, `0x80097` x301 | 1205 | 112.5 MiB |
| `churn-vk` (venus direct) | 301 | **301** | `0x12` x301 | 603 | 84.4 MiB |

**One image per buffer.** vkr allocates exactly once per exportable image in both arms; gbm
under zink simply asks for two. The 2x is zink's linear scanout shadow, it is not vkr's, and
a Vulkan-native compositor does not pay it. Nothing to fix on the host — **#9 closes here.**

The charge counts also dispose of the loose end §0.5 left hanging. The backlog's "1207 charges
for 611 creates" on synoik looked like the same 2x, and it is not: **a charge is not an
IOSurface.** Every scanout buffer costs two charges — the surface and its dedicated device
memory — so ~2 charges per create is what a 1:1 allocator looks like. The vk arm's 603 charges
for 301 surfaces is exactly that ratio, and synoik's 1207/611 matches the vk arm, not the gbm
one. Synoik was already at one surface per buffer.

Reading this table the other way is the more useful result for the stack: the gbm-under-zink
path costs **twice the host IOSurfaces and 33% more peak host GPU memory** for the same
pixels. That is an argument for the Vulkan-native allocation path on its own, independent of
any leak.

The L2 guard (`crates/limina-test/tests/scanout_churn_retention.rs`) moved onto this arm for
the same reason, re-proven RED first: **+1213 MiB with `SurfaceStore`'s cap lifted vs +98 MiB
with it**, at 1280x800 over 301 buffers. 1213 MiB is 301 x 4 MiB — one whole framebuffer per
frame, exactly. The gbm arm's own RED/GREEN (2340 / 98 MiB) is above; the vk arm's is half
because it allocates half the surfaces.

Repeating the census: apply `iosurface-site-census.patch` to `third_party/virglrenderer`,
rebuild, boot, `systemctl isolate multi-user.target`, then run both arms and tally
`[IOSITE]` in the worker log. Revert and rebuild after — the patch is instrumentation, not a
change we carry.

### What this cost, and the rule that would have saved it

Four storms and most of a day were spent instrumenting the worker because the memory appeared in
the worker's footprint. **On macOS, an IOSurface's storage is billed to the task that created it,
not the task that retains it — so a footprint tells you who allocated, never who is holding.**
Before instrumenting the process the bytes appear in, split the kill: take down each participant
separately and see which one's death frees the memory. That test costs one storm and would have
been decisive on day one.

The corollary for the balanced ledgers: three independent, correctly-implemented refcount
censuses all said "we released it," and all three were true. When every ledger in a process
balances and the memory is still there, the next question is not "which ledger is lying" but
"which *other process* is holding" — cross-process retention is invisible to any of them.

---

## 0.0 THE ANSWER, as far as elimination can take it (2026-08-07, 10-second storm)

A 10-second `SYNOIK_VK_FULL_DAMAGE` storm (vkcube on a seated synoik) reproduces the whole thing
on demand, and the refcount census added for this run eliminates every candidate we own.

```
                       footprint   owned unmapped   regions   ledger live
baseline                    3.9G            86.4M        18       98.8 MiB
after 10s of storm         12.4G             8.6G       657      129.2 MiB
+20s after killing vkcube  12.4G             8.6G       633      106.6 MiB
after KILLING synoik       14.0G             8.7G       637         4096 B
```

```
refs — iosurface 613/608 (+5) texture 0/0 (+0) registry 613/608 (+5) lookup 1226/1220 (+6) publish 613 ok 0 err
ctx 2 [synoik] destroyed — lifetime 1292 charges totalling 17.4 GiB, peak 142.7 MiB   <- NO residual line
```

Read those two blocks together and almost everything is ruled out:

- **Not guest-held.** The compositor's context was destroyed and the ledger went to **4096 B**.
  Peak was 142.7 MiB against 17.4 GiB of lifetime charges — this context churned ~1200 surfaces
  and held three at a time, exactly as a well-behaved 4-slot swapchain should.
- **Not our refcounts.** Every `+1` site we take on an IOSurface balances: 613 allocated / 608
  released, registry symmetric, 613 publishes with 0 errors. The five outstanding are the live
  swapchain.
- **Not the supervisor.** Note its 17.1 MB footprint does NOT by itself prove this: IOSurface
  storage is billed to the **creator**, so surfaces the supervisor held would still show in the
  *worker's* `owned unmapped`. The real eliminators are read from the code plus arithmetic —
  `SurfaceStore` is capped at 32 and drops its `CFRetained` on eviction
  (`crates/limina/src/window/present.rs:63`), and the receiver deallocates the moved send right
  immediately after `IOSurfaceLookupFromMachPort` (`crates/limina-surfaceport/src/lib.rs:215`).
  32 × 14.1 MiB ≈ 451 MiB, three orders off 8.6 GB.
- **Not the teardown sweep.** §0.4 shows the sweep reclaims tracked objects, and here it had
  nothing left to reclaim — the bytes were already credited.

**So 8.7 GB of IOSurface-sized storage survives the death of the context that created it, with
every reference we know about released.** `owned unmapped` is the signature: memory this task
*owns* but has no mapping for — 637 regions at ~14 MiB. The holder is below our code, in the
host Vulkan/Metal stack (KosmicKrisp, AGXMetal, or the IOAccelerator layer retaining textures
built over those surfaces), or in a Mach memory entry nobody drops.

**This is why killing the offending guest process reclaims nothing**, which was the original
report. It is not a virtio/venus bookkeeping bug at all — by the time the guest process dies,
our books are already square.

The retention is ~100% of every surface ever created — 613 x 14.06 MiB = 8.42 GiB against the
8.6 G measured, *including the 608 we released*. So the question was never "which surfaces
leak"; it is "what adds one unreleased reference to every surface that gets used". That kills
every partial explanation at once (an eviction race, a stuck frame, a few pinned scanouts).

## 0.1 THE HOLDER IS NOT A VULKAN OR METAL OBJECT (2026-08-07, second 10-second storm)

The next run answered it, and the answer was not on anyone's list. Two instruments, one storm:
dealloc **sentinels** (an associated object dies with its host, so its `-dealloc` proves the
host actually died — a balanced release tally never could) on every IOSurface and MTLTexture we
create, and `ioclasscount` sampled across the compositor's death and the worker's.

```
census after 10s: iosurface 611 created / 606 released / 605 DEALLOCATED (6 alive)
                  vkr-tex   606 deallocated (5 alive)     [self-test: OK]
worker: footprint 12.6 G, owned unmapped 8.6 G in 641 entries
```

**Our objects die.** 605 of 611 surfaces and their textures were genuinely deallocated, and the
6 still alive are exactly the live swapchain. Not one Vulkan or Metal reference is outstanding.

And yet, system-wide (`ioclasscount`, so read only the differences):

```
                                IOSurface   IOSurfaceDeviceCache   free pages (16 KiB)
post-storm, compositor alive          766                    732                  411k
after killing the compositor          765                    731                  302k
after killing the WORKER              128                     96                1,195k
```

Killing the worker dropped **637 kernel IOSurfaces** — the same count as the 636 `owned
unmapped` entries — and returned **~13.6 GB** to the system.

So 637 kernel IOSurface objects were alive at a moment when our own wrappers for them were
already deallocated. **Releasing the last CF reference does not destroy the kernel surface.**
A kernel-side reference outlives it, and nothing short of the worker dying drops it — which is
precisely why killing the guest process reclaims nothing.

Read "released at worker death" carefully: killing the worker *also* tears down the supervisor's
VM state, so on its own that observation is equally consistent with a worker-task kernel ref and
with a supervisor-held one. What separates them here is code, not the kill: the supervisor's
receive loop drains continuously into a `SurfaceStore` capped at 32
(`crates/limina/src/window/present.rs:63`) and `mach_port_deallocate`s each moved right right
after the lookup (`crates/limina-surfaceport/src/lib.rs:215`), so it can hold ≤ 451 MiB.

What this does retire outright is the whole "which Vulkan/Metal object holds a ref" line of
inquiry, KK included: the answer is none of them. Three candidate kernel refs remain, all ours:

1. the Mach send right from `IOSurfaceCreateMachPort` in `limina_publish_surface` (611 published
   this run) — a port holds a reference to its surface until every right is gone. Weak by the
   code above, and only fully falsifiable by reading the port spaces;
2. a GPU-stack reference taken when the surface is *used*. `IOSurfaceClient` (812 → 168) and
   `IOSurfaceDeviceCache` (731 → 96) both drop by ~640 at worker death, one per surface;
3. a rutabaga/libkrun blob mapping of the surface into guest physical address space. Only its
   `mach_vm` form survives — any in-worker `CFRetain` would have blocked the dealloc we measured.

Do **not** rank these by count: publish, first-GPU-use and blob-map all happen exactly once per
surface, so 611 matches all three equally.

**Two instrument failures to avoid repeating.** `lsmp -p <pid>` without root prints
`task_for_pid() failed` and exits 0 — a `grep -c` over that reports "0 send rights" and reads
exactly like a clean port space. It needs `sudo`. And §0.4's "kill reclaimed all of it" is a
**ledger** statement only: that probe never writes to its buffers, so it had no resident pages to
reclaim. It therefore says nothing about whether a rendered-into surface's pages come back, and
cannot be used to exonerate the publish path.

## 0.2 "A kernel ref is taken on GPU USE" — REFUTED (2026-08-07, scanout-bind vs scanout-render)

The obvious next hypothesis was that the surviving kernel reference is taken when the GPU
touches the surface. Two arms, 200 iterations each at 2560x1440, identical except for that:
both create a scanout image, bind dedicated exported memory, then destroy the image and free
the memory — a complete release every iteration, live set of one. `scanout-render` additionally
clears the image on the GPU and waits for the queue.

```
                                 footprint   owned unmapped (res)   entries   kernel IOSurface
baseline                              4.2G                 254.8M        55                158
after scanout-bind   x200             4.1G                 126.6M        48                176
after scanout-render x200             4.5G                 580.6M        73                176
after killing the render arm          4.5G                 576.6M        48                179
then 200 non-resident publishes       4.1G                 154.7M        50                  -
```

The render arm leaves ~450 MB that its own death does not reclaim, which looks like the storm
for exactly as long as you do not divide: 450 MB / 14.06 MiB = **32**, and `SURFACE_STORE_CAP`
is 32. Publishing 200 fresh non-resident surfaces evicted them and the resident bytes fell back
to 154.7M — so it is the supervisor's bounded store doing precisely what it is documented to do.
(The bind arm leaves nothing only because it never writes; both arms publish 200 surfaces, and
the counter confirms it: 19 -> 419 created, 418 freed.)

**So a clean guest-side lifecycle does not leak, even with real GPU rendering.** 400 surfaces
created, 418 released, retention bounded and evictable. Whatever the storm does, it is not
"use takes a reference".

### The wait mode is not it either (`scanout-render-fence`, same run)

`vkQueueWaitIdle` drains the whole queue, which no compositor ever does, and it was a fair
suspect for running the driver's deferred-release work. So a third arm waits on a per-image
**fence** instead, leaving the queue busy:

```
                                 footprint   owned unmapped (res)   entries
baseline                              4.2G                 239.1M        50
after scanout-render-fence x200       4.6G                 646.9M        60
after killing it                      4.6G                 646.9M        53
```

+408 MB = 29 surfaces. The store cap again, indistinguishable from the queue-drain arm. Across
all three arms the sentinels stayed clean throughout: **827 created, 826 released, 825
deallocated, 2 alive.**

### What the probe still cannot do: PRESENT

Three arms, 600 clean cycles, two wait modes, real GPU writes — and retention never exceeded the
supervisor's bounded store. The remaining structural difference between this probe and the storm
is that the probe's surfaces are **never scanned out**. A compositor issues SET_SCANOUT with a
fresh resource every frame; these images are created, rendered, destroyed, and never shown.

That makes the sharpened suspect the **display path in the worker** — libkrun/rutabaga's
per-resource scanout state — rather than anything in the Vulkan or Metal layers. It fits every
measurement so far: worker-scoped (only worker death reclaims), unbounded by nature (one entry
per resource id ever scanned out), invisible to the venus ledger and to the sentinels (it would
hold the kernel surface, not our CF wrapper), and unreachable by a probe that never presents.

**Next: count, over a storm, how many scanout resources the worker's display path has ever seen
against how many it has released.** That is a counter in code we own, and it needs one storm.

Two side observations from the same run, neither load-bearing:

- The ledger charged **1207** "IOSurface" charges for a run that created **611** surfaces —
  close enough to 2x to look systematic. It over-reports rather than under-reports, so it does
  not affect any conclusion above, but the charge sites need an audit.
- `import-tex 0` and no KK banner: `mtl_new_texture_with_descriptor_iosurface` is the *vrend*
  import path. venus hands KK the finished MTLTexture (`kk_device_memory.c:183`,
  `mtl_retain`), so KK never wraps the IOSurface itself here. The KK instrument is committed
  and correct; it simply has nothing to say about a venus scanout.

Reproduce with: rig booted `LIMINA_GPU_MEM_BUDGET_CENSUS=5`, a
`SYNOIK_VK_FULL_DAMAGE=1` drop-in on `org.gnome.Shell@user.service` (verify it reached the
process's own `/proc/PID/environ` — a drop-in on `@wayland` is inert), then
`vkcube --wsi wayland` for **ten seconds**. It costs ~50 GB/min; do not run it longer.

## 0. The decisive run

Rig: `leak-sample.raw`, seated synoik, `SYNOIK_VK_FULL_DAMAGE=1` on the unit that actually
starts it (`org.gnome.Shell@user.service` — **not** `@wayland`, which is the one that looks
right and is inert), plus `vkcube` to supply continuous damage. Census at 15 s.

| | |
|---|---|
| ledger, `ctx 2 [synoik]` | **194.2 MiB live** — flat across consecutive censuses |
| worker physical footprint | **16.8 G** (from ~1 G), in ~4 minutes |
| `owned unmapped` | **13.4 G / 1010 regions** |
| region size histogram | **978 × 14.1M**, 32 × 128K, 7 × 16K |

`14.1 MiB` = 2560×1440×4 = 14,745,600 B — this rig's scanout surface size, exactly as
`767 × 31.6M` was 3840×2160×4 on the 4K dogfood display. Same leak, same object, scaled to
the display.

**The ledger and the process disagree, and that disagreement is the finding.** The ledger
charges at three host allocators (driver `vkAllocateMemory`, `vkr_mtl_iosurface_alloc*`,
`vkr_mtl_shm_alloc`) and credits at the matching frees. It says the guest's live set is small
and stable — i.e. **the guest freed them and our release paths ran**. The memory stayed
anyway. So the storage is retained by a holder *downstream of* the release we account for.

Two consequences worth stating plainly:

- **The budget cap would NOT have prevented the 2026-08-06 crash.** It bounds what the guest
  *holds*; this memory is not held by the guest, so the ledger never approaches the cap and
  no refusal fires. The cap protects against a guest that over-allocates; it does nothing
  about a host that fails to release. Both are real, and only the first is fixed.
- The census is nonetheless what settled it. `vmmap` alone could not — see §2.

**Refuted, by reading the code rather than assuming:** the supervisor's Mach-port surface
store was the obvious suspect (`limina_publish_surface` in `vkr_metal_helpers.m` has **no**
matching unpublish, and the supervisor outlives every guest process). But `SurfaceStore`
(`crates/limina/src/window/present.rs:63`) is LRU-capped at `SURFACE_STORE_CAP = 32`. It
cannot be holding 978 surfaces.

### 0.1 Charged-and-credited, or never charged? — CHARGED (measured 2026-08-07)

The census originally printed only *live* counts, which cannot tell "we freed it and the
storage stayed" from "we never saw the allocation". Adding lifetime counters settled it in one
run. A bounded 60 s storm (footprint 3.8 G → **52.1 G**):

```
ctx 2 [synoik]: 120.6 MiB live, 81.0 GiB over 5912 charges
              — 3 x 14.1 MiB (IOSurface, 5889 ever), 1 x 31.9 MiB (device memory, 1 ever), …
ctx 5 [vkcube]:   8.5 MiB live, 16.8 MiB over 24 charges — …
```

**5889 IOSurfaces of 14.1 MiB charged, 3 live.** So 5886 went through
`vkr_mtl_iosurface_free` — our refcount dropped — and the process kept the storage anyway.
The hunt is for a **stray reference**, not an uninstrumented allocator.

Note also `ctx 5 [vkcube]`: 24 charges, 8.5 MiB live, flat. A well-behaved client on the same
stack in the same run. The leak is specific to the churned scanout path, not to venus at large.

**Two suspects eliminated by reading the code, both of which looked strong:**

- *The worker leaking its own Mach send right.* `limina_publish_surface` creates one per
  surface via `IOSurfaceCreateMachPort`, but sends it with `MACH_MSG_TYPE_MOVE_SEND` (consumed
  on success) and `mach_port_deallocate`s it on failure. Balanced.
- *The supervisor leaking the received right.* `SurfacePortReceiver::recv`
  (`crates/limina-surfaceport/src/lib.rs:212`) deallocates the moved right immediately after
  `IOSurfaceLookupFromMachPort`. Balanced. And its `SurfaceStore` is LRU-capped at 32, so it
  cannot be hoarding surfaces either.

### 0.2 Attempts at a minimal repro — two shapes tried, neither reaches the allocation

Goal: reproduce the retention without a seated compositor, a debug flag, or a 50 GB storm.
`vkchurn.py scanout` creates and destroys images with a live set of 1, so *any* host growth is
retention by construction. The ledger is the check that the probe is on the right path at all —
and both times it said no.

| shape | images | ledger IOSurface charges | `owned unmapped` |
|---|---|---|---|
| `VkExternalMemoryImageCreateInfo`, OPTIMAL tiling | 1000 | **1** | 131.8M → 563.4M (+431M, region count flat at 47) |
| + `VkImageDrmFormatModifierListCreateInfoEXT`, `DRM_FORMAT_MODIFIER` tiling | 300 | **0** | 566.5M → 566.5M — flat |

Neither entered the allocation-side scanout path in `vkr_image.c`, so neither is a negative
about the leak — the same "the differential is not reaching the system under test" failure as
§1, caught the same way (by checking the instrument rather than the outcome).

Two things worth keeping from the attempts:

- **The +431M with only ONE IOSurface charged is unexplained** and worth its own look: ~431 KB
  per image of growth that the ledger does not account for, with the region count *unchanged*.
  Small, but it is host growth per guest image create.
- **A field-ordering bug nearly produced a third false negative.** `make_image` set `ci.tiling`
  in its defaults *after* the external-memory branch, silently stomping
  `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` back to `OPTIMAL`. Caught by reading, not by
  running. When a probe reports "no effect", suspect the probe.

**Next step: stop guessing the shape — observe it.** Add a one-shot log at the IOSurface
allocation in `vkr_image.c` recording the create parameters that got it there (format, tiling,
usage, extent, which branch), run a **10-second** storm, and read what synoik actually sends.
Then make the probe match exactly. The storm grows the host ~50 GB/min, so ten seconds is the
budget — do not repeat the 60-second run.

**Then: find who still references the IOSurface after `vkr_mtl_iosurface_free`.** That
function drops the registry retain and `CFRelease`s both the texture and the surface, and the
ledger confirms it ran — so a reference taken elsewhere is outliving it. Candidates, in the
order they are worth checking: the virgl resource / blob layer wrapping the surface, the
render-server proxy's marshaling of it, and a cross-context import `+1`
(`mem->imported_iosurface`) whose owning context died down the `vkr_context_destroy`
bare-`free()` path. The teardown matrix in the buffer-lifetime handoff is the systematic
version of this search.

### 0.3 CORRECTION — the probe DOES reach the allocation path; §0.2's ledger readings were stale

Both "0 charges" and "1 charge" rows above are **wrong**, and the instrument was at fault, not
the probe. `vkr_budget_forget_context` zeroes a context's slot at teardown, and the census is a
**sampler on a timer**: a probe process that starts and exits between two ticks takes its whole
row with it. Every §0.2 charge count was read from a census that ran after the probe had exited.

Re-run with the trace tagged by context id and a lifetime total logged at every teardown:

```
[SCANOUT-TRACE n] ctx 5: 2560x1440 fmt=44 tiling=1000158000 usage=0x11 ext=1 modlist=1 ... -> IOSurface id=NN
ctx 5 [python3] destroyed — lifetime 20 charges totalling 281.2 MiB, peak 14.1 MiB
```

20 images → 20 ctx-tagged traces → **20 charges**, and teardown printed **no residual line**.
So on this shape we allocate a real host IOSurface per image and release every one of them.
`peak 14.1 MiB` (one surface, not twenty) confirms the live set of 1 is real.

**The probe therefore does not reproduce the leak — and that is now a result, not a miss.** It
performs the compositor's allocate/free with byte-identical create parameters and retains
nothing. What it never does is **present**: it does not bind memory, export a blob, attach the
surface to a virgl resource, or hand it to the supervisor over a Mach port. The retention lives
in one of those, not in allocation or release.

Two instrument traps, both of which cost a wrong conclusion here:

- **Census rows are samples; absence of a row means nothing.** Short-lived clients are exactly
  what one reaches for when reproducing a churn bug, and they are precisely what the census
  cannot see. Fixed by logging lifetime totals at *every* teardown, not just residual ones —
  which is also the leak signature worth having in production logs.
- **Log timestamps cluster at drain time, so they cannot order events.** Twenty 14 MiB
  allocations appeared to land within 2.5 ms (~110 GB/s) immediately after a census line. They
  are emitted through the rutabaga log bridge and drained in blocks. Correlate by **context id**
  (now on every trace line), never by timestamp.

### 0.4 A DIFFERENT retention class reproduced — and its kill-recovery works (2026-08-07)

**This is not the storm.** Compare the two signatures before reading the numbers below:

| | charges | live at peak | worker | ledger explains it? |
|---|---|---|---|---|
| storm (synoik, FULL_DAMAGE) | 5912 | **3** (~120 MiB) | **52.1 GiB** | **no** |
| probe (`scanout-leak`) | 40 | **20** (281.2 MiB) | flat | yes, fully |

The storm's memory was **charged, credited, and still retained** — the holder is invisible to the
ledger. The probe's memory is **guest-held and tracked**: it sits in the context's object tables
where the ledger and the teardown sweep can both see it. Reproducing the second says nothing
directly about the first — but it does localize it, because it rules the tracked path *out*.

`vkchurn.py scanout-leak` binds a dedicated, exported `VkDeviceMemory` to each scanout image,
destroys the image, never frees the memory, and holds the process open. 20 images at 2560×1440:

```
13:33:00  ctx 5 [python3]: 281.2 MiB live, 562.5 MiB over 40 charges — 20 x 14.1 MiB (IOSurface, 40 ever)
13:33:53  ctx 5 [python3] destroyed — lifetime 40 charges totalling 562.5 MiB, peak 295.3 MiB   <- SIGKILL
13:34:00  census — 126.9 MiB live      (no python3 row; back to synoik's baseline)
```

40 charges = 20 IOSurfaces + 20 device memories (identical size, so one bucket). Live settles at
20 × 14.1 MiB: the images were destroyed and credited, the memories leaked and stayed. **That is
host-side retention driven purely by a guest-side leak**, with no compositor, no seated session
and no debug flag — the *tracked* class, not the storm's.

Note the probe never **writes** to these buffers, so it retains objects and address space rather
than resident pages: `Physical footprint` sat at 3.9G and vmmap stayed flat while the ledger
held 281 MiB, because IOSurface pages materialise on first write. The jetsam-relevant quantity
is resident pages, so any present-path extension of this probe must fill the buffer or it may
reproduce the retention without reproducing the footprint.

**And `kill -9` reclaimed all of it.** The teardown printed lifetime totals with **no residual
line**, and the next census was back to baseline. The generic recovery path works: a SIGKILLed
guest arrives at `vkr_context_destroy` with `ctx->instance` still live, so
`vkr_instance_destroy`'s sweep runs `vkr_device_memory_release` / `vkr_image_release` per
leftover object (`vkr_device.c:335`), which is what drops `imported_iosurface` and credits the
ledger. The bare-`free()` canary path below it only fires when the instance is *already* gone.

This makes the user's observation *consistent* with the kill test rather than contradicted by
it: the storm's bytes were **already credited** at kill time, so they were no longer in the
object tables and the sweep had nothing left to release for them. A working sweep and a
non-recovering compositor are the same story, once the holder is outside the tables.

So "killing the compositor reclaimed nothing" is **not** explained by leaked guest memory plus a
broken teardown. Both halves of that work. Whatever holds synoik's surfaces is
something this probe does not do — it allocates and binds, but never **presents**: no blob
export, no attach to a virgl resource, no hand-off to the supervisor over a Mach port. That is
where the search continues, and the probe is now the vehicle to extend rather than a dead end.

Scale check worth carrying: `SurfaceStore` (`crates/limina/src/window/present.rs:63`) is capped
at 32 and drops its `CFRetained` on eviction, so the supervisor can account for at most
~451 MiB. It cannot by itself explain 52 GiB.

### 0.5 A third repetition of the same instrument error — read the teardown, not the census

Three times now a conclusion was drawn from a census block that had no row for the probe:
"charged 0 IOSurfaces", "charged 1", "the allocations never reached the host allocator". All
three were false, and all three had the same cause: **a census with no row for a context is not
evidence about that context.** The row appears only if the sampler happens to tick while the
context is alive. The `destroyed — lifetime N charges` line is the one that cannot lie, because
it is emitted by the teardown itself.

Corollary for `vmmap`: `owned unmapped` sat at exactly **450.9M** across a 300-image run, a
20-image run, and an idle interval. It is a plateau, not a measurement. Trap #2 confirmed again
— do not use it at this scale; use the ledger.

### 0.6 Next: instrument the publish/release balance instead of reproducing

The synthetic route has answered what it can. The remaining suspects are all on the present
path, and there is already a real reproducer — synoik with `SYNOIK_VK_FULL_DAMAGE`, whose storm
data is in §0: **5889 IOSurfaces charged, 3 live, worker at 52.1 GiB.** 52 GiB / 14.1 MiB ≈ 3700
surfaces retained out of 5889 charged, which is consistent with most of them being held by a
reference taken *outside* the ledger's view.

So: count publishes against releases on the scanout hand-off (`limina_publish_surface` and the
supervisor's `SurfaceStore`), then run a **10-second** storm and read the balance. That names
the holder without needing a synthetic repro. Ten seconds is the budget — the storm grows the
host ~50 GB/min and the worker gets jetsam'd.

## The question

synoik's own investigation (the compositor guest's `synoik/vmm-memory-exhaustion-scanout-churn.md`,
2026-08-07) found the trigger and it is guest-side: a debugging flag,
`SYNOIK_VK_FULL_DAMAGE=1`, made `reset_buffer_ages()` run every frame, and smithay's
implementation *replaces* swapchain slots it cannot get a unique `Arc` to — which in a
running compositor is always. Result: 4 fresh 4K scanout buffers per frame, ~118/s, ≈3.9 GB/s
of 3840×2160×4 = 33,177,600 B images handed to venus. The crash session minted ~153 GB.

Their live set never exceeded 4 buffers (~133 MB). The host went to 50 GB and **stayed
there** — and, per the user, **killing the compositor did not give it back**, even after a
long wait. That is the host half of the question:

> Is host-side retention of freed/dead-context GPU memory expected, and can we recover once
> the offending guest process is gone?

## THE TRAP: venus suballocates, so a naive churn probe never reaches the host

`vkchurn.py` (this directory) allocates and frees with a bounded live set. Four runs against
the enhanced rig, censused with `vmmap`:

| probe | volume | result |
|---|---|---|
| `mem` mode, 33 MB × 1000, live 4 | 33 GB churned | flat |
| `image` mode, 33 MB × 1000, live 4 | 33 GB churned | flat |
| `vkfdcycle.py` × 400 (cross-context buffer export/import) | — | flat |
| `image` mode, 33 MB × 100000, live 60 | "3.3 TB" | flat |

The fourth line is the tell, and it is why none of the first three can be read as a negative:
**100,000 iterations of a 33 MB image completed in under a minute.** That is ~56 GB/s of host
allocation, which is impossible. The host was never allocating.

Mesa's venus **suballocates** device memory guest-side below a size threshold
(`vn_device_memory_alloc`, `src/virtio/vulkan/vn_device_memory.c`) — a plain, non-exported,
non-dedicated 33 MB allocation is carved out of a pool the guest already owns and never
reaches `vkr_dispatch_vkAllocateMemory`. Confirmed by the ledger: across all four runs the
probe's context never appeared in a census, while a 256 MiB allocation in the `vkr_budget`
L2 test *is* charged normally.

**So a probe intending to exercise the host allocator must defeat suballocation** — allocate
well above the threshold (256 MiB is known to work), or use the flags that force a real host
object: `VkExportMemoryAllocateInfo`, a dedicated allocation, or a DRM-format-modifier image.
synoik's scanout buffers reach the host precisely because they are exported, which is also
why 33 MB is the right size *there* and the wrong size in a naive probe.

Second trap, smaller: **`vmmap` is not sensitive enough here.** A 60-image live set (~2 GB)
moved `owned unmapped` from 16.1M/13 to 19.2M/38 and back — indistinguishable from noise.
`vmmap` found the original leak only because it had reached tens of GB. Use the ledger.

## The instrument that does work

`LIMINA_GPU_MEM_BUDGET_CENSUS=<seconds>` (virglrenderer, `docs/design/gpu-memory-budget.md`)
logs the per-context live total and an exact-size histogram on a timer. On this rig, against
a *healthy* synoik, it reads:

```
limina GPU budget: census — 126.9 MiB live of 16.0 GiB cap (0%)
limina GPU budget:   ctx 2 [synoik]: 126.9 MiB live — 4 x 14.1 MiB (IOSurface),
                     1 x 31.9 MiB (device memory), 1 x 31.6 MiB (device memory), ...
limina GPU budget:   ctx 4 [synoik]: 4096 B live — 1 x 4096 B (device memory)
```

`4 x 14.1 MiB (IOSurface)` is exactly synoik's 4-slot swapchain at 2560×1440 — the healthy
signature, named, per context, with no `vmmap` correlation needed. Under the storm the same
line should show that count climbing (or not), which settles guest-vs-host directly.

It pairs with synoik's own guest-side census (`synoik-vk/src/devmem.rs`, `5aa6674c`): guest
live flat + host live climbing localises retention to the host, and vice versa.

## The one solid result: a healthy teardown DOES release

`pkill -9 -x synoik` (SIGKILL — no clean Vulkan teardown, the `vkr_context_destroy`
bare-`free()` path):

| | total live | contexts |
|---|---|---|
| before | 126.9 MiB | ctx 2 [synoik], ctx 4 [synoik] |
| after | **4096 B** | ctx 3 [gnome-shell] only |

Both synoik contexts released everything, and no `destroyed with N still charged` line was
emitted. So the abrupt-exit path is **not** unconditionally broken — which makes the
reported "killing the compositor reclaimed nothing" more interesting, not less: something
about the storm state, not abrupt exit as such, is what retains.

## Next attempt

Reproduce the storm locally with the census attached — that is the missing run, and it needs
the flag the compositor bug actually depended on:

1. Boot the rig with the census: `LIMINA_GPU_MEM_BUDGET_CENSUS=15 LIMINA_DISK=… \
   spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`
2. `Environment=SYNOIK_VK_FULL_DAMAGE=1` in
   `~/.config/systemd/user/org.gnome.Shell@wayland.service.d/override.conf` (already added on
   this rig), then a **real GDM login** — the session must come up through the greeter to get
   a seat. `systemctl --user start org.gnome.Shell@wayland.service` is refused (manual start
   disabled), and `loginctl terminate-user` alone left the shell inactive. This is the same
   seat fight that cost three attempts when the rig was built.
3. Watch the census, then SIGKILL synoik and watch it again.

Reading the two censuses answers both halves: whether host live climbs with the storm, and
whether it comes back on client death.
