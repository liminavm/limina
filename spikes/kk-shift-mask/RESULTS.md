# KosmicKrisp shift counts: NIR wraps them, MSL does not

NIR takes a shift's count modulo the shifted value's bit size, and its algebraic passes rely on it:
ggml's IQ scale read, `(scales[i / 2] >> (4 * (i & 1))) & 0xF` on a byte, reaches the backend as
`b >> (i << 2)`. MSL shifts by the whole count after promoting a char or a short to int, so for
`i >= 2` the byte's bits are shifted away and the scale reads as 0. That is why ggml's Vulkan backend
got IQ1_M, IQ2_S, IQ2_XS, IQ3_S and IQ4_XS wrong on KosmicKrisp (test-backend-ops: their GET_ROWS,
MUL_MAT and MUL_MAT_ID cases), while every type without a byte-sized scale field passed.

`shift.comp` has the shape for 8-, 16- and 32-bit operands; `probe.c` dispatches it and compares
every value with the CPU's. `gen.sh` regenerates `shift_comp.h`.

    cc -Wall -I/opt/homebrew/include probe.c -L/opt/homebrew/lib -lvulkan -o smp
    VK_ICD_FILENAMES=<kk build>/.../kosmickrisp_mesa_devenv_icd.aarch64.json ./smp

Fixed in limina-kk `kk: mask shift counts the way NIR defines them`. Before it the 8- and 16-bit rows
have 10 wrong values of 16 (M1 Max, macOS 27); the 32-bit rows come out right only because the GPU
wraps 32-bit counts itself, which MSL does not promise. After it all 32 values match, and in a guest
on the Metal transport test-backend-ops passes every IQ case of GET_ROWS (41), MUL_MAT (246) and
MUL_MAT_ID (40). `MESA_KK_DEBUG=msl` shows the emitted shifts: a count is masked
(`t17 >> (t18 & 7u)`) unless it is a constant already in range.
