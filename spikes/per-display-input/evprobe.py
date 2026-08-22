#!/usr/bin/python3
# evprobe — does a normal toolkit app receive input from a tablet-class device?
#
# The measurement that decides whether per-display tablets are usable as limina's pointer:
# a tablet tool is a separate Wayland device (zwp_tablet_tool_v2) with its own cursor and
# its own focus, so a click from it is not a wl_pointer button. GTK maps tablet tools onto
# its own event stream, but only for clients that GTK itself handles — this prints what
# actually arrives, with the device and its source, so "the app saw it" is observed rather
# than assumed.
#
# Run in the guest session:  python3 evprobe.py 2>&1 | tee /tmp/evprobe.log

import os

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk, Gdk, GLib  # noqa: E402


def describe(device):
    if device is None:
        return "device=None"
    src = device.get_source().value_nick if hasattr(device.get_source(), "value_nick") else device.get_source()
    return f"{device.get_name()!r} source={src}"


class Probe(Gtk.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="limina input probe")
        self.set_default_size(700, 400)
        self.label = Gtk.Label(label="waiting for input…")
        self.set_child(self.label)

        motion = Gtk.EventControllerMotion()
        motion.connect("motion", self.on_motion)
        self.add_controller(motion)

        click = Gtk.GestureClick()
        click.set_button(0)
        click.connect("pressed", self.on_press)
        self.add_controller(click)

        scroll = Gtk.EventControllerScroll(flags=Gtk.EventControllerScrollFlags.BOTH_AXES)
        scroll.connect("scroll", self.on_scroll)
        self.add_controller(scroll)

        self.last = None

    def on_motion(self, ctrl, x, y):
        dev = describe(ctrl.get_current_event_device())
        line = f"MOTION {x:.0f},{y:.0f} from {dev}"
        if line != self.last:
            print(line, flush=True)
            self.last = line
        self.label.set_text(line)

    def on_press(self, gesture, n_press, x, y):
        dev = describe(gesture.get_current_event_device())
        line = f"PRESS  {x:.0f},{y:.0f} n={n_press} from {dev}"
        print(line, flush=True)
        self.label.set_text(line)


    def on_scroll(self, ctrl, dx, dy):
        dev = describe(ctrl.get_current_event_device())
        line = f"SCROLL {dx:+.2f},{dy:+.2f} from {dev}"
        print(line, flush=True)
        self.label.set_text(line)
        return True


# Which monitor the window opens on is the compositor's choice, and GNOME remembers it per
# app — so a probe that must land on a named connector has to ask for it. LIMINA_PROBE_CONNECTOR
# fullscreens the window on that connector; unset, the compositor decides as before.
def on_activate(app):
    w = Probe(app)
    want = os.environ.get("LIMINA_PROBE_CONNECTOR")
    if want:
        for mon in w.get_display().get_monitors():
            if mon.get_connector() == want:
                w.fullscreen_on_monitor(mon)
                break
        else:
            print(f"no monitor with connector {want!r}", flush=True)
    w.present()
    GLib.timeout_add_seconds(1, lambda: (print(f"monitor: {w.get_display().get_monitor_at_surface(w.get_surface()).get_connector()}", flush=True), False)[1])


app = Gtk.Application(application_id="dev.limina.InputProbe")
app.connect("activate", on_activate)
app.run(None)
