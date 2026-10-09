# KosmicKrisp guest-input bounds probe

Under venus, a guest's Vulkan calls reach KosmicKrisp without validation, so invalid usage is
input KK must refuse or survive. `probe.c` drives KK directly through the Vulkan loader, one case
per process, and reports each as PASS, FAIL or CRASH: descriptor set layouts and allocation,
descriptor set binds, image limits, descriptor writes and copies, and push descriptors.

Every case passes from limina-kk `1bf40e3a14a` on, in builds with and without asserts; before the
fixes each misbehaved or crashed (except `mip-legal`, the control). Some cases pass on an unfixed
KK too, so a pass alone does not prove a fix; run the whole set.

    cc -Wall -I/opt/homebrew/include probe.c -L/opt/homebrew/lib -lvulkan -o probe
    VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./probe [case]

Each case re-executes the binary: a bare `fork()` cannot reach Metal's XPC services.

## `cache-shader-probe.c` — pipeline-cache and dynamic-rendering inputs

`probe.c`'s companion, same harness, covering two more guest-reachable surfaces found by the
virglrs host-validation audit:

- **Serialized shaders via the pipeline cache.** A guest's `vkCreatePipelineCache` `pInitialData`
  is forwarded unchanged. KK registers no `pipeline_cache_import_ops`, so each entry is kept as a
  raw-data cache object under the guest's key; at the next matching pipeline creation
  `vk_pipeline_cache_lookup_object` re-deserializes it through the shader ops
  (`-> kk_deserialize_shader`), a path with no header or BLAKE3 to vet the body. The probe
  round-trips a real compute pipeline to obtain a valid blob and the shader's key, poisons the kk
  shader entry, re-seeds a new cache and recreates the pipeline. `cache-bad-stage` (a stage index
  far past the `MESA_SHADER_STAGES` array) **crashes** an unfixed KK; `cache-huge-code-len` and
  `cache-no-nul` are real but non-crashing (an oversized allocation that then overruns; an
  unterminated string that reaches the MSL compiler), so they pass even unfixed — run the set and
  read them together.
- **Dynamic-rendering bounds.** `render-huge-area` / `render-huge-layers` hand
  `vkCmdBeginRendering` a `renderArea`/`layerCount` beyond KK's advertised
  `maxFramebufferWidth/Height`/`maxFramebufferLayers`; KK must refuse (the error surfaces at
  `vkEndCommandBuffer`). No host failure has been observed from accepting them (Metal bounds the
  target by the real attachment), so this is defence in depth; `render-ok` is the in-range control.

Fixed in limina-kk `kk: validate guest-supplied serialized shaders and render-area bounds`: before
it `cache-bad-stage` crashes and the two `render-huge-*` cases are accepted (FAIL); after it all
cases PASS, with and without asserts.

    cc -Wall -I/opt/homebrew/include cache-shader-probe.c -L/opt/homebrew/lib -lvulkan -o csp
    VK_ICD_FILENAMES=<kk build>/.../kosmickrisp_mesa_devenv_icd.aarch64.json ./csp [case]
