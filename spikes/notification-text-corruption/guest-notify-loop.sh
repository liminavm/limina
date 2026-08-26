#!/usr/bin/env bash
# Guest side of the notification-text-corruption repro. Replaces one notification in place as fast
# as the shell will take it: each replace re-renders the card's text actors (the suspected failure
# unit) AND keeps the compositor presenting, which is what makes the host-side frame capture
# refresh at all (LIMINA_WINDOW_CAPTURE fires once per 120 presents, and an idle desktop presents
# almost never).
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
N="${1:-400}"
for i in $(seq 1 "$N"); do
    notify-send -r 4242 -t 60000 "notifyprobe" "MMMM WWWW MMMM WWWW MMMM $i"
    sleep 0.15
done
