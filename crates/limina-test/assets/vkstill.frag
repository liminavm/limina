#version 450
layout(location = 0) in vec3 v_color;
layout(location = 0) out vec4 o_color;
void main() {
    // High-frequency structure on top of the barycentric gradient, so a cell-mean
    // comparison sees real detail rather than a smooth ramp that survives corruption.
    vec2 p = gl_FragCoord.xy * 0.08;
    float band = 0.75 + 0.25 * sin(p.x) * cos(p.y);
    o_color = vec4(v_color * band, 1.0);
}
