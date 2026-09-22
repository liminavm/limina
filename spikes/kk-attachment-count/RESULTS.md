# A render-pass begin with too few attachment views

A guest's venus stream reaches KosmicKrisp's render-pass begin (`vk_common_CmdBeginRenderPass2`,
`src/vulkan/runtime/vk_render_pass.c`) with no validation of how many attachment image views came
with it. Upstream asserts the counts match; with asserts compiled out, as in limina's build, every
later access in the pass indexes the short array by the pass's own attachment numbers.

`rp_attach_count.c` begins a 2-attachment render pass three bad ways plus a valid control, then
ends the pass and the command buffer. `run.sh [icd.json]` builds it and runs all modes (default ICD:
the host build limina uses, `/Volumes/mesa-cs/build-kk`).

| mode | what the guest supplies |
|---|---|
| `imageless` | imageless framebuffer; `VkRenderPassAttachmentBeginInfo` carries 1 view |
| `fb` | ordinary framebuffer created with 1 view |
| `nobegin` | imageless framebuffer begun with no `VkRenderPassAttachmentBeginInfo` (0 views) |
| `ok` | 2 views for 2 attachments |

**Measured 2026-09-22, M1 Max, macOS 26.6.2.**

- `limina-kk` bb3994fc6db with asserts on: `imageless` and `fb` abort (`SIGABRT`, asserts at
  `vk_render_pass.c:2690` / `:2669`).
- With only the begin-refusal commit: `nobegin` compared the pass against an uninitialised
  `attachment_count` (read as 2863311530 under a 0xAA-filling allocator) and segfaulted. That is the
  second commit's bug: `vk_common_CreateFramebuffer` never set the count for an imageless framebuffer.
- `limina-kk` b8c379ee5f0 (both commits), release build: every bad mode logs
  `render-pass begin VU violation … N attachment image views supplied for a render pass with 2
  attachments; render pass not begun`, the process survives, and `vkEndCommandBuffer` returns
  `VK_ERROR_UNKNOWN` (-13). `ok` returns 0.

Not covered: a draw recorded inside the refused pass (it needs a pipeline), and NULL framebuffer,
render-pass or image-view handles — see `docs/hardening-backlog.md` §KosmicKrisp.
