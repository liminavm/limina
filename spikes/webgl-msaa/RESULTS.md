# A WebGL page that asks for antialiasing loses the Vulkan device and kills the VMM

**Status:** OPEN. The failing work is named — the guest's own 4-sample render passes — and the
failure is stochastic, which governs how every arm here must be run.
**Vehicle:** `webgl-msaa.html`, self-contained (no network, generated texture). Three textured
cubes on a `webgl` context, drawn as indexed `TRIANGLES` with `UNSIGNED_SHORT` indices; `?aa=0`
requests `{antialias:false}` for the control arm.
**One arm, end to end:** `run-arm.sh <name>` — clones a pristine image, boots it, launches the
page in the seated session, watches, and declares the arm VOID unless the browser exists *and* a
multisampled blit reached vrend.
**Backlog entry:** `docs/hardening-backlog.md` §"A guest WebGL page that requests MSAA loses the
Vulkan device and aborts the VMM".

## What happens

An `{antialias:true}` context — which is also what `getContext('webgl')` with no options gives,
since `antialias` defaults to true in the spec — loses the device, usually within a couple of
minutes:

```
MESA: error: ZINK: vkQueueSubmit failed (VK_ERROR_DEVICE_LOST)
[LIMINA-ALLOC-POOL] class 0 grew to 65 allocators — in-flight depth is outrunning completion
   ... thousands of growth lines ...
VM stopped — worker terminated by signal 6
```

The pool runaway is **downstream**, not a second fault: once the device is lost nothing completes,
so no allocator ever drains, and `kk_alloc_pool_get` mints on every request by design. The abort is
downstream again — it lands in Apple's `IOGPUMetalCommandBufferStorageAllocResourceAtIndex`,
reached via `cs_get_compute` ← `kk_dispatch_precomp` ← `kk_draw`, which is AGX refusing to allocate
with thousands of live `MTLCommandAllocator`s outstanding. It is **not** zink's device-lost abort:
that is gated on `ZINK_HANG_ABORT`, default false.

`{antialias:false}` runs indefinitely with the frame still animating.

## MSAA is genuinely taken, which is new

The page prints what the driver **granted**, not what it asked for: `granted aa=true SAMPLES=4
SAMPLE_BUFFERS=1`. So the long-documented Firefox behaviour — MSAA backbuffer reports
`FRAMEBUFFER_INCOMPLETE_ATTACHMENT`, Firefox silently falls back to non-AA, cosmetic only — **does
not hold on the current KosmicKrisp stack.** That finding was measured on the retired MoltenVK
backend and was never re-measured on KK.

This is why the failure went unnoticed: pages that never take the MSAA path are fine, and until
MSAA started actually working there was no MSAA path to take. Any A/B here **must** read
`getContextAttributes().antialias`; "requested AA" is not evidence AA happened.

## The failure is stochastic, and that is the load-bearing fact

Four consecutive arms on the same build, same clone, same page, same resolve on the wire:

| arm | client | outcome |
|---|---|---|
| ssh-launched kiosk, fresh VM | sole | ~12 min, 16,512 AA frames, **survived** |
| session-launched kiosk, fresh VM | sole | 6 min, 23,456 AA frames, **survived** |
| session-launched kiosk, same VM after the above was stopped | second | lost, ~2 s after the transition |
| session-launched kiosk, fresh VM | sole | lost, ~25 s after its first resolve |

The last row refutes the row above it: a sole first client on a fresh VM both survives six minutes
and dies in twenty-five seconds, with nothing in the configuration separating them.

That kills every one-arm discriminator this investigation produced — display size, canvas size,
fullscreen, the Activities overview, the launch method, a preceding client's teardown, and the
"+66 to +85 s schedule" the archived logs seemed to show. It also disposes of the
launch-environment theory by measurement: dumped from `/proc/<pid>/environ`, the two launches are
**byte-identical**, twenty variables, empty `diff`.

**A single arm decides nothing here.** A hypothesis needs a survival rate over k repeats per side,
and the archived table below records outcomes that were read from single arms — treat every "both
die" in it as weak.

## What the GPU was actually doing

KosmicKrisp records every command buffer as it closes and marks the failing commit's own entries in
the device-loss report (`kk_limina_work_record`, dumped from `commit_callback`). Six arms, six
losses — five of them the unchanged configuration, one with the barrier scope widened — and the
marked work is the same every time:

| arm | Metal error | the failing commit held |
|---|---|---|
| probe1 | Hang, 86.0 ms | 3x `render 1280x720 s4 rts1 fmt37` |
| r1 | PageFault, 5.7 ms | 3x `render 1280x720 s4 rts1 fmt37` |
| r2 | Hang, 510 ms | 1x `render 1280x720 s4 rts1 fmt37` |
| r3 | PageFault, 6.4 ms | 3x `render 1280x636 s4 rts1 fmt37` |
| a1 | Hang, 46.1 ms | 3x `render 1280x720 s4 rts1 fmt37` |
| w1 | Hang, 46.9 ms | 3x `render 1280x720 s4 rts1 fmt37` |

`fmt37`/`fmt44` are `VK_FORMAT_R8G8B8A8_UNORM` and `B8G8R8A8_UNORM`. A frame is three 4-sample
RGBA8 passes, a compute buffer, then one single-sample BGRA8 pass at the same size — that last one
*is* vrend's shader blitter doing the resolve — and separately the compositor's own `2560x1440 s1`
passes:

