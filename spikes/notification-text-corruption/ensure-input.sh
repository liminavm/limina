#!/usr/bin/env bash
# Restore synthetic input in the guest and prove the session is showing banners.
#
#   ensure-input.sh <ssh-port>
#
# ydotoold does NOT survive a gdm restart, and its death is silent: ydotool exits fine, the socket
# is simply gone, so every key press calib.sh makes is a no-op. The session then stays wherever it
# was -- and if that is the Activities overview, GNOME shows no banners at all and an entire run
# comes back NOBANNER. Worse, in the overview the header strip lands on the search entry, whose ink
# scores as a PRESENT header and turns damaged samples into clean ones. Run this after every arm
# change, before measuring.
#
# It also waits for a USER session shell. GDM's greeter runs a gnome-shell too (--mode=gdm), and it
# loads the same instrumented libraries, so it prints the same "instrumented mutter live" banner --
# an arm can look armed while the user session is not running at all. Autologin does not re-fire
# after the first gdm restart in a boot, so this is reachable, and every sample then comes back
# NOBANNER because notifications are posted to a session nobody is displaying.
set -e -o pipefail
PORT="${1:?}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/cal-%r@%h:%p -o ControlPersist=900 claude@127.0.0.1)

# A user-session shell, not the greeter's.
user_shell() { "${SSH[@]}" 'pgrep -a gnome-shell | grep -qv -- "--mode=gdm"' 2>/dev/null; }
if ! user_shell; then
    echo "ensure-input: no user-session shell (greeter only) -- restarting gdm"
    "${SSH[@]}" 'sudo systemctl restart gdm' || true
    for _ in $(seq 1 40); do sleep 2; user_shell && break; done
fi
user_shell || { echo "ensure-input: user session never came up" >&2; exit 1; }

"${SSH[@]}" 'sudo pkill -x ydotoold 2>/dev/null; sudo rm -f /tmp/.ydotool_socket
sudo systemd-run --unit=limina-ydotoold --collect \
    ydotoold --socket-path=/tmp/.ydotool_socket --socket-own="$(id -u):$(id -g)" >/dev/null'
for _ in $(seq 1 20); do
    "${SSH[@]}" 'test -S /tmp/.ydotool_socket' 2>/dev/null && break
    sleep 0.5
done
"${SSH[@]}" 'test -S /tmp/.ydotool_socket' || { echo "ensure-input: ydotoold did not come up" >&2; exit 1; }

# Leave the overview. Escape is sent twice: the first can be swallowed while the shell settles.
"${SSH[@]}" 'export YDOTOOL_SOCKET=/tmp/.ydotool_socket
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
ydotool key 1:1 1:0; sleep 0.5; ydotool key 1:1 1:0' >/dev/null 2>&1
echo "ensure-input: ydotoold up, Escape delivered"
