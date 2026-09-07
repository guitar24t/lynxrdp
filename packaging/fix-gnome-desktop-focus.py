#!/usr/bin/env python3
"""Repair empty-desktop keyboard focus in RHEL 9's Desktop Icons extension.

Run with sudo on the Linux host, then log out and back in. The original is
backed up beside the file. This only recognizes the affected GNOME 40 handler;
it deliberately refuses unfamiliar extension code.
"""
from pathlib import Path
import shutil

TARGET = Path("/usr/share/gnome-shell/extensions/"
              "desktop-icons@gnome-shell-extensions.gcampax.github.com/desktopGrid.js")
BEFORE = """    _onPressButton(actor, event) {
        let button = event.get_button();
        let [x, y] = event.get_coords();

        if (button == 1) {
"""
AFTER = BEFORE + "            this._grid.grab_key_focus();\n"


def repair(path=TARGET):
    source = path.read_text(encoding="utf-8")
    if AFTER in source:
        print("Desktop keyboard focus is already fixed.")
        return
    if source.count(BEFORE) != 1:
        raise SystemExit("Unrecognized desktop handler; no files changed.")
    backup = path.with_suffix(".js.lynxrdp-original")
    if backup.exists():
        raise SystemExit("Backup already exists; no files changed.")
    shutil.copy2(path, backup)
    path.write_text(source.replace(BEFORE, AFTER, 1), encoding="utf-8")
    print("Fixed desktop keyboard focus. Log out and back in to load it.")


if __name__ == "__main__":
    repair()