```
      911  compute ops=2
 *    912  render 1280x720 s4 rts1 fmt37 c=0x862c61180 a=0x1547fe8000 d=130 ops=5
 *    913  render 1280x720 s4 rts1 fmt37 c=0x862c61180 a=0x1547fe8000 d=130 ops=5
 *    914  render 1280x720 s4 rts1 fmt37 c=0x862c61180 a=0x1547fe8000 d=130 ops=5
      915  compute ops=1
      916  render 1280x720 s1 rts1 fmt44 ops=5      <- the resolve
      919  render 2560x1440 s1 rts1 fmt44 ops=40    <- the compositor
```

**The faulting commit holds only multisampled passes — the guest's own antialiased rendering.
The resolve is never in it, and neither is the compositor.** The resolve is what *made* the content
multisampled and what led us to the shape, but the work the GPU dies on is the render into the
4-sample target, not the copy out of it. Both Metal error classes agree, which also retires the
idea that `Hang` and `PageFault` are two different bugs.

### The attachments are not dangling

`LIMINA_KK_ADDR_LOG=1` prints every heap-backed BO and every image plane at create and destroy with
its `[gpu, gpu+size)` range, stamped with the work sequence. In the failing arm both 4-sample
attachments — colour at `0x1547fe8000` (15,073,280 B, `1280x720 s4`) and its `D32_SFLOAT_S8_UINT`
companion at `0x1548e50000` — are allocated once, ~1,200 command buffers before the loss, and are
**never freed**; there is no `img-` or `bo-` event anywhere near the failing sequence range. The
render digest carries the attachment address for exactly this comparison, and it is the same
address in every frame of the run, healthy and failing alike.

So the leading "dangling attachment" reading is out. What the pass reaches *besides* its
attachments is where this has to go next.

### What the kernel says a fault was

`/Library/Logs/DiagnosticReports/gpuEvent-limina-vmm-*.ips` carries what Metal's error object does
not: `restart_reason_desc` and, for a page fault, the faulting VA. Across a day of arms, **every
fault is a read**, at page-table level 1 or 2, and the addresses are scattered across the whole
GPU VA space (`0x93bebcd440`, `0x252f204400`, `0xaaa79d000`, `0x3b9f87d000`, …). A dangling
attachment would fault at a clustered address; a scattered read fault is what an out-of-range
*fetch* looks like — an index or descriptor scaling into an arbitrary address. `restart_reason`
splits with the Metal error class: `BIF0 page fault` for PageFault, `MMU interrupt` for Hang.

These reports are written by an analytics daemon on its own schedule and did not appear at all
after a reboot in one session, so they are an oracle to check *later*, never to wait on.

## What the failing pass actually is

The digest names the pass's draws, so the failing work is read rather than inferred:

```
 *    959  render 1280x720 s4 rts1 fmt37 c=0xaae159e00 a=0x15494b8000 d=130 ops=5 draws=1 unroll=0
 *    960  render 1280x720 s4 rts1 fmt37 c=0xaae159e00 a=0x15494b8000 d=130 ops=5 draws=1 unroll=0
 *    961  render 1280x720 s4 rts1 fmt37 c=0xaae159e00 a=0x15494b8000 d=130 ops=5 draws=1 unroll=0
      963  render 1280x720 s1 rts1 fmt44 ops=5 draws=1 unroll=1     <- the resolve
      966  render 2560x1440 s1 rts1 fmt44 ops=40 draws=11 unroll=11 <- the compositor
```

**One indexed draw per pass, and none of them unrolled.** The failing work is a plain indexed draw
into a 4-sample RGBA8 target with a `D32_SFLOAT_S8_UINT` companion, reading the guest's own index
and vertex buffers. Three such passes per frame, one cube each.

That matters because the rest of the guest's GL traffic looks nothing like it. Per ten seconds KK
records `unroll_geometry calls=2561`, all issued mid-pass through the `pre_gfx` route, allocating
from a device-wide 128 MiB bump heap whose pointer is reset once per command buffer — a genuinely
suspicious arrangement, and the one this investigation spent four arms on. **It is not the failing
work.** The unrolls belong to the compositor and to vrend's resolve; the antialiased draws that
kill the device never touch that heap. `KK_LIMINA_HEAP_NORESET=1` accordingly does not stop the
loss, and the heap's bump pointer reads 0 in the report.

Nor is it an unclamped vertex fetch. `LIMINA_KK_FORCE_ROBUST=1` lowers robustness2 vertex-attribute
clamping into every pipeline whatever it asked for, and the device is still lost — though note that
this clamps the *attribute* fetch only: a garbage `firstIndex` or `indexCount` is not covered, so
this does not by itself retire out-of-range indices.

### Nothing concurrent, and no bad address

`LIMINA_KK_SERIALIZE_SUBMIT=1` commits one Metal command buffer at a time and blocks until its
completion callback fires — no timeouts, so the wait really held. **The device is still lost.** The
faulting work is therefore one render pass, one draw, alone on the GPU. That retires the whole
family of inter-commit races at once: the poly heap's reset, the `pre_gfx`/`gfx` split,
upload-pool and allocator reuse, a residency removal or a released texture landing under work
already in flight. Whatever the GPU reads badly is put there by the CPU before the pass is
committed.

`LIMINA_KK_ADDR_CHECK=1` looks up every address the draw hands the GPU — root table, index buffer,
the first two vertex bindings — in the BO address registry at encode, with a destroyed BO's entry
dropped so a freed range reads as unknown. On a losing arm it **reports nothing**: every address
the draw binds is live.

