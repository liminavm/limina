#!/usr/bin/env bash
# Post notifications into a HEADLESS NESTED gnome-shell and screenshot each one.
#
# Why nested: the session shell cannot be traced -- LD_PRELOAD is inherited by the Xwayland it
# spawns and both processes fight over the same trace file. `--headless --virtual-monitor --no-x11`
# contends with nothing, spawns no Xwayland, screenshots itself, and is the one process we can
# preload. It renders through the same /dev/dri/renderD128 virtio_gpu -> virgl -> vrend -> zink/KK
# path as the session, so it is a candidate vehicle for the fault, not just for the tooling.
#
#   nested-run.sh <ssh-port> <posts> <outdir>
set -u
PORT="${1:?}"; N="${2:-12}"; OUT="${3:-nested}"
NTITLE="${NTITLE:-Critical Updates}"
NBODY="${NBODY:-Install critical updates as soon as possible}"
MON="${MON:-1920x1080}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/cal-%r@%h:%p -o ControlPersist=600 claude@127.0.0.1)
mkdir -p "$OUT"

# The shell's Screenshot D-Bus method is sender-gated. A caller that OWNS one of the allowlisted
# names is let through -- but the shell resolves those names to unique names asynchronously, so a
# call issued immediately after RequestName is still refused. The helper sleeps before calling;
# without that pause this looks like a hard permission wall rather than a race.
"${SSH[@]}" 'cat > /tmp/nshot.py <<PYEOF
import sys, time, gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib
conn = Gio.DBusConnection.new_for_address_sync("unix:path=/tmp/nested-bus",
    Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
    None, None)
for name in ["org.gnome.Screenshot", "org.gnome.SettingsDaemon.MediaKeys",
             "org.freedesktop.impl.portal.desktop.gnome"]:
    conn.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                   "RequestName", GLib.Variant("(su)", (name, 4)), None,
                   Gio.DBusCallFlags.NONE, -1, None)
time.sleep(1.5)
def call(m, sig, args):
    return conn.call_sync("org.gnome.Shell.Screenshot", "/org/gnome/Shell/Screenshot",
                          "org.gnome.Shell.Screenshot", m, GLib.Variant(sig, args), None,
                          Gio.DBusCallFlags.NONE, 20000, None)
def notify(t, b):
    r = conn.call_sync("org.freedesktop.Notifications", "/org/freedesktop/Notifications",
                       "org.freedesktop.Notifications", "Notify",
                       GLib.Variant("(susssasa{sv}i)",
                                    ("Software", 0, "software-update-available-symbolic",
                                     t, b, [], {}, 5000)),
                       None, Gio.DBusCallFlags.NONE, 20000, None)
    return r.unpack()[0]
def close(i):
    conn.call_sync("org.freedesktop.Notifications", "/org/freedesktop/Notifications",
                   "org.freedesktop.Notifications", "CloseNotification",
                   GLib.Variant("(u)", (i,)), None, Gio.DBusCallFlags.NONE, 20000, None)
# One post + one shot per invocation keeps the name-settle cost off the per-sample path only if we
# loop in-process, so loop here: argv = <count> <title> <body> <outdir>
count = int(sys.argv[1]); title = sys.argv[2]; body = sys.argv[3]; outdir = sys.argv[4]
last = None
for i in range(1, count + 1):
    if last is not None:
        close(last)
        time.sleep(0.4)
    last = notify("%s %03d" % (title, i), body)
    time.sleep(1.1)
    p = "%s/nested-%03d.png" % (outdir, i)
    call("Screenshot", "(bbs)", (False, False, p))
    print("post %03d id=%s -> %s" % (i, last, p), flush=True)
    time.sleep(1.0)
PYEOF
mkdir -p /tmp/nested-out && rm -f /tmp/nested-out/*.png'

"${SSH[@]}" "python3 /tmp/nshot.py $N '$NTITLE' '$NBODY' /tmp/nested-out"
scp -q -P "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o ControlPath=/tmp/cal-%r@%h:%p "claude@127.0.0.1:/tmp/nested-out/*.png" "$OUT/"
echo "pulled $(ls "$OUT"/*.png 2>/dev/null | wc -l) shots into $OUT"
