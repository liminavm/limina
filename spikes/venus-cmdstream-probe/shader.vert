#version 450
layout(push_constant) uniform PC { vec4 tint; } pc;
layout(location=0) out vec4 v_tint;
void main() {
    vec2 pos[3] = vec2[](vec2(-1,-1), vec2(3,-1), vec2(-1,3));
    gl_Position = vec4(pos[gl_VertexIndex]*0.01 + pc.tint.xy*0.0001, 0, 1);
    v_tint = pc.tint;
}
