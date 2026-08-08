# Hardening / finish-what's-shipped backlog

Consolidated "finish what's shipped" punch-list (cross-cutting, drawn from the milestone sections in
`docs/roadmap.md`). These are loose ends on **already-shipped** milestones (M1–M5) plus the cheap M8
polish wins — closing them rather than opening a new milestone. Each entry points at the roadmap
section / file where it's detailed. Prioritized roughly by user-visible value; pick top-down or by
appetite.

Done first (2026-06-23, with user): **runtime window resize** — ✅ SHIPPED, see below.

## Display / window
- **Runtime window resize / EDID hotplug** — ✅ SHIPPED 2026-06-23 (all 4 layers; L1 sysfs test GREEN
  + windowed-VM log-verified, guest re-modesets with no oscillation). Resizing the limina window
  reflows the guest resolution, no reboot. Design + as-built notes:
  `docs/design/runtime-display-resize.md`; memory `limina-display-resize`. libkrun patches 0025/0026.
  **Extended 2026-07-31** with stable EDID identity + real connector events (libkrun 0119-0121):
  the guest sees the identity/density/refresh of the host display the window is on, and a pushed
  disconnect genuinely disconnects the connector. `docs/design/stable-edid-hotplug.md`.
- **OPEN (found 2026-08-03): the host-derived EDID identity does NOT survive a GUEST reboot.**
  Observed on a windowed venus VM whose host display never changed (BenQ LCD attached throughout,
  confirmed via `system_profiler SPDisplaysDataType`): at first boot mutter reported the connector
  as `('Virtual-1', 'LMN', 'BenQ LCD', '0x6c42fae5')` — correctly mirroring the host display — and
  after a plain `systemctl reboot` **inside the same VM session** it came back as
  `('Virtual-1', 'RHT', 'krun-display', '0x00000001')`, libkrun's generic fallback EDID. The host
  side was not restarted; only the guest was.
  **Why it matters beyond cosmetics:** GNOME keys `monitors.xml` on the `<monitorspec>`
  (connector + vendor + product + serial). When the identity changes, mutter silently *discards*
  the saved configuration and re-picks a default scale — so a user's saved resolution/scale/
  arrangement is lost on every guest reboot, and it fails silently (no error, just a different
  display). It cost a perf run here: the run aborted on its display-pin verify because the config
  written before the reboot no longer matched (`scale=1.3333` instead of the pinned 1.0).
  Per-VM window/fullscreen restore is keyed on the same identity (`limina-display-modes`), so it
  is likely affected too — unverified.
  **Workaround used:** write `monitors.xml` *after* the guest reboot, then `systemctl restart gdm`
  (a session restart applies it without another reboot, so the identity cannot change underneath).
  **Not yet investigated:** whether this is specific to an explicit `--display-resolution` boot
  (mode overridden, so the host-match path may not re-engage), whether the first boot's identity is
  set once at initial scanout and not recomputed on the guest's re-probe, and whether a host-side
  window move/display change is needed to restore it. Reproduce before fixing — a one-shot is not
  yet ruled out, though the host display demonstrably did not change.
- **Capability-scope the scanout IOSurfaces** (security) — ✅ **DONE 2026-06-23 (sw2d + venus)**.
  The worker used to export each scanout as a machine-global `IOSurfaceID` any same-user process
  could brute-force-read (`spikes/venus-draw-probe/iosdump.swift` PoC). Now **both** display paths
  create their scanout/cursor IOSurfaces **non-global** and hand each one's Mach port to the
  supervisor (`limina-surfaceport`: `SurfacePortSender`/`Receiver`, bootstrap rendezvous), keyed by
  id; the supervisor resolves ids from the Mach map. `LIMINA_GLOBAL_SCANOUT=1` re-enables global for
  the debug oracle.
  - **sw2d/baseline path** — the `limina-display` `WindowBackend` publishes its ring. Spike
    `spikes/iosurface-machport`; commits `5980de2 8cafa44 13c6428 383460e`; RED-first test
    `non_global_scanout_is_hidden_from_strangers`.
  - **venus zero-copy path** — the renderer runs in the worker process, so `vkr_mtl_iosurface_alloc`
    publishes directly to the same receiver (no rutabaga/krun_display/virtio_gpu FFI). Patch
    `spikes/virgl-zink-kk/patches/virglrenderer-venus-iosurface-scoping.patch`;
    `LIMINA_SURFACE_PORT_NAME` env from `--surface-port-name`; commit `138d7f6`. Verified live
    (dev-enh + KosmicKrisp): the exact venus-allocated scanout ids (from `SET_SCANOUT_BLOB`) return
    "not alive" to a stranger while a `LIMINA_GLOBAL_SCANOUT=1` contrast dumps the screen; pure
    zero-copy (`LIMINA_PRESENT_COPY=0`) presents them via the Mach map with 0 "unresolved" skips.
- **CapsLock/NumLock LED parity** — surface the statusq LED feedback (libkrun `worker.rs` no-op).
  Roadmap M8.

## Lifecycle robustness
- **Windowed guest reboot** — ✅ **DONE (shipped `efa285f` 2026-06-13, verified live 2026-06-23).**
  A guest reboot in a window keeps the same NSWindow and relaunches the worker, re-wiring everything:
  input/ack fds (`WorkerConn::swap`), a fresh scanout/control reader (`spawn_reader`), gvproxy recycle,
  the resize listener (unlink+rebind), and the surface-port receiver (persists across relaunch; the new
  worker re-publishes its non-global scanouts). Verified: `systemctl reboot` over SSH → worker exit 125
  → relaunch → guest SSH back + window re-displays the desktop and stays interactible, 0 "unresolved"
  present skips, surface-port re-scoped. Regression guard: `worker_conn_swap_retargets_every_field`.
- **libkrun panic→graceful exit paths** — ✅ **DONE 2026-06-23 (libkrun patch 0028).** The aarch64
  HVF vCPU loop no longer `panic!`s on unhandled guest traps: an unknown PSCI/SMC function returns
  `PSCI_RET_NOT_SUPPORTED` and the guest keeps running (standard PSCI semantics), while every other
  unhandled trap (exception class, system register, exit reason, MMIO size) logs the specifics and
  returns `Error::Unhandled`, which `vstate::run_emulation` maps to a clean VM teardown
  (`FC_EXIT_CODE_GENERIC_ERROR`) instead of aborting the worker process. Healthy guests never hit
  these arms (L1 boot still green). RED-first: `spikes/hvf-trap-probe` (96-byte bare-metal arm64
  Image) + `crates/limina-test/tests/hvf_graceful.rs` — verified RED (SIGABRT) before, GREEN after.
- **surface-port `recv` leaks a port name on a malformed message** — cosmetic today, worth a line
  when that file is next touched. `SurfaceReceiver::recv` (`crates/limina-surfaceport/src/lib.rs`)
  returns an error on `descriptor_count != 1` **without** deallocating `msg.port.name`, so a
  complex message carrying an unexpected descriptor count would strand a right in our port space.
  Unreachable in practice: the only sender is our own worker and it sends exactly 0 descriptors
  (release) or 1 (publish), and the release path is discriminated earlier by the complex bit, so
  the branch has never run. Fix = `mach_port_deallocate` before the early return.
