# limina — project guide for Claude

## What this is

limina is a native macOS app (Apple Silicon) that runs **Linux** desktop guests on
**libkrun + Hypervisor.framework**, built in Rust, aiming to **replace Parallels**.
First milestone: boot a local stock Fedora image to a usable desktop (current base
`Fedora-Workstation-43.accessible.raw`; canonical image inventory in `docs/images.md`).

Target features (most deferrable; all must be considered in the design from the
start): great 3D acceleration, first-class fullscreen, mouse capture, macOS
key-combo capture, customizable keybindings + keyboard-layout remap (swap
Command/Option), clipboard sharing, USB passthrough, NAT + bridged networking,
low host+guest memory overhead, and **dynamic memory** (give the VM a `min..max`
range; it takes/returns RAM via ballooning).

## The core tenet: we own and may patch the entire stack

This is the most important thing to internalize. **limina is not a thin wrapper over
fixed dependencies.** We are willing to fork, patch, and rebuild *any* layer to get
the behavior we want, and the design should reach for that lever whenever it's the
right tool — not treat upstream as immutable:

- **libkrun** — vendored under `third_party/` (gitignored), built from source, carried as a
  `git format-patch` series under `patches/libkrun/` (with `UPSTREAM_BASE`); apply with
  `scripts/apply-libkrun-patches.sh`. We add devices, APIs, and behavior it lacks (e.g.
  software 2D scanout for GL-less hosts [shipped], balloon target/inflate control, runtime
  display resize, zero-copy scanout, USB). To change libkrun: edit the checkout, commit on
  a `limina/*` branch, re-export the series (see `patches/libkrun/README.md`).
- **virglrenderer / rutabaga** — patchable; we already depend on the Apple-blob
  additions and will fork if Homebrew's build lacks them.
- **The guest Linux kernel** — we control it for the *enhanced* tier. We can change
  its config (page size, drivers), carry kernel patches, or **build entirely new
  kernel features** when that's the cleanest fix (e.g. host-page-aware free-page
  reporting for the 16 KiB-host / 4 KiB-guest mismatch). Both boot paths
  (libkrunfw's bundled `linux-6.12.x`, and Fedora's own kernel on the EFI path) are
  fair game — but the stock-distro kernel must always still **boot** (degraded is
  OK; see the two-tier guarantee below). A custom kernel is an upgrade, not a
  requirement.
- **Custom guest drivers and a guest agent** (`limina-agent`) — ours to write.

"The sky is the limit; we have the sources and can try things out." When a problem
looks like a wall, check whether owning the source turns it into a menu. Prefer the
*smallest* change that works (host-side > guest-config > guest-patch > new device),
but don't rule out the deep fix — and keep patches minimal and **upstreamable**
(mechanism in the dependency, policy in limina) so we can carry them cheaply and give
them back.

### Two-tier guarantee: stock-compatible baseline + enhanced custom tier

This is a hard design constraint, not a preference. limina must always support **two
tiers**, and nothing in our design may collapse them into one:

1. **Stock baseline (compatibility floor).** limina must *always* be able to boot an
   **unmodified stock distro** (Fedora's own kernel, stock Mesa, no limina guest
   components) on **upstream-shaped libkrun**. This is allowed to be **degraded** —
   lower 3D perf, no dynamic memory, no USB, no clipboard, software fallbacks —
   but it must **boot and be usable**. We never make a stock guest *fail to run*;
   missing limina enhancements degrade gracefully, they don't break the VM.
2. **Enhanced tier (full experience).** Installing our **custom kernel / drivers /
   guest agent** (and running our patched libkrun) **restores/unlocks** full
   performance and features — 16 KiB pages, host-page-aware ballooning, accelerated
   3D, USB, clipboard, dynamic display, etc. Enhancements are *additive*: they layer
   on top of the baseline, they are not a precondition for it.

Concretely: **we are bound to neither Fedora's stock kernel nor libkrun's defaults**
— krun config/defaults are just configuration management for our work, ours to set.
But every feature should be designed with a stock-guest fallback path, and a guest
that hasn't installed our components must still come up. Custom kernel/drivers/agent
are the *upgrade*, never the *entry fee*. (Example: dynamic memory uses
`MADV_FREE_REUSABLE` + host-side coalescing so it does *something* on a stock 4 KiB
Fedora kernel; a 16 KiB / host-page-aware-reporting custom kernel makes it optimal.)

Two refinements that are easy to forget:

