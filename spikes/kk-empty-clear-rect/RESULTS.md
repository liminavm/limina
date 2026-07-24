# kk-empty-clear-rect — guest-triggerable host VMM abort via empty VkClearRect

## The bug
A guest `vkCmdClearAttachments` with a zero-extent `VkClearRect`
(`rect.extent = {0,0}`, invalid usage per
VUID-vkCmdClearAttachments-rect-02682/-02683, but guest-controlled) flows
unvalidated through venus → vkr (`vkr_dispatch_vkCmdClearAttachments`, a bare
passthrough) → KosmicKrisp, which replays it at `vkQueueSubmit` into
`vk_meta_clear_attachments` → `vk_meta_draw_rects`/`setup_viewport_scissor`:

    assert(rects[r].x0 < rects[r].x1 && rects[r].y0 < rects[r].y1)
    src/vulkan/runtime/vk_meta_draw_rects.c:163 (r==0) / :167 (r>0)

→ `SIGABRT`, whole worker/VMM process dies (guest-triggerable host DoS). With
asserts compiled out it is still unsafe: `x1 == offset.x + 0` can be `0`, so
`x1 - 1` wraps to `UINT32_MAX`, and the viewport/scissor — computed from the
**union** of *all* rects (`xbits |= rects[r].x1 - 1`) — blows up to `1 << 32`
(UB), corrupting the clears for the valid rects sharing the call.

## The probe
`probe.c` links the Homebrew Vulkan loader and selects the KK ICD via
`VK_ICD_FILENAMES` (build-kk devenv ICD json). It:
1. creates a device with dynamic rendering,
2. begins rendering to a 64×64 offscreen R8G8B8A8 attachment (whole-FB
   loadOp=CLEAR to black),
3. issues **one** `vkCmdClearAttachments` clearing red with **two** rects — a
   VALID sub-rect `{8,8 16×16}` and an EMPTY rect `{0,0 0×0}` (the poison; the
   valid rect is there to witness the union-corruption argument),
4. ends rendering, copies the image to a host buffer, submits, waits,
5. reads back pixel (16,16) [inside valid] and (32,32) [outside].

Build: `./build.sh`  ·  Run: `./run.sh`  (no codesign — no `hv_vm_*`).

## RED — current/unfixed KK (before Fix B)
`/Volumes/mesa-cs/build-kk` at `2cf4599ed74` (before the fix):

    device: Apple M1 Max (api 1.3.353)
    issuing vkCmdClearAttachments: valid rect {8,8 16x16} + EMPTY rect {0,0 0x0}
    submitting (KK replays the command buffer here)...
    Assertion failed: (rects[r].x0 < rects[r].x1 && rects[r].y0 < rects[r].y1),
      function setup_viewport_scissor, file vk_meta_draw_rects.c, line 167.
    Abort trap: 6            ./probe        (exit 134)

(Line 167 = the r≥1 loop assert; rect[0] is the valid rect, rect[1] the empty
one — either ordering aborts.)

## GREEN — fixed KK (Fix B built, KK commit `379f5c6ad5e`)
    device: Apple M1 Max (api 1.3.353)
    issuing vkCmdClearAttachments: valid rect {8,8 16x16} + EMPTY rect {0,0 0x0}
    submitting (KK replays the command buffer here)...
    submit completed without abort
    readback: inside-valid-rect (16,16) = [255 0 0 255] (want red 255,0,0)
    readback: outside          (32,32) = [0 0 0 255] (want black 0,0,0)
    GREEN: no abort; valid rect cleared red, empty rect skipped cleanly, union intact.
    exit=0

The readback is the **union-corruption witness**: with the empty rect skipped,
the valid rect still clears exactly its region (red inside, black outside) — the
viewport/scissor union was computed from valid rects only.

## What the probe proves — precisely
The probe calls the **KK ICD directly**. It exercises **Fix B (KK/Mesa
vk_meta)** ONLY. It does **NOT** go through virglrenderer/vkr, so it says
**nothing** about **Fix A** (the vkr trust-boundary sanitizer). Fix A is proven
separately by:
- the L2 guard `l2_kk_empty_clear_rect` (`crates/limina-test/tests/venus.rs`),
  which drives a guest vehicle through the full venus → vkr → KK stack, and
- code review: Fix A compacts the decoded rect array before the passthrough.

Fix B alone makes the host driver robust regardless of caller; Fix A closes the
trust boundary at the venus decode layer so an untrusted guest stream never
reaches the host driver with a degenerate rect (defense in depth).

## The fixes
- **Fix A** — `patches/virglrenderer/0045-vkr-sanitize-guest-empty-VkClearRects-in-vkCmdClearA.patch`
  (virgl fork commit `705f24a`): drop zero-extent/zero-layer rects in
  `vkr_dispatch_vkCmdClearAttachments`; skip the call if none survive.
- **Fix B** — `patches/kosmickrisp/0009-vk-meta-skip-empty-rects-in-vk_meta_clear_attachment.patch`
  (KK branch commit `379f5c6ad5e`): skip zero-extent/zero-layer rects at all
  three rect-consuming sites in `vk_meta_clear_attachments`. Upstream-Mesa
  shaped (the file is Mesa common `src/vulkan/runtime/`), MR-ready.

## Adjacent observations (listed, NOT fixed)
- KK `0003` already clamps the sibling case — an **attachment-less**
  `vkCmdBeginRendering` with a `0×0` guest `renderArea` (from gst-plugin-scan
  zink probes) — proving guest-fed zero-area render geometry reaches KK in
  practice. Same root class.
- The `kk_CmdBeginRendering` force-load funnel (`kk_cmd_draw.c:521-530`) builds
  a clear rect from the guest `renderArea` and routes through
  `kk_CmdClearAttachments` → `vk_meta_clear_attachments`, so **Fix B covers it
  too** — no separate guard added (would be dead code).
- Other guest-fed asserts in the same files worth an audit (not touched here):
  `vk_meta_draw_rects.c:183-184` (`xmax_log2/ymax_log2` range from
  guest-derived rect extents), `vk_meta_clear.c:326-327`
  (`baseArrayLayer==0`/`layerCount==1` asserted for the view_mask path — a
  non-conformant multiview guest clear could trip these).
