#version 450
layout(location = 0) out vec3 v_color;
void main() {
    const vec2 pos[3] = vec2[3](vec2(0.0, -0.75), vec2(0.8, 0.65), vec2(-0.8, 0.65));
    const vec3 col[3] = vec3[3](vec3(0.95, 0.15, 0.25), vec3(0.15, 0.85, 0.35), vec3(0.20, 0.35, 0.95));
    gl_Position = vec4(pos[gl_VertexIndex], 0.0, 1.0);
    v_color = col[gl_VertexIndex];
}
