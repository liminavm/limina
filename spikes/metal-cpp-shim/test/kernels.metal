#include <metal_stdlib>
using namespace metal;

// y = a * x + y, with its arguments bound as MLX binds them: buffers and a setBytes scalar.
kernel void axpy(device const float* x [[buffer(0)]],
                 device float* y [[buffer(1)]],
                 constant float& a [[buffer(2)]],
                 uint i [[thread_position_in_grid]]) {
    y[i] = fma(a, x[i], y[i]);
}

// A variant picked by function constants, as MLX picks its kernels'.
constant bool NEGATE [[function_constant(0)]];
constant int K [[function_constant(1)]];

kernel void spec(device int* dst [[buffer(0)]], uint i [[thread_position_in_grid]]) {
    dst[i] = (NEGATE ? -K : K) + int(i);
}

// One thread stepping an LCG `n` times: long-running work that occupies a sliver of the GPU, so
// two of them on two queues can run side by side.
kernel void spin(device uint* out [[buffer(0)]], constant uint& n [[buffer(1)]]) {
    uint v = out[0];
    for (uint k = 0; k < n; k++)
        v = v * 1664525u + 1013904223u;
    out[0] = v;
}