`LIMINA_KK_TEX_LEAK=1 LIMINA_KK_VIEW_LEAK=1` makes a stale resource ID unrepresentable — no
texture and no view is ever released — and the device is lost in **4 arms of 4**. A dead-ID scan
agrees from the other side: KK remembers every `MTLResourceID` that dies with its view and walks
the memory of every descriptor set a multisampled draw binds looking for one, and on a losing arm
it finds none.

### The fragment shader's texture sample is required

So the driver-side levers are exhausted, and the workload itself is where the ingredient is.
`?notex=1` swaps the fragment shader for one that returns a gradient and samples nothing,
changing nothing else — same three passes, same 4-sample RGBA8 target, same depth companion, same
resolve on the wire, `granted aa=true SAMPLES=4`.

**It survives 3 arms of 3** — the first with 22,880 frames and the cubes visibly rendering in the
capture. Against a baseline that loses roughly three arms in four, three survivals in a row is
p ≈ 0.016. This is the first lever in the investigation that changed the outcome at all, and it is
on the workload rather than in the driver.

The change is the sample and only the sample: the KK pass counts hold their shape across it
(`seen+LOAD` ≈ 2 × `seen+CLEAR`, `breaks_pass=0`, `restarts=0`), so the arm is not quietly
rendering a different frame graph.

### What a sample touches that a gradient does not

Reading the sampled-image path afterwards turns up one lifetime hazard that fits every constraint
the arms have left standing. A `COMBINED_IMAGE_SAMPLER` descriptor holds the view's own
`MTLResourceID` *and* a **16-bit index into a device-wide sampler table**
(`kk_descriptor_set.c:162`, `kk_descriptor_types.h:15`). The table is
`kk_query_table`: retiring an entry writes **0** into the GPU-visible slot and pushes the index
straight back onto the free list (`kk_query_table.c:kk_query_table_remove`), and
`kk_sampler_heap_remove_locked` calls it as soon as the last `VkSampler` reference goes away, with
nothing waiting on the command buffers already submitted against it. A draw still executing then
loads a zeroed — or recycled — sampler ID out of the table and dereferences it.

That path is reachable only from a shader that samples, and the dead-RID scan cannot see it,
because the descriptor carries an *index* and a recycled index is indistinguishable from a live
one. **It is not what kills this workload, though: the counter says zero slots are ever retired
during an arm**, so the free list is never exercised and the run never loads a zeroed sampler. The
hazard stands on its own and is filed in `docs/hardening-backlog.md`; the WebGL loss is not it.

The same arm reports the residency set at its death: **141 heaps, 0 buffers, 95 textures**. That is
small and steady, so set size is not the story either.

### What the sampling shader dereferences

`MESA_KK_DEBUG=msl` on a losing arm shows the chain a `texture2D` compiles to. The root buffer is
argument-table binding 0, rebound per draw; the sampler table is binding 1, bound once when the
command buffer is created and never changed:

```
t6  = &buf0.contents[0];                     // root
t8  = t6 + 864;
t9  = *(constant ulong*)(t8);                // the descriptor set's address, LOADED from the root
t16 = *(constant texture2d<float>*)t9;       // the view's resource ID, out of the descriptor
t18 = *(coherent device ushort*)(t9 + 8);    // sampler_index
t19 = sampler_table.handles[t18];            // a second, device-wide buffer
t20 = *(constant ulong*)(t9);
if (t20 != 0) t23 = t16.sample(t19, t4, bias(t10));
```

The texture handle is null-guarded; the sampler index is not. Three pointer chases the gradient
shader makes none of.

