#!/usr/bin/env bash
# (Re)start the headless nested gnome-shell vehicle in the guest.
#
#   nested-start.sh <ssh-port> [WxH]
#
# Three things this sets up that are not obvious:
#
#  * A PRIVATE session bus at /tmp/nested-bus. A second shell on the real session bus collides with
#    the running one over org.gnome.Shell and org.freedesktop.Notifications, so the nested shell
#    gets its own bus and the test notifications are posted to that bus, not the user's.
#
#  * --force-animations. Headless disables animations, and this bug rides the insertion/expand
#    repaint -- a vehicle with animations off is testing a different workload than the session.
#
#  * A SCREENCAST, started purely as a frame driver. A headless virtual monitor with no consumer
#    never ticks its frame clock: the stage paints once at startup and then freezes, so every
#    screenshot returns the same startup frame and every sample scores clean. Measured: 6 shots
#    over 4.2 s after a Notify, all pixel-identical, banner absent. With a screencast attached the
#    banner appears and the stage tracks damage. This is how headless GNOME actually runs
#    (gnome-remote-desktop does the same), but note it as a confounder: the capture path is extra
#    work the session does not do.
set -u
PORT="${1:?}"; MON="${2:-1920x1080}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/cal-%r@%h:%p -o ControlPersist=600 claude@127.0.0.1)

"${SSH[@]}" "
export XDG_RUNTIME_DIR=/run/user/\$(id -u)
# Anchor the patterns. An unanchored 'pkill -f gnome-shell --headless' also matches the ssh
# command line that CONTAINS that text -- it kills its own wrapper, and the restart silently
# never happens while the script reports success.
pkill -f '^gnome-shell --headless' 2>/dev/null; sleep 1
pkill -f '^dbus-daemon --session --address=unix:path=/tmp/nested-bus' 2>/dev/null; sleep 0.5
rm -f /tmp/nested-bus /tmp/ncast-*.webm
dbus-daemon --session --address=unix:path=/tmp/nested-bus --nofork >/dev/null 2>&1 &
sleep 1
DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/nested-bus nohup \
    gnome-shell --headless --virtual-monitor $MON --no-x11 --force-animations \
    >/tmp/nested-shell.log 2>&1 &
sleep 10
grep -c 'GNOME Shell started' /tmp/nested-shell.log
pgrep -f 'gnome-shell --headless' | head -1
"
# Start the frame driver. Sender-gated like Screenshot, and gated the same way: own an allowlisted
# name, then WAIT -- the shell resolves those names to unique names asynchronously, so a call issued
# immediately after RequestName is refused and reads like a hard permission wall.
"${SSH[@]}" 'cat > /tmp/ncast-start.py <<PYEOF
import time, gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib
conn = Gio.DBusConnection.new_for_address_sync("unix:path=/tmp/nested-bus",
    Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION, None, None)
for n in ["org.gnome.Screenshot","org.gnome.SettingsDaemon.MediaKeys","org.freedesktop.impl.portal.desktop.gnome"]:
    conn.call_sync("org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus","RequestName",
                   GLib.Variant("(su)",(n,4)),None,Gio.DBusCallFlags.NONE,-1,None)
time.sleep(1.5)
r = conn.call_sync("org.gnome.Shell.Screencast","/org/gnome/Shell/Screencast","org.gnome.Shell.Screencast",
    "Screencast", GLib.Variant("(sa{sv})",("/tmp/ncast-%d%u.webm", {"draw-cursor": GLib.Variant("b", False)})),
    None, Gio.DBusCallFlags.NONE, 20000, None)
print("frame driver:", r.unpack())
# Hold the connection: the shell stops the cast when the caller disconnects.
GLib.MainLoop().run()
PYEOF
export XDG_RUNTIME_DIR=/run/user/$(id -u)
nohup python3 /tmp/ncast-start.py >/tmp/ncast.log 2>&1 &
sleep 4
cat /tmp/ncast.log'
