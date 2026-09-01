#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Force a GL app in the running enhanced/Venus guest onto zink → Venus → MoltenVK → Apple GPU
# and report its renderer, so you can verify 3D acceleration. Run this in a second terminal
# while scripts/run-venus-window.sh has the desktop up.
#
# It (1) prints the GL renderer headlessly (glxinfo/eglinfo — proves which driver binds), then
# (2) launches a visible GL app on the GNOME wayland desktop (shown in the limina window).
#
# The patched Mesa (zink + MR!37115) lives at /opt/mesa-zink in the guest image; this points
# the GL/EGL loaders + Vulkan ICD at it and overrides the gallium driver to zink.
#
# Usage: scripts/venus-gl-test.sh [glmark2|glxgears|vkcube|glinfo]   (default: glmark2)
#   Env: LIMINA_SSH_PORT (default 2222), LIMINA_GL_TIMEOUT (default 25s for the windowed app).
set -euo pipefail

APP="${1:-glmark2}"
PORT="${LIMINA_SSH_PORT:-2222}"
TIMEOUT="${LIMINA_GL_TIMEOUT:-25}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 claude@127.0.0.1)

"${SSH[@]}" true 2>/dev/null || { echo "guest not reachable on :$PORT — is run-venus-window.sh up?" >&2; exit 1; }

# zink-on-Venus environment (Mesa loaders -> /opt/mesa-zink, Vulkan ICD -> guest virtio/venus).
# VN_PERF=no_*_feedback forces Venus's sync onto the virtio-gpu per-context ring fence instead of
# its host-visible *feedback* buffers. It is a LEGACY carry-over kept only so this script's numbers
# stay comparable with its older runs: host-visible blob coherency (#28) was fixed 2026-07-03 and
# the shipped stack stopped setting this on 2026-07-25, so a guest under this script is NOT
# configured the way users run. Drop the line to measure the real thing.
read -r -d '' ZINK_ENV <<'EOF' || true
export LD_LIBRARY_PATH=/opt/mesa-zink/lib64
export LIBGL_DRIVERS_PATH=/opt/mesa-zink/lib64/dri
export __EGL_VENDOR_LIBRARY_DIRS=/opt/mesa-zink/share/glvnd/egl_vendor.d
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
export VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback
EOF
# Desktop session env so windowed apps appear on the GNOME wayland display (uid 1000).
read -r -d '' DESK_ENV <<'EOF' || true
export XDG_RUNTIME_DIR=/run/user/1000
export WAYLAND_DISPLAY=wayland-0
export DISPLAY=:0
EOF

# Renderer check via a SURFACELESS EGL probe (no display/Xauth, ONE context) — a single
# glGetString binds zink→Venus and reports the renderer. (We avoid glxinfo — it needs an Xauth
# cookie over SSH — and eglinfo, which spins up many contexts and can wedge the GPU.)
echo "==> [1/2] GL renderer string (zink→Venus binds?)"
"${SSH[@]}" "$ZINK_ENV; cat > /tmp/limina-eglprobe.c <<'PROBE'
#include <stdio.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
int main(void){
  EGLDisplay (*g)(EGLenum,void*,const EGLAttrib*)=(void*)eglGetProcAddress(\"eglGetPlatformDisplay\");
  EGLDisplay d=g(EGL_PLATFORM_SURFACELESS_MESA,EGL_DEFAULT_DISPLAY,NULL);
  EGLint a,b; if(!eglInitialize(d,&a,&b)){fprintf(stderr,\"eglInitialize 0x%x\n\",eglGetError());return 1;}
  eglBindAPI(EGL_OPENGL_ES_API);
  EGLint ca[]={EGL_SURFACE_TYPE,EGL_PBUFFER_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_ES2_BIT,EGL_NONE};
  EGLConfig c; EGLint n; eglChooseConfig(d,ca,&c,1,&n);
  EGLint xa[]={EGL_CONTEXT_CLIENT_VERSION,2,EGL_NONE};
  EGLContext x=eglCreateContext(d,c,EGL_NO_CONTEXT,xa);
  if(x==EGL_NO_CONTEXT){fprintf(stderr,\"createContext 0x%x\n\",eglGetError());return 3;}
  eglMakeCurrent(d,EGL_NO_SURFACE,EGL_NO_SURFACE,x);
  printf(\"GL_RENDERER = %s\n\",glGetString(GL_RENDERER));
  printf(\"GL_VERSION  = %s\n\",glGetString(GL_VERSION));
  return 0;
}
PROBE
cc /tmp/limina-eglprobe.c -o /tmp/limina-eglprobe -lEGL -lGLESv2 2>&1 && timeout 20 /tmp/limina-eglprobe 2>&1 || echo '(probe failed — see message above)'" \
  | grep -vE 'VN-DBG|VN_DEBUG|LIMINA-DIAG'

echo
echo "==> [2/2] launch '$APP' on zink→Venus (watch the limina window; ${TIMEOUT}s)"
# timeout -k: SIGTERM at $TIMEOUT, SIGKILL 5s later (a stuck-on-fence app may ignore TERM).
case "$APP" in
  glmark2)
    CMD="timeout -k 5 $TIMEOUT glmark2-wayland --run-forever 2>&1 | grep -iE 'GL_RENDERER|GL_VENDOR|GL_VERSION|Error|fail' | head" ;;
  glxgears)
    CMD="timeout -k 5 $TIMEOUT glxgears -info 2>&1 | grep -iE 'GL_RENDERER|renderer|Error|fail' | head" ;;
  vkcube)
    CMD="timeout -k 5 $TIMEOUT vkcube --c 200 2>&1 | grep -iE 'GPU|Selected|Error|fail' | head" ;;
  glinfo)
    CMD="timeout -k 5 $TIMEOUT glxinfo 2>&1 | grep -iE 'OpenGL renderer|OpenGL version' | head" ;;
  *) echo "unknown app '$APP' (use glmark2|glxgears|vkcube|glinfo)" >&2; exit 2 ;;
esac

set +e
"${SSH[@]}" "$ZINK_ENV; $DESK_ENV; $CMD"
rc=$?
# Reap any process left stuck on the fence path so it doesn't wedge the GPU for the next run.
"${SSH[@]}" "pkill -9 -f 'glmark2|glxgears|vkcube|glxinfo' 2>/dev/null; true" >/dev/null 2>&1
set -e
if [ "$rc" = 124 ]; then
  echo
  echo "!! '$APP' was still running at the ${TIMEOUT}s timeout (normal for --run-forever / a"
  echo "   continuous app — it was reaped). A 'GL_RENDERER ... Venus' line above means it rendered."
  echo "   A true HANG (no output, uninterruptible) would mean VN_PERF feedback-disable didn't take"
  echo "   — confirm the env above is exported into the guest."
fi
echo "==> a 'renderer = ... Virtio-GPU Venus (Apple M1 Max)' line means GL is on the GPU."
