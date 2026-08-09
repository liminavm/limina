#!/bin/bash
# Scoping A/B: does the IOAccelerator(graphics) ratchet also happen on zink-on-venus?
# Ratchet on both -> common layer (KK/Metal or virglrenderer core).
# vrend-only      -> the vrend GL path (EGLImage scanout / gbm import).
set -u
SSH="ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1"
PID=$(pgrep -f "limina-vmm --cpus" | head -1)

snap() {
  echo "--- $1 ---"
  vmmap --summary "$PID" 2>/dev/null \
    | grep -E "^IOAccelerator \(graphics\)|^IOSurface|Physical footprint:" | sed 's/^/  worker  /'
}

snap "ZINK cycle: closed (start)"
$SSH "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
      systemctl --user stop ff-bench 2>/dev/null; sleep 3
      systemd-run --user --unit=ff-bench --setenv=WAYLAND_DISPLAY=wayland-0 \
        --setenv=MOZ_ENABLE_WAYLAND=1 --setenv=MOZ_DISABLE_GPU_SANDBOX=1 \
        --setenv=XDG_RUNTIME_DIR=/run/user/1000 \
        --setenv=MESA_LOADER_DRIVER_OVERRIDE=zink --setenv=GALLIUM_DRIVER=zink \
        /usr/bin/firefox --kiosk 'https://webglsamples.org/aquarium/aquarium.html?numFish=5000'" >/dev/null 2>&1
sleep 40
snap "ZINK cycle: open"
$SSH "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
      systemctl --user stop ff-bench 2>/dev/null; true" >/dev/null 2>&1
sleep 30
snap "ZINK cycle: closed"
