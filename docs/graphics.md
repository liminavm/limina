# Graphics: render, present, and the tier ladder

**Scope.** Everything between a guest draw call and a pixel on the host window: the virtio-gpu
device, the three rendering tiers (software-2D / vrend GL / venus Vulkan), KosmicKrisp, blob
mapping, IOSurface scanout and present, the host GPU-memory budget, and the pitfalls that keep
catching people. Display *identity and geometry* — EDID, hotplug, display modes, runtime resize,
cutouts, fullscreen and input policy — are a different subsystem and live in
`docs/design/{stable-edid-hotplug,display-modes,runtime-display-resize,display-cutouts,fullscreen-pointer-grab}.md`.

**Provenance.** Every claim below was re-derived from booted VMs on 2026-08-16 rather than carried
forward from older notes; the measurements and the falsified claims they replaced are in
`spikes/graphics-doc-audit/RESULTS.md`. Where this document and an older one disagree, this one is
right — that is why the older ones were deleted.

---

## 1. The stack in one picture

```
guest app ──GL──► mesa virgl (gallium)  ─┐
guest app ──VK──► mesa venus (libvulkan_virtio) ─┐
                                                 │  virtio-gpu (one device, coexist)
                          ┌──────────────────────┴──────────────────────┐
                          ▼                                             ▼
                 virglrenderer / vrend                        virglrenderer / vkr (venus)
                 (host-side GL decode)                        (host-side Vulkan decode)
                          │                                             │
                          └──► zink ──────────► KosmicKrisp ◄───────────┘
                                                    │
                                                  Metal
                                                    │
                                      IOSurface ──► limina supervisor ──► CAMetalLayer
```

One host device serves both paths. `libvulkan_virtio.so` in the guest is venus; the guest's GL
goes out as a virgl command stream and is decoded host-side by vrend, which runs on zink over
KosmicKrisp. So **both** tiers ultimately land on KosmicKrisp → Metal; they differ in where the
API boundary is crossed.

**KosmicKrisp (KK)** is Mesa's Vulkan-on-Metal driver and the **sole** host Vulkan backend.
MoltenVK was retired as a venus backend on 2026-06-13 (it crashed the compositor); the archived
instrumentation lives in `spikes/archive/moltenvk/`. KK currently advertises **Vulkan 1.4** (since
the MTL4 rebase, 2026-08-05).

## 2. The host device: coexist by default

The worker builds one virtio-gpu device with `GPU_COEXIST_FLAGS`
(`crates/limina-vmm/src/krun/mod.rs`):

```
VIRGLRENDERER_VENUS | USE_EGL | USE_GLES | USE_SURFACELESS
                    | THREAD_SYNC | USE_ASYNC_FENCE_CB | RENDER_SERVER      = 0x35b
```

`NO_VIRGL` is **off** — that is what "coexist" means: venus and vrend are both live on the same
device, and the guest picks per-API. This is the default and there is no reason to leave it.
`--gpu-software-2d` clears the whole set (see §3.1); it is a probe mode, not an alternative.

The worker logs the resolved mode at `info` level and defaults to `warn`, so a normal boot prints
nothing. To see it:

```sh
RUST_LOG=limina_vmm=info …    # "virtio-gpu virgl_flags = 0x35b, software_2d = false (coexist = true)"
```

### The link trap (read this before diagnosing any GPU bug)

The worker **must** link `third_party/virgl-prefix/lib/libvirglrenderer.*`. Homebrew ships a
virglrenderer with no venus render-server support; if the worker picks that one up,
`virgl_renderer_init` returns −1 and the GPU **silently degrades to software-2D**. The VM still
boots, 2D and ssh work, and venus simply never enumerates — which reads exactly like a venus bug
and has burned hours. `build.rs` now prepends our prefix to `PKG_CONFIG_PATH` and prints a
`cargo:warning` naming the resolved library, so a plain `cargo build` is safe and a wrong link is
loud. Verify anyway:

```sh
otool -L target/debug/limina-vmm | grep virgl     # must show third_party/virgl-prefix/…
```

If a worker log shows `degrading to software-2D` or `ComponentError(-1)` right after `virgl_flags`,
check the link before suspecting anything else.

## 3. The three tiers

### 3.1 Software-2D — a probe mode, not a desktop

`--gpu-software-2d` gives the guest a virtio-gpu with no 3D capability. `/dev/dri/card0` and
`renderD128 `still appear and `drm_info` reports a full mode list, but gbm has no driver behind it,
so a GNOME session **cannot start at all**:

