#!/usr/bin/env python3
"""Exercise both macOS packaging paths with a real, already-built client."""

import pathlib
import plistlib
import shutil
import subprocess
import sys
import tempfile


ROOT = pathlib.Path(__file__).resolve().parents[1]


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def check_bundle(app):
    # The linker signs the executable, not the completed bundle. This fails
    # if packaging forgets to seal its resources or changes them after signing.
    run("codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app))
    with (app / "Contents/Info.plist").open("rb") as source:
        info = plistlib.load(source)
    assert info["CFBundleIdentifier"] == "io.github.guitar24t.lynxrdp"
    assert info.get("NSLocalNetworkUsageDescription", "").strip(), info
    signature = run(
        "codesign", "--display", "--verbose=2", str(app), capture_output=True, text=True
    ).stderr
    assert "Identifier=io.github.guitar24t.lynxrdp\n" in signature, signature
    run(str(app / "Contents/MacOS/lynxrdp"), "--version")


def main():
    if sys.platform != "darwin":
        raise SystemExit("This check requires macOS (codesign and hdiutil).")
    binary = pathlib.Path(sys.argv[1]).resolve(strict=True)
    # package-client.sh owns and removes stage/. Run the real scripts in an
    # isolated tree so this test cannot touch a developer's build artifacts.
    with tempfile.TemporaryDirectory(prefix="lynxrdp-packaging-") as temp:
        root = pathlib.Path(temp)
        shutil.copytree(ROOT / "packaging", root / "packaging")
        (root / "assets").mkdir()
        shutil.copy2(ROOT / "assets/lynxrdp.icns", root / "assets/lynxrdp.icns")
        for name in ("Cargo.toml", "README.md", "LICENSE"):
            shutil.copy2(ROOT / name, root / name)
        (root / "target/release").mkdir(parents=True)
        shutil.copy2(binary, root / "target/release/lynxrdp")
        run("bash", str(root / "packaging/package-client.sh"),
            "aarch64-apple-darwin", "macos-aarch64", "lynxrdp")
        archive, = (root / "dist").glob("*.tar.gz")
        unpacked = root / "unpacked"
        unpacked.mkdir()
        run("tar", "-xzf", str(archive), "-C", str(unpacked))
        archived_app, = unpacked.glob("*/LynxRDP.app")
        check_bundle(archived_app)

        run("bash", str(root / "packaging/make-app-bundle.sh"),
            str(binary), str(root / "stage"))
        run("bash", str(root / "packaging/make-dmg.sh"),
            str(root / "stage/LynxRDP.app"), str(root / "dist"), "macos-aarch64")
        dmg, = (root / "dist").glob("*.dmg")
        mount = root / "mounted"
        mount.mkdir()
        run("hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", str(mount), str(dmg))
        try:
            check_bundle(mount / "LynxRDP.app")
        finally:
            run("hdiutil", "detach", str(mount))
    print("PASS: archive and disk-image applications have valid bundle signatures")


if __name__ == "__main__":
    main()