- **Detect capabilities granularly and additively, not as one tier switch.** A guest may
  have *some*, *all*, or *none* of the enhanced pieces (16k kernel, venus mesa, limina-agent,
  virtiofs, clipboard backend, …), and **partial states are normal** (a guest mid-upgrade,
  or one that only installed part). Take advantage of whatever is present — light up each
  feature when *its own* prerequisite is there — rather than gating everything on a monolithic
  "enhanced vs stock" flag. The host/control-plane must tolerate any mix.
- **The basic tier is the bootstrap substrate — required, not merely valid.** A fresh install
  *starts* as a basic guest, and the enhanced components are delivered *into* it from there. So
  "basic must just work" is the prerequisite the whole upgrade path stands on: the installer /
  delivery mechanism runs **in the basic guest, before any enhanced components exist**, and so
  **must not depend on the very things it installs** (it can't require the 16k kernel, venus, or
  a custom driver to deliver them). There is a minimal bootstrap floor — enough to *receive and
  apply* enhancements (plain networking or virtiofs + a way to run the installer) — that lives in,
  or is trivially installable onto, the stock tier; everything richer layers on per-feature, in
  whatever order the components land.

## High-level architecture decisions (the load-bearing ones)

- **Raw HVF via libkrun, NOT Apple Virtualization.framework.** Vz is a black box
  and forbids the custom devices/USB/ballooning/agents that are limina's whole point.
- **The VMM runs in a dedicated child process.** `krun_start_enter` loops forever
  and the guest's PSCI SYSTEM_OFF tears the *whole process* down; the AppKit UI
  process must survive and supervise it (over vsock + the shutdown eventfd).
- **Native AppKit/Metal front-end** (NSWindow + CAMetalLayer, NSEvent → evdev),
  not the GTK/SDL example backends (those are milestone crutches only).
- **Mechanism in libkrun, policy in limina.** Keep our libkrun patches small and
  upstreamable; behavior (keymap, balloon/PSI policy, etc.) lives in the app.
- **A single multiplexed vsock control plane + a `limina-agent`** in the guest is the
  channel for clipboard, dynamic display resize, memory-pressure reporting,
  time sync, and lifecycle.

See `docs/research/00-overview.md` for the full picture and `docs/roadmap.md` for
the milestone plan (M1 boot → M8 polish). `docs/research/GAPS-and-verification.md`
tracks claims still needing verification.

## Working conventions (learned the hard way)

- **Verify against real source, not memory or summaries.** Every non-obvious claim
  about libkrun/deps in the docs carries a `path:line` citation into `third_party/`.
  Keep doing that. The research clones are in `third_party/` (gitignored).
- **Full validation = `scripts/test-boot.sh` (sets `LIMINA_HVF_TESTS=1`).** This is the
  go-to "did I break anything" command: it builds, codesigns the worker, and runs the
  real boot tests against HVF (`limina` → `limina-vmm` → guest). A plain `cargo test`
  deliberately **skips** the HVF tests (no codesign / sandbox) — green there means
  almost nothing for boot behavior, so always run `scripts/test-boot.sh` before
  declaring something works. It needs `dangerouslyDisableSandbox` (hits `hv_vm_*`).
- **The worker MUST link our `third_party/virgl-prefix` virglrenderer, not Homebrew's.**
  This is a costly silent trap: a plain `cargo build -p limina-vmm` with no `PKG_CONFIG_PATH`
  used to relink the worker against Homebrew's stock `virglrenderer` (whose `.pc` pkg-config
  finds by default). That build lacks venus render-server support → on boot
  `virgl_renderer_init` returns -1 and the GPU **silently degrades to software-2D**: the VM
  still runs (2D/SSH/llvmpipe desktop all fine) but venus never enumerates in the guest
  (`vkEnumeratePhysicalDevices → ERROR_INITIALIZATION_FAILED`), which reads like a venus/guest
  bug and burns hours. `build.rs` now prepends our prefix to `PKG_CONFIG_PATH` and prints a
  `cargo:warning` naming the resolved lib, so plain builds are safe and a wrong link is loud.
  Still: **verify with `otool -L target/debug/limina-vmm | grep virgl`** (must show
  `third_party/virgl-prefix/…`); if the worker log shows `degrading to software-2D` /
  `ComponentError(-1)` after `virgl_flags`, check the link before suspecting venus. To debug
  venus host init, run with `RUST_LOG=debug` (the worker + supervisor default to `warn` and now
  honor `RUST_LOG` — `RUST_LOG=limina_vmm=debug` for just the worker, `RUST_LOG=trace` adds the
  per-frame GPU present DIAGs `[FLUSH2]`/`[FENCEPRESENT]`).
