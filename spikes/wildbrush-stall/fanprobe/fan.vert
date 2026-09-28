#version 450
// Must match vpos() in fanprobe.c. Case 0 passes the fan's firstVertex in `first`; case 1
// (multi-draw) passes seg == 0 and the shader recovers each fan from the known firsts.
layout(push_constant) uniform P { int first; int seg; float radius; float pad; } p;
layout(location = 0) flat out uint prov;
void main() {
   int v = gl_VertexIndex, inst = gl_InstanceIndex;
   int first = p.first, seg = p.seg, which = inst;
   if (seg == 0) {
      if (v >= 300) { first = 300; seg = 3; which = 1; } else { first = 20; seg = 5; which = 0; }
   }
   float cx = which == 0 ? -0.45 : 0.45;
   int local = v - first;
   vec2 pos = vec2(cx, 0.0);
   if (local != 0) {
      float a = float(local - 1) * 6.2831853 / float(seg);
      pos += p.radius * vec2(cos(a), sin(a));
   }
   gl_Position = vec4(pos, 0.0, 1.0);
   prov = uint(v) + 1u + uint(inst) * 100000u;
}