- **`vkr_dispatch_vkAllocateMemory` strands an IOSurface ref on one early return** — same shape as
  the entry above, same disposition. `vkr_mtl_iosurface_lookup`
  (`third_party/virglrenderer/src/venus/vkr_device_memory.c:324`) returns a **retained** surface,
  and its `*out_base` is set only by `IOSurfaceGetBaseAddress`. If the lookup succeeds but hands
  back a NULL base (and the MTLTexture import arm doesn't take it), `limina_res_imported` stays
  false and the `VK_ERROR_INVALID_EXTERNAL_HANDLE` early return at `:411` returns **without**
  `vkr_mtl_iosurface_release_ref` — the +1 outlives the call. Every *later* failure path already
  releases it (`:752`). Not RED-first-able: a non-purgeable IOSurface we allocated always has a
  base address, so the trigger can't be produced naturally — which is why this is a backlog line
  and not a fix with a test. Fix = release the ref before that return. **Batch it** with the stale
  "KNOWN LIMIT — attribution on the vrend path" paragraph at `vkr_budget.h:66-71` (obsoleted by
  virgl `53e660e6`, still telling the reader to bind the TLS) into one virgl commit the next time
  that fork is touched.

## Guest-reachable aborts (a guest must never kill the VMM)

Two classes already landed as targeted fixes: the empty-clear-rect vk_meta assert
(kk 0009 + virgl 0045) and the render-pass-begin VU asserts, log-only per the user's call
(kk 0019, `spikes/kk-format-mismatch-abort/`). A THIRD instance of the clear-rect class hit
dogfood-mac 2026-08-04 (a compositor rect with a NEGATIVE offset wrapped past both shipped filters
into an inverted u32 rect → `vk_meta_draw_rects.c:163` assert) — **fixed 2026-08-04**: the
i64-math offset/overflow checks now live in both `vk_meta_clear_rect_is_empty` (mesa
`limina-kk` f7145c1263c) and the vkr sanitize (virgl `limina` 14c22c40); probe + L2 guard
extended (`spikes/kk-empty-clear-rect/`, `vkclearrect.py` — valid + empty + negative + huge
rects). Remaining:

- **Clamp (don't just log) the pass-vs-framebuffer attachment COUNT asserts** in
  `vk_render_pass.c` `begin_render_pass` (`attach_begin->attachmentCount ==
  pass->attachment_count`, `framebuffer->attachment_count >= pass->attachment_count`):
  with asserts off these are an OOB read of the attachment array, not merely undefined
  rendering — the loop must bound `a` by the *actual* array length. Deliberately left out
  of kk 0019.
- **The full "no guest-reachable aborts" audit** (scoped 2026-07-24): per-layer policy —
  asserts = internal invariants only; drivers TOLERATE (clamp/skip; `vkCmd*` can't return
  errors); vkr = THE trust boundary (validate → poison context); libkrun Rust decoders =
  error returns, not unwrap. Surface: 59 asserts in hand-written vkr, 178 in KK vulkan/,
  71 in vk_meta* (compiled into KK), 89 unwrap/panic/assert! in libkrun virtio-gpu.

## Guest app crashes (venus/KK correctness)

- **~~`vkGetPipelineCacheData` returns `VK_ERROR_OUT_OF_HOST_MEMORY`, and GTK4 aborts on it~~ —
  ROOT-CAUSED + FIXED the same day (virglrenderer 0058).** It was never a KosmicKrisp or
  pipeline-cache bug. `vkr_context_wait_ring_seqno` tested `thrd_timeout` while the c11 shim
  returns `thrd_busy` for ETIMEDOUT, so its "STUCK >500ms" branch was dead and **any ring wait
  over the threshold marked the context FATAL** — a diagnostic manufacturing the wedge it was
  added to observe. The poisoned context then refused blob creates, venus could not grow its
  reply-shmem pool, and the next reply-bearing call returned `VK_ERROR_OUT_OF_HOST_MEMORY`; GTK4,
  which never checks its pipeline-cache size query, aborted in `g_malloc` on the uninitialised
  size. A/B at a 1 ms threshold: 1 FATAL before, 0 after (3 STUCK diagnostics instead, every wait
  completing). Write-up + probe: `spikes/venus-ring-fatal-timeout/`.

  **Still open, upstream:** GTK4 aborting the process when `vkGetPipelineCacheData` fails is its
  bug — a driver is allowed to fail that call. Worth reporting; low priority now the trigger is
  gone.

## M4 venus residue
- **~~Concurrent VM makes NEW venus instance creation fail in another guest~~ ROOT-CAUSED + FIXED
  2026-07-20 — it was never a GPU bug: the TEST HARNESS ssh'ed into the WRONG VM.** With a second
  VM holding host port 2222, the supervisor correctly auto-allocated the test VM's ssh forward
  elsewhere (2224), but `Guest::boot` stored `cfg.ssh_port.unwrap_or(2222)` — so every `ssh_exec`
  landed in the BYSTANDER's guest, where identical test creds made all checks "pass" against the
  wrong guest. The `vkCreateInstance → -1` came from that bystander (the f44-kbuild guest: STOCK
  4 KiB kernel + STOCK mesa, whose venus hits the known MAP_BLOB offset-alignment gap — degraded
  exactly as the two-tier floor intends). In-guest forensics that unmasked it: `free -m` showed
  12 GiB in a "4 GiB" VM, battery-driver dmesg in a `--no-battery` VM, uptime 514 s in a 75 s-old
  test. Fix: the harness now pre-allocates an ephemeral port and ALWAYS passes `--ssh-port`
  (limina-test lib.rs); `two_vms_run_in_parallel_on_distinct_ssh_ports` upgraded to ride the
  auto-allocated path and prove per-handle guest IDENTITY (markers), which banner checks cannot.
  Multi-VM GPU coexistence verified working (two windowed KK VMs, venus live in both). LESSON:
  the assume-2222 rule ("READ the port from the log") applies to the harness too, not just
  interactive ssh — and wrong-VM crosstalk is invisible when guests share creds.
- **GLX / Xwayland apps present black on venus** (open, low priority — diagnosed 2026-06-29) — on the
  F44 enhanced tier, `glmark2` (no args → GLX) and `glxgears` show a **black window**; native-Wayland
  GL (Firefox WebGL) is fine. **Root-caused to the PRESENT path, not render/context** (verified on the
  live enhanced VM): `glxinfo -B` shows the GLX context creates and is accelerated — `direct rendering:
  Yes`, renderer `zink Vulkan 1.3(Virtio-GPU Venus … MESA_KOSMICKRISP)`, GL 3.1 (the KK
  custom_border_color cap, [[limina-kk-feature-gaps]]) — and `glxgears` renders **395 FPS** while the
  window stays black. So GL renders fine; the X11 present (zink's **kopper** DRI3/Present WSI on the
  Xwayland-backed-by-venus surface) never gets the rendered pixmaps to the window. This is the known
  "kopper X11 regression" the `venus_replay` X11 probe trips over (`venus_replay.rs:211`). User doesn't
  use GLX apps → deferred. Investigation start: the kopper DRI3 pixmap sharing / X Present on a
  venus-backed Xwayland (is the present-buffer a virtio-gpu blob that isn't flushed/attached to the
  Xwayland Wayland surface?); compare client-direct DRI3 present vs Xwayland glamor.
- **Stock/basic tier: guest Vulkan doesn't degrade to lavapipe** — **ROOT-CAUSED + FIX AUTHORED &
  VALIDATED 2026-07-01** (was: open, reported 2026-06-29 dogfooding). The earlier guesses were wrong:
  lavapipe IS installed, and the loader DOES skip ICDs that fail at *enumerate*. The real mechanism:
  on a 4 KiB-page guest with the coexist GPU, venus's **instance ring** shmem blob (132 KiB — a 4k
  multiple, not 16k) can't be `hv_vm_map`ed (`size%16k=4096`, patch 0011's alignment log), guest mmap
  fails, and **venus returns `VK_ERROR_OUT_OF_HOST_MEMORY` from `vkCreateInstance` — which the loader
  treats as fatal for the WHOLE instance** (unlike `INCOMPATIBLE_DRIVER`, which it skips), killing
  lavapipe with it (`vulkaninfo: vkCreateInstance failed with ERROR_OUT_OF_HOST_MEMORY`). Fix =
  `patches/mesa/0012` (venus: degrade to the existing STUB instance — 0 devices — when post-connect
  ring/version setup fails, mirroring the wire-version-mismatch path). Validated RED→GREEN on a stock
  F44 guest: patched venus + lavapipe → llvmpipe enumerates cleanly. Ships in `build-venus.sh` + the
  F44 mesa RPM (protects the enhanced image's **stock-kernel GRUB-fallback boot** — a 4k kernel + our
  mesa); **truly-stock guests are fixed only when 0012 lands upstream** (Wave-1 upstream candidate) and
  trickles into Fedora — until then the stock-tier Vulkan floor still fails on coexist boots (residual,
  accepted). Headless boots (no GPU device) were never affected — venus declines cleanly there.
  - **2026-07-03 update — decision: address long-term by UPSTREAMING 0012; no host-side mitigation.**
    The mapping failure itself is now two-thirds fixed: the SIZE half host-side (libkrun 0043 +
    virglrenderer 0023), the OFFSET half guest-side via the `limina-virtio-gpu` DKMS module
    (`guest/virtio-gpu-dkms/`; memory `limina-blob-map-16k-alignment`) — with the module installed
    **venus fully works on a stock 4 KiB guest**. A truly-stock guest (no module, Fedora mesa) still
    loses ALL Vulkan on coexist boots: post-0043 the ring's odd-size (0x21000) blob maps fine, but its
    node misaligns the NEXT window allocation's offset → same fatal OOM out of `vkCreateInstance`.
    Host-side mitigations were considered and rejected: an adaptive "venus quarantine" (drop the venus
    capset on the boot after seeing the alignment-failure signature) still leaves first-boot Vulkan
    dead and needs a re-enable policy; 16 KiB-rounding the vkr-reported memoryRequirements only helps
    well-behaved apps — any legal odd-size `vkAllocateMemory` re-poisons later offsets, turning
    "cleanly absent" into "randomly OOMs mid-run". Guest-side stopgap if anyone asks:
    `VK_LOADER_DRIVERS_DISABLE='*virtio*'`. Meanwhile `tests/venus_fallback.rs` (in test-boot.sh since
    2026-07-03) pins the truthful contract — explicit-lavapipe floor works, default path fails
    structuredly, session survives — and auto-tightens the day the default path starts succeeding.