- **fmt + clippy stay clean.** A pre-commit hook (`.githooks/pre-commit`, enabled via
  `scripts/setup-hooks.sh` → `core.hooksPath`) runs `cargo fmt --check` on every workspace
  we own and `cargo clippy --workspace -- -D warnings` on the shipped code (+ the guest at
  its `aarch64-unknown-linux-musl` target). It never touches `third_party/`. Run
  `scripts/setup-hooks.sh` once per clone; bypass a commit with `--no-verify` only in a
  pinch.
- **Fix bugs RED-first.** Every bug fix starts with a failing test that reproduces it,
  then the fix turns it green. Tests drive the *shipped binaries* (`limina` → `limina-vmm`),
  not libkrun internals — the harness is `crates/limina-test`. See the testing section in
  `docs/roadmap.md` for the L0/L1/L2 layers.
- **Spikes prove the risky assumptions.** Standalone experiments live in `spikes/`
  with a `RESULTS.md` (source kept, build artifacts gitignored). When a finding
  drives an architecture decision, measure it before committing to it. **Read the
  numbers before writing the conclusion** — we have inverted a finding by writing
  from expectation; don't repeat it.
- **Re-confirm OS-specific behavior** on the shipping macOS version; HVF/`madvise`
  semantics vary by release.
- **Commit early and often for bisectability**, but only meaningful commits — the
  user has authorized committing without asking. End commit messages with the
  `Co-Authored-By` trailer.
- **Keepable artifacts go in the repo, NOT `/tmp`.** Debug scripts, probes, oracles, and
  notes we'll want again belong under `spikes/` (or the relevant crate), committed. `/tmp`
  is only for genuinely throwaway scratch and transient runtime deploy dirs — it gets wiped
  and the work is lost. When the source we instrument lives in gitignored `third_party/`,
  save the change as a `*.patch` in the repo (e.g. `spikes/archive/moltenvk/mvk-instrument.patch`).

### Debugging discipline: verify premises, verify pixels, instrument what we own

