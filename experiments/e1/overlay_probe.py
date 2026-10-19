# Fullscreen transparent overlay layer (namespace scrawl-overlay), click-through, with a red box.
import gi, cairo
gi.require_version('Gtk', '4.0'); gi.require_version('Gtk4LayerShell', '1.0')
from gi.repository import Gtk, Gtk4LayerShell as LS
def draw(area, cr, w, h):
    cr.set_source_rgba(0, 0, 0, 0); cr.set_operator(cairo.OPERATOR_SOURCE); cr.paint()
    cr.set_operator(cairo.OPERATOR_OVER); cr.set_source_rgba(1, 0, 0, 1); cr.rectangle(200, 200, 400, 300); cr.fill()
def act(app):
    win = Gtk.Window(application=app); LS.init_for_window(win); LS.set_layer(win, LS.Layer.OVERLAY)
    LS.set_namespace(win, "scrawl-overlay"); LS.set_keyboard_mode(win, LS.KeyboardMode.NONE); LS.set_exclusive_zone(win, -1)
    for e in (LS.Edge.LEFT, LS.Edge.RIGHT, LS.Edge.TOP, LS.Edge.BOTTOM): LS.set_anchor(win, e, True)
    css = Gtk.CssProvider(); css.load_from_data(b"window { background: transparent; }")
    Gtk.StyleContext.add_provider_for_display(win.get_display(), css, 800)
    a = Gtk.DrawingArea(); a.set_draw_func(draw); win.set_child(a); win.present()
    win.get_surface().set_input_region(cairo.Region())
app = Gtk.Application(application_id="dev.ochre.overlayprobe"); app.connect('activate', act); app.run([])