```
libEGL warning: egl: failed to create dri2 screen
Failed to open gpu '/dev/dri/card0': … Failed to create gbm device: No such file or directory
Failed to setup: No GPUs found
org.gnome.Shell@gdm.service: Failed with result 'protocol'
Gdm: GdmLocalDisplayFactory: maximum number of display failures reached. Giving up.
```

The window shows a blinking console cursor. That is the expected outcome, not a bug — but it means
software-2D is for **GL-less hosts, the capture oracle, and early-boot/console work**, and it is
never the right answer to "venus is misbehaving". Do not describe it as a degraded desktop tier.

### 3.2 vrend — accelerated GL, on any guest, with nothing installed

A bone-stock Fedora 44 guest (stock 4 KiB kernel, stock mesa, no limina components, no limina
environment) gets hardware-accelerated GL out of the box:

```
OpenGL renderer string: virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))
```

This is the compatibility floor doing its job, and it is why **GL rides vrend on both tiers**. The
enhanced session runs the same path — guest-side zink-on-venus for GL was dropped as a supported
configuration on 2026-08-04. `gnome-shell` in an enhanced guest maps `libgallium-*.so` and no venus
ICD at all.

vrend also owns the scanout (§4), so zero-copy present is a host-side property available to *both*
tiers, not an enhanced-tier feature.

### 3.3 venus — Vulkan, and the one thing that needs the enhanced guest

venus is the Vulkan side only. In an enhanced guest:

```
$ vulkaninfo --summary
GPU0: Virtio-GPU Venus (Apple M1 Max),  driverName = venus
```

**venus requires a 16 KiB-page guest.** This is the single hard tier boundary in the graphics
stack, and its mechanism is precise: `hv_vm_map` demands 16 KiB granularity for the host address,
the guest address *and* the size. The size half was fixed host-side in 2026-07 (libkrun + virgl);
the **offset** half is guest-side — a 4 KiB guest packs several blobs into one host page, so a
blob's guest address is 4 KiB-aligned but not 16 KiB-aligned:

```
hv_vm_map failed: ret=0xfae94003 … guest=0x280021000 size=0x100000
                  (host%16k=0 guest%16k=4096 size%16k=0)
ResourceMapBlob -> ErrUnspec
```

Full analysis and the ways out (16 KiB kernel — what we ship; `guest/virtio-gpu-dkms`;
or a `VIRTGPU_PARAM_BLOB_ALIGNMENT` Mesa chain, unwritten) are in
`docs/design/16k-page-requirement.md`. Note the DKMS module is a *lab* answer: no shipped image or
payload carries it, so "venus works on a stock 4 KiB guest" is only true of a guest somebody
modified by hand.

#### Open: stock-tier Vulkan is dead, not degraded

This is a live violation of the two-tier guarantee and the highest-value open item in this
document. On a stock guest, venus's failure does not degrade — it takes the whole Vulkan loader
down with it:

```
$ vulkaninfo
vkCreateInstance failed with ERROR_OUT_OF_HOST_MEMORY
```

Isolating the ICDs proves the loader is the amplifier: `VK_DRIVER_FILES=…/lvp_icd…` alone
enumerates llvmpipe perfectly; `…/virtio_icd…` alone produces the OOM. A stock guest is therefore
left with *no* Vulkan, when it should have had a working software one.

The fix exists — `patches/mesa-guest/0003-venus-degrade-to-the-stub-instance-when-ring-setup-f.patch`
does exactly this — but it is in **our guest series**, which by definition a stock guest does not
have. Closing it means getting that patch upstream so every future distro mesa carries it; it is
item 1 of the shopping list in `docs/design/16k-page-requirement.md` and it is tracked in
`docs/upstreaming/ledger/mesa.md`.

### 3.4 Detecting tiers: additively, never as one switch

A guest may have some, all, or none of the enhanced pieces, and partial states are normal (a guest
mid-upgrade, or one that installed only part). Light up each feature when *its own* prerequisite is
present — 16 KiB pages for venus, the limina mesa for the venus WSI fixes, the agent for its own
features — rather than gating everything on a monolithic "enhanced" flag.

## 4. Scanout and present: IOSurface is the macOS dmabuf

There is no dma-buf on macOS. The equivalent currency is an **IOSurface**, and the whole present
path is built on it.

