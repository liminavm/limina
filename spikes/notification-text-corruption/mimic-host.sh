#!/usr/bin/env bash
# Run glyphmimic natively on the host, on zink-on-KosmicKrisp -- the same host GL stack vrend
# serves the guest's virgl from. A bare run with NONE of this env aborts in GPU init
# ("Couldn't open libEGL.dylib"); that is the missing env, not a stack fault.
#
# This is the leg that unlocks Metal tracing: Apple's GPUToolsCapture segfaults on the VM's
# command stream whenever the window spans the failing pass, so a host-side process that
# reproduces is the only route to a capture.
#
#   ./mimic-host.sh [episodes]          -- arms come from the GM_* environment
#
# VALIDITY GATE, run this before believing any verdict: the mimic is only a mimic if it
# actually compiles to the measured pipeline, and source that looks right is not evidence.
#
#   mkdir -p /tmp/vi
#   MESA_SHADER_CACHE_DISABLE=true KK_LIMINA_SHADER_DUMP=/tmp/vi ./mimic-host.sh 2
#   for f in /tmp/vi/*stage0.nir; do echo "-- $f"; grep '// vi' "$f"; done
#
# MESA_SHADER_CACHE_DISABLE is not optional: the dump fires on COMPILE, so a second run serves
# the pipelines from zink's on-disk cache and silently emits a PARTIAL set of tables. Reading a
# missing table as "the pass is gone" is a ready-made false conclusion.
#
# Two tables must appear, and they are the whole point of the vehicle:
#   journal pass  format 106 / 37 / 103,  binding strides 32 / 32 / 32
#   glyph pass    format 103 / 109 / 103, binding strides 16 /  0 / 16   <- the failing shape
# (a third, attributes_valid=0x0, is the composite's gl_VertexID shader -- expected.)
#
# A stride of 16 where 0 is expected means the constant attribute did NOT become a zero-stride
# binding and the vehicle is testing something else; a clean verdict then means nothing. Same for
# a journal pass showing two attributes instead of three -- that is uv being dead-stripped
# because the fragment shader stopped consuming it.
set -eu
cd "$(dirname "$0")"
. ./kk-env.sh

[ -x ./glyphmimic ] || ./mimic-build.sh
exec ./glyphmimic "$@"
