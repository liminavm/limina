#version 450
/* Quad (6 verts, two tris) or triangle (first 3 verts) from gl_VertexIndex.
 * Both backends' row 0 in readback corresponds to NDC y=-1 (GL reads
 * bottom-up from a bottom-left origin, VK top-down from a top-left origin
 * with +y down), so the same shader yields byte-identical readbacks. */
layout(push_constant) uniform PC
{
   vec4 rect; /* xy offset (NDC), zw scale */
   vec4 tint;
}
pc;
layout(location = 0) out vec2 uv;

void
main()
{
   const vec2 corners[6] = vec2[6](vec2(0, 0), vec2(1, 0), vec2(1, 1),
                                   vec2(0, 0), vec2(1, 1), vec2(0, 1));
   vec2 c = corners[gl_VertexIndex];
   uv = c;
   gl_Position = vec4(pc.rect.xy + c * pc.rect.zw, 0.0, 1.0);
}
