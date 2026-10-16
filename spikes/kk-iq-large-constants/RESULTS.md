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

Still slower than it should be: IQ4_XS, and every IQ type at n=2..8 (iq4_xs 1652 µs at n=2 against
q4_0's 282). None of those kernels uses scratch. ggml's q8_1 integer-dot path (MMVQ), which it uses
for small batches elsewhere, needs `integerDotProduct4x8BitPackedSignedAccelerated`, which
KosmicKrisp does not advertise, so `GGML_VK_DISABLE_MMVQ` changes nothing here.

What keeps them slow is loops KosmicKrisp never unrolls: ggml writes `[[unroll]]` loops bounded by
specialization constants (`NUM_COLS`, `NUM_ROWS`), and KosmicKrisp runs no `nir_opt_loop_unroll` on
compute shaders, so arrays like `sum[NUM_COLS]` stay indexed by a loop counter (an if-else tree at
32 bytes or less, scratch above). Running the unroller before that lowering honours the SPIR-V
`Unroll` hint (NIR unrolls a hinted loop of known trip count whatever `max_unroll_iterations` says),
and it is not a global win, measured the same day (µs, before → after):

| kernel             | before | unrolled |
| ------------------ | -----: | -------: |
| iq4_xs n=2         |   1641 |      907 |
| iq4_xs n=8         |   7053 |     2163 |
| iq3_s n=8          |   4233 |     1575 |
| iq2_s n=8          |   4822 |     2064 |
| q4_0 n=2           |    283 |      354 |
| q4_0 n=512 (mul_mm)|  65539 |   281669 |

The tiled matmul behind prompt processing gets 4x slower (its scratch arrays become registers),
whatever the iteration cap. Sending every indirectly indexed temp to scratch instead of if-else
trees is worse on both kinds of kernel. A fix has to choose per loop.
