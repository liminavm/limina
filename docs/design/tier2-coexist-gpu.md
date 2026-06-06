# Tier-2 design: the coexist virtio-gpu device (software-2D 2D + Venus 3D)

Status: design (2026-06-06). Grounds the M4 work after the `spikes/venus-viability` gate.
See `docs/roadmap.md` M4 and the spike RESULTS for why this shape is forced.

## Goal

One virtio-gpu device that simultaneously:
- serves **2D** resource/scanout commands via our software-2D CPU path (patch 0001) — the
  firmware GOP, efifb, fbcon, the compatibility floor, **and** the scanout *present*; and
- serves **3D** context commands (CTX_CREATE / SUBMIT_3D / capsets / blob) via a rutabaga built
  with **`VENUS|NO_VIRGL`** (the only flag set that initializes on macOS — no EGL, no GL context).

The spike proved these can't be either/or: venus-only can't boot (2D fails → firmware ASSERT),
and software-2D-only can't accelerate. They must live in the same device.

## Key finding that makes this small: the methods are already coexist-shaped

Patch 0001 didn't just add a 2D fast-path; it left every handler able to serve both backends:
- `resource_create_2d` (`virtio_gpu.rs:475`) **always** allocates an `Sw2dResource` — it never
  touched rutabaga to begin with. So 2D works whether or not a renderer exists.
- The mixed handlers already check `sw2d` first, then fall through to rutabaga:
  `unref_resource:553`, `attach_backing:837`, `detach_backing:851`, `transfer_write:800`, and —
  crucially — `flush_resource:673` (sw2d → memcpy; else rutabaga → `read_2d_resource` CPU readback).
- The 3D handlers (`resource_create_3d`, `create_context`, `submit_command`, `get_capset*`,
  `resource_create_blob`) already use `self.rutabaga.as_mut().ok_or(ErrUnspec)` — they "just work"
  once rutabaga is `Some`, and degrade to `ErrUnspec` when it's `None`.

So a venus-rendered resource presented through the **normal** `SET_SCANOUT`+`RESOURCE_FLUSH` path
already presents today (via `read_2d_resource`). The only reasons it's currently either/or are the
**switches** below (three mechanism changes + a graceful fallback).

## The switches to flip

### 1. Decouple rutabaga creation from the 2D path (`VirtioGpu::new`, `device.rs`)

