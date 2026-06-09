# metal-repro — standalone Metal reproduction attempt for the tier-2 tri2-drop (#31)

Replicates the broken desktop draw from the Xcode .gputrace (which DOES reproduce on replay):
zink MSL verbatim (vs/fs.metal), cogl record layout (32B stride, interleaved pos/color views,
garbage padding), dynamic-stride bindings at Metal indices 29/30, uint16 {0,1,2,0,2,3} indices,
blending disabled + writeMask All, BGRA8 offscreen, no depth. Knobs (env): REPRO_STATIC_STRIDE,
REPRO_NEGVP, REPRO_DEPTH, REPRO_SEPARATE, REPRO_BIG, REPRO_QUADS, REPRO_MULTIDRAW, REPRO_RESTART,
REPRO_COMPUTE, REPRO_INDIRECT_MIX.

STATUS 2026-06-09: baseline + ALL variants render CLEAN (no repro). The defect reproduces in
Xcode's replay of the real trace but not in this reconstruction => the trigger is in a not-yet-
copied detail (full PSO descriptor fields, attachment texture descriptor/usage, or the preceding
draws in the same encoder). Next fidelity source: the trace's Insights pane + full PSO/encoder
dumps. Build: clang -fobjc-arc -framework Metal -framework Foundation -o repro repro.m
