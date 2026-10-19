# GTK4 test client: logs what the *app* receives, stamped with CLOCK_MONOTONIC us.
import sys, time, gi
gi.require_version('Gtk', '4.0')
from gi.repository import Gtk, GLib
log = open(sys.argv[1], 'a', buffering=1)
def w(s): log.write(f"{time.clock_gettime_ns(time.CLOCK_MONOTONIC)//1000} {s}\n")
def act(app):
    win = Gtk.ApplicationWindow(application=app, title="scrawl-e1-client")
    area = Gtk.DrawingArea(); area.set_hexpand(True); area.set_vexpand(True)
    win.set_child(area)
    g = Gtk.GestureClick(); g.set_button(0)
    g.connect('pressed', lambda g, n, x, y: w(f"press b{g.get_current_button()} {x:.0f} {y:.0f}"))
    g.connect('released', lambda g, n, x, y: w(f"release b{g.get_current_button()} {x:.0f} {y:.0f}"))
    area.add_controller(g)
    s = Gtk.EventControllerScroll(flags=Gtk.EventControllerScrollFlags.BOTH_AXES)
    s.connect('scroll', lambda c, dx, dy: (w(f"scroll {dx} {dy}"), False)[1])
    area.add_controller(s)
    k = Gtk.EventControllerKey()
    k.connect('key-pressed', lambda c, kv, kc, st: (w(f"key {kc} state={int(st)}"), False)[1])
    win.add_controller(k)
    d = Gtk.GestureDrag(); d.connect('drag-end', lambda g, x, y: w(f"drag-end {x:.0f} {y:.0f}")); area.add_controller(d)
    win.fullscreen(); win.present(); w("client up")
app = Gtk.Application(application_id="dev.ochre.scrawle1"); app.connect('activate', act); app.run([])
