#include <metal_stdlib>
using namespace metal;

// y = a * x + y, with its arguments bound as MLX binds them: buffers and a setBytes scalar.
kernel void axpy(device const float* x [[buffer(0)]],
                 device float* y [[buffer(1)]],
                 constant float& a [[buffer(2)]],
                 uint i [[thread_position_in_grid]]) {
    y[i] = fma(a, x[i], y[i]);
}