This earned its place the hard way on the venus/tier-2 work (#30/#31): the fixes came not from
cleverness but from refusing to trust anything we hadn't directly observed.

- **Enumerate and verify premises before you deep-dive.** List the assumptions a bug "obviously"
  rests on, then prove each one empirically — don't inherit them. We twice built on false premises
  ("the present pipe is broken", "glmark2 18/18 means GL renders") and burned hours; the cheap check
  up front would have saved them. When you catch yourself reasoning *from* a premise, stop and test it.
- **A scary warning is a LEAD, not a cause.** We nearly "fixed" two non-bugs by reasoning from a log
  line (`depth_clip_enable`, then `primitive restart`). Before acting on a warning, read its emission
  site in the (owned) source to see whether it's even fatal — then confirm by observation. Never
  conclude from the message text alone.
- **Pixel-verify; proxies lie.** FPS counters, "no GL error", "18/18 scenes", exit-0 — none prove
  anything actually rendered. Read the real pixels: the IOSurface scanout via
  `spikes/venus-draw-probe/iosdump.swift` (cross-process, any global IOSurface id), or the window
  capture (`LIMINA_WINDOW_CAPTURE`). NOT `glReadPixels` (#28 black readback). When only a human can see
  the window, ask the user to eyeball — and allow that they're human and may be slow to look.
- **Instrument the stack you own.** When behavior is opaque, a few `fprintf`s in the dependency
  (the host Vulkan driver / virglrenderer / libkrun) beat any amount of outside-in guessing. The
  instrumented host Vulkan driver, loaded into the worker via `VK_ICD_FILENAMES`, is what turned
  "venus renders black" from open theories into one fact: the vertex buffer the GPU fetches is
  all-zero. (That oracle was an instrumented *MoltenVK* — now archived under
  `spikes/archive/moltenvk/`; MoltenVK was **retired as a venus backend** 2026-06-13 because it
  crashes the compositor. **KosmicKrisp (KK) is the one supported backend now** — instrument KK the
  same way.) Keep such oracles in the repo; they pay off repeatedly.
- **Isolate with a minimal vehicle, then reason to rule out the innocent explanation.** `tri.c` (a
  textureless, self-contained draw) had no confounders, so its result was decisive. And when an
  observation has a benign alternative ("the buffer's zero only because a copy hasn't run yet"), kill
  it with logic (the render is black ⟹ the GPU read zeros *at execution*) rather than assuming.
- **Verify the fix is actually LOADED before judging it — at the path the process maps.** A half-day
  was lost (2026-06-10) bisecting a "regression" that was really a half-installed fix: the bake put
  `libmutter-17.so.0.0.0` in `/usr/lib64/mutter-17/`, but gnome-shell loads it from `/usr/lib64/`
  directly — the lib holding the actual #32 mitigation sat inert while a *different* piece of the fix
  (cogl's one-time warning) kept firing and made the install look alive. A sub-oracle proving one
  piece is loaded proves nothing about the load-bearing piece: check the artifact itself (mtime/size
  at the path in `/proc/PID/maps`). Guest mutter installs go through
  `spikes/venus-draw-probe/install-mutter-fix.sh`, never by hand.
- **Identical A/B results across many configs mean the differential isn't reaching the system under
  test — stop toggling and re-verify the baseline.** Five "exonerations" in a row (private API, D24S8
  emu, sampler fix, clipped redraws, stock-vs-fixed mutter) all returned pixel-identical damage
  because every one of them ran the same unmitigated stack. Invariance is a smell, not a verdict.
- **The user's episodic memory and the session transcripts are oracles.** "I could not reproduce it
  at any point" falsified a comfortable no-regression theory, and grepping the transcript for the
  verified moment recovered the exact working install commands. Reconstructing ground truth from
  records beats re-deriving it with boot cycles.

### Environment quirks

- Host: macOS 26.5, Apple M1 Max, 32 GB, arm64. Full Xcode 26.4 / clang 21.
  Rust 1.88. **16 KiB host pages.** Homebrew already has the whole VM stack
  (libkrun, krunkit, libkrunfw, virglrenderer, molten-vk, vulkan-loader, gvproxy,
  libusb, qemu, cmake/meson/ninja).
- **The Bash tool is sandboxed with no network and can't write outside the repo.**
  Pass `dangerouslyDisableSandbox: true` for `git`/network/spike builds. Anything
  touching `hv_vm_*` must be codesigned with `com.apple.security.hypervisor` (see
  `spikes/balloon-madvise/hv.entitlements`).
- The big disk images (`*.raw`, `*.raw.xz`) and `third_party/` are gitignored.
- **Boot a WINDOWED dev VM with `spikes/venus-draw-probe/boot-seated-kk.sh` — coexist venus WORKS;
  don't reach for `--gpu-software-2d` to "avoid" it.** The coexist GPU (venus 3D + software-2D)
  needs the host KK/zink env (VK_ICD_FILENAMES → KosmicKrisp, the zink-on-KK DYLD/Mesa selectors);
  the script sets all of it and boots `--window` to the seated GNOME desktop on the 16 KiB kernel.
  A *bare* `target/debug/limina --window` with NONE of that env **aborts on GPU init**
  (`Couldn't open libEGL.dylib` → SIGABRT) — that's the missing env, **not** a coexist problem, and
  **not** a reason to fall back to software-2D. `LIMINA_DISK=<writable clone>` reuses a disk (the
  script's hard-coded `dev-enh.raw` is retired — clone `Fedora-Workstation-43.enhanced.raw`);
  `LIMINA_EXTRA_ARGS="--swap-cmd-opt …"` passes extra limina flags through. `--gpu-software-2d` is
  ONLY for when software-2D is the explicit subject (the capture oracle / a GPU-less host), per the
  coexist-default rule in `limina-tier2-venus`. Driving the window: osascript UI scripting works for
  key+modifier combos (e.g. Cmd-Ctrl-F), but synthetic *lone-modifier* keystrokes may not reach the
  guest — the human is the oracle for those (see `limina-window-control`). **When you need the
  user to act or perceive (interact with the window, eyeball the screen, plug in hardware, run a
  host command), request it via the AskUserQuestion tool, not in prose** — they may not be watching
  the streaming text and will miss a buried request (see `ask-tool-for-user-actions`). For a
  *time-sensitive* test (e.g. catching the ~5s GRUB countdown in a windowed boot), ask **before**
  launching the run, not after — so they're positioned and reading the instruction when the brief
  window arrives.
- **Run a VM with networking + SSH:** `limina --net` spawns a supervised gvproxy user-mode NAT (no
  root) and the supervisor logs the exact SSH command — `guest SSH forward ready: ssh -p N <user>@127.0.0.1`.
  The host port **auto-allocates from 2222 up** (so 2+ VMs run concurrently without colliding); pin it
  with `--ssh-port <1024-65535>`, and capture gvproxy's packet log (the host-side net oracle) with
  `--net-log <file>`. Read N from the log — don't assume 2222. Full recipe + creds in the
  `limina-fedora-access` memory; design in `limina-m3-networking` / `crates/limina/src/gateway.rs`.