**A fragment shader that reads a uniform and samples nothing survives** (`?fsuniform=1`, 360 s), so
it is not fragment-stage descriptor access in general — the root → set → load chain is exercised
there too (that shader's uniform comes through `root + 848`), and by the vertex shader in every
surviving arm.

**Binding the sampler is not enough either.** `?texbound=1` declares the sampler, binds it, and puts
the sample behind a loop whose trip count is a uniform pinned to 0 — a loop rather than an `if`,
because NIR flattens a small `if` around a side-effect-free tex into a `bcsel` and runs it anyway.
The compiled MSL confirms the shape: the loop and the sample are still there, and the break comes
before the descriptor load. It **survives**, 22,944 frames at `SAMPLES=4`. So every CPU-side step —
zink writing the descriptor, binding the set, the sampler sitting in the device table — is
exercised in a surviving arm.

What is required is the **GPU actually walking the chain**: `root + 864` → the sampled-image
descriptor → the view's resource ID and `sampler_table.handles[idx]` → `sample()`. Nothing before
that dereference is sufficient.

## The faulting address is in nothing KK allocates, and it recurs across processes

The kernel writes `gpuEvent-limina-vmm-*.ips` under `/Library/Logs/DiagnosticReports/`. They are
readable directly — the files are `root:_analyticsusers` with group read, and the account is in
that group. Each carries `restart_reason_desc` and, for a page fault, `bif0_fault` with the
address, the page-table level, the direction, and **`requestor`/`sideband`**, which name the
hardware unit that asked.

Seventeen faults, every one a **read**, and every one exactly 64-byte aligned (five of them
4 KiB-aligned). Seventeen aligned values would be a remarkable coincidence for random bytes, so the
value being dereferenced looks well-formed rather than garbage — but this is **not established**:
a GPU fault reporter may simply record the access granule rather than the exact byte, and there is
no control sample on this host (the only two non-limina `gpuEvent-*.ips` files carry no address at
all). Treat "the pointer is well-formed" as a lead, not a fact.

The population, then:

| | |
|---|---|
| KK's own allocation band | `0x1500000000` … `0x1c44b90000` — **84.0 … 113.1 GiB** |
| what that band covers | every `kk_alloc_bo` (MTLHeap + MTLBuffer) **and** every locally created image plane; `LIMINA_KK_ADDR_LOG` logs `bo+`/`bo-` and `img+`/`img-` alike |
| what it does **not** cover | every import path in `kk_device_memory.c` — an IOSurface texture, a Metal heap, and a host pointer via `newBufferWithBytesNoCopy` — none of which is logged |
| faults inside that band | **0 of 17** |
| fault spread | 42.7 GiB … 808.6 GiB |
| requestor / sideband | 13 × `174/103`, 4 × `112`\|`96`\|`80` `/65` — two different units |

So the address is in no BO and no image plane KK allocates **locally**. Imported memory is a
separate population and the log does not cover it, which is the honest limit of this negative —
though the loss report's own residency census narrows it: `160 heaps, 0 buffers, 192 textures`.
A host-pointer import is the one thing that calls `kk_device_add_buffer_to_residency_set`, so at
the death there was **no live host-pointer import on the faulting device** at all.

The reading of the alignment below is likewise bounded:  That closes the stale/dangling/recycled
*resource* theories — a freed BO, a recycled slot or a dangling view would all fault inside the
band — and closes "one fixed buffer is not resident" (the sampler table at `0x15000b0000` is one
range, not seventeen). It does **not** make the address garbage: the band excludes everything
Metal allocates for itself (argument tables, texture descriptors, the sampler heap), and a fault
there would look exactly like this.

The distribution says the address is structured, not random. Across **different processes**, minutes
to half an hour apart:

| pair | apart |
|---|---|
| 643.78 / 643.80 GiB, both requestor 112 | **22.6 MiB** |
| 238.24 / 238.49 GiB | 256 MiB |
| 590.70 / 590.98 GiB | 289 MiB |

Seventeen uniform draws over that range would give well under one such pair; three, one of them
23 MiB, is a deterministic allocator putting a real object at a stable VA — a region that has no
mapping *at that instant*, rather than a wild pointer.

### What the rest of the report says

The files had only ever been grepped for three fields. Parsed whole (one JSON header line, then a
JSON body), the `analysis` object carries more, and it is consistent across all 23 reports:

| field | every report |
|---|---|
| `guilty_dm` | **2** — the same data master every time, page faults and MMU interrupts alike |
| `signature` | 562 for a page fault, 674 for an MMU interrupt — the two classes, nothing finer |
| `command_buffer_trace_id` | a distinct increasing id per event; no Metal API hands this back, so it cannot yet be matched to a command buffer |
| `bif0_fault.level` | **1 or 2** |
| `registers` | empty |

`level` is the page-table level at which the walk failed, and 1 or 2 means it failed **high** — the
address has no entry at all, not a leaf that lost its mapping. That distinction matters more than
anything else in the report: a resource whose pages were evicted, or a texture dropped from the
residency set, faults at the leaf. Every fault here is in a region the GPU page tables never
described. Which is why every residency arm has failed to change anything, and why the un-resident
minted views could not have been the cause.

Taken with the 64-byte alignment, the shape that fits is **a valid base plus a wild offset** — a
fetch addressed off a real allocation with a stride, index or extent that is wrong by orders of
magnitude — rather than a corrupt pointer or an evicted page. That is a lead, not a conclusion:
the alignment may be the reporter's granule, and the `level` encoding is not documented.

Cross-referencing closes the local half for good: `fault-vs-addrlog.py` run over the fully
instrumented arm's log — **16932 logged ranges spanning 0 … 112.6 GiB** — matches **0 of the 18
addressed faults**. The import paths added no ranges to that map, because an imported IOSurface
texture has no GPU address any Metal API returns; that population stays unaddressable from here.

The daemon lags: no `.ips` appeared for six losses between 21:36 and 22:20 while the newest file
was 13:27, and some sessions produce none at all. Read them later and match by timestamp; never
wait on one.

## …and the chain is clean when it is recorded

`LIMINA_KK_ADDR_CHECK=1` now walks, at every multisampled draw, the chain the generated MSL walks —
and reads it out of the **uploaded root buffer's own bytes** at `offsetof(root, sets) + i*8`, not
the CPU struct the upload was made from, so a root uploaded stale or bound from another draw would
show. Sampled-image slots are visited by layout, so each reported value is the one the GPU will
dereference; every texture resource ID KK mints is registered at view create.

Through a full death: **`ROOTSKEW` 0, `ALIENRID` 0, `BADSAMPIDX` 0, `BADADDR` 0, `DEADRID` 0.**

A silent check is only evidence if the walk reached the slots, so it counts what it inspected and
the loss report prints it. A dying arm (`nd1`):

    chain check: 108 sets walked by layout, 54 sampled slots inspected,
                 0 slots past set size, max slot offset 0

The slots were visited, none was skipped for being past the set size, and the sampler's descriptor
sits at offset 0 of its own set — binding 128 is a binding *number*, not a byte offset. So the
result stands: the descriptor chain is consistent when it is written, every ID in it is one KK
minted, every sampler index is in range, and serialising submits already excluded anything
changing it after the record. **The faulting address is not in the bytes we write.** It is
produced downstream of them — in the table lookup Metal owns behind a `MTLResourceID`, whose
entries mean something only while the allocation they name is resident.

Keep the counters in any future check. This negative was reported once before the counters existed,
while the layout walk was bounded by a size clamped to 4 KiB — the number could not then say
whether the walk had reached anything at all.

## The bytes are still right when the device dies

The encode-time walk proves what was written. It cannot tell that apart from something overwriting
it afterwards — serialising submits excludes a CPU race between commits, but not a GPU write from
an earlier command buffer landing in descriptor memory. So the loss callback now re-reads the same
bytes: the last sixteen multisampled draws' root address, set address, resource ID and sampler
index are kept, and each is read back at the loss.

First, the walk is reading the right bytes. The generated MSL loads `t9 = *(constant ulong*)(root
+ 864)`; the run prints

    KK draw address checking ON; root.sets at +848
    [LIMINA-SLOT] set2 (root+864) binding128[0] +0 id=0x118 samp=2 (pass 1280x720 s4)

`(864 − 848) / 8 = 2`, so the slot inspected is exactly the one the shader dereferences — set 2,
offset 0, which is zink's sampler-view set. The clean-chain negative is not hollow.

Then, at the death:

    draw[-12] s4 set2: at encode set=0x1512ae9cc0 id=0x118 samp=2 | now set=0x1512ae9cc0 id=0x118 samp=2 | sampler handle 0x1f
    …
    draw[-1]  s4 set2: at encode set=0x15125f1cc0 id=0x118 samp=2 | now set=0x15125f1cc0 id=0x118 samp=2 | sampler handle 0x1f

**Twelve draws unchanged**, covering the three failing passes and the three frames before them: same
set address in the root buffer, same resource ID, same sampler index, and the sampler table entry
that index selects is a live `0x1f`. (`draw[-16]`…`draw[-13]` read back as `set=0x0` or as a later
descriptor — that is the upload pool wrapping and rewriting those addresses, which is what it is
for. `KK_LIMINA_HEAP_NORESET=1` freezes the poly heap, not the descriptor pool.)

So every byte on the path from the root table to the sampler is correct when written **and** still
correct when the GPU faults. Combined with the fault landing in no allocation of ours, the whole
CPU-visible half of this bug is exonerated: a valid, live resource ID, dereferenced by the texture
unit, resolves to an address in no page table.

**That ends the knobs.** There is no remaining byte in our stack to check, and no A/B that can
narrow it further — the next thing worth building is a repro without the VM (see *Toward a
no-VM repro* below).

## The multisampled depth companion is not an ingredient

`?nodepth=1` — a WebGL context requested with `depth:false`, no depth test — **dies**, and the
failing commit is the same three `render 1280x720 s4 rts1 fmt37 … draws=1 unroll=0` passes, now
recorded with `d=0`: no depth attachment exists at all.

So the conjunction is not "sample + 4-sample D32S8". `?notex=1`, `?fsuniform=1` and `?texbound=1`
make the executed texture sample *necessary*; this makes the depth companion *irrelevant*. What is
left of the failing draw's shape is a fragment shader that actually executes a `sample()` into a
4-sample colour target.

## The blit, and how the route was read

`LIMINA_VREND_TRACE=256` on the killing configuration: in a 31-second window, 3,220 draws, 248
framebuffer changes, and **134 BLITs, every one the same shape**:

```
src  res=1209  1280x720  nr_samples=4  format=67  bind=0xa
dst  res=1217/1218/…     1280x720  nr_samples=0  format=1  bind=0x10000a
```

An explicit multisample resolve, one 4-sample colour texture into each of six rotating
single-sample destinations carrying the shared/scanout bind bit. The whole guest run contains
exactly two multisampled resources: that colour target and its 4-sample depth companion.

**Source and destination formats differ, and that decides the route.** On a GLES host,
`vrend_renderer_prepare_blit` returns false for an MS-source RGBA blit whose
`src.format != dst.format`, so the blit falls through to `vrend_renderer_blit_gl`, the shader
blitter, in its own GL context (`third_party/virglrenderer/src/vrend/vrend_renderer.c:12751-12764`,
dispatch at `:12965-12973`). Read directly from the renderer with `LIMINA_VREND_BLIT_LOG=1`:

```
FBO   src=fmt67/s0 2560x1440 -> dst=fmt67/s0 2560x1440  (redblue_or_fmt=0 …)
GLFB  src=fmt67/s4 1280x720  -> dst=fmt1/s0  1280x720   (redblue_or_fmt=1 …)
```

The desktop's compositing blit stays on the FBO path; every MSAA resolve takes the shader blitter.
Since the failing commit never contains the resolve, this now describes *how the content gets
multisampled*, not the fault.

**`LIMINA_VREND_FORCE_FBO_BLIT=1` returns no reading.** Forced onto the FBO path the device dies
within a second — but forcing it is itself invalid usage (an MS-source `glBlitFramebuffer` between
differing formats is what the predicate exists to refuse), so its fault may be the knob's own. A
readable version has to make the blit legal rather than merely allowed: view the destination as
fmt 67 through `vrend_make_view`.

**The probes measured a different path.** `host-msaa-loop.c` and `guest-msaa-present.c` both use
`EXT_multisampled_render_to_texture` — implicit MSAA, zink's shadow-attachment emulation, no
`VIRGL_CCMD_BLIT` at all. Their negatives constrain that path and say nothing about this bug.

## What the validator's reports do and do not say

The dominant report class is a texture-type mismatch (`MTLTextureType2DArray bound … expected
MTLTextureType2D`). It is **chronic** — it fires on many pipelines in every run, a healthy desktop
included — **bidirectional**, and carries no texture identity, address or binding index, so a
report alone can never name the resource that produced it. Nothing in this class currently names a
faulting access.

One write in the descriptor log is worth chasing: a `COMBINED_IMAGE_SAMPLER` naming a
`VK_IMAGE_VIEW_TYPE_2D` view of a **4-sample** image (`id=0x117`, `mtl_type=4`). The log records
the descriptor *write*, not who reads it, and that write is perfectly legal — u_blitter's MSAA
resolve consumes exactly this through `texture2DMS`. It is a violation only if a shader declaring
`texture2d<float>` reads it, which no descriptor byte can show. `[LIMINA-MSBIND]` in the draw walk
is the test: it fires only in multisampled passes, so the resolve is never walked, and the cube
pass's only legitimate sampled image is the 64x64 checker (`id=0xa3`, `mtl_type=2`).

**It never fires.** A death (`ms1`) with the check live:

    chain check: 24 multisampled draws walked, 0 multisampled textures bound as sampled images

Every sampled slot the failing pass binds names a single-sample texture. So `0x117` is not read by
the cube shader, and the mistyped-read theory is dead: the descriptor bound at the failing draw is
the checker, correctly typed.

**Pipelines are self-identifying.** KosmicKrisp labels each `MTLRenderPipelineState` with a hash of
its generated MSL, and `KK_LIMINA_SHADER_DUMP` names its files by the same hash. The label must be
content-derived: it is hashed into the UID Metal reports against, so a per-run pointer would make
every UID per-run too.

## Arms run

Everything above the rule is a rate over repeats; everything below it was read from single arms and
is weak by the stochastic finding.

| arm | repeats | result |
|---|---|---|
| baseline, unchanged configuration | 5 | 5 lost |
| `KK_LIMINA_HEAP_NORESET=1` — the shared bump heap never recycled | 4 | 1 survived (226k unrolls), 3 lost |
| `LIMINA_KK_FORCE_ROBUST=1` — every vertex fetch clamped to its range | 3 | 1 survived, 2 lost |
| `LIMINA_KK_SERIALIZE_SUBMIT=1` — one command buffer on the GPU at a time | 2 | 2 lost |
| `LIMINA_KK_ADDR_CHECK=1` — every bound address looked up at encode | 1 | lost, **nothing reported** |
| `LIMINA_KK_TEX_LEAK=1 LIMINA_KK_VIEW_LEAK=1` — no texture or view ever released | 4 | 4 lost |
| `?notex=1` — the fragment shader samples nothing | 3 | **3 survived** |
| `LIMINA_KK_SAMPLER_LEAK=1` — no sampler slot ever retired | 1 | lost, **0 retirements to suppress** |
| `?fsuniform=1` — the fragment shader reads a uniform, samples nothing | 1 | **survived** |
| `?texbound=1` — sampler bound and reachable, sample never executed | 1 | **survived**, 22,944 frames |
| `LIMINA_KK_SAMPTAB_RESIDENT=1` — sampler table pinned in the residency set | 1 | lost (and the fault spread had already refuted it) |
| `LIMINA_KK_ADDR_CHECK=1` with the full chain walk | 1 | lost, **every check silent** |
| `KK_LIMINA_BARRIER=widen` (pre_gfx barrier scope ALL) | 1 | lost |
| `?nodepth=1` — no depth buffer, no depth test, `d=0` in the failing pass | 1 | lost |
| `LIMINA_KK_TEX_LEAK=1` — also suppresses the texture's residency-set removal | 4 | 4 lost |
| `LIMINA_KK_BO_LEAK=1` — also suppresses the BO's residency-set removal | 1 | lost |
| the chain walk again, now counting what it inspected | 1 | lost, 54 slots inspected, **all silent** |
| `[LIMINA-MSBIND]` — is a multisampled texture bound as a sampled image at the failing draw | 1 | lost, **0 of 24 draws** |
| post-mortem re-read of the descriptor bytes at the loss | 1 | lost, **12 of 12 draws unchanged** |
| --- | | |
| KK revision: pinned `552edc3f62f` vs two commits older | 1 each | both die |
| scanout path: windowed vs `--display-capture` | 1 each | both die |
| guest tier: stock F44 vs enhanced F44 | 1 each | both die |
| context attributes: AA vs `{antialias:false}` | 1 each | only AA dies |
| `LIMINA_ZINK_NO_FANS=1` | 1 | dies |
| `LIMINA_KK_ALLOC_DESTROY=0` (34 retirements → 0) | 1 | dies |
| `LIMINA_KK_BO_LEAK=1` — nothing released or de-resident | 1 | dies |
| `LIMINA_KK_VIEW_LEAK=1` — no view released | 1 | dies |
| input/render/subres views registered in the residency set (`kk_image_view.c`) | 1 | **lost at 0 s** — the un-resident minted views are a real defect, not this one |
| every instrument at once (`ADDR_CHECK` + `ADDR_LOG` + `IMPORT_TRACE`) | 1 | lost at 0 s; 132 texture imports, **0 host-pointer and 0 heap imports**, chain clean over the failing passes |
| nil texture views (`mtl_new_texture_view_with` → nil) | 3 | zero |
| Metal render-pass resolve (upstream `db5ab8de776`) | 1 | **never runs** |
| the zink shadow blit alone, no VM | 1 | **no mismatch** |
| `host-msaa-loop`, 2560x1440, 4 samples, no VM | 1 | 20,520 frames, survives |
| the same, `--churn 30` | 1 | 30,712 frames, survives |
| `msaa-loop` in the guest over virgl, MSAA granted | 1 | 226,909 frames, survives |
| `guest-msaa-present --fullscreen --mode surface` | 1 | 144,037 frames, survives |
| `guest-msaa-present --fullscreen --mode msrtt` | 1 | 139,000+ frames, survives |
| Firefox's native compositor (`gfx.webrender.compositor=false`) | 1 | dies in 65 s |

**No Vulkan resolve is involved, in either implementation.** Upstream replaced KK's meta/shader
resolve with Metal's native render-pass resolve (MR 43216); ported onto `limina-kk` it does not
help, and instrumenting it shows why: `kk_attachment_do_renderpass_resolve` never sees an
attachment with `resolve_mode != VK_RESOLVE_MODE_NONE` on this workload. zink emulates
`EXT_multisampled_render_to_texture` with a `util_blitter` draw, so the multisample traffic is
ordinary rendering to and sampling from a 4-sample texture.

## A GPU capture of the failing passes exists; Xcode cannot read it

The capture works. `spikes/webgl-msaa/traces/` holds a ~760 MB `.gputrace` covering the three
multisampled passes, taken one frame before an arm that then faulted on the *same* attachment and
the *same* command-buffer object — a byte-identical iteration of the work that dies.

**Xcode cannot replay it, and the reason is unrelated to this bug.** The replayer SIGSEGVs at
`KERN_INVALID_ADDRESS 0xe0` inside Apple's own shader compiler
(`AGXMetalG13X`, `createVertexProgramVariant`) while loading the trace, behind 25 failures of
`-[MTL4Compiler newLibraryWithDescriptor:error:]`. The MSL the trace hands back is **mangled**:
identifiers have lost leading bytes at varying offsets (`at4` for `float4`, `ong` for `long`,
`ype` for `type`), which chews up the function signatures and leaves 5828 statements at program
scope. Every library then fails to build and the driver dereferences a null pipeline instead of
reporting the error. Artefacts: `traces/cap3-replay-errors.txt`, `traces/cap3-replay-crash.ips`.

A replayer crash is therefore **not** evidence about the workload, and must not be read as one.
The trace records the shaders wrongly; nothing about the recorded *fault* is being reproduced.

Making a capture, with the lever in `kk_limina_capture.c`:

```
MTL_CAPTURE_ENABLED=1 KK_LIMINA_CAPTURE=any KK_LIMINA_CAPTURE_SAMPLES=4 \
KK_LIMINA_CAPTURE_ARM=any KK_LIMINA_CAPTURE_REPEAT=1 KK_LIMINA_CAPTURE_PASSES=3 \
KK_LIMINA_CAPTURE_MAX_CBS=20 KK_LIMINA_CAPTURE_DIR=<dir> spikes/webgl-msaa/run-arm.sh <name>
```

Two things make that recipe work where the obvious one does not:

- **Arm on the sample count, never on the extent.** The browser's window layout picks the canvas
  size, so consecutive arms of the same repro rendered 1280x636 and 1280x720. An extent read off
  the previous arm's dump is a lottery; `s4` is not, because only the antialiased canvas passes are
  multisampled and everything the compositor draws is `s1`.
- **`_REPEAT=1` opens the window on the second render into an attachment already seen**, which is
  exactly the second of the three passes — and because the pass is noted before its Metal command
  buffer exists, that pass is inside the trace rather than just before it.

The capture is itself a synchronisation, so the captured frame usually survives and the arm dies a
frame or two later. That is the useful case, not a failure of aim.

Writing the trace to a directory does **not** segfault on this command stream, unlike the
notification-text one the lever was built for, so no attached Xcode is needed to record.

## The host loop that does not reproduce it

`spikes/webgl-msaa/host-msaa-loop.c` is the attempt: GLES-over-EGL on the same host
zink-on-KK, so it exercises the same driver without a guest, a vrend context or an
IOSurface scanout. It already renders into an implicitly-multisampled texture, samples a
64x64 LINEAR texture from the fragment shader inside that pass, and composites the resolved
result in a second single-sample pass.

The shape it has to match, read off the loss dumps:

| the guest does | the loop does |
|---|---|
| three `1280x720 s4 rts1 fmt37` passes per frame, each LOAD + **one** textured draw | `--pass-per-draw` (a `glFlush()` after each of the three draws) |
| an `s1 fmt44` pass reading the multisampled texture as 2DMS | composite pass, but it samples the **resolved** texture |
| a `2560x1440` composite | `--size` |
| many zink contexts and venus rings on one `VkDevice` | one context |
| textures backed by guest memory, transferred in | locally created |

**It survives everything tried so far**: 111322 frames at 885 fps with `--pass-per-draw`, and
20k frames before that without. So neither the sampled draw into a 4-sample target nor the
pass boundary is the ingredient by itself — which is a real narrowing, because both were
prime suspects. What is left in the table above is the 2DMS read and the multi-context
environment, and the environment is the harder one to bring across.

The loop is cheap (no boot, no image clone, and it strands no host memory), so it is the
right place to test any theory that does not need the guest.

## What the imports turn out to be

With every import path logged, a losing run carries **132 `import-tex+`** — IOSurface and Metal
texture imports, which is the vrend/venus "IOSurface world" — and **zero `import-heap+` and zero
`import-host+`**. Nothing on this route aliases host or guest pages into the GPU, which kills the
whole family of theories where a CPU-side `madvise`, a balloon reclaim or a guest free changes
pages under a live GPU mapping. (These arms run at a fixed `--ram-mib` with no balloon range and
free-page reporting off by default, so there was no reclaim to do it anyway.) It agrees with the
residency census: no live host-pointer import means no `kk_device_add_buffer_to_residency_set`,
which is why the census reads `0 buffers`.

An imported texture has no GPU address any Metal API will hand back, so `import-tex+` records the
handle and the size and cannot place a fault inside it. That is the current hole in the address
map, and it is the population the fault is most likely to belong to.

`spikes/webgl-msaa/fault-vs-addrlog.py` cross-references the `.ips` faults against every logged
range; run it against a losing arm's `worker.log` the day after, once the reports land.

## Where to look next

1. **The bindless texture read.** The attachments are alive, the draw is not unrolled, and vertex
   fetch clamping does not help — so of everything a single indexed draw touches, the descriptor's
   texture `gpuResourceID` is the one that can outlive what it names. Log image-view create and
   destroy with their sampled/storage resource ids alongside the existing `img+`/`img-` lines, and
   run a "no texture ever released" arm the way `LIMINA_KK_BO_LEAK` did for buffers.
2. **Get the faulting VA and the allocation log into the same frame.** `LIMINA_KK_ADDR_LOG=1` plus
   the `.ips` VA names the resource outright. The log is in place; the reports arrive late.
3. **Forward zink's debug labels to the Metal encoder.** KK advertises `EXT_debug_utils` but only
   labels encoders for capture; `MESA_TRACE=markers` makes zink emit `blit_resolve(...)` labels,
   and passing them through would make the report name the operation.

Arms no longer worth running: anything varying display size, canvas size or fullscreen.

## Method rules this bug earned

- **One arm decides nothing when the failure is stochastic, and you will not know it is stochastic
  until you repeat an arm.** Every discriminator here was tried once per side and each looked
  decisive until the same configuration produced the opposite outcome.
- **The workload must be shown running before a survival is read.** A survival is the reading a
  broken arm produces for free: no browser, no session, wrong driver, all look like health.
  `run-arm.sh` therefore checks the browser process *and* a multisampled blit on the wire, and
  exits 75 (VOID) if either is missing. Two arms in one session reported "survived 361 s" with no
  browser at all.
- **`sshd` answering is not "the desktop is up".** Wait for `/run/user/1000/wayland-0` before
  launching a client into the session.
- **Never edit a script while it is running.** bash re-reads from a byte offset; an edit mid-run
  corrupts the rest of the arm (`line 76: e: command not found`).
- **Copy the capture before killing the VM.** Killing the supervisor shuts the guest down, and the
  capture is overwritten once a second, so the file left behind otherwise shows systemd stopping
  units — for a survival and a death alike.
- **A validator class that fires on healthy frames explains nothing.** Diff *instances* between a
  failing and a passing half of the same run, and check the halves are what they claim.
- **Relaunching a browser does not reset it.** Session restore makes the second half run both
  pages; give each half `--profile <dir> --new-instance`, and create the directory.
- **Never change two variables to make an arm cheaper.** Dropping the display size to make
  validation affordable silently moved the run to a configuration that does not crash.
- **A label can perturb what it names.** A pipeline label is hashed into the pipeline UID.
- **A liveness check must not match itself.** Bracket the pattern (`'[l]imina-vmm'`), and confirm a
  survival against the worker log, not against the watcher that was supposed to notice.
- **Read the size the workload actually got.** A fullscreen kiosk on a 2560x1440 display at scale 2
  is 1280x720 logical; a table built without the drawing-buffer size inverted its conclusion.
- **Prove the mask is not the slowdown.** Validation costs ~10x, and full shader validation is not
  survivable on this workload for more than a couple of minutes (the slowed GPU makes the allocator
  pool run away and the worker aborts). Use selective validation.

## Reproducing

```
LIMINA_KK_ADDR_LOG=1 spikes/webgl-msaa/run-arm.sh <name>
```

That is the whole arm: it clones `Fedora-Workstation-44.enhanced.test.raw`, boots it at
`LIMINA_RAM_MIB=3072 LIMINA_CPUS=4 --display-size 2560x1440`, waits for the session, launches the
page through `systemd-run --user`, watches for `LIMINA-DEVICE-LOST`, and prints the report with the
failing commit's command buffers marked. Artefacts land in `/tmp/webgl-msaa-<name>/` (worker log,
live capture, `vm_stat` either side). Exit 75 means VOID, not survived.

