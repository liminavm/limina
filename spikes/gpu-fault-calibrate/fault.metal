// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Deliberate GPU address faults, so the kernel's gpuEvent report can be read
// against a KNOWN cause. Two questions this answers, both of which the WebGL
// device-loss investigation has been guessing at:
//
//   1. What `requestor` does a plain shader load report? The real faults carry
//      174/sideband 103 in most reports and 80|96|112/sideband 65 in the rest --
//      two different hardware units, neither identified.
//   2. Does `address` record the byte the shader asked for, or the granule the
//      unit fetched? Every real fault is 64-byte aligned, which has been read as
//      "the pointer was well-formed" with no control sample to justify it.
//
// So `load` reads an address that is unmapped AND deliberately misaligned: if the
// report comes back rounded, the alignment in the real faults means nothing.

#include <metal_stdlib>
using namespace metal;

kernel void load(constant ulong *cfg [[buffer(0)]],
                 device uint *out [[buffer(1)]],
                 uint tid [[thread_position_in_grid]])
{
   device uint *p = (device uint *)cfg[0];
   out[tid] = *p;
}

// A bindless texture read through a resource id that names nothing, which is how
// KosmicKrisp reaches every texture: the shader loads a texture handle out of a
// buffer and reads through it. Calibrates the texture unit's requestor.
kernel void texload(constant ulong *cfg [[buffer(0)]],
                    device float4 *out [[buffer(1)]],
                    uint tid [[thread_position_in_grid]])
{
   constant texture2d<float> &t = *(constant texture2d<float> *)(cfg + 1);
   out[tid] = t.read(uint2(0, 0));
}
