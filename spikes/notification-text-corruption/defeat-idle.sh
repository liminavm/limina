#!/usr/bin/env bash
# Reset the guest's idle timer. GNOME withholds notification BANNERS while its idle monitor says the
# user is away — the notification goes straight to the tray, nothing repaints, and an unattended
# repro loop measures a static screen while reporting perfectly healthy ink. Verified with
# org.gnome.Mutter.IdleMonitor.GetIdletime: after this, idletime drops to <1s.
#
# F13, not Escape: Escape would dismiss the very banner under measurement. A host cursor warp does
# NOT work here (it moves the pointer without delivering a motion event the guest sees).
osascript -e 'tell application "System Events" to tell process "limina" to set frontmost to true' \
          -e 'delay 0.15' \
          -e 'tell application "System Events" to key code 105' >/dev/null 2>&1
