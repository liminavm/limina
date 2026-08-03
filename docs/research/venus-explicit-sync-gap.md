# Venus external-semaphore / explicit-sync gap — VM-stack handoff

**Audience:** the Claude session that works on the *VM / host graphics stack* (guest Mesa
`venus`, `virglrenderer`'s Venus backend, the VMM's virtio-gpu device, host Vulkan). Written
from inside the guest (`gnome-shell-rs` dev VM) on 2026-07-04.

**One-line summary:** a Vulkan probe found that this stack exposes **only `SYNC_FD` *binary*
external semaphores** (export+import); **`OPAQUE_FD` and *timeline* external semaphores are
absent**. That sounds fatal for modern explicit sync, but it very likely **is not**: the guest
**kernel DRM node already advertises `drm_syncobj` *timeline* support**, and the same probe
returns **identical results on lavapipe (pure-CPU llvmpipe)**, so this is not a virtio-specific
regression — it's the normal state of these drivers. The portable explicit-sync path
(kernel `drm_syncobj` timeline ⟷ `sync_file` ⟷ binary `SYNC_FD` VkSemaphore) uses only what is
present. Read §5–§6 before touching anything: **do not chase "make `OPAQUE_FD` cross virtio" —
that is an architectural dead end and is not needed.**

---

## 1. Why this matters

The `gnome-shell-rs` fork is building an owned Vulkan renderer (currently a Stage-0 offscreen
spike; GLES still daily-drives). Two later stages depend on GPU↔GPU and GPU↔KMS
synchronization:

- **Wayland explicit sync** — the `linux-drm-syncobj-v1` protocol (what mutter, wlroots/niri,
  kwin implement). Clients hand the compositor `drm_syncobj` **timeline** acquire/release
  points per surface commit; the compositor must wait on / signal those points against its
  renderer's GPU work.
- **KMS explicit sync** — atomic commit `IN_FENCE_FD` (wait before scanout) and
  `OUT_FENCE_PTR` (signal on flip), plus per-plane `drm_syncobj` properties on newer kernels.

Both require bridging the compositor's Vulkan submissions to kernel fence/syncobj objects. The
probe below measures exactly which bridges Vulkan gives us here.

---

## 2. Environment fingerprint (this exact stack)

| Layer | Value |
|---|---|
| Guest kernel | `Linux 7.1.2-limina16k #1 SMP PREEMPT_DYNAMIC aarch64` |
| `systemd-detect-virt` | `vm-other` (VMM not auto-identified; Apple-Silicon host) |
| virtio-gpu transport | `virtio_mmio` (`a008000.virtio_mmio`), `virtio_gpu 0.1.0`, DRM features: **`+context_init`** |
| DRM nodes | `card0` (connector `card0-Virtual-1`), `renderD128`; DRM core `1.1.0` |
| Vulkan driver | `driverID = DRIVER_ID_MESA_VENUS`, `driverName = venus`, `driverInfo = Mesa 26.1.3` |
| Vulkan device | `Virtio-GPU Venus (Apple M4 Pro)`; nested host device name `Apple M4 Pro` |
| Vulkan API | `1.3.353` |
| Host GPU | Apple M4 Pro (host-side Vulkan reached through virtio-gpu Venus) |

`+context_init` (= `VIRTIO_GPU_F_CONTEXT_INIT`) is the feature Venus rides on, so the Venus
data path itself is up. `renderD128` is world-rw; `Seccomp: 0`.

---

## 3. What was measured, and how to reproduce it

### 3a. Vulkan external-semaphore capabilities (the gap)

Measured with `vkGetPhysicalDeviceExternalSemaphoreProperties` for the cross product
{`OPAQUE_FD`, `SYNC_FD`} × {binary, timeline}, reading
`externalSemaphoreFeatures & {EXPORTABLE, IMPORTABLE}`. Source lives in this repo at
`vk-spike/src/probes.rs` (`external_semaphores()`); the timeline variant is requested by
chaining `VkSemaphoreTypeCreateInfo{ semaphoreType = TIMELINE }` into the query's `pNext`.

