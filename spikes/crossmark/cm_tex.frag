#version 450
layout(set = 0, binding = 0) uniform sampler2D tex;
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
   o = texture(tex, uv) * pc.tint;
}
