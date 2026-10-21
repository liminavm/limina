#version 450
// A triangle from the vertex index alone, so the probe needs no vertex buffers.
// The varying sits at location 31, the top of the range a separable stage's generics count down
// from: a SPIR-V stage meets its neighbour only by location.
layout(location = 31) out vec4 v;
void main()
{
    v = vec4(1.0);
    vec2 p[3] = vec2[](vec2(-0.5, -0.5), vec2(0.5, -0.5), vec2(0.0, 0.5));
    gl_Position = vec4(p[gl_VertexID], 0.0, 1.0);
}
