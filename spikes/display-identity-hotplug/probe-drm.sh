#!/bin/bash
# Read-only DRM/EDID/monitor probe, run in a guest over ssh.
echo "--- compositor ---"
ps -eo args | grep -E "^(/usr/bin/gnome-shell|/usr/local/bin/synoik)" | sort -u
echo "--- connectors ---"
for c in /sys/class/drm/card*-*; do
  [ -e "$c/status" ] || continue
  echo "$c status=$(cat $c/status 2>/dev/null) enabled=$(cat $c/enabled 2>/dev/null) edid_bytes=$(stat -c %s $c/edid 2>/dev/null)"
  echo "    modes: $(tr '\n' ' ' < $c/modes 2>/dev/null)"
done
echo "--- edid hexdump ---"
for c in /sys/class/drm/card*-Virtual*; do hexdump -C "$c/edid" 2>/dev/null | head -10; done
echo "--- kernel ---"
uname -r
echo "--- DisplayConfig GetCurrentState ---"
gdbus call --session --dest org.gnome.Mutter.DisplayConfig \
  --object-path /org/gnome/Mutter/DisplayConfig \
  --method org.gnome.Mutter.DisplayConfig.GetCurrentState 2>&1 | head -c 4000
echo
echo "--- monitors.xml ---"
cat ~/.config/monitors.xml 2>&1
