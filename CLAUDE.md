# limina — project guide for Claude

## What this is

limina is a native macOS app (Apple Silicon) that runs **Linux** desktop guests on
**libkrun + Hypervisor.framework**, built in Rust, aiming to **replace Parallels**.
First milestone: boot the local `Fedora-Workstation-43.raw` to a usable desktop.

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

- **libkrun** — vendored under `third_party/`, built from source, carried as a
  patch series we rebase onto upstream. We add devices, APIs, and behavior it
  lacks (e.g. balloon target/inflate control, runtime display resize, zero-copy
  scanout, USB).
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
