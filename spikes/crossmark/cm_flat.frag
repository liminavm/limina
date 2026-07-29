#version 450
layout(constant_id = 0) const float VARF = 1.0;
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
   o = vec4(pc.tint.rgb * VARF, pc.tint.a);
}
