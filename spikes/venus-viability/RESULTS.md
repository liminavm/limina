# Spike: Venus viability (tier-2 gate)

**Question:** Can we turn on the real renderer (rutabaga → virglrenderer-Apple → Venus →
MoltenVK → Metal) for the Fedora guest on this host, as the foundation for tier-2 (GPU 3D +
zero-copy scanout)? What flags, and what breaks?

**Method:** boot the real Fedora-43 guest headless (`--display-capture`, GOP firmware) with
`LIMINA_VIRGL_FLAGS=<flags>` (which flips the gpu device off software-2D and into virglrenderer),
bounded ~12–75 s, reading the worker log + console + macOS crash report. `spikes/venus-viability/run.sh`.
Host: macOS 26.5, M1 Max. virglrenderer = `slp/krun` bottle 0.10.4e (links MoltenVK; venus
compiled in — `VK_MESA_venus_protocol`, `vkr-ring`; **no `virgl_render_server` binary**).

## Findings

### 1. Venus initializes on this host — with the right flags (in-process, no render server)
The flag bits (`crates/limina-vmm/src/krun/mod.rs`): USE_EGL=0x1, THREAD_SYNC=0x2, VENUS=0x40,
NO_VIRGL=0x80, ASYNC_FENCE_CB=0x100, RENDER_SERVER=0x200.

| flags | meaning | result |
|---|---|---|
| `0x343` | EGL\|THREAD_SYNC\|VENUS\|ASYNC\|RENDER_SERVER (the "gui_vm" set) | `virgl_renderer_init` → **ComponentError(-1)**, clean fail → fallback |
| `0x143`,`0x43` | …with EGL, no render-server | same clean **-1** fail → fallback |
| `0x40`,`0x42` | VENUS(\|THREAD_SYNC), **no EGL, no NO_VIRGL** | **SIGSEGV** in `create_gl_context` ← `vrend_renderer_init` ← `virgl_renderer_init` |
| `0xC0`,`0x1C2` | **VENUS\|NO_VIRGL**(\|THREAD_SYNC\|ASYNC) | **clean init**, no fallback, no crash, scanout configures |

Two hard rules for macOS, both confirmed by the data:
- **Never set `USE_EGL` (0x1).** No EGL on macOS → `virgl_renderer_init` returns -1.
- **Always set `NO_VIRGL` (0x80).** Without it, virglrenderer still runs the GL renderer path
  (`vrend_renderer_init → create_gl_context`) which **null-derefs (SIGSEGV at 0x0)** — no GL
  context on macOS. (Crash report: faulting frame `create_gl_context`, `EXC_BAD_ACCESS` at 0x0.)
- `RENDER_SERVER` (0x200) is unavailable (no `virgl_render_server` binary) but harmless if not
  relied on — venus runs **in-process** against the linked MoltenVK.

→ **The macOS venus flag set is `VENUS|NO_VIRGL` (0xC0)**, optionally `|THREAD_SYNC|ASYNC_FENCE_CB`
(0x1C2). NOT the in-tree Linux gui_vm `0x343`.

### 2. The blocker: a venus-only device cannot serve 2D — and 2D is the whole early boot + present
With `0xC0` the device inits fine, but the firmware's first 2D GOP command returns **ERR_UNSPEC
(0x1200)** → `ASSERT [VirtioGpuDxe] Gop.c(109)` → boot wedges. Cause: rutabaga with `VENUS|NO_VIRGL`
provides only Vulkan **3D contexts**; it does not implement the 2D virtio-gpu commands
(`RESOURCE_CREATE_2D`/`TRANSFER_TO_HOST_2D`/`SET_SCANOUT`/backing). Those 2D commands are what the
**firmware GOP, efifb, fbcon, and the scanout present** all use. Our software-2D patch (libkrun
0001) serves exactly those — but it is currently **mutually exclusive** with the renderer:
`software_2d=true ⟹ rutabaga=None` (`virtio_gpu.rs:389`). So today you get *either* 2D *or* venus,
never both, and venus-only can't boot a desktop.

### 3. The current fallback is not graceful either
When venus init fails cleanly (EGL combos), libkrun falls back to a `NO_VIRGL`-only rutabaga
(`create_fallback_rutabaga`), **not** our software-2D — so 2D still fails (same ERR_UNSPEC /
firmware ASSERT) and `create_fence` returns `ErrRutabaga(ComponentError(22))`. The fallback should
be our software-2D path. (Also: `create_rutabaga` swallowed the real error with `.ok()`; this spike
added a `warn!` of the `RutabagaError` — keep it.)

## Conclusion → architecture for tier-2

Tier-2 is **not a flag flip; it is a "coexist" device.** One virtio-gpu device must serve:
- **2D resource + scanout commands** via our software-2D CPU path (firmware/fbcon/present, and the
  compatibility floor), AND
- **3D context commands** (CTX_CREATE, SUBMIT_3D, capsets, venus) via rutabaga `VENUS|NO_VIRGL`.

i.e. patch libkrun so `software_2d` 2D handling and a `VENUS|NO_VIRGL` rutabaga live in the **same**
device, routed by command/resource type — instead of `software_2d` meaning "rutabaga = None." The
scanout *present* stays the software-2D CPU path initially (guest compositor renders 3D via venus
into a resource; the existing 2D `SET_SCANOUT`/`FLUSH` presents it). **Zero-copy present
(`SET_SCANOUT_BLOB` → IOSurface) is a later optimization layered on top**, not the first step.

Still unverified (guest side, next spike): does Fedora 43 Mesa select the **venus** Vulkan driver
and does **zink-on-venus** accelerate GNOME (vs llvmpipe)? And does the guest kernel's virtio-gpu
expose the 3D/context path? These gate whether the coexist device actually gets exercised.

## Repro
`spikes/venus-viability/run.sh` (env: `SECS`, `FLAGS`). Artifacts in `out/`. Crash backtrace via
`~/Library/Logs/DiagnosticReports/limina-vmm-*.ips`.
