# A render-pass begin with bad attachments

A guest's venus stream reaches KosmicKrisp's render-pass begin (`vk_common_CmdBeginRenderPass2`,
`src/vulkan/runtime/vk_render_pass.c`) with no validation of the render pass, the framebuffer or its
attachment image views. virglrs forwards `VK_NULL_HANDLE` to the driver unchanged (it is an ordinary
value on the wire, `third_party/virglrs/src/venus/cs.rs`). Upstream asserts the view count matches;
with asserts compiled out, as in limina's build, every later access in the pass indexes the view array
by the pass's own attachment numbers.

`rp_attach_count.c` begins a 2-attachment render pass each bad way below, plus valid controls, then
ends the pass and the command buffer, and submits and waits on any command buffer that ended
successfully. The draw modes bind a full-screen-triangle pipeline (`tri.vert`, `tri.frag`) and exit 3
if it cannot be built. `run.sh [icd.json]` builds it and runs all modes (default ICD: the host build
limina uses, `/Volumes/mesa-cs/build-kk`).

| mode | what the guest supplies |
|---|---|
| `imageless` | imageless framebuffer; `VkRenderPassAttachmentBeginInfo` carries 1 view |
| `fb` | ordinary framebuffer created with 1 view |
| `nobegin` | imageless framebuffer begun with no `VkRenderPassAttachmentBeginInfo` (0 views) |
| `nullrp` | `renderPass = VK_NULL_HANDLE` |
| `nullfb` | `framebuffer = VK_NULL_HANDLE` |
| `nullfb-il` | `framebuffer = VK_NULL_HANDLE`, 2 views in the begin info |
| `nullview` | imageless, 2 views in the begin info, the second `VK_NULL_HANDLE` |
| `nullfbview` | ordinary framebuffer created with 2 views, the second `VK_NULL_HANDLE` |
| `draw` | the `imageless` begin, then a draw |
| `drawnopass` | a draw with no render pass begun |
| `ok`, `drawok` | 2 views for 2 attachments, without and with a draw |

**Measured 2026-09-23, M1 Max, macOS 26.6.2, release build.**

- `limina-kk` b8c379ee5f0 (refuses a short view count): the three short-count modes log
  `render-pass begin VU violation … render pass not begun`, and `vkEndCommandBuffer` returns
  `VK_ERROR_UNKNOWN` (-13). All five null modes segfault.
- `limina-kk` f3a220f279c: every bad mode logs its violation (`renderPass is VK_NULL_HANDLE`,
  `framebuffer is VK_NULL_HANDLE`, `attachment 1 has a VK_NULL_HANDLE image view`, or the short
  count), survives, and fails `vkEndCommandBuffer` with -13. `ok` and `drawok` end, submit and wait
  with 0.
- **A draw with no active render pass is harmless, on both builds.** A refused begin leaves the
  command buffer outside any pass, the same state as a draw recorded with no begin at all: KK has no
  render encoder and none pending, `cs_get_render` returns nil, and the draw's Metal calls are
  messages to nil. `draw` survives recording; `drawnopass` also ends, submits and waits with 0. This
  covers a plain draw only; tessellation, geometry and transform-feedback draws, which add compute
  work around the draw, were not probed.

A NULL framebuffer is refused even when the begin info supplies the views: `begin_subpass()` and
the layout transitions read its layer count.
