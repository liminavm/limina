#version 450
/* st_pbo-shaped upload shader: fetch texels from a uniform texel buffer,
 * addressed by the fragment's position within a DIMxDIM quad. */
layout(set = 0, binding = 0) uniform samplerBuffer tb;
layout(push_constant) uniform PC
{
   vec4 rect;
   vec4 tint;
}
pc;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 o;

void
main()
{
   int dim = int(pc.tint.x);
   int idx = int(uv.y * float(dim)) * dim + int(uv.x * float(dim));
   o = texelFetch(tb, idx);
}