- **GNOME notification shows a green corruption artifact** (open, low priority — reported 2026-06-29
  while dogfooding the F44 enhanced tier) — a notification ("Disk Usage Analyzer / Low Disk Space on
  'boot'") renders a small **green glitch** (a few stray bright-green pixels) just below the bold
  summary line, where the body text / whitespace should be. Native-Wayland venus path; a localized
  region of garbage/uninitialized pixels — smells like a damage-tracking, glyph-cache, or subsurface
  compositing artifact on zink→venus→KK→Metal. Evidence:
  `spikes/venus-draw-probe/notification-green-artifact-2026-06-29.png`. Not chased yet; needs the
  venus pixel-verify discipline (reproduce a notification, capture the IOSurface, isolate the damaged
  rect). Cosmetic, single-widget — low priority.
- **venus TSD-destructor SIGSEGV on libtest worker-thread teardown** (open, isolated + reproduced
  2026-06-30 — fix belongs in venus/mesa) — a wgpu **Vulkan device on venus** (`libvulkan_virtio.so`),
  created and dropped inside a libtest `#[test]`, SIGSEGVs as the test's worker thread exits. The
  *identical* code as a plain binary (`cargo run`) is clean, and the same test under **lavapipe** is
  clean — so it is neither an app bug nor a general venus bug; it fires only when a venus-touching
  **non-main thread exits while the process keeps running** (exactly what a test runner does). Root
  cause (core backtrace via `coredumpctl`): venus registers a thread-specific-storage destructor
  `tss_create(&vn_tls_key, vn_tls_free)` (`vn_common.c`); the libtest worker `pthread_exit`s and glibc
  `__nptl_deallocate_tsd` calls `vn_tls_free` — but `vkDestroyInstance`/`vkDestroyDevice` already tore
  down the per-thread/instance state (and the ICD code may be unmapped) when wgpu dropped the
  device/instance, so the destructor jumps into freed/unmapped memory (frame `#0` = `n/a + 0x0`, no
  loaded module). A plain binary does the work on the **main** thread (no mid-process `pthread_exit`),
  so it is clean; lavapipe registers no such fatal destructor. **Fix layer = venus mesa**
  (`src/virtio/vulkan/vn_common.c`, the `vn_tls_key`/`vn_tls_free` lifecycle): make `vn_tls_free` safe
  to run after the instance/device it relates to is destroyed (NULL the TSD value on teardown, or guard
  against freed state) so a late thread-exit destructor is a no-op rather than a fault; a secondary
  suspect is the Vulkan loader's ICD-unload ordering (whether the ICD can be unmapped while threads
  with pending TSD destructors are alive), but the faulting destructor is venus's. **Minimal repro +
  full analysis: `spikes/venus-teardown-repro/`** (its own cargo workspace, stock crates.io wgpu 29 /
  winit 0.30 pinned to match ghost-ui, no ghost code) — `t0_headless_device_only` is the canonical case
  (no window/surface/present); `t1`/`t2` add a window + present only to show those don't matter; run
  one test at a time (a SIGSEGV takes the whole process down), then `coredumpctl info teardown` for the
  elfutils backtrace. ghost-ui's `frontends/ghost-ui/harness/tests/windowed.rs` currently works around
  it with `std::process::exit(0)` after its assertions (the real-frames-presented goal is verified
  before teardown). Low urgency (test-harness-only; the workaround holds) but it is a real venus
  lifecycle bug worth upstreaming.
- **virtio-gpu flip-completion gap** — ✅ **RESOLVED (verified 2026-06-23); item was stale.** Already
  fixed by `patches/linux/0001` (drm/virtio fence blob-scanout flushes, 2026-06-11): host3d_blob
  (venus) scanout FBs now carry the same fence the dumb path has, so `virtio_gpu_resource_flush`
  `dma_fence_wait`s (50 ms cap) before commit-tail, which gates `drm_atomic_helper_fake_vblank` →
  the (fake) page-flip-complete event fires. Verified on the enhanced tier with `kmscube -A`
  (atomic + fencing): two clean runs rendered 299 and 359 frames at a steady **30 fps**, rc=0, no
  dmesg errors — event-driven atomic clients render, they do not hang. Legacy `drmModePageFlip`
  events also work. GOTCHA that masked this: kmscube polls **stdin** alongside the DRM fd, so over a
  non-interactive SSH session it sees EOF→POLLIN, prints "user interrupted!", and bails after ~1
  frame — run it as `sleep N | kmscube …` to give it a quiet stdin.
