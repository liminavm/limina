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
**two switches** below.

## The two switches to flip

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

### 3. Graceful fallback = software-2D, not a broken rutabaga (two-tier guarantee)

`create_fallback_rutabaga` currently falls back to a `NO_VIRGL`-only rutabaga that **still can't do
2D** (the spike's EGL combos hit this → firmware ASSERT). Change the fallback: if the venus rutabaga
fails to build, set `rutabaga = None` **and** drop to `SoftwareOnly` feature/capset advertisement,
so the guest comes up degraded-but-booting on pure software-2D. This is the compatibility floor —
a host without working venus must still boot the desktop.

## limina facade / CLI

- Replace `set_gpu_software_2d(bool)` with a mode: default `SoftwareOnly`; opt into `Coexist` via a
  `--gpu-3d` flag (worker) / supervisor passthrough, and/or keep `LIMINA_VIRGL_FLAGS` as the
  power-user override. When `Coexist`, the facade passes venus flags `0xC0` (not the raw user value,
  unless overridden) — encode the "no EGL / yes NO_VIRGL" rule in one place with a comment pointing
  at the spike.
- Keep tier-1 the default so stock boots are unchanged until 3D is explicitly requested.

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
- **L2 (stock Fedora):** boot stays green in `SoftwareOnly` (default) — proves we didn't disturb the
  compatibility floor. A separate, explicitly `Coexist` Fedora boot is the Phase-1 venus check
  (headless: assert renderer init + a non-black frame; `vulkaninfo` once a guest shell exists).

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
