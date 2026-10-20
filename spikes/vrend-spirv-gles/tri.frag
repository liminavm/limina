#version 450
// The colour comes from a uniform block at an explicit binding: the way vrend's SPIR-V will bind
// everything, since GL's SPIR-V path has no names to look up.
layout(location = 0) out vec4 o;
layout(std140, binding = 0) uniform U { vec4 color; };
// vrend's constants are a default-block uvec4 array set with glUniform4uiv by location: GL's
// SPIR-V path takes that only with an explicit Location. Unset, it reads zero and o turns black.
layout(location = 0) uniform uvec4 consts[2];
void main()
{
    o = color * vec4(consts[1]);
}
