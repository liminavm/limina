# A/B: the native-context GPU transport against venus

**Subject:** guest GPU work on the native-context transport (the guest's KosmicKrisp on the limina
virtio-gpu extension, replayed host-side by `limina-gpu-host`) against the same work on venus. GL
runs through Fedora's zink in both arms, so GL workloads go through the transport too.
- **Host:** limina-kk `d1ed9e2d660`; virglrs `d389a00`; libkrun `5adb732f`; limina `35a31c97`
  (optimized GPU host).
- **Guest:** `Fedora-Workstation-44.metal.test.raw`, stock kernel `6.19.10-300.fc44` at 4 KiB pages,
  Fedora mesa `26.1.8-1`, with the transport's guest half delivered; 4 vCPUs, 4 GiB, 1280x800 @ 1.0,
  60 Hz.

Drivers, rows and evidence are in `perf/metal-transport-2026-10-13/`; nothing went to
`perf/ledger.csv`.

## What this pass can and cannot see

- **The arms differ in the transport only.** Same image, same host build, same boot. The venus arm
  runs every GPU workload on the venus ICD. The metal arm runs it on the transport's ICD, with the
  worker started with `LIMINA_GPU_METAL=1`. Each point's `arm.txt` proves its arm: `Virtio-GPU Venus`
  or `Apple M1 Max / KosmicKrisp`, and zink on that device. perf-ledger refuses a metal run whose
  renderer names venus or llvmpipe.
- **The desktop is vrend in both arms.** gnome-shell and Xwayland are not on the transport, so every
  windowed number includes a hand-off to a vrend compositor.
- **The aquarium runs Firefox with software WebRender in both arms**, because hardware WebRender
  yields no frame on either arm (below). WebGL still renders through zink on the arm's ICD, but the
  page reaches the compositor through shared memory. So aquarium numbers here are not comparable
  with earlier passes, which ran vrend WebGL under hardware WebRender on the enhanced image.
- **Order:** v0 m0 v1 m1 v2, so the baseline is measured at both ends.
- **Host health:** the `gl-replay-llvmpipe` control read 677-691 throughout. No point aborted or
  wedged, and no worker log has a poisoned-context line. Two other VMs, not part of this pass, were
  up on the host throughout.

## Results

Five points, one boot each, measured 2026-10-13:

| workload | venus (v0 v1 v2) | metal (m0 m1) | metal vs venus |
|---|---|---|---|
| gl-replay-llvmpipe (control) | 687 681 677 | 691 685 | — |
| gl-replay | 49.1 49.3 49.6 | 48.9 48.8 | same |
| vk-replay headless | 1857 1845 1763 | 1474 1457 | **−20%** |
| glmark2-wayland | 2200 2200 2188 | 1088 1081 | **−51%** |
| vkmark | 2166-2187 (9 runs) | 2205-2214 (6 runs) | +1.5% |
| aquarium 25k | 25 24, 26 25, 26 25 | 38 39, 39 38 | **+54%** |
| aquarium 30k | 22 22, 22 21, 22 21 | 35 34, 35 36 | **+61%** |

- **The aquarium, 25-30k draws a frame, is over half again as fast on the transport.** Per-command
  cost is where the transport was meant to win, and this is the per-command workload.
- **vkmark is level.** Its scenes run at ~2200 fps either way, so a vkmark frame is ~0.45 ms in
  both arms.
- **glmark2-wayland halves and vk-replay loses a fifth.** Under glmark2 the GPU host sat at ~62% of a
  core, mostly parked in waits (`sample`: `Executor::run` is the only GPU host frame of weight), so
  it is not throughput-bound. The lead is a per-frame wait, a round trip or a fence the guest blocks
  on, which glmark2's ~0.5 ms frames expose and vkmark's do not. Not measured further.
- **gl-replay does not move.** It replays through Xwayland and is paced by that present path
  (tier 0 moved it, `perf/2026-10-05-tier0.md`), so the transport does not show in it.

## Found on the way

- **A debug `limina-gpu-host` cost the transport up to 6x.** objc2 verifies every message's signature
  under debug assertions, the GPU host had no opt-level override, and `cargo xtask build` did not
  build the GPU host at all. Under that build the aquarium read 3-4 fps and the transport lost
  every workload but gl-replay. Fixed in `35a31c97`. The superseded rows are in `debug-replayer/`.
  At 1 fish that build still held 60 fps, so the cost was per command, not per frame.
- **Firefox's hardware WebRender segfaulted on zink over the transport**, in
  `zink_set_sampler_views` within 5 s of start, even on `about:blank`. The GPU host refused every
  texel buffer (a `TextureBuffer` texture), and zink dereferences the view it did not get; WebRender
  samples a texel buffer each frame. Fixed in `26d53779`: the aquarium then runs under hardware
  WebRender on the transport.
- **On venus with stock guest Mesa, a zink client under a vrend compositor presents wrong.** Stock
  venus WSI copies each frame into an exported linear buffer, which the host cannot give an
  IOSurface, so mutter composites it through vrend and a direct scanout reads it back. Composited,
  red and blue were swapped (vrend's fill from pages skipped the GLES BGRA swap; fixed in virglrs
  `63051cf`). Scanned out before mutter ever composited it, it reads back black: vrend has no
  type for the buffer. Also so on the pre-transport build `Limina-2026-10-09-0`. Our guest Mesa
  allocates window buffers as dedicated images, which present zero-copy, and `enhanced.test`
  shows neither. Left open: it needs stock guest Mesa, a vrend desktop and a zink client.
- **The app bundles did not carry the GPU host**, so `Limina.app` could not start the transport.
  `scripts/build-app.sh` and `xtask bundle` now ship `limina-gpu-host` beside the worker.