**vrend renders directly into the display IOSurface.** The scanout surface is wrapped as an
`EGL_IOSURFACE_LIMINA` EGLImage, so vrend's framebuffer *is* the surface — no readback, no blit.
The worker says so on every boot, on both tiers:

```
iosurface scanout: 2560x1440 PIPE_FORMAT_B8G8R8X8_UNORM EGL-backed (IOSurface id N),
                   renders land in the surface directly
```

**venus presents via `SET_SCANOUT_BLOB` + `present_surface`**, importing the guest's image as an
`MTLTexture` over the IOSurface rather than copying it.

**Worker → supervisor** hands over the surface by Mach port, not as a global IOSurface id. Global
scanout still exists as an opt-in (`LIMINA_GLOBAL_SCANOUT=1`) purely so the cross-process pixel
oracle can find the surface; do not rely on it in normal operation.

Any older statement that present is "a full-frame CPU readback per flush", that `SET_SCANOUT_BLOB`
panics, or that "there is no zero-copy scanout of a GPU texture" describes the pre-KosmicKrisp
stack and is wrong.

### Format modifiers

KK implements `VK_EXT_image_drm_format_modifier` for real, LINEAR-only, over the
IOSurface-shareable colour formats, with truthful subresource layouts; vkr passes modifier creates
through verbatim, and upstream venus's own passthrough gate does the rest. That is what let the
guest-side modifier fiction (mesa 0010, both halves) be **retired whole** on 2026-08-04 — the
renderer now negotiates honestly instead of the guest driver being patched to compensate. The
`vk_image.h` `drm_format_mod` guard carries `DETECT_OS_APPLE` on the `limina-kk` branch.

Guest-side, the enhanced kernel advertises **no** format modifiers on the KMS side
(`DRM_CAP_ADDFB2_MODIFIERS = 0`, primary plane `XR24` only, cursor `AR24`) — that is deliberate,
post-r9, and unrelated to the Vulkan extension above.

## 5. Host GPU-memory budget

Host memory allocated on the guest's behalf is invisible to the guest, so a guest-side leak ends
with macOS jetsamming the worker and killing the whole VM with no guest-side evidence. The budget
ledger in `third_party/virglrenderer/src/venus/vkr_budget.{c,h}` fixes the attribution: exact-size
per-context accounting (always on) plus an enforced cap that kills the offending context rather
than the VM. The guest is told the cap through `VK_EXT_memory_budget` — the one backpressure
channel the venus transport does not discard.

Default cap: `max(8 GiB, 2 × guest RAM)`; at 8 GiB of guest RAM the worker logs
`limina GPU budget: cap 16384 MiB`. Design, policy knobs, and how to read a refusal:
`docs/design/gpu-memory-budget.md`.

## 5.1 Delivery: how the enhanced graphics stack reaches a guest

The enhanced tier is delivered as **rebuilt Fedora RPMs replacing stock at `/usr`**, versionlocked
— not as a sysext overlay. The reason is specific to mesa: our mesa has a different `libgallium`
soname than the stock one, and an overlay can only *shadow* a file, never remove it, so the guest
ends up with a blended ABI that breaks mutter's KMS EGL. An RPM replaces the old soname outright.
(The retired sysext builders are in `scripts/archive/`.)

Alongside the RPMs, `install-enhanced.sh` writes the driver selection into
`/etc/environment.d/90-limina-zink.conf` (the filename predates the GL flip and was kept):

```sh
GALLIUM_DRIVER=virgl
MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu
VK_DRIVER_FILES=<venus ICD>
VN_DEBUG=mem_budget          # venus only advertises VK_EXT_memory_budget under this gate
```

then sets GRUB's default to the 16 KiB kernel and reboots. `MESA_LOADER_DRIVER_OVERRIDE=zink` /
`GALLIUM_DRIVER=zink` — zink as the *guest's* GL driver — is no longer a supported configuration.

An enhanced image is safe to boot even if the 16 KiB kernel is not the selected one: venus init
fails, GL keeps working on vrend, and the desktop comes up degraded rather than broken. Component
versions and the host-first ordering prerequisite are in `docs/images.md`.

## 6. Performance

**`perf/ledger.csv` is the live record.** Do not maintain a ranking table in prose — the last one
outlived its stack by months and ended up recommending a tier the session no longer ran on.

Two things a reader needs to know before touching a number:

- **The 2026-08-04 GL flip is a discontinuity in the ledger.** The workload named
  `glmark2-wayland-venus` has measured **vrend**, not zink-on-venus, since that date. Rows before
  and after the flip are not comparable; the name was not changed because renaming it would
  falsify the earlier half instead.
- **Both GL tiers hit the frame clock on the local rig.** As of the 2026-08-08 re-measure, the
  WebGL aquarium held a steady 60 fps to 20 000 fish on vrend. A benchmark that returns "60" is
  measuring vsync, not the renderer — push the load until it stops being 60, or you are recording
  the display's refresh rate.

Live analysis worth keeping: `docs/perf/overhead-inventory.md` (wakeups and exits under load — note
the venus-ring poll budget is absent from the shipped GL path, so that lever is worth only what
Vulkan clients cost) and `docs/perf/venus-cmdstream-overhead.md` (where a Vulkan command stream
spends its time crossing the venus boundary).

## 7. Pitfalls

These are the ones that have actually cost days. Most are epistemic, not technical.

**"Out of memory" in this stack is almost never about memory.** `VK_ERROR_OUT_OF_HOST_MEMORY`,
`ENOMEM`, `ResourceCreateBlob -> ComponentError(-1)` — the venus transport has exactly one error
code for "could not get a buffer": `vn_call_*` returns OOM whenever `vn_ring_get_command_reply`
comes back NULL, for any reason. Distinct causes so far: a poisoned context from a slow ring wait, a
host address-space leak (vm regions 3.5k→23.6k, RSS flat), launchd's 256-fd limit on Dock-launched
apps, and the budget cap refusing deliberately. It is a symptom class, never a diagnosis. Ask **what
refused the allocation**: read the worker log at the timestamp of the guest symptom — the cause is
usually a different, earlier line there — and check the exhaustible resource that is not RAM
(vm regions, fds, mappings). Consider real memory pressure last.

**A scary warning is a lead, not a cause.** We nearly "fixed" two non-bugs by reasoning from a log
line. Before acting on a warning, read its emission site in the source we own and see whether it is
even fatal, then confirm by observation. The `[KK-MODIFIER]` warning is the standing example: a
zero-warning width once sheared 8 px per row, so the warning was not the oracle for the bug it
looked like it described.

**Permanent benign boot noise.** On both tiers, every boot: `context N failed to dispatch
CREATE_VIDEO_BUFFER: 22` followed by `vrend_decode_ctx_submit_cmd: context error reported N
"gst-plugin-scan" Illegal command buffer`. The victim is a throwaway probe context. It is not a
regression and does not need chasing.

**Verify premises before deep-diving.** List the assumptions a bug "obviously" rests on and prove
each one; do not inherit them. Two false premises ("the present pipe is broken", "glmark2 18/18
means GL renders") cost hours apiece.

**Pixel-verify; proxies lie.** FPS counters, "no GL error", "18/18 scenes", exit-0 — none of them
prove anything rendered. Read the real pixels: the IOSurface scanout via
`spikes/venus-draw-probe/iosdump.swift`, or the window capture (`LIMINA_WINDOW_CAPTURE`). Not
`glReadPixels`. When only a human can see the window, ask the user to look.

**Identical A/B results across many configurations mean the differential is not reaching the system
under test.** Five consecutive "exonerations" once all returned pixel-identical damage because
every arm ran the same unmitigated stack. Invariance is a smell, not a verdict — stop toggling and
re-verify the baseline.

**Verify the fix is loaded, at the path the process actually maps.** Half a day went to bisecting a
"regression" that was a half-installed fix: the library sat in `/usr/lib64/mutter-17/` while
gnome-shell loaded it from `/usr/lib64/`. A sub-oracle proving one piece is live proves nothing
about the load-bearing piece — check the artifact itself (mtime/size at the path in
`/proc/PID/maps`).

**Instrument the stack we own.** A few `fprintf`s in KK, virglrenderer, or libkrun beat any amount
of outside-in guessing; an instrumented host Vulkan driver loaded via `VK_ICD_FILENAMES` is what
turned "venus renders black" into the single fact that the vertex buffer the GPU fetched was
all-zero. Keep those oracles in the repo.

**Pin the guest display mode and scale before any graphics benchmark.** An unpinned display makes
runs incomparable. (Note the guest-side helper `~/bin/set-guest-display.py` that older docs cite is
**not** on the current enhanced image — pin by hand or ship the script.)

## 8. Verification recipes

### Boot the thing (the default, and almost always the right one)

```sh
cargo xtask run --disk <enhanced.raw>
```

EFI → GRUB → the guest's own kernel, SELinux enforcing, coexist venus, `--window --net`. This is
the configuration that ships, so it is the configuration to test. The disk boots **in place** —
clone it first (`cp -c`) to keep the original pristine. It sets all the host KK/zink environment
for you; a bare `target/debug/limina --window` with none of it aborts on GPU init
(`Couldn't open libEGL.dylib`), which is missing environment, not a coexist problem.

Fringe modes, for when they are the explicit subject and not otherwise: `--kernel` injection
(bypasses GRUB and SELinux; for deterministic test kernels and early-boot debugging) and
`--gpu-software-2d` (§3.1).

### Confirm which tier is actually live

Inside the seated session — the renderer string is what settles it:

```sh
systemd-run --user --wait --pipe --collect --setenv=WAYLAND_DISPLAY=wayland-0 \
    glxinfo -B                     # → "virgl (zink Vulkan 1.4(… MESA_KOSMICKRISP))"
vulkaninfo --summary               # → "Virtio-GPU Venus (Apple M1 Max)", driverName = venus
```

On the current F44 enhanced image a plain non-login `ssh … vulkaninfo` **does** enumerate venus,
and `/etc/environment.d/90-limina-zink.conf`'s variables **are** present in that shell (the user
manager holds them). Older notes said the opposite as an unconditional rule; it is not one. It was
only verified with a seated session up, though, so the durable rule is:

> Print the variable in the shell you are actually using. Treat an *empty* result as "check the
> environment", not as "venus is broken".

Host-side, active `Mesa:` GL errors in the worker log mean venus is rendering.

### Read the pixels

`spikes/venus-draw-probe/iosdump.swift` dumps any IOSurface by id cross-process (needs
`LIMINA_GLOBAL_SCANOUT=1`); `LIMINA_WINDOW_CAPTURE` captures the window. See
`spikes/graphics-doc-audit/RESULTS.md` for the full re-run recipe used to write this document.

### Full validation

`cargo xtask test` (~28 min, needs `dangerouslyDisableSandbox`). Detaching hands you a **fake exit
code** — the launch shell's, not the suite's. Wait on the real pid, then read the log. Never build
or commit while it runs.

## 9. Open items

| item | where |
|---|---|
| **Stock-tier Vulkan is dead, not degraded** — upstream the venus stub-instance patch so a stock guest keeps llvmpipe | §3.3, `docs/design/16k-page-requirement.md`, `docs/upstreaming/ledger/mesa.md` |
| Retire the 16 KiB requirement for venus on stock guests (`VIRTGPU_PARAM_BLOB_ALIGNMENT` chain) | `docs/design/16k-page-requirement.md` |
| **Fence-accurate present is not wired for vrend** — vrend's flush path never reaches `try_park_present`, so `FENCEPRESENT` never fires and the #24 tear/pacing work does not apply to the tier the desktop actually runs on. Tearing here is a human-eyeball verdict. | `docs/hardening-backlog.md` |
| zink reads `heap.size − heapUsage` instead of `heapBudget`, so GL clients do not see our cap | `docs/design/gpu-memory-budget.md` §Known limits |
| Pure-GL guests are unbounded — the cap is only enforced at `vkAllocateMemory` | same |
| Explicit sync: only binary `SYNC_FD` external semaphores exist; timeline/`OPAQUE_FD` do not | `docs/research/venus-explicit-sync-gap.md` (and read §5–6 before chasing `OPAQUE_FD`) |
| Ship or stop citing `~/bin/set-guest-display.py` | §7 |

## 10. Related documents

- `docs/design/16k-page-requirement.md` — the page-size wall, both halves, and the ways out
- `docs/design/gpu-memory-budget.md` — the host GPU-memory ledger and cap
- `docs/design/{venus,vrend}-snapshot-replay.md` — GPU state across suspend/resume
- `docs/design/venus-ring-idle-wakeups.md` — the idle-wakeup work
- `docs/research/venus-explicit-sync-gap.md` — external semaphores and explicit sync
- `docs/images.md` — image inventory and shipped guest component versions
- `docs/perf/`, `perf/` — analysis and the measurement ledger
- `spikes/graphics-doc-audit/RESULTS.md` — the measurements this document was written from