- **Direct-KMS double-buffered clients cap at 30 fps** (investigated 2026-06-23) — understood, narrow,
  NOT fixing now. kmscube `-A` runs 31 fps regardless of `LIMINA_FENCE_LATCH_MS` (8 vs 35 ms both
  31 fps), so it is *not* the open-loop latch fallback — the present fences complete via the truthful
  CA-latch ack. Host is 60 Hz, so 31 fps ≈ 2 vsyncs/frame: a strictly double-buffered client that
  blocks on flip-complete misses every other vsync because the #8 fence-accurate present does two
  sequential waits (GPU-render-complete, then CA-latch) and that round-trip exceeds one vsync. The
  Wayland desktop + Wayland fullscreen apps hit 60 fps (mutter triple-buffers and pipelines the next
  frame while the current one latches). So only strictly-double-buffered, blocking, *direct-KMS*
  clients (kmscube, bare SDL-KMS demos) are affected — not the real workload. Fix directions if ever
  pursued: (1) decouple the atomic-KMS fake-vblank from the full CA-latch (fire at render-complete /
  on a vsync-cadence timer — mirrors real hardware, but a #8 design change that must not reintroduce
  tearing), or (2) shave the present round-trip below one vsync (needs worker instrumentation to
  quantify it first). Revisit only if direct-KMS double-buffered fullscreen clients become a target.
