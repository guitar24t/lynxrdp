#!/usr/bin/env python3
"""Check graphical session input on isolated Xvfb displays.

Build the workspace first. Requires Xvfb, xterm, xdpyinfo, and xdotool.
Clipboard synchronization is disabled; typing targets only the test shell.
On WSL, use a mount namespace with a writable /tmp/.X11-unix.
"""
import argparse
import os
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import time


def wait_for(predicate, label, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise RuntimeError("Timed out: " + label)


def run(args, env):
    return subprocess.check_output(
        args, env=env, text=True, stderr=subprocess.DEVNULL
    ).strip()


def find_window(class_name, env):
    args = ["xdotool", "search", "--class", class_name]
    wait_for(
        lambda: subprocess.run(
            args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        ).returncode == 0,
        "window " + class_name,
    )
    return run(args, env).splitlines()[0]


def check(bin_dir):
    children = []
    with tempfile.TemporaryDirectory(prefix="lynxrdp-gui-live-") as tmp:
        with open(tmp + "/test.log", "w", encoding="utf-8") as log:
            try:
                local_env = dict(os.environ, XDG_RUNTIME_DIR=tmp)
                local_env.pop("WAYLAND_DISPLAY", None)
                display = subprocess.Popen(
                    ["Xvfb", "-displayfd", "1", "-screen", "0", "1200x900x24", "-nolisten", "tcp"],
                    stdout=subprocess.PIPE, stderr=log, text=True,
                )
                children.append(display)
                assert select.select([display.stdout], [], [], 10)[0], "Xvfb did not select a display"
                local_env["DISPLAY"] = ":" + display.stdout.readline().strip()
                wait_for(
                    lambda: subprocess.run(
                        ["xdpyinfo"], env=local_env,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    ).returncode == 0,
                    "local display",
                )
                with socket.socket() as sock:
                    sock.bind(("127.0.0.1", 0))
                    port = sock.getsockname()[1]
                server = subprocess.Popen(
                    [str(bin_dir / "lynxrdp-session"), "--listen", f"127.0.0.1:{port}",
                     "--width", "1000", "--height", "700",
                     "--startwm", "xterm -geometry 120x36+0+0",
                     "--runtime-dir", tmp + "/session", "--upload-dir", tmp + "/uploads",
                     "--print-display"],
                    stdout=subprocess.PIPE, stderr=log, text=True,
                )
                children.append(server)
                assert select.select([server.stdout], [], [], 15)[0], "Server did not start"
                remote = server.stdout.readline().strip()
                assert remote.startswith(":"), remote
                remote_env = dict(
                    os.environ, DISPLAY=remote,
                    XAUTHORITY=str(next(Path(tmp + "/session").glob("Xauthority*"))),
                )
                run(["xdotool", "windowfocus", find_window("XTerm", remote_env)], remote_env)
                client = subprocess.Popen(
                    [str(bin_dir / "lynxrdp"), "--connect", f"127.0.0.1:{port}",
                     "--no-clipboard", "--scale", "1"],
                    env=local_env, stdout=log, stderr=log,
                )
                children.append(client)
                window = find_window("lynxrdp", local_env)
                run(["xdotool", "windowfocus", window], local_env)
                run(["xdotool", "key", "ctrl+alt+t"], local_env)
                time.sleep(0.5)  # Let egui position and paint the details window.
                # Details stay open on the right. Click the desktop on the left.
                run(["xdotool", "mousemove", "--window", window, "100", "200", "click", "1"], local_env)
                marker = tmp + "/remote-typing-worked"
                # The mixed case also checks shifted press/unshifted release pairs.
                run(["xdotool", "type", "--clearmodifiers", "--delay", "15",
                     f"printf GUI_OK > {marker}"], local_env)
                run(["xdotool", "key", "Return"], local_env)
                wait_for(lambda: Path(marker).exists(), "remote typing with details open")
                assert Path(marker).read_text() == "GUI_OK"
                # A drag started remotely must continue when it crosses the panel.
                run(["xdotool", "mousemove", "--window", window, "100", "200",
                     "mousedown", "1", "mousemove", "--window", window, "800", "200",
                     "mouseup", "1"], local_env)
                wait_for(
                    lambda: "X=800" in run(["xdotool", "getmouselocation", "--shell"], remote_env),
                    "remote drag crossing the local panel",
                )
                assert client.poll() is None, "Client exited during graphical interaction"
                print("PASS: remote typing and dragging work with the graphical Transfers window open.")
            except Exception:
                log.flush()
                print(Path(tmp + "/test.log").read_text()[-6000:])
                raise
            finally:
                for process in reversed(children):
                    process.terminate()
                for process in reversed(children):
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default="target/debug")
    check(Path(parser.parse_args().bin_dir).resolve())