**Result (Venus — and byte-for-byte identical on lavapipe):**

```
external semaphore handle types (export/import):
    OPAQUE_FD binary:   export=false import=false
    OPAQUE_FD timeline: export=false import=false
    SYNC_FD   binary:   export=true  import=true      <-- the only usable bridge
    SYNC_FD   timeline: export=false import=false
```

Reproduce (from the guest):

```sh
# Venus (default ICD on this VM):
cargo run -p vk-spike        # prints the "probes" block at the end
# lavapipe (deterministic CPU baseline — shows the SAME semaphore result):
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json cargo run -p vk-spike
```

`vulkaninfo` in this Mesa build does **not** print an external-semaphore section, so the probe
is the source of truth. (If you rebuild `vulkaninfo`/Mesa, the same data appears under
`VkPhysicalDeviceExternalSemaphoreInfo` per handle type.)

### 3b. Extensions ARE present (this is the subtle part)

The relevant extensions are all advertised — the semaphores *exist and work in-process*; only
their **external export/import** is missing:

```
VK_KHR_external_semaphore_fd            rev 1     VK_KHR_timeline_semaphore     rev 2
VK_KHR_external_fence_fd                rev 1     VK_KHR_synchronization2       rev 1
VK_KHR_external_memory_fd               rev 1     VK_EXT_external_memory_dma_buf rev 1
VK_EXT_image_drm_format_modifier        rev 2
```

So: you *can* create timeline semaphores and use `VK_KHR_external_semaphore_fd`; you just
**cannot export a timeline semaphore, and cannot use `OPAQUE_FD` at all.**

### 3c. Kernel DRM syncobj capabilities (the mitigating finding)

Queried `DRM_IOCTL_GET_CAP` on both DRM nodes:

```
/dev/dri/renderD128 and /dev/dri/card0:
  DRM_CAP_PRIME            = 3   (IMPORT|EXPORT — dmabuf both directions)
  DRM_CAP_SYNCOBJ          = 1   (drm_syncobj supported)
  DRM_CAP_SYNCOBJ_TIMELINE = 1   (TIMELINE drm_syncobj supported)   <-- key
```

Reproduce (no extra tooling; pure ioctl):

```python
import fcntl, os, struct
REQ = (3<<30)|(16<<16)|(0x64<<8)|0x0c   # DRM_IOWR('d',0x0c, drm_get_cap{u64 cap;u64 val})
for name,cap in [("SYNCOBJ",0x13),("SYNCOBJ_TIMELINE",0x14),("PRIME",0x05)]:
    fd=os.open("/dev/dri/renderD128",os.O_RDWR)
    _,val=struct.unpack("<QQ", fcntl.ioctl(fd,REQ,struct.pack("<QQ",cap,0)))
    print(name,val); os.close(fd)
```