- **#28 coherency residue policy** — ✅ **CLOSED (2026-06-23): no action needed; keep venus feedback
  disabled.** Re-framed after a design panel over-stated it. `VN_PERF=no_*_feedback` turns off venus's
  host-visible *feedback* buffers (host writes fence/semaphore/event/query completion into a
  guest-pollable buffer so waits resolve locally with no guest→host round-trip); off, sync rides the
  virtio-gpu per-context ring fence our stack already retires. **We never want feedback on:** the
  round-trip elimination it buys only matters for fine-grained-sync-heavy GPU **compute/ML** (krunkit's
  domain), not a vsync-paced GNOME+WebGL desktop (a handful of syncs per 16 ms frame, blocked on the
  frame fence anyway — the saving is invisible under 60 Hz). And enabling feedback would *exercise* the
  #28 SLC-beyond-PoC host-visible-coherency fragility, i.e. trade robustness for perf we can't use.
  Feedback-off (ring fence) is already tier-2 GREEN — more robust **and** sufficient. So nothing to fix,
  productize, or spike. The earlier "productize or a fresh enhanced guest *hangs*" claim was an
  unverified inference; the two-tier floor is already safe via venus's graceful llvmpipe degrade
  (venus-init failure → software-2D, `VN_PERF` only read when venus actually renders = the enhanced
  tier, which is our own baked image). Two of the panel's "real fix" candidates stay dead on physics
  regardless (host-clean-to-PoC is a no-op — Shared `MTLBuffer` already host-coherent; HVF stage-2
  attrs are not expressible — `hv_memory_flags_t` is permission-only, `hvf/src/lib.rs:289`). **Revisit
  only if limina ever grows a venus-compute tier** (out of charter — it's a desktop VM).
- **Cosmetics** — ✅ **mostly DONE 2026-06-23 (libkrun 0029/0030).** Verified on the seated venus
  tier: the desktop now boots with **zero** `virtio_gpu` dmesg errors (was: a `capset_id=2` GL-probe
  EINVAL + `0x1200` responses for `CTX_ATTACH/DETACH_RESOURCE` 0x202/0x203), venus rendering
  unchanged (gnome-shell `init=0x4` contexts, tier-2 GREEN).
  - `num_capsets` hardcoded 5 → **fixed (0029):** the device hardcoded 5 while `create_rutabaga`
    passed `capset_mask=0` (registers all 9); now both derive from `virgl_flags` via one helper, so
    a `VENUS|NO_VIRGL` guest enumerates exactly the venus capset and never probes ones we can't serve.
  - `0x1200`/`0x202`/`0x203` `CTX_ATTACH/DETACH_RESOURCE`→ErrUnspec → **fixed (0030):** in coexist
    mode the 2D scanout resources (boot fb / fbcon) aren't in the 3D renderer's map, so the kernel's
    attach/detach of them to a 3D context is now an idempotent no-op (also covers the detach/teardown
    race). Real 3D resources (de)attach normally.
  - **Firefox MSAA silent non-AA** → **documented, NOT chasing (known cosmetic).** Core MSAA works on
    zink/venus (`spikes/venus-draw-probe/msaa-test.c` passes); the gap is Firefox-specific — its
    `MozFramebuffer::CreateImpl` combo (color RENDERBUFFER + DEPTH24_STENCIL8 @ samples=4) reports the
    backbuffer incomplete, so it silently falls back to non-AA. One app's AA quality, with the general
    path working — not worth a venus/zink/KK FBO-completeness rabbit-hole. Reopen only if MSAA breaks
    broadly. (`msaa-test.c` is the standing oracle; memory `limina-tier2-venus` thread 7.)
  - Remaining (untouched, genuinely low-value): KK GPU-side per-draw root re-fetch (only if GPU-bound
    workloads reappear). Roadmap M4 (~line 413).
- **Khronos VK-GL-CTS on the enhanced guest as an opt-in validation layer** (noted 2026-07-20, not
  started) — add a way to run the Khronos conformance suite
  (<https://github.com/KhronosGroup/VK-GL-CTS>: dEQP-VK for Vulkan, KHR-GL/dEQP-GLES for GL) inside
  the enhanced guest, exercising the full stack we own end-to-end: guest mesa (venus, zink) →
  virtio-gpu → virglrenderer (vkr) → KosmicKrisp → Metal. Rationale: our current oracles
  (pixel-verify probes, venus_replay, glmark) catch crashes and gross wrong-rendering; CTS is the
  conformance-grade net that catches subtle wrong results (format/precision/sync edge cases), and
  since we own every layer, each failure is actionable — same spirit as the KK feature-gap probing
  ([[limina-kk-feature-gaps]]). **Explicitly NOT in the default suite** (`test-boot.sh` stays as-is):
  a full dEQP-VK run is hours-long; this is an additional, on-demand layer. Sketch: build the CTS for
  aarch64-linux (in the limina-build container or the F44 build guest), stage it into the enhanced
  test image (or a virtiofs share), drive it over ssh via a `scripts/`/xtask runner with curated
  caselists — start with the `*-main` mustpass subsets and a smoke list sized for minutes, keep a
  known-failures baseline so runs diff against expectations rather than demand 100%. Useful
  precedent: virglrenderer/crosvm CI runs exactly this shape of guest-CTS-subset job.

## M5 hardening
- **Clipboard test-coverage gaps** — the ext-data-control (enhanced) backend is live-verified only,
  not under automation (`l1_session_helper` exercises only the RemoteDesktop fallback); plus
  stale-serial races, multi-peer broadcast + dead-peer pruning, helper reconnect after supervisor
  restart / D-Bus session death. Roadmap M5 (~line 522); memory `limina-m5`.
- **virtiofs DAX/shm window** — `VirtioShmRegion` in `fs/device.rs`; confirm shm-window alignment +
  FUSE_SETUPMAPPING/SHMCAP on 16 KiB host pages (enhanced tier already runs 16k guest = host-page;
  test stock-4k separately) + host↔guest uid mapping. Roadmap M5 (~line 502).

## M3 networking
- **gvproxy reconnect-on-HANG_UP** — today the net worker logs FATAL and permanently disables the NIC
  on HANG_UP (`worker.rs:146`); the supervisor recreates the path. A small libkrun reconnect patch
  would survive a gvproxy restart without a full VM restart. Roadmap M3 (~line 321); memory
  `limina-m3-networking`.

## M2 / M8 polish wins (cheap, host-side)
- **fn/aux-key buckets: settings UI + the Accessibility cliff** (added 2026-07-31, with the
  bucket policy in `crates/limina-input/src/auxkey.rs`). Two follow-ups the design review raised.
  **(a)** The buckets (`Media` soft-grab, `Volume` full-grab-only, `Brightness`/`Other` host-only)
  are meant to become per-key runtime settings; shape that config as `nx_key -> Option<GrabMode>`
  with buckets as defaults, not per-bucket overrides, or the first split *within* a bucket forces
  a refactor. **(b) The settings UI must render these toggles disabled with a "requires
  Accessibility" note when the tap isn't installed** (`TAP_PORT` null). Aux keys are delivered
  *only* to a CGEventTap — never to a local NSEvent monitor — so without the grant the whole
  feature is inert while ordinary keys keep working: a *partially* working keyboard, which reads
  as a limina bug rather than a permission problem. There is also no way to detect the press
  without the tap, so no just-in-time prompt is possible; the UI is the only place to say it.
  **(c) ANSWERED 2026-07-31 — fn+F3–F6 are not aux keys at all.** Mission Control, Spotlight,
  Dictation and Do Not Disturb arrive as **ordinary keyDowns with keycodes 0xA0/0xB1/0xB0/0xB2**
  (Globe = 0xB3), not as NX_SYSDEFINED — a third mechanism, unrelated to the buckets. So
  promoting one (Mission Control → GNOME overview) is a **`keymap.rs` entry**, not an `auxkey`
  bucket edit; `macos_special_action_keycodes_have_no_guest_mapping` fails the moment someone
  maps one, which is the point to decide the routing deliberately. These keys are **inert under a
  grab by design**: the tap drops any keycode with no guest mapping rather than handing it to
  macOS, because forwarding a key blind can fire a destructive host action (reboot/sleep/eject on
  keyboards that have one) that a grabbed user can't cancel — "classify and route on purpose, or
  drop; never forward blind". Ctrl-Opt reaches them meanwhile. Evidence + the rejected
  pass-through alternative: `spikes/fn-key-probe/RESULTS.md`, `output-f3f6.txt`.
- ~~**Pointer warps / pointer capture**~~ — **DONE** (2026-06-27). `Cmd-Ctrl-G` capture mode feeds
  the guest a separate relative-mouse virtio-input device; closes the guest-warp gap. Host cursor
  pinned by warp-to-centre (CGAssociate-false alone insufficient on macOS 26).
- **Pointer grab: review / redesign / validation** (reopened 2026-07-11, user-requested). Three
  strands. **(a) On the dogfood deployment (dogfood-mac) the Accessibility permission seemingly still
  isn't sticking, so the grab's CGEventTap can't capture Cmd-Tab** (user, 2026-07-11). This is the
  suspected tail of the ad-hoc-signing TCC churn: the fix (Apple Development identity + team-pinned
  designated requirement in `build-app.sh`, plus tap retry / AX re-prompt on Cmd-Ctrl-G, `301a702`)
  shipped 2026-07-03 but was **never validated on dogfood-mac** — the planned one-time TCC re-add there is
  still outstanding. Validate: confirm the deployed .app is identity-signed (`codesign -dr -`
  shows the team-pinned DR, not ad-hoc), have the user delete + re-add the Accessibility entry
  once, then check the grant survives a redeploy. If it *still* drops with a stable DR, that's a
  new bug worth its own root-cause pass. Diagnostics on dogfood-mac read-only; the re-add/redeploy steps
  are the user's.
  **(b) Grabbed cursor feel** — even where the grab works, captured-mode movement should be
  indistinguishable from non-grabbed. **(c) — BUILT and dogfooded:** the confinement grab sketched
  below shipped as the fullscreen pointer grab (`docs/design/fullscreen-pointer-grab.md`; five
  dogfood rounds recorded in `1021acb`). **(c) Redesign to explore: a *confinement* grab** — keep the
  exact non-grabbed pointer path (absolute coordinates through the fit rect, host cursor wearing
  the guest shape, identical movement/acceleration) and have the grab only (1) clamp the cursor to
  the window's fitted content and (2) capture system combos (Cmd-Tab etc., extending the
  CGEventTap/`match_host_shortcut` framework). That drops the relative-mouse device + warp-to-centre
  scheme from the ordinary grab entirely — sidestepping both (a) and (b) and likely the RD fight —
  while the relative device stays for guest pointer-lock (games), which genuinely needs deltas.
  Prior verified findings in the next entry still apply.
- **Pointer-capture containment — prior findings** (parked 2026-06-27). The current scheme re-pins the
  (hidden) host cursor to display-centre on *every* captured motion event. Verified facts: macOS 26
  `CGAssociateMouseAndMouseCursorPosition(false)` does NOT freeze the cursor (its `CGEventGetLocation`
  tracks the mouse 1:1), the session `CGEventTap` never disables under load, and with the per-event
  warp removed the relative deltas fed to the guest are perfectly clean. **Works well locally.** The
  warp's weakness shows only when *another* agent also drives the macOS cursor — a **remote-desktop
  client** used to operate this Mac — where the constant re-pin fights the RD cursor and reads as
  jitter/snapping (an edge-only-warp variant was tried and reverted; it didn't clearly help under RD).
  **Plan when we return:** research how VNC/RDP **servers** solve the same capture problem (they have
  this exact problem space and are far more numerous) before redesigning. Candidate angles: a real
  cursor-freeze API instead of warp, `CGDisplayHideCursor`+associate semantics, an `IOHIDEventSystem`
  relative-tap, or detecting/co-existing with an upstream RD capture. (2026-07-23: the guest-side
  half of the old scheme — the enhanced-tier flat pointer profile + `LIMINA_CAPTURE_SENS` — was
  retired: captured motion now integrates the macOS-accelerated deltas into a virtual cursor
  driving the absolute tablet, so no guest profile tweak is needed. The per-event centre re-pin,
  and thus this RD-confound item, is unchanged.)
- ~~**Non-grabbed guest cursor renders too large in a non-fullscreen window**~~ (user-reported
  2026-07-11) — **DONE.** The shape cache is now keyed on **(IOSurface id, scale_key)** with
  `cursor_scale_key(fit_w, guest_w)` and the fit-rect scale applied to the `NSImage` size and
  hotspot (`crates/limina/src/window/cursor.rs:111,132,140,215`; tests at `:284`), so the sprite
  tracks the window's fit scale instead of rendering at 1 px = 1 pt.
- ~~**Fullscreen**~~ (`Cmd-Ctrl-F`) and ~~**keymap remap / Command-Option swap**~~ (`--swap-cmd-opt`)
  — **DONE** (2026-06-27). The formerly-remaining M8 polish is done too: ~~**system-combo
  capture**~~ (the capture/soft-grab CGEventTap) and ~~**multi-display**~~ (display modes +
  per-host-display identity; `docs/design/display-modes.md`, `docs/design/display-cutouts.md`).
  Roadmap M8.

## Dogfooding / Parallels migration
Surfaced 2026-06-29 while planning the migration of a stock Fedora 44 Parallels VM onto limina on a
second Apple-Silicon Mac (full runbook: `docs/dogfooding-parallels-migration.md`).
- **`gvproxy` not bundled** — ✅ **DONE 2026-06-29** (`458664b`). `limina --net` resolved gvproxy only
  from `$LIMINA_GVPROXY_BIN`/Homebrew/`PATH`, so networking was dead on a Mac without Homebrew. Now
  vendored into the app (`Contents/MacOS/gvproxy`, copied + ad-hoc signed by `build-app.sh`) and
  resolved bundle-relative; priority `override > bundled > Homebrew > PATH` is a pure, unit-tested
  policy (`gateway.rs::resolve_gvproxy_bin`, RED-first test).
- **Agent install was a separate SSH flow** — ✅ **DONE 2026-06-29** (`458664b`). `install-enhanced.sh`
  now also installs `limina-agent` (+ unit + flat-pointer gschema override) when staged into the
  payload, and runs `restorecon` so it starts on a stock SELinux-**Enforcing** guest (the dev guest
  dodged this with `selinux=0`). The whole enhanced upgrade now rides the one offline virtiofs channel
  instead of also needing network + gvproxy. (`install-guest-agent.sh` remains the dev-loop SSH path.)
- **Enhanced install could brick the guest (unsafe boot-default switch)** — ✅ **DONE 2026-06-29**.
  `install-enhanced.sh` made the *unproven* 16k kernel the permanent GRUB default (Fedora's kernel
  install also auto-promotes the newest kernel). When the 16k initramfs failed to mount root (here:
  `/boot` ran low on space → an incomplete/driverless initramfs → the dracut emergency shell), the
  guest was stranded — limina has no keyboard at GRUB/emergency (see next) to pick stock. **Real
  dogfooding brick.** Fix: the installer now (1) pre-checks `/boot` free space, (2) force-includes the
  virtio/input/FS drivers in the initramfs (so root mounts *and* the emergency shell has a keyboard),
  (3) verifies the initramfs actually contains the root driver before trusting it, and (4) keeps
  **stock** as the permanent default while booting 16k **once on trial** (`grub2-reboot` next_entry)
  with an on-success systemd unit that promotes 16k only after it reaches multi-user — so a failed 16k
  boot auto-returns to stock on a power-cycle, no keyboard required. Recovery for an already-bricked
  guest: revert to a pre-install disk clone. (Two-tier guarantee: stock must always stay reachable.)
- **16k kernel can't mount a v1-space-cache btrfs (second migrated-guest brick)** — ✅ **DONE
  2026-06-29**. A 2021-origin Parallels guest's btrfs root still used the legacy **v1 free-space
  cache**, which a **16 KiB-page** kernel refuses to mount (`BTRFS error: open_ctree failed: -22`;
  "v1 space cache is not supported for page size 16384 with sectorsize 4096") → `sysroot.mount`
  fails → the (keyboard-less) dracut emergency shell. The stock 4k kernel mounts it fine and a fresh
  accessible base already uses v2, so this only bites *migrated/old* installs. Diagnosed from a
  verbose one-shot serial capture; confirmed fixed on the guest by setting `space_cache=v2` on every
  btrfs fstab entry → a reboot builds the **free-space tree** (`compat_ro 0x3` =
  `FREE_SPACE_TREE|VALID`, permanent) → the 16k then mounts (no cmdline option needed). Fix in
  `install-enhanced.sh`: `ensure_btrfs_free_space_tree` detects any mounted btrfs still on v1 (no FST
  compat_ro bit), sets `space_cache=v2` on all btrfs fstab lines, builds the tree live
  (`remount,clear_cache,space_cache=v2`) and **verifies** it. The 16k one-shot is armed only once the
  tree exists; otherwise a `limina-arm-16k.service` arms it after a plain stock boot has built it (so
  the 16k never boots onto a still-v1 fs). **NEEDS end-to-end validation on a real v1-btrfs guest**
  (only the awk + `bash -n` are checked so far).
- ~~**No keyboard at GRUB / early boot / dracut emergency shell**~~ — **(a) FIXED & user-validated
  2026-06-30** (commit `3210a36`): VirtioKeyboardDxe vendored into the GOP firmware + ConIn wiring
  (`patches/edk2/`), plus the libkrun virtio-input Inactive-on-reset fix (patch 0037) so desktop
  input survives the firmware→kernel handoff. The keyboard works at the GRUB menu; the RELEASE GOP
  firmware is rebuilt. **(b) partially open:** `virtio_input` is forced into the *enhanced*
  initramfs by `install-enhanced.sh`; a *stock* initramfs may still lack it → the dracut emergency
  shell on a never-enhanced guest can still be keyboard-less. Small residual, revisit with the
  import tooling.
- **No Parallels-import tooling** (open) — converting an existing Parallels disk (merge snapshots →
  `qemu-img -f parallels` → raw, the `virtio_mmio` initramfs regen, `console=` GRUB args, Tools
  removal) is documented in the runbook but not scripted. A guided `import` helper would de-risk the
  footgun (`virtio_mmio`, not `virtio_pci`).
- **No guest-tools distribution path** (open, architectural) — the enhanced tier can't be built or
  installed from `limina.app` alone (no RPMs, no toolchain on a second Mac). Recommended: a versioned
  out-of-band payload + a `limina install-guest-tools` subcommand that stages it into a `--share`. The
  F44 in-guest build prep addresses the *build* half; host orchestration is still open.
- **No payload↔guest version manifest check** (open) — `install-enhanced.sh` doesn't verify the
  payload's Fedora release matches the booted guest; a mismatch (esp. mutter↔gnome-shell ABI) breaks
  the desktop silently. Add a manifest + `/etc/os-release` guard.
- **KK/Metal never tested cross-machine** (open/unknown) — the host Vulkan-on-Metal stack was only
  exercised on the M1 Max / macOS 26.5 dev Mac. `--gpu-software-2d` is the degraded fallback if venus
  init aborts on different silicon/macOS.
- ~~**F44 enhanced tier blocked** — GNOME 49→50 mutter/cogl scanout regression~~ — **FALSIFIED
  2026-06-29**: the feared regression (and the mutter-50 `kk_encoder.c:299` assert) did NOT
  reproduce; the F44 enhanced desktop was validated end-to-end (16k + venus + patched mutter 50.1,
  pixel-verified; see `docs/images.md` §Component versions and [[limina-enh-delivery]]).

## M14 USB / auth

- ~~**No `--no-fido` opt-out (product-parity gap, 2026-07-25).**~~ — **DONE 2026-07-25.**
  `--no-fido` / `[hardware] fido`, defaulting true like the rest. It gates the passkey **store**,
  so the opt-out is transport-wide (no uhid capability advertised *and* no USB gadget) rather than
  just dropping the gadget — a half-disabled credential surface is not a disabled one. Independent
  of `--no-usb` in both directions, because FIDO's uhid transport rides the agent and not the
  controller. See `docs/fido-authenticator.md` §"Turning it off".

## M9 snapshot hardening (from the 2026-07-18 transport-restore removal)

- **OPEN 2026-07-28: gen-2 restore loses buffer creates → `seated_gnome_session_survives_snapshot_restore`
  FAILS INTERMITTENTLY (pre-existing, NOT the virgl-0054/0055 journal batching).** Failed
  3 consecutive runs on 2026-07-28 (including once on the pre-0054 baseline dylib), then
  passed the same evening's full-suite run with 0055 — timing-dependent, not deterministic.
  Failure shape: the gen-1 restore's replay logs ~7 tolerated `replay: entry failed` drops (buffer ids,
  `failed to look up object N of type 9`, gnome-shell ctx), meaning the FIRST live-recorded
  journal is already missing those `vkCreateBuffer` entries; the journal re-baselined during
  gen-1 replay inherits the holes, and after the SECOND restore the parked `vkpipeline.py`
  client's queue dies (`vkQueueWaitIdle` → -13 at beat 29). Bisect evidence: reproduces
  identically on the pre-0054 dylib (0053 tip) — three runs, same shape. The suite was
  reported 40/40 at 12e7fd9 the same day, so either that run's pass was environmental or the
  gen-2 leg silently skipped; treat TODAY's failure as the ground truth. Repro:
  `LIMINA_TEST_KEEP_SCRATCH=1 scripts/test-boot.sh debug seated_gnome_session_survives_snapshot_restore`
  (keep-scratch preserves all three generations' supervisor logs). Attack: find why live
  recording misses consecutive buffer creates (orphan_adds? dropped_fatal? check
  `vkr_journal_get_stats` counters at first export) before suspecting the replay side.

- **FIXED 2026-07-20 (virglrenderer 0040): the vkmark-on-resume crash — journal create-arg
  closure.** Root cause was neither candidate shape: the journal pruned a destroyed object's
  create entry even when a retained CREATE referenced the id in its wire args (pipeline ←
  destroyed shader modules/layout, legal and universal). The dropped pipeline create left a
  guest-live pipeline missing at replay; the first parked ring command referencing it after
  `replay_end` (FATAL sticky again) killed the ring → guest-visible FATAL status → vkmark
  abort. Fixed by pinning every CREATE's decoded handle refs (generalizing the blob←memory pin);
  RED/GREEN via the new `vkpipeline.py` leg in `venus_session_preserved`. Full forensics:
  `spikes/m9-vkmark-resume-crash/RESULTS.md`; design:
  `docs/design/venus-snapshot-replay.md` §"vkmark-on-resume crash FIXED".
  Remaining follow-ups from the same incident, still open:
  - guest kernel `RESOURCE_UNREF → 0x1203` right at resume (guest unref of a resource the host
    lost) — benign-looking (kernel logs and continues), unexplained.
  - ctx 4 (gnome-shell) `vkr_dispatch_vkWaitRingSeqnoMESA:399` ring FATAL 55 s post-resume
    (wait for a seqno the restored ring never reached) — desktop survived; plausibly its own
    first use of an affected pipeline (would be cured by 0040) or a seqno-epoch gap; watch
    post-deploy.
  - ~~guest-side hardening candidate (upstreamable mesa): venus failing submits with
    `VK_ERROR_DEVICE_LOST` instead of `abort()` on ring loss.~~ **DONE 2026-07-20** =
    `patches/mesa/0016-venus-ring-loss-device-lost-not-abort.diff`, shipped in guest mesa
    `26.1.4-2.limina.fc44` (both F44 enhanced images refreshed). Validated by A/B: with a
    pre-0040 host (replay gap present) the `vkpipeline.py` client now prints
    `PIPE FAIL beat7-vkQueueWaitIdle -1` and exits cleanly — zero coredumps (pre-0016 the same
    scenario SIGABRTed); with the 0040 host the full gate stays green (239 s, both generations).
    Watchdog/renderer-hang aborts deliberately unchanged. F43 pickup at its next respin.

- **Worker-quiesce during `dump_ram` (torn-dump race).** A device worker writing guest RAM while the
  RAM dump runs can tear the dump (used.idx advanced, payload half-copied). Pausing vCPUs stops new
  kicks but not asynchronous writers already in motion — loudest is **net RX from gvproxy** (delivers
  regardless of vCPU state) on the raw path; on the s2idle production path it narrows to the **GPU
  renderer** (the guest froze net/blk to INIT before we snapshot). Fix = *stop the writers, not the
  rings*: park the separate-thread writers (GPU renderer / blk) for the dump's duration. If
  `save_snapshot` runs on the main event-loop thread, EventManager-dispatched devices quiesce for free;
  verify the thread inventory vs source. Pre-dates M9.3; the removed drain accidentally masked it.
- **`Queue::len` unwraps the avail index (`queue.rs:443`).** Any spurious kick of a **not-ready** queue
  (e.g. the balloon free-page-reporting queue when `F_REPORTING` is masked, patch 0059) reaches
  `Balloon::process_frq → Queue::pop → Queue::len`, which `unwrap()`s `avail_idx` on an unconfigured
  ring → panic (exit 101). The M9.3 drain removal deleted the caller that tripped it, but the unwrap is
  a live balloon-hardening item (also on the upstreaming triage list) — `Queue::len`/`is_empty` should
  fail soft on a not-ready/invalid ring.

## GPU / rendering perf
- **Should the enhanced tier stop forcing zink? (i.e. delete `/etc/environment.d/90-limina-zink.conf`)**
  — 📋 open, raised by the user 2026-08-01 now that vrend is well supported. Attractive for the right
  reasons: they are blunt globals hitting every process in the guest, and the baseline tier already
  runs vrend without them. **But it is not a "stop forcing" — it is a TIER SWITCH**, so measure before
  believing:
  - **Where it lands:** not llvmpipe. The guest ships both `virtio_gpu_dri.so` and `zink_dri.so`, and
    the host advertises both capsets in coexist (`GPU_COEXIST_FLAGS` in `crates/limina-vmm/src/krun/mod.rs`
    — `VENUS` plus the vrend EGL/GLES trio, `NO_VIRGL` deliberately off). Unset ⇒ GL runs on **vrend**.
  - **Our own honest numbers say venus still wins.** Post fence-honesty (`f0fe78a` + `98777bf`),
    crossmark has venus winning or tying **every** guest cell; the vrend small-frame advantage was
    fences retiring at decode (`glFinish` waited for nothing). That belief died on measurement — the
    reverse belief has to clear the same bar. See `limina-virgl-vrend-perf`.
  - **The real blocker is pacing, not throughput:** fence-accurate present for vrend is still OPEN
    (`docs/design/vrend-iosurface-scanout.md`) — vrend's flush path never reaches `try_park_present`,
    so `FENCEPRESENT` never fires and the whole #24 tear/pacing arc (`c569129`, `c33d9a0`) does not
    apply. Moving the desktop to vrend today gives that up, and tearing is a human-eyeball verdict.
  - **`VK_DRIVER_FILES` is a SEPARATE knob and should stay regardless.** Unset, the loader enumerates
    `lvp_icd` beside `virtio_icd`, so a client that takes device 0 without checking silently lands on
    lavapipe. Our venus-specific guest mesa patches (0015 WSI present, 0016 ring-loss, 0017 submit
    free-list) also only pay off on venus.
  - **How to settle it:** A/B on a **clone** of an enhanced image (never in place), env file present vs
    removed. PIN display mode + scale first or the run is void (`limina-perf-display-pinning`); use
    `vkmark` as the control (Vulkan — it should not move at all), judge on the crossmark trio +
    aquarium via `scripts/perf-ledger.sh` (glmark2 swings ±10% between boots), and eyeball for tearing
    since no counter reports it. Revisit after vrend gets fence-accurate present — that is what would
    make removal a genuine simplification rather than a downgrade.
- **Stock-tier virgl (vrend GL) desktop slowness — ROOT-CAUSED 2026-07-28: a DEBUG-build present-path
  artifact, NOT a virgl regression.** The virgl present path is readback-per-frame (only venus blobs are
  IOSurface-backed; `flush_resource` falls back to `transfer_read` → staging → per-pixel RGBA→BGRA
  convert → canvas upload, `virtio_gpu.rs` / `limina-display::iosurface`). In a debug build that
  per-pixel convert (with debug asserts) costs ~60-100 ms per 2560×1440 frame → ~8-9 presents/s —
  *slower than software-2D* because sw-2D's guest framebuffer is already canvas-ordered (plain memcpy,
  cheap even unoptimized). Both sightings (2026-07-24 and 2026-07-28) were debug boots
  (`cargo xtask run` builds debug). **On a release worker the same guest animates at 60 fps** (median
  FLUSH2 gap 16.8 ms during overview animation; the apparent "repeating ~0.5/1 s stalls" were the drive
  loop's own idle gaps — user eyeball confirms "night and day").
  - **Fixed for dev boots:** `[profile.dev.package.limina-display] opt-level = 3` in the root
    `Cargo.toml`, so debug boots present at representative speed.
  - **UPDATE: zero-copy vrend scanout SHIPPED 2026-07-28** (virglrenderer 0053, plan B+A1) — the
    readback-per-frame description below is history for the scanout path. **Fence-accurate present for
    vrend is still open**, which is what the entry above turns on. Original text kept for the
    reasoning:
  - **Remaining, by design but worth fixing — zero-copy vrend scanout.** virgl presents pay
    readback (~2 ms) + convert (~4 ms release) + upload every frame, and the readback path bypasses
    the fence-accurate present (`FENCEPRESENT` never fires there). Residual jank on release (user,
    2026-07-28) is the expected symptom. The venus zero-copy chain (KK IOSurface-backed VkImage →
    `rutabaga.iosurface_id()` → `present_surface`) has a natural vrend analogue, all in layers we own:
    zink allocates `PIPE_BIND_SCANOUT` resources IOSurface-backed on KK → vrend exposes a
    resource→IOSurface query → rutabaga extends `iosurface_id()` to vrend resources → the worker's
    plain `set_scanout` resolves it exactly like `set_scanout_blob` already does. Spike first: prove
    zink-on-KK can export an IOSurface-backed scanout texture. This also puts virgl on the
    fence-accurate present path.
  - **Unverified leftover:** the 07-24 "~17 fps WebGL blob" was measured on a debug boot too — re-bench
    GL throughput on release before treating vrend shader/draw perf as a problem. The
    `>100 copy boxes` zink warning was a red herring for presents (it's upload-side, latches once per
    resource); revisit only if release GL throughput still disappoints.
  - **Benign, ruled out:** the `Mesa: error GL_INVALID_ENUM in glTexImage2D(...)` lines are one-time
    startup format probing; `vrend_decode_ctx_submit_cmd … "gst-plugin-scan" Illegal command buffer`
    (guest kernel `SUBMIT_3D → RESP_ERR_UNSPEC` at session start) is startup GL probing.
  - Probe recipe that cracked it: boot the stock clone with
    `RUST_LOG=info,limina_vmm=debug,krun_devices=trace` (worker logs now carry **µs timestamps**),
    drive the overview via `busctl --user set-property … OverviewActive b true|false` over ssh, take
    `sample <worker-pid>` during animation, and read FLUSH2 gap distributions. Memory:
    `limina-virgl-vrend-perf`.

## Clipboard (M5)
- **Whose clipboard wins when a guest has several sessions?** — 📋 open design question, raised
  2026-07-31. A guest routinely runs one `limina-agent-session` per graphical session (dogfood-guest had
  three: a GNOME session, a niri session, and the gdm greeter). All of them are clipboard-capable
  peers, so with per-peer serials (the fix for the offer-drop bug below) **any** session can push to
  the host pasteboard and the last write wins.
  - **This is not obviously wrong — it's a trade-off.** Copying in one session and pasting in another
    is a genuinely nice property, and sharing with the **greeter** is desirable too (user, 2026-07-31:
    "it's good to share clipboard with the greeter if possible"). So do *not* reflexively restrict
    peers by session class.
  - The alternative — bind guest→host to the seat's **active** session — is more predictable when a
    background session copies something the user never meant to send, but it costs the cross-session
    paste and needs new protocol: the agent would have to report its session's id/active state (the
    host cannot see `loginctl` from outside).
  - Weigh before building. Possible middle ground: keep last-write-wins, but have the host log which
    peer/session a copy came from so surprises are explainable.
  - **M12 now depends on this same seam** (2026-08-01). SPICE `vdagentd` serves only the logind-ACTIVE
    session, so "which session does vdagent cover" is the *same* active-session question wearing a
    different hat — and the per-session native-vs-SPICE arbitration (roadmap M12 task 4) has to answer
    it. Current lean: native always claims **inactive** sessions and SPICE serves only the active one,
    which keeps cross-session paste. Decide both together rather than twice.
  - Related, already fixed: the host used to ratchet ONE `guest_serial` across all peers while each
    agent numbers its offers from 1, so a long-lived session permanently silenced newer ones
    (dogfood-guest: the niri session's copies never arrived). Serials are now per-connection —
    `crates/limina-test/tests/l1_clipboard_multi_session.rs`.

---

## Suspected test flake — `l1_real_session_helper_bridges_clipboard_via_mock_mutter`

Seen **once**, 2026-08-04, during the full HVF suite run that validated virgl 0059/0060. Treated as
a flake by decision, not by analysis — recorded here so a second sighting is recognised as a repeat
rather than re-investigated from scratch.

```
mock log never contained "PASTED sess-host-to-guest-42"; current content:
CLAIMED_NAME / CREATE_SESSION / START / ENABLE_CLIPBOARD
```

`crates/limina-test/tests/l1_session_helper.rs:277`. The bridge got as far as enabling the
clipboard and then no paste arrived — consistent with a timing/wait bound rather than a logic
error, but that is a guess, not a diagnosis. Everything else in the run passed, including
`venus_desktop_pixel_verifies_through_host_capture`, and nothing connects vkr's external-memory
advertisement to a mock-mutter clipboard bridge. **If it fires again, stop treating it as noise:**
the first thing to check is whether the wait for `PASTED` is bounded generously enough under a
parallel nextest lane, since the suite has run parallel since 2026-08-03.

---

When a milestone's loose ends are all closed, fold the remainder back into the roadmap milestone
status. Greenfield milestones still ahead: **M7 USB**, **M8 audio + x86**. (**M6 dynamic memory** shipped
2026-06-26 — see `docs/design/m6-dynamic-memory.md` + memory `limina-m6-dynamic-memory`.)
