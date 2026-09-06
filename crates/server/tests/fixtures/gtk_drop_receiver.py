"""GTK's asynchronous file inspection, as used by Nautilus before accepting a drop."""
import pathlib
import shutil
import sys
from urllib.parse import unquote, urlparse

import gi
gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, Gtk

window = Gtk.Window()
window.set_default_size(300, 200)
window.move(0, 0)
window.drag_dest_set(Gtk.DestDefaults(0), [Gtk.TargetEntry.new("text/uri-list", 0, 0)], Gdk.DragAction.COPY)
files = []
requested = False
motions = 0

def motion(widget, context, x, y, timestamp):
    global requested, motions
    motions += 1
    if not requested:
        requested = True
        widget.drag_get_data(context, Gdk.Atom.intern("text/uri-list", False), timestamp)
    Gdk.drag_status(context, Gdk.DragAction.COPY if files else Gdk.DragAction(0), timestamp)
    return True

def received(widget, context, x, y, data, info, timestamp):
    files.extend(pathlib.Path(unquote(urlparse(uri).path)) for uri in data.get_uris())

def drop(widget, context, x, y, timestamp):
    assert motions >= 2, "source did not renegotiate after the initial rejection"
    assert files
    for path in files:
        shutil.copyfile(path, pathlib.Path(sys.argv[1]) / path.name)
    Gtk.drag_finish(context, True, False, timestamp)
    print("COPIED", flush=True)
    return True

window.connect("drag-motion", motion)
window.connect("drag-data-received", received)
window.connect("drag-drop", drop)
window.show_all()
Gdk.Display.get_default().sync()
print("READY", flush=True)
Gtk.main()
