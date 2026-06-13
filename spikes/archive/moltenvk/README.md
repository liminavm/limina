# Archived: MoltenVK host-Vulkan backend (retired 2026-06-13)

These are the custom **MoltenVK** build, instrumentation, and boot artifacts from the
tier-2 venus bring-up. They are kept for history only — **MoltenVK is no longer a
supported host Vulkan backend for limina's venus path.**

## Why MoltenVK was dropped

venus (the guest 3D path) is driven on the host by a Vulkan driver. On MoltenVK the
full GNOME compositor (`gnome-shell` → zink → venus → MoltenVK) **SIGSEGV-loops**:
MoltenVK still carries the coherency / stencil / fan class of bugs (#28, #32, …) that
corrupt the guest's buffers, and the corruption is upstream of the guest so cogl can't
catch it — it just crashes instead of degrading. Verified 2026-06-13: the *same* image
+ kernel that crash-loops the greeter on MoltenVK comes up clean (0 segvs, steady
greeter) on **KosmicKrisp (KK)**.

So limina locked in **KK as the one supported venus backend**: every venus boot path
forces the KK ICD, and when KK isn't present the worker degrades to **software-2D
(llvmpipe)** — it never falls through to the Vulkan loader's MoltenVK default. See
`spikes/venus-draw-probe/boot-seated-kk.sh`, `scripts/run-venus-window.sh`, and the
`kosmickrisp_icd()` gate in `crates/limina-test/src/lib.rs`; rationale in memory
`limina-tier2-venus`.

## What's here

- `rebuild-mvk.sh` — built + deployed an **instrumented** host MoltenVK as a venus
  draw-debugging oracle (`[LIMINA-DRAW]`/`[LIMINA-VTX]` traces), loaded via `VK_ICD_FILENAMES`.
- `mvk-instrument.patch` — the MoltenVK source instrumentation that `rebuild-mvk.sh` applied.
- `boot-mvkinst.sh` / `boot-seated-mvkinst.sh` — booted the guest (multi-user / seated)
  with that instrumented MoltenVK to read the real indexed draws gnome-shell issued.

These oracles did their job (they're what turned "venus renders black/garbage" into
specific root causes); they are not part of any supported path now. The cross-driver
Vulkan/GL bug reproducers (`vkcoh.c`, `vkds.c`, `stencil-test.c`, `texfan.c`, …) stay in
`spikes/venus-draw-probe/` — they run against KK too.
