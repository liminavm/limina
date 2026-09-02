# Render with one GSK renderer, then re-read that texture through another.
# Isolates "was the dmabuf written correctly" from "is the readback broken".
import sys, gi
gi.require_version("Gtk","4.0"); gi.require_version("Gdk","4.0"); gi.require_version("Gsk","4.0")
from gi.repository import Gtk, Gdk, Gsk, Graphene
Gtk.init()
disp = Gdk.Display.get_default()

def mk(kind):
    r = {"gl": Gsk.GLRenderer, "vulkan": Gsk.VulkanRenderer, "cairo": Gsk.CairoRenderer}[kind]()
    r.realize(Gdk.Surface.new_toplevel(disp))
    return r

def render(r, tex, w, h):
    s = Gtk.Snapshot()
    s.append_texture(tex, Graphene.Rect().init(0, 0, w, h))
    return r.render_texture(s.to_node(), Graphene.Rect().init(0, 0, w, h))

src = Gdk.Texture.new_from_filename(sys.argv[4])  # cross.py <first> <second> <out.png> <source.png>
w, h = src.get_width(), src.get_height()

first, second, out = sys.argv[1], sys.argv[2], sys.argv[3]
t1 = render(mk(first), src, w, h)
print(f"pass1 ({first}) -> {type(t1).__name__}", file=sys.stderr)
if second == "none":
    t1.save_to_png(out)
else:
    t2 = render(mk(second), t1, w, h)
    print(f"pass2 ({second}) -> {type(t2).__name__}", file=sys.stderr)
    t2.save_to_png(out)
