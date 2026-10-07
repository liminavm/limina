# Why host zink-on-KK offers GLES 3.1, not 3.2

Mesa grants ES 3.2 only when all 19 extension flags in `compute_version_es2()`
(`src/mesa/main/version.c`, `ver_3_2`) are set. `run.sh` builds `es32gate.c` against a zink-on-KK
prefix and reports each one.

## Measured 2026-10-07 (M1 Max, shared builds at limina-kk `88b1341efe8`)

ES context `OpenGL ES 3.1`; desktop context `4.6 (Core Profile)`. 3 of 19 missing:

| Missing | Why |
|---|---|
| `OES_geometry_shader` | KK has no geometry shaders, so zink reports `max_instructions = 0` for the stage (`st_extensions.c`, "OES_geometry_shader requires instancing"). Desktop 4.6 is reported anyway: Mesa's desktop version check does not require geometry shaders. |
| `OES_texture_cube_map_array` | Gated on `OES_geometry_shader` (`st_extensions.c`), not on cube arrays, which desktop GL has. Clears with geometry shaders. |
| `KHR_blend_equation_advanced` | zink exposes it only through fbfetch, and `zink_screen.c` sets `caps->fbfetch = 0` under `#if defined(MVK_VERSION)`. That macro comes from MoltenVK's headers, which zink includes on every `__APPLE__` build; Homebrew's molten-vk is installed, so the MoltenVK workaround is compiled into the KK build too, although KK offers `VK_KHR_dynamic_rendering_local_read` (and `VK_EXT_blend_operation_advanced`). |

The same compile-time gate also disables dynamic vertex input stride on KK:
`zink_internal_setup_moltenvk` sets `have_dynamic_state_vertex_input_binding_stride = false` and only
a MoltenVK API call can turn it back on, so zink-on-KK keys pipelines on vertex strides.

Fix shape for the MoltenVK gates: decide at run time (`instance_info->have_MVK_moltenvk`) instead
of on the header's presence. Geometry shaders are KK work.
