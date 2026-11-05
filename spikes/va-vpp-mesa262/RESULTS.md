# VA post-processing cleared on Mesa 26.2 guests

**Cause: the host translator, triggered by a new guest shader.** Mesa 26.2's video compositor
(`vl_compositor_gfx.c`) fetches alpha with a swizzled sampler operand,
`TEX TEMP.w, coord, SAMP[0].wwww, 2D_ARRAY`. vrend pasted the swizzle onto the sampler's name
(`texture(fssamp0.wwww, …)`). The host GL refused the shader, and the refused draw poisoned the
guest's virgl context, so every VA post-process (scale, colour conversion, non-native download)
came back all zeros. Decoding itself was unaffected. Fixed in virglrs `b9d75f1`, which applies a
sampler operand's swizzle to the fetched texel, the way `tgsi_to_nir` reads it. The C
virglrenderer has the same defect.

With the fix, the 26.2 compositor is also colour-correct for RGB → RGB, since its new `vl_proc`
path writes the colour matrix: 33.18 dB against the same scale on the CPU, where 26.1 scored
~3.3 dB (`crates/limina-test/tests/l2_video_vaapi.rs`, `VPP_PSNR_FLOOR`).

## Measured 2026-11-05

`probe.sh` runs the test's BGRA `scale_vaapi` oracle on a CoW clone of a stock image and keeps the
worker log.

| Guest Mesa | Host | `bgra_distinct` | Host log |
|---|---|---|---|
| 26.1.8 (pre-update stock image) | virglrs `49ddd13` | 235 | clean |
| 26.2.6 (current stock image) | virglrs `49ddd13` | 1 (all zeros) | `vrend: shader failed to compile`: `texture(fssamp0.wwww, …)`; `the context is poisoned` |
| 26.2.6 | virglrs `b9d75f1` | 238, 33.18 dB | clean |

```
spikes/va-vpp-mesa262/probe.sh <image.raw> <out-dir> [extra guest env]
```
