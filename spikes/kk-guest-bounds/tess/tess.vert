#version 450
// A triangle covering the whole target, from gl_VertexIndex alone (no vertex buffers).
void main() {
  vec2 p[3] = vec2[](vec2(-1, -1), vec2(3, -1), vec2(-1, 3));
  gl_Position = vec4(p[gl_VertexIndex % 3], 0, 1);
}