(`drm_info`, if you install it, prints the same under the node's capabilities.)

---

## 4. Interpreting the results — two capability domains

These are **different things** and it is easy to conflate them:

**(A) Vulkan external-semaphore export/import** (§3a) — lets the *renderer* turn a VkSemaphore
into an FD and vice-versa. Here: only binary `SYNC_FD`.

**(B) Kernel `drm_syncobj` (incl. timeline)** (§3c) — the objects the `linux-drm-syncobj-v1`
Wayland protocol and KMS atomic properties are defined in terms of. Here: **fully present,
including timeline.**

The compositor's explicit-sync obligations are stated in terms of **(B)**. Vulkan **(A)** is
only needed as the *glue* that converts between a `drm_syncobj` timeline point and a GPU
submit's wait/signal. And that glue does **not** require `OPAQUE_FD` or Vulkan timeline export
— see §5.

### Why `OPAQUE_FD` is absent — and why that's expected, not a bug to "fix"

`OPAQUE_FD` is a **driver-private** handle: it references host-GPU-driver-internal state and is
only meaningful to the same driver instance. Under virtio-gpu, the guest and the host GPU
driver are **different processes on different sides of the VM boundary**, so a host Vulkan
opaque semaphore FD is meaningless if injected into the guest. Venus therefore *cannot*
meaningfully implement `OPAQUE_FD` external semaphores — this is structural, not a missing
patch.

`SYNC_FD` is different: a `sync_file` is a **generic kernel `dma_fence` FD**, not driver-private.
The guest virtio-gpu driver can create a guest `sync_file` backed by a host fence and pass it
around. That is why `SYNC_FD` works and `OPAQUE_FD` does not — and why the lavapipe result
matches (llvmpipe likewise only bothers with `SYNC_FD`).

**Takeaway:** targeting these drivers, the renderer must use the **`sync_file` / binary
`SYNC_FD`** path for GPU sync regardless of which driver is loaded. Design for that, not for
`OPAQUE_FD`/timeline export.

---

## 5. The portable explicit-sync path (uses only what's present)

This is the wlroots-style approach and it needs nothing beyond §3a's binary `SYNC_FD` + §3c's
kernel timeline syncobj:

**Wait on a client's timeline point (acquire):**
1. Client gives a `drm_syncobj` timeline handle + point via `linux-drm-syncobj-v1`.
2. Kernel: materialize that point as a `sync_file` FD —
   `DRM_IOCTL_SYNCOBJ_TRANSFER` / `DRM_IOCTL_SYNCOBJ_EXPORT_SYNC_FILE`
   (wait-for-submit semantics as needed).
3. Vulkan: `vkImportSemaphoreFdKHR` with `handleType = SYNC_FD` into a **binary** VkSemaphore,
   then use it as a wait semaphore on the compositing `vkQueueSubmit`. ✅ supported here.

**Signal completion back (release):**
1. Vulkan: add a **binary** VkSemaphore as a signal semaphore on the submit; export it with
   `vkGetSemaphoreFdKHR` → `SYNC_FD`. ✅ supported here.
2. Kernel: `DRM_IOCTL_SYNCOBJ_IMPORT_SYNC_FILE` that `sync_file` into a syncobj, then
   `DRM_IOCTL_SYNCOBJ_TRANSFER` it to the client-visible timeline point.

**KMS scanout:** feed the acquire `sync_file` as atomic `IN_FENCE_FD`; take `OUT_FENCE_PTR`
back as a `sync_file` for buffer-release. (Verify the VkFence→`SYNC_FD` export half — see §6.)

The only thing the standard `OPAQUE_FD`-timeline recipe buys over this is skipping the
per-submit `sync_file` conversions. Not having it costs a few ioctls, not correctness.

---

## 6. Is this actually blocking? What the VM-stack session should verify

**Probably not blocking** — but the probe only proves the *API surface*. Before relying on §5,
confirm the pieces the guest can't see from a capability bit:

1. **VkFence → `SYNC_FD` export** (the companion to §3a; needed for KMS `OUT_FENCE`/CPU waits).
   `VK_KHR_external_fence_fd` is advertised (rev 1) but was **not** measured. Check with
   `vkGetPhysicalDeviceExternalFenceProperties` for `SYNC_FD` binary (EXPORTABLE|IMPORTABLE) —
   same pattern as `probes.rs::external_semaphores()`. Expected: supported (dma-fence-backed,
   like the semaphore case), but confirm.

2. **End-to-end host-GPU-backed timeline signalling** — the deep question. `DRM_CAP_SYNCOBJ_TIMELINE=1`
   proves the *guest kernel* implements the syncobj **API**; it does **not** prove that a guest
   syncobj point actually gets signalled by **host GPU work completion** rather than only by CPU
   `SIGNAL` ioctls. Write a real test: submit GPU work via Venus, export its completion as a
   `sync_file`, import into a syncobj timeline point, and confirm a `TIMELINE_WAIT` on that point
   unblocks *only after the GPU finished* (not immediately, not on a CPU poke). This is the
   actual thing that can be broken in the virtio-gpu ↔ virglrenderer ↔ host-fence plumbing.

3. **virtio-gpu host-fence feature negotiation** — determine whether the VMM/virglrenderer
   negotiate host fences for the Venus context (the mechanism behind resource/fence completion
   signalling). Relevant knobs/inspection:
   - VMM: which one is it (`vm-other`)? Check its virtio-gpu device config —
     `context_types`/`venus` enabled, and whether it forwards host fences. (crosvm:
     `--gpu context-types=venus`; other VMMs vary.)
   - `virglrenderer` version + build flags (Venus backend, `VIRGL_RENDERER_*` caps).
   - Guest kernel `virtio-gpu` config: `CONFIG_DRM_VIRTIO_GPU`, and whether the running
     `7.1.2-limina16k` build includes the syncobj/host-fence patches (it advertises the caps,
     which is a good sign).
   - Mesa venus debug: run with `VN_DEBUG=init,wsi` (and `MESA_VK_ABORT_ON_DEVICE_LOSS=1`) to
     surface what the guest driver negotiates for external sync at device creation.

4. **If you *do* want timeline over `SYNC_FD`** (`SYNC_FD timeline: false` above): that is
   inherently limited — `sync_file` carries a single binary `dma_fence`, so "timeline over
   SYNC_FD" isn't really a thing. Don't pursue it; use §5 (kernel timeline syncobj +
   binary `sync_file` conversions).

