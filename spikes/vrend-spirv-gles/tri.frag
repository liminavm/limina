#version 450
// The colour comes from a uniform block at an explicit binding: the way vrend's SPIR-V will bind
// everything, since GL's SPIR-V path has no names to look up.
layout(location = 0) out vec4 o;
layout(std140, binding = 0) uniform U { vec4 color; };
void main()
{
    o = color;
}
