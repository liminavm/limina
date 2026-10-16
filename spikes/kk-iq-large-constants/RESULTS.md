# KosmicKrisp IQ quants: lookup grids rebuilt in every thread

ggml's IQ shaders copy a const lookup grid into shared memory (`iq2s_grid_const`, 1024 × uvec2 =
8 KiB for IQ2_S; IQ1, IQ2_XS and IQ3 have their own). glslang gives that dynamically indexed array an
initializer and also stores the same constant into it where it is used, and spirv-opt's
merge-return puts the use inside a loop. `nir_opt_large_constants` only took a variable whose stores
all come from one block, so it left this one a function temp, and KosmicKrisp's
`nir_lower_vars_to_scratch` turned it into `uchar scratch[8192]` that every invocation filled before
the matrix-vector product. `MESA_KK_DEBUG=msl` shows it: about 4000 lines of `scratch[...] = ...`
ahead of the kernel's loop.

Fixed in limina-kk `nir/opt_large_constants: accept stores elsewhere of the values already held`:
a store in another block of exactly the bytes the variable already holds no longer disqualifies it,
and the grid becomes `constant_data`. nir_tests carries the two cases (an identical store elsewhere
is lowered, a differing one is not).

`backend-ops.sh` runs llama.cpp's test-backend-ops on the host against a KosmicKrisp build, which
compiles the same NIR to MSL as the guest driver; it is the quick oracle for a ggml kernel's cost.

Measured 2026-10-16, M1 Max, macOS 27: MUL_MAT m=4096 k=14336, µs per run (host).

| type    | n=1 before | n=1 after | n=512 before | n=512 after |
| ------- | ---------: | --------: | -----------: | ----------: |
| q4_0    |        193 |       189 |        64261 |       65967 |
| iq2_s   |       4676 |       492 |       103578 |      103946 |
| iq2_xs  |       2103 |       266 |        84514 |       84602 |
| iq1_s   |       1881 |       235 |        83924 |       84033 |
| iq1_m   |       1945 |       232 |        85877 |       84860 |
| iq2_xxs |        939 |       379 |        77266 |       77495 |
| iq3_s   |        935 |       430 |        78742 |       78889 |
| iq3_xxs |        714 |       346 |        70441 |       67763 |
| iq4_xs  |        438 |       431 |        65605 |       67102 |

test-backend-ops MUL_MAT and GET_ROWS match the CPU throughout. Llama-3.2-1B token generation in a
guest on the Metal transport (Ollama, greedy, three prompts): IQ3_M 20.6 → 110 t/s (CPU 65),
IQ4_XS unchanged at 96 t/s (CPU 114), outputs as before.

## Hinted loops, unrolled within a budget

ggml writes `[[unroll]]` loops bounded by specialization constants (`NUM_COLS`, `NUM_ROWS`, the
mul_mm tile sizes), and KosmicKrisp ran no loop unrolling on compute shaders, so arrays indexed by
those counters stayed in if-else trees (32 bytes or less) or scratch. NIR unrolls a hinted loop of
known trip count whatever `max_unroll_iterations` says, so honouring every hint is all or nothing,
and all makes ggml's tiled mul_mm 4x slower. limina-kk `kk: unroll the loops the source asks to,
while they stay small` honours a hint only while the loop's NIR cost times its trip count is at
most 512, re-measured after each round of unrolling; no unhinted loop is unrolled.

The budget came from a sweep (64 to 4096; 512 and 768 best). Measured 2026-10-16, MUL_MAT m=4096
k=14336, µs per run:

| kernel        | before | budget 512 |
| ------------- | -----: | ---------: |
| q4_0 n=512    |  64948 |      17015 |
| iq4_xs n=512  |  65734 |      17593 |
| f16 n=512     | 261452 |      17252 |
| iq4_xs n=2    |   1656 |        558 |
| iq4_xs n=4    |   3796 |       3238 |
| iq4_nl n=8    |   3837 |       1372 |
| q4_0 n=8      |   1346 |        996 |
| iq1_m n=4     |    910 |       1817 |
| q8_0 n=2      |    603 |        745 |

Geometric mean over 60 shapes: 1.58x; over n=1..8 alone, 1.28x. test-backend-ops still matches the
CPU in all 18646 cases. Llama-3.2-1B prompt processing (llama-bench pp512): Q4_0 445 → 1543 t/s,
IQ4_XS 435 → 1482, IQ3_M 421 → 1358 (CPU Q4_0: 636). Token generation is unchanged.

## What is left

IQ4_XS token generation (n=1) stays at 431 µs per product against q4_0's 190, and below the CPU
end to end (95 t/s against 118). The kernel looks up every 4-bit weight in a 16-entry table in
threadgroup memory, as iq4_nl does (350 µs); q4_0 has no table. That is ggml's shader, not
KosmicKrisp's code generation: KosmicKrisp compiles with Metal's fast math, so the multiply-adds
NIR splits are fused again. ggml's q8_1 integer-dot path (MMVQ) needs
`integerDotProduct4x8BitPackedSignedAccelerated`, which KosmicKrisp does not advertise, so
`GGML_VK_DISABLE_MMVQ` changes nothing here.