---

## 7. Where a fix would live (per layer), and where it would not

- **Guest Mesa `venus` (26.1.3):** could in principle grow `SYNC_FD` timeline glue, but see
  §6.4 — not the right lever. It already exposes binary `SYNC_FD` correctly. If §6.2 fails, the
  guest-side venus external-fence/semaphore import code is one suspect.
- **`virglrenderer` Venus backend (host):** the most likely home of a *real* fix if §6.2 shows
  host-GPU completion isn't propagating to guest fences — i.e. host fence contexts not wired to
  guest `sync_file` signalling.
- **VMM virtio-gpu device:** must negotiate the Venus context type **and** host-fence support;
  a missing feature bit here silently degrades sync to CPU-only.
- **Guest kernel virtio-gpu:** already advertises the syncobj timeline caps, so likely fine;
  double-check the running kernel actually has the host-fence path compiled/enabled.
- **NOT a fix:** making `OPAQUE_FD` external semaphores work across virtio (§4) — structurally
  impossible and unnecessary.

---

## 8. References

- Vulkan spec: `VkExternalSemaphoreProperties`, `VkPhysicalDeviceExternalSemaphoreInfo`,
  `VkSemaphoreImportFlagBits`; `VK_KHR_external_semaphore_fd`, `VK_KHR_timeline_semaphore`,
  `VK_KHR_external_fence_fd`.
- Wayland: `linux-drm-syncobj-v1` protocol (timeline explicit sync); the deprecated
  `zwp_linux_explicit_synchronization_v1` (binary, for contrast).
- Kernel DRM: `drm_syncobj` (`DRM_IOCTL_SYNCOBJ_*`, `TRANSFER`, `EXPORT/IMPORT_SYNC_FILE`),
  `DRM_CAP_SYNCOBJ{,_TIMELINE}`; atomic KMS `IN_FENCE_FD` / `OUT_FENCE_PTR`.
- Prior art to copy (C): `wlroots` `render/vulkan` + `types/wlr_linux_drm_syncobj_v1.c`
  (timeline syncobj ⟷ sync_file ⟷ binary VkSemaphore, exactly §5); Mesa `src/virtio/venus`
  external-sync handling; `virglrenderer` Venus backend fence code.
- This repo: `vk-spike/src/probes.rs` (the probe), and the memory note
  `render-stack-maturity.md` (Stage 0 / Stage 3 planning) in
  `~/.claude/projects/-home-kov-Projects-gnome-shell-rs/memory/`.

---

## 9. Bottom line for the compositor (gnome-shell-rs side)

Design the Vulkan renderer's sync around **binary `SYNC_FD` semaphores + kernel `drm_syncobj`
timeline conversions** (§5). Do not depend on `OPAQUE_FD` or exported Vulkan timeline
semaphores — they are absent on both Venus and lavapipe and (for Venus) structurally can't
exist. The one open risk worth an early spike is §6.2 (host-GPU-backed timeline signalling
through virtio-gpu); everything else in the path is confirmed present on this VM.