Today `software_2d: bool` overloads two meanings: "use the sw2d 2D path" AND "rutabaga = None".
Split them. The 2D sw2d path is **always on** (on macOS it's the only 2D that works); rutabaga is
created **iff 3D is enabled**:

```
rutabaga = if enable_3d {
    create_rutabaga(VENUS|NO_VIRGL …)   // may fail → None (see fallback)
} else {
    None
}
```

Thread an `enable_3d` (or a small `GpuMode { SoftwareOnly, Coexist }`) from the limina facade →
`VmResources` → `Gpu::new` → `Worker::new` → `VirtioGpu::new`, replacing the bare `software_2d`
bool. `create_rutabaga` keeps taking `virgl_flags`; the facade supplies `0xC0` (or `0x1C2`).

### 2. Feature + capset advertisement (`device.rs`)

- `avail_features`: `Coexist` advertises the **full** `AVAIL_FEATURES` (VIRGL | EDID | RESOURCE_UUID
  | RESOURCE_BLOB | CONTEXT_INIT). `CONTEXT_INIT` lets the guest select the venus capset;
  `RESOURCE_BLOB` is venus memory; `VIRTIO_GPU_F_VIRGL` is what gates the guest kernel's 3D ioctl
  path (without it the kernel disables context/3D entirely). `SoftwareOnly` keeps
  `AVAIL_FEATURES_SOFTWARE_2D`.
  - Risk noted: advertising VIRGL means a GL app could try the virgl-**GL** gallium driver, which
    our `NO_VIRGL` rutabaga rejects (ErrUnspec). Mitigation is guest-side policy (steer GL → zink →
    venus via `MESA_LOADER_DRIVER_OVERRIDE=zink`), an enhanced-tier env knob — not a host change.
    Vulkan apps go straight to venus and are unaffected.
- `num_capsets`: stop hardcoding `5`. Derive from the actual rutabaga (its active capset list — with
  `VENUS|NO_VIRGL` that's just VENUS, count 1). `get_capset_info`/`get_capset` already delegate to
  rutabaga, so only the *count* in `read_config` needs to match. Resolve the exact rutabaga
  count API during impl (`RUTABAGA_CAPSETS` + the build's capset_mask; or loop `get_capset_info`).

### 3. Fence routing by ring (the actual `0xC0` wedge — confirmed in source)

This is the real cause of the spike's `0xC0` firmware wedge, not the 2D data path (sw2d already
serves 2D, and the scanout configured fine). `worker.rs:456-475` calls `virtio_gpu.create_fence`
for **every** fenced command. Patch 0001's `create_fence` (`virtio_gpu.rs:960`) only sync-completes
when `rutabaga` is `None`; with a venus rutabaga it routes **all** fences to `rutabaga.create_fence`
— including the firmware's **2D global-ring** fences, which venus rejects (`ComponentError(22)`) →
ERR_UNSPEC → `ASSERT Gop.c(109)`.

Fix: route by ring. A fence on the **Global ring** (`flags & VIRTIO_GPU_FLAG_INFO_RING_IDX == 0` —
2D/sw2d commands, firmware/fbcon) → `mark_fence_completed_sync` (it already finished synchronously).
A **context-specific** fence (a real venus 3D context) → `rutabaga.create_fence`. The
`RutabagaFence` already carries `flags`/`ctx_id`/`ring_idx`, so the routing is local to
`create_fence`. This is what lets 2D and venus fences coexist.

### 4. Graceful fallback = no rutabaga, not a broken one (two-tier guarantee)

`create_fallback_rutabaga` currently falls back to a `NO_VIRGL`-only rutabaga that **still can't do
2D** (the spike's EGL combos hit this → firmware ASSERT). Change the fallback: if the venus rutabaga
fails to build, set `rutabaga = None` (drop the broken-fallback rutabaga entirely). The methods
already handle `None` — 2D via sw2d keeps working, 3D commands return `ErrUnspec`.

**Ordering caveat (important):** feature/capset advertisement happens at `read_config`, *before*
`activate` → worker → rutabaga creation, so we **can't** retroactively drop to software-2D
advertisement once venus fails. Features are decided up-front by **intent** (`enable_3d`). If venus
then fails at activate, the device has advertised VIRGL/CONTEXT_INIT/capsets but answers 3D commands
with `ErrUnspec` → the **guest** Mesa falls back to llvmpipe, while 2D (firmware/fbcon/scanout via
sw2d) is unaffected and the desktop still boots. So graceful degradation manifests at the
guest-rendering layer, not the host-feature layer — still the compatibility floor (boots, usable),
just software-rendered. (Pre-OS firmware never uses 3D features, so boot is never at risk from the
advertisement.)

## limina facade / CLI

- **`Coexist` is the DEFAULT** — 3D is not opt-in. Because venus-init failure degrades gracefully to
  software-2D (see fallback above), the default path just *tries* venus and you get 3D when it works.
  This is the two-tier guarantee: the enhancement is additive and self-degrading, never a gate.
- Keep a **`--gpu-software-2d` override** (replacing today's implicit `software_2d` default) that
  forces `SoftwareOnly` — needed for: (i) the headless capture/PNG test oracle and the L2
  compatibility-floor test (assert the floor *specifically*); (ii) **the local-Terminal GPU-init
  hang** — graceful degradation catches venus *failure* (an error) but NOT the launch-context
  *hang* (a block in virglrenderer/Metal init from the user's local Terminal; `.app`/ssh/scripts are
  fine). The override is the escape hatch for `cargo run` from a local Terminal. `LIMINA_VIRGL_FLAGS`
  stays as the power-user flag override.
- The facade passes venus flags `0xC0` for `Coexist` (not a raw user value unless overridden) —
  encode the "no EGL / yes NO_VIRGL" rule in one place with a comment pointing at the spike.

## Phasing (each independently testable)

- **Phase 1 — boot + offscreen venus.** Coexist device boots Fedora (2D firmware/fbcon via sw2d)
  AND offers the venus capset. Prove `vulkaninfo` in the guest reports the **virtio-gpu/venus**
  driver (not llvmpipe) and an offscreen Vulkan workload runs on the GPU. Present is irrelevant here
  (offscreen), so no blob-scanout needed. **This is the milestone that proves the core.**
- **Phase 2 — accelerated desktop present.** GNOME/mutter renders via venus and reaches the display.
  If mutter uses normal `SET_SCANOUT` on a transfer-readable resource, `flush_resource` already
  presents it via `read_2d_resource`. If it uses **blob scanout**, implement `SET_SCANOUT_BLOB`
  (`worker.rs:391` panic) with a CPU-readback present first (correctness before zero-copy).
- **Phase 3 — zero-copy present.** Replace the readback with an IOSurface-backed Metal texture
  exported from the scanout blob (`virgl_renderer_resource_get_map_ptr` / `BLOB_FD_TYPE_APPLE`),
  plus a `present_texture(scanout_id, surface)` display-vtable callback → `CALayer.contents`. This is
  the original "tier-2 zero-copy" and rides on the IOSurface↔MoltenVK interop spike (still owed).

## RED-first test plan (drives the shipped binaries; `crates/limina-test`)

- **L0/unit (libkrun-adjacent, runs anywhere):** `Coexist` mode advertises full features + venus
  capset and `num_capsets≥1`; `SoftwareOnly` advertises the 2D set + 0; venus-build-failure path
  flips to `SoftwareOnly` (inject a forced-failure). These assert the negotiation without HVF.
- **L1 (HVF, custom kernel):** the custom guest kernel needs the 3D bits
  (`DRM_VIRTIO_GPU` 3D / `CONFIG_DRM_VIRTIO_GPU` already on; verify venus needs nothing else) — a
  test that with `Coexist` the guest sees `num_capsets≥1` and a 3D context can be created (a tiny
  guest-side ioctl probe), while 2D `l1_display` **still passes** (the floor mustn't regress).
- **L2 (stock Fedora):** the default (`Coexist`) boot stays green via graceful degradation; a
  `--gpu-software-2d`-forced boot asserts the compatibility floor *specifically* (proves we didn't
  disturb it). The default `Coexist` Fedora boot is also the Phase-1 venus check (headless: assert
  renderer init + a non-black frame; `vulkaninfo` once a guest shell exists).

## Open questions / risks (resolve in Phase 1)

- Does Fedora 43's Mesa ship the venus Vulkan driver (`libvulkan_virtio.so`)? If not it's a
  guest-side enhanced-tier install. (Can't easily inspect the btrfs `.raw` from macOS; learn it from
  a booted Coexist guest.)
- Does advertising VIRGL while backing only venus confuse the guest kernel/Mesa (virgl-GL probe
  failures in dmesg)? Acceptable if Vulkan still works; steer GL→zink if noisy.
- Exact rutabaga capset-count API.
- `force_ctx_0`/fence handling under venus (async fence cb flag 0x100) — the `create_fence`
  `ComponentError(22)` seen in the spike was the *fallback* rutabaga; re-check under real venus.

## Out of scope (later milestones)

Zero-copy IOSurface present (Phase 3 / roadmap M4 task 4), GPU SHM vRAM window tuning vs M6 balloon,
MoltenVK feature-coverage gaps, x86 guest. Keep patches minimal + upstreamable (mechanism in
libkrun, venus-flag/zink policy in limina).
