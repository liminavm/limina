#version 450
// A triangle from the vertex index alone, so the probe needs no vertex buffers.
void main()
{
    vec2 p[3] = vec2[](vec2(-0.5, -0.5), vec2(0.5, -0.5), vec2(0.0, 0.5));
    gl_Position = vec4(p[gl_VertexID], 0.0, 1.0);
}
