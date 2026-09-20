#!/usr/bin/env python3
"""Exercise the shipped desktop launcher without starting a real desktop.

Only desktop executables are replaced: the real startwm.sh selects and execs
them, and they report its arguments/environment. Profiles and session overrides
belong to temporary child-process homes, never the developer's real home.
Requires only Python's standard library and a POSIX shell.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


STARTWM = Path(__file__).resolve().parents[1] / "packaging" / "startwm.sh"
PROBE = """import json, os, sys
from pathlib import Path
print(json.dumps({
    "command": Path(sys.argv[0]).name,
    "args": sys.argv[1:],
    "desktop": os.environ.get("XDG_CURRENT_DESKTOP"),
    "session": os.environ.get("XDG_SESSION_DESKTOP"),
    "mode": os.environ.get("GNOME_SHELL_SESSION_MODE"),
    "type": os.environ.get("XDG_SESSION_TYPE"),
    "wayland": os.environ.get("WAYLAND_DISPLAY"),
}))
"""


class DesktopSelection(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="lynxrdp-startwm-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.bin_dir = self.root / "bin"
        self.user_dir = self.root / "user"
        self.data_dir = self.root / "share"
        for directory in (self.bin_dir, self.user_dir, self.data_dir):
            directory.mkdir()
        # /etc/profile can reset PATH. This child-only profile restores the
        # fixture executables so a real installed desktop cannot be selected.
        (self.user_dir / ".profile").write_text(
            'export PATH="$LYNXRDP_TEST_PATH"\n'
            'export XDG_DATA_DIRS="$LYNXRDP_TEST_DATA_DIRS"\n'
        )
        cat = shutil.which("cat")
        self.assertIsNotNone(cat)
        (self.bin_dir / "cat").symlink_to(cat)
        self.env = {
            "HOME": str(self.user_dir),
            "PATH": os.defpath,
            "LYNXRDP_TEST_PATH": str(self.bin_dir),
            "LYNXRDP_TEST_DATA_DIRS": str(self.data_dir),
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent/lynxrdp-test",
            "WAYLAND_DISPLAY": "wayland-test",
        }

    def executable(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        # An absolute interpreter keeps PATH limited to fixture desktops.
        path.write_text(f"#!{sys.executable}\n" + PROBE)
        path.chmod(0o755)
        return path

    def desktop(self, name):
        return self.executable(self.bin_dir / name)

    def ubuntu_session(self, data_dir=None):
        path = (data_dir or self.data_dir) / "gnome-session/sessions/ubuntu.session"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("[GNOME Session]\nName=Ubuntu\n")

    def launch(self):
        result = subprocess.run(
            ["/bin/sh", str(STARTWM)], env=self.env, cwd=self.root,
            text=True, capture_output=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        record = json.loads(result.stdout)
        self.assertEqual(record["type"], "x11")
        self.assertIsNone(record["wayland"])
        return record

    def assert_ubuntu(self, record):
        self.assertEqual(record["command"], "gnome-session")
        self.assertEqual(record["args"], ["--session=ubuntu"])
        self.assertEqual(record["desktop"], "ubuntu:GNOME")
        self.assertEqual(record["session"], "ubuntu")
        self.assertEqual(record["mode"], "ubuntu")

    def test_ubuntu_uses_its_session_identity_and_shell_mode(self):
        self.desktop("gnome-session")
        self.ubuntu_session()
        self.assert_ubuntu(self.launch())

    def test_ubuntu_session_is_found_in_later_data_dirs_without_glob_expansion(self):
        self.desktop("gnome-session")
        vendor = self.root / "vendor data [literal]"
        self.ubuntu_session(vendor)
        (self.root / "vendor data l").mkdir()
        self.env["LYNXRDP_TEST_DATA_DIRS"] = f"{self.data_dir}:{vendor}"
        self.assert_ubuntu(self.launch())

    def test_relative_data_dirs_cannot_select_an_ubuntu_session(self):
        self.desktop("gnome-session")
        self.ubuntu_session(self.root / "relative")
        self.env["LYNXRDP_TEST_DATA_DIRS"] = "relative"
        self.assertEqual(self.launch()["desktop"], "GNOME")

    def test_generic_gnome_on_rhel_does_not_request_an_ubuntu_session(self):
        self.desktop("gnome-session")
        record = self.launch()
        self.assertEqual(record["command"], "gnome-session")
        self.assertEqual(record["args"], [])
        self.assertEqual(record["desktop"], "GNOME")
        self.assertEqual(record["session"], "gnome")
        self.assertIsNone(record["mode"])

    def test_wallpapers_alone_do_not_select_an_uninstalled_ubuntu_session(self):
        self.desktop("gnome-session")
        (self.data_dir / "backgrounds").mkdir()
        (self.data_dir / "backgrounds/ubuntu-wallpaper-d.png").touch()
        self.assertEqual(self.launch()["desktop"], "GNOME")

    def test_xfce_keeps_priority_even_when_ubuntu_is_installed(self):
        self.desktop("startxfce4")
        self.desktop("gnome-session")
        self.ubuntu_session()
        record = self.launch()
        self.assertEqual(record["command"], "startxfce4")
        self.assertEqual(record["args"], [])
        self.assertEqual(record["desktop"], "XFCE")
        self.assertEqual(record["session"], "xfce")
        self.assertIsNone(record["mode"])

    def overrides(self):
        self.desktop("gnome-session")
        self.ubuntu_session()
        self.desktop("chosen-desktop")
        self.env.update(
            XDG_CURRENT_DESKTOP="Chosen", XDG_SESSION_DESKTOP="chosen",
            GNOME_SHELL_SESSION_MODE="chosen",
            LYNXRDP_DESKTOP="chosen-desktop --from=environment",
        )

    def assert_override(self, record, command, args):
        self.assertEqual(record["command"], command)
        self.assertEqual(record["args"], args)
        self.assertEqual(record["desktop"], "Chosen")
        self.assertEqual(record["session"], "chosen")
        self.assertEqual(record["mode"], "chosen")

    def test_executable_user_session_keeps_first_priority(self):
        self.overrides()
        self.executable(self.user_dir / ".lynxrdp/session")
        self.executable(self.user_dir / ".xsession")
        self.assert_override(self.launch(), "session", [])

    def test_text_user_session_keeps_priority_over_xsession(self):
        self.overrides()
        path = self.user_dir / ".lynxrdp/session"
        path.parent.mkdir()
        path.write_text("chosen-desktop --from=user-session\n")
        self.executable(self.user_dir / ".xsession")
        self.assert_override(self.launch(), "chosen-desktop", ["--from=user-session"])

    def test_xsession_keeps_priority_over_environment_override(self):
        self.overrides()
        self.executable(self.user_dir / ".xsession")
        self.assert_override(self.launch(), ".xsession", [])

    def test_environment_override_is_not_replaced_by_ubuntu_autodetection(self):
        self.overrides()
        self.assert_override(self.launch(), "chosen-desktop", ["--from=environment"])

    def test_xterm_is_still_the_last_fallback(self):
        self.desktop("xterm")
        self.assertEqual(self.launch()["command"], "xterm")


class InstallerAgreesWithTheLauncher(unittest.TestCase):
    """install.sh refuses a host with no desktop, and decides that with its
    own copy of the launcher's candidate list, because it runs before the
    package that carries startwm.sh exists on the host. A desktop known to
    one and not the other would be refused for no reason, or waved through
    to a bare xterm."""

    def test_the_two_candidate_lists_are_the_same(self):
        import re
        launcher = re.search(r"for candidate in \\\n(.*?); do", STARTWM.read_text(), re.S)
        self.assertIsNotNone(launcher, "startwm.sh candidate loop not found")
        launcher_list = re.findall(r'"([^"]+)"', launcher.group(1))
        installer = re.search(r'^DESKTOPS="([^"]+)"', (STARTWM.parent / "install.sh").read_text(), re.M)
        self.assertIsNotNone(installer, "install.sh DESKTOPS not found")
        self.assertGreater(len(launcher_list), 5)
        self.assertEqual(installer.group(1).split(), launcher_list)


if __name__ == "__main__":
    unittest.main(verbosity=2)
