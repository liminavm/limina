#!/bin/bash
# Heavy concurrent venus mix in the seated session, for DUR seconds.
DUR=${1:-300}
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0
set -a; [ -f /etc/environment.d/90-limina-zink.conf ] && . /etc/environment.d/90-limina-zink.conf; set +a
vulkaninfo --summary 2>/dev/null | grep -m2 deviceName
vkcube --wsi wayland >/tmp/vkcube.log 2>&1 & p1=$!
vkcube --wsi wayland >/tmp/vkcube2.log 2>&1 & p2=$!
firefox --new-window 'https://webglsamples.org/aquarium/aquarium.html?numFish=5000' >/tmp/ff.log 2>&1 & p3=$!
glmark2-es2-wayland --run-forever >/tmp/glmark.log 2>&1 & p4=$!
end=$((SECONDS+DUR))
while [ $SECONDS -lt $end ]; do
  vkmark --winsys wayland -b vertex:duration=5 -b texture:duration=5 -b shading:duration=5 >/tmp/vkmark.log 2>&1
done
kill $p1 $p2 $p4 2>/dev/null
for pid in $(pgrep -u claude -x firefox); do kill $pid; done
sleep 5
echo "mix done; alive: $(pgrep -u claude -l 'vkcube|glmark2|firefox|vkmark' | tr '\n' ' ')"
tail -3 /tmp/vkmark.log