The KK-side knobs — `LIMINA_KK_ADDR_LOG`, `KK_LIMINA_HEAP_NORESET`, `KK_LIMINA_BARRIER`,
`LIMINA_KK_ALLOC_DESTROY`, `LIMINA_KK_BO_LEAK`, `LIMINA_KK_VIEW_LEAK`, `LIMINA_KK_DESCLOG`,
`KK_LIMINA_SHADER_DUMP`, `LIMINA_KK_LABELS` — all live on the `limina-kk` branch of
`/Volumes/mesa-cs/mesa`, and each prints a line when engaged. Read that line: an arm whose lever
cannot be observed in the log is worse than no arm.

**Survival happens by chance — about one arm in four.** A single survival is not a result and a
rate under five is barely one. Budget three arms per side, and prefer an instrument that reads the
failure deterministically over an A/B that needs repeats.

**Keep the guest small — the arms poison the host, whatever their outcome.** Compressor occupancy
doubled per arm (203k → 406k → 810k → 1,014k pages) on 8 GiB guests and a day of arms panicked the
machine on compressor-segment exhaustion; at 3 GiB it still grows by 40k pages on a short losing
arm and **208k on a full six-minute survival** — the cost tracks how long the VM ran, not how it
ended, and a survival is the *expensive* outcome. Budget about four arms per reboot, record
`vm_stat` around each (`run-arm.sh` does), and ask for a reboot at ~900k pages occupied.

**A fix must show all four:** the page for ≥5 minutes across repeats, no `DEVICE_LOST`, no nil
views, and the cubes visibly rendering in the capture. Black cubes with no fault is the validation
mask, not a fix.
