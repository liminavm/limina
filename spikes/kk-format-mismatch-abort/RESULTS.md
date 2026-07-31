# kk-format-mismatch-abort — guest-triggerable VMM abort, reproduced locally

**2026-07-31 00:13, dev Mac.** Repro of the four dogfood-mac dogfood crashes of 2026-07-30 night
(23:07 / 23:11 / 23:19 / 23:28, `limina-vmm-*.ips` on dogfood-mac).

## The abort

```
Assertion failed: (image_view->format == pass_att->format),
  function vk_common_CmdBeginRenderPass2, file vk_render_pass.c, line 2708.
```

KK is built with asserts on; the mesa vk_common runtime's render-pass begin faults when a
framebuffer attachment view's format differs from the pass's declared attachment format, and
the abort takes down the whole worker → the VM dies. Fires on the decode thread (immediate
submit) and the submit thread (threaded) alike — kk 0017/0018 threading was exonerated by
crash #3 (`LIMINA_KK_SUBMIT_THREAD=0` verified engaged via env + `sample`, still crashed).

## The trigger (guest-side, legitimate development activity)

gnome-shell-rs' in-progress format flip (`IMAGE_VK_FORMAT: R8G8B8A8_UNORM → B8G8R8A8_UNORM`,
uncommitted on dogfood-guest 2026-07-30 — their §30 swizzle-kill; copy in
`pending-format-flip.diff`) paired with test runs: the half-applied tree creates a render
pass in one byte order and an attachment view in the other. Invalid Vulkan usage — but from
a *dev's test suite in a guest*, which this product must survive.

## Repro recipe (the RED vehicle)

1. `cp -c nirirepro.raw nirirepro.crashrepro.raw` and boot it:
   `LIMINA_DISK=$PWD/nirirepro.crashrepro.raw spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`
2. In-guest (`claude@`, port from the log): gnome-shell-rs tree at `4f21e3c2` +
   `pending-format-flip.diff` applied, smithay fork at `83175a56` as `~/smithay`
   (path dep). Ship sources with `COPYFILE_DISABLE=1 tar` — AppleDouble `._*` files
   break their shader build script otherwise.
3. `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json cargo test
   --no-fail-fast` (ran via `systemd-run` unit so ssh drops don't kill it).
4. Worker aborts within minutes; oracle = the assert line in the worker log
   (`/tmp/enhanced-efi-kk-worker.log`) + supervisor "worker terminated by signal 6".

## The host-side work this motivates (booked, not yet done)

A guest's invalid Vulkan usage must never abort the VMM — this is the second
guest-triggerable abort class (first: the empty-clear-rect vk_meta assert,
`limina-kk-empty-clear-rect`). Fix directions to weigh: ship the bundle's KK without
asserts (NDEBUG) so invalid usage degrades to undefined-but-contained rendering; and/or
sanitize/validate at the vkr decode boundary; and/or convert this specific class in the
vk_common runtime to skip-and-log. Whichever lands, this spike's recipe is the RED test.
