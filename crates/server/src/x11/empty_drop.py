"""Locate the same empty Nautilus content view beneath a rejecting placeholder.

Only returns another point inside that empty view. Never resolves a filesystem
destination, activates a window, changes tabs, or modifies the clipboard.
"""
import json
import sys


def inside(rect, x, y):
    rx, ry, width, height = rect
    return width > 0 and height > 0 and rx <= x < rx + width and ry <= y < ry + height


def candidate(views, x, y, window):
    matches = [r for r, empty in views if empty and inside(r, x, y)
               and inside(window, r[0], r[1])
               and inside(window, r[0] + r[2] - 1, r[1] + r[3] - 1)]
    if len(matches) != 1:
        return None
    left, top, width, height = matches[0]
    if width < 64 or height < 64:
        return None
    # The empty-state illustration is centered; the lower inner edge belongs
    # to the same empty content view and avoids its scrollbars.
    return [max(left + 24, min(x, left + width - 25)), top + height - 25]


def locate(pid, x, y, window):
    import gi
    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi
    Atspi.set_timeout(500, 500)
    desktop = Atspi.get_desktop(0)
    frames = []
    for i in range(min(desktop.get_child_count(), 256)):
        app = desktop.get_child_at_index(i)
        if app.get_process_id() != pid:
            continue
        for j in range(min(app.get_child_count(), 128)):
            frame = app.get_child_at_index(j)
            r = frame.get_component_iface().get_extents(Atspi.CoordType.SCREEN)
            if (r.x, r.y, r.width, r.height) == tuple(window):
                frames.append(frame)
    if len(frames) != 1:
        return None
    pending = [(frames[0], 0)]
    views = []
    visited = 0
    while pending and visited < 1024:
        node, depth = pending.pop()
        visited += 1
        count = node.get_child_count()
        role = node.get_role()
        states = node.get_state_set()
        if role in (Atspi.Role.LAYERED_PANE, Atspi.Role.TREE_TABLE, Atspi.Role.TABLE):
            if states.contains(Atspi.StateType.SHOWING) and states.contains(Atspi.StateType.VISIBLE):
                r = node.get_component_iface().get_extents(Atspi.CoordType.SCREEN)
                empty = count == 0 if role == Atspi.Role.LAYERED_PANE else node.get_table_iface().get_n_rows() == 0
                views.append(((r.x, r.y, r.width, r.height), empty))
            continue
        # Notebook tab headers can be offscreen while their page is visible;
        # do not prune these ancestors based on their own geometry.
        if depth < 24:
            pending.extend((node.get_child_at_index(i), depth + 1) for i in range(min(count, 256)))
    return candidate(views, x, y, window) if not pending else None


if __name__ == "__main__":
    try:
        pid, x, y, *window = map(int, sys.argv[1:])
        result = locate(pid, x, y, window)
    except Exception:
        result = None
    print(json.dumps(result))
