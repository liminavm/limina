#!/bin/bash
# Staged open/close cycle: does IOAccelerator(graphics) ratchet, or does it return?
set -u
SSH="ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1"
PID=$(pgrep -f "limina-vmm --cpus" | head -1)
SPID=$(pgrep -f "target/debug/limina --vmm-bin" | head -1)

snap() {
  echo "--- $1 ---"
  vmmap --summary "$PID" 2>/dev/null | grep -E "^IOAccelerator \(graphics\)|^IOSurface|Physical footprint:" \
    | sed 's/^/  worker  /'
  vmmap --summary "$SPID" 2>/dev/null | grep -E "^IOSurface|Physical footprint:" \
    | sed 's/^/  superv  /'
}

launch() {
  $SSH "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
        systemctl --user stop ff-bench 2>/dev/null; sleep 3
        systemd-run --user --unit=ff-bench --setenv=WAYLAND_DISPLAY=wayland-0 \
          --setenv=MOZ_ENABLE_WAYLAND=1 --setenv=MOZ_DISABLE_GPU_SANDBOX=1 \
          --setenv=XDG_RUNTIME_DIR=/run/user/1000 \
          /usr/bin/firefox --kiosk 'https://webglsamples.org/aquarium/aquarium.html?numFish=5000'" >/dev/null 2>&1
  sleep 40
}
stopff() {
  $SSH "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
        systemctl --user stop ff-bench 2>/dev/null; true" >/dev/null 2>&1
  sleep 30
}

snap "CYCLE0 closed (start)"
for i in 1 2 3; do
  launch;  snap "CYCLE$i open"
  stopff;  snap "CYCLE$i closed"
done
