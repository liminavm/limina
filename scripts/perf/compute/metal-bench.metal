#include <metal_stdlib>
using namespace metal;

// The smallest unit of work: what a dispatch or a commit costs beyond the kernel itself.
kernel void tiny(device uint* o [[buffer(0)]], uint i [[thread_position_in_grid]]) {
    o[i] += 1;
}

// Memory bandwidth: one read and one write of every float4.
kernel void copy4(device const float4* src [[buffer(0)]], device float4* dst [[buffer(1)]],
                  uint i [[thread_position_in_grid]]) {
    dst[i] = src[i];
}

// One thread stepping an LCG `n` times: long-running work that occupies a sliver of the GPU, so
// two of them on two queues can run side by side.
kernel void spin(device uint* out [[buffer(0)]], constant uint& n [[buffer(1)]]) {
    uint v = out[0];
    for (uint k = 0; k < n; k++)
        v = v * 1664525u + 1013904223u;
    out[0] = v;
}
