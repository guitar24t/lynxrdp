"""Tests that drive the real Qt window, headless.

These need PySide6 and run under the "offscreen" platform, so they work in CI
without a display. They are skipped if PySide6 is missing, because the
parsing layer in test_model.py is what matters most and should not be held
hostage to a heavy GUI dependency.
"""

from __future__ import annotations

import json
import os
import socket

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

pytest.importorskip("PySide6", reason="PySide6 is not installed")

from PySide6.QtCore import QCoreApplication, QEventLoop, QTimer  # noqa: E402
from PySide6.QtGui import QGuiApplication  # noqa: E402
from PySide6.QtWidgets import QApplication  # noqa: E402

from lynxrdp_monitor.app import COL_IP, COL_NODE, MonitorWindow  # noqa: E402
from lynxrdp_monitor.crypto import seal  # noqa: E402


@pytest.fixture(scope="session")
def qapp():
    app = QApplication.instance() or QApplication([])
    yield app


@pytest.fixture
def window(qapp):
    # Port 0 lets the OS pick a free one, so tests never collide with a real
    # monitor or with each other.
    w = MonitorWindow("127.0.0.1", 0, stale_after=150.0)
    yield w
    w.socket.close()
    w.tick.stop()


def send(window: MonitorWindow, obj: dict) -> None:
    """Send one sealed datagram to the window's socket, as a server would."""
    send_raw(window, seal(json.dumps(obj).encode()))


def send_raw(window: MonitorWindow, data: bytes) -> None:
    """Send raw bytes, for the cases that are deliberately not valid."""
    port = window.socket.localPort()
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.sendto(data, ("127.0.0.1", port))
    finally:
        sock.close()
    spin(150)


def spin(ms: int) -> None:
    """Run the Qt event loop briefly so queued datagrams are delivered."""
    loop = QEventLoop()
    QTimer.singleShot(ms, loop.quit)
    loop.exec()
    QCoreApplication.processEvents()


def report(**overrides) -> dict:
    obj = {
        "node": "desk01",
        "ip": "10.0.0.5",
        "port": 3390,
        "version": "LynxRDP/0.1.0",
        "sessions": 1,
        "uptime_secs": 120,
        "time": 1756900000,
    }
    obj.update(overrides)
    return obj


def test_binds_and_reports_where_it_is_listening(window):
    assert window.socket.localPort() != 0
    assert "Listening on 127.0.0.1" in window.statusBar().currentMessage()


def test_a_report_appears_as_a_row(window):
    send(window, report(ip="127.0.0.1"))
    assert window.table.rowCount() == 1
    assert window.table.item(0, COL_NODE).text() == "desk01"
    assert window.table.item(0, COL_IP).text() == "127.0.0.1"


def test_repeat_reports_update_in_place(window):
    send(window, report(sessions=1))
    send(window, report(sessions=4))
    assert window.table.rowCount() == 1
    assert window.store.rows()[0].sessions == 4


def test_several_hosts_each_get_a_row(window):
    send(window, report(node="alpha", ip="127.0.0.1"))
    send(window, report(node="beta", ip="127.0.0.1"))
    assert window.table.rowCount() == 2
    assert [window.table.item(r, COL_NODE).text() for r in range(2)] == ["alpha", "beta"]


def test_malformed_datagrams_do_not_disturb_the_table(window):
    send(window, report(ip="127.0.0.1"))
    for junk in (b"", b"garbage", b"[]", b"\xff\xfe\x00", b'{"node":""}'):
        send_raw(window, junk)
    # The good row survives and nothing was added.
    assert window.table.rowCount() == 1
    assert window.table.item(0, COL_NODE).text() == "desk01"


def test_unsealed_plaintext_is_ignored(window):
    # Reports are sealed now, so a plaintext JSON report -- an old server, or
    # someone guessing the format -- must not appear.
    send_raw(window, json.dumps(report(node="plaintext-host")).encode())
    assert window.table.rowCount() == 0


def test_a_report_sealed_with_the_wrong_key_is_ignored(window):
    # Same shape, wrong key: the tag check must reject it.
    from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305

    from lynxrdp_monitor.crypto import FORMAT_VERSION, MAGIC, NONCE_LEN

    wrong = ChaCha20Poly1305(bytes(32))
    nonce = bytes(NONCE_LEN)
    body = wrong.encrypt(nonce, json.dumps(report()).encode(), MAGIC + bytes([FORMAT_VERSION]))
    send_raw(window, MAGIC + bytes([FORMAT_VERSION]) + nonce + body)
    assert window.table.rowCount() == 0


def test_copying_puts_the_address_on_the_clipboard(window):
    send(window, report(ip="192.0.2.77"))
    window.table.selectRow(0)
    assert window.selected_ip() == "192.0.2.77"
    window.copy_selected_ip()
    assert QGuiApplication.clipboard().text() == "192.0.2.77"


def test_copy_button_is_disabled_until_a_row_is_chosen(window):
    assert not window.copy_button.isEnabled()
    send(window, report())
    window.table.selectRow(0)
    assert window.copy_button.isEnabled()


def test_copying_an_ssh_command_is_ready_to_paste(window):
    send(window, report(ip="192.0.2.77"))
    window.table.selectRow(0)
    window._copy_ssh()
    assert QGuiApplication.clipboard().text() == "lynxrdp 192.0.2.77"


def test_a_nat_mismatch_is_marked_and_still_copies_the_reported_address(window):
    # The datagram comes from 127.0.0.1 but claims 10.0.0.5, which is what a
    # host behind NAT looks like.
    send(window, report(ip="10.0.0.5"))
    assert window.table.item(0, COL_NODE).text().endswith(" *")
    assert window.table.item(0, COL_NODE).toolTip()
    window.table.selectRow(0)
    # The marker must not leak into what gets copied.
    assert window.selected_ip() == "10.0.0.5"


def test_stale_hosts_are_marked_without_being_dropped(window):
    send(window, report())
    window.store.stale_after = -1.0  # everything is now stale
    window.refresh()
    assert window.table.rowCount() == 1
    assert "stale" in window.table.item(0, 6).text()
    assert window.table.item(0, COL_NODE).font().italic()


def test_removing_a_host_takes_it_out_of_the_table(window):
    send(window, report(node="gone"))
    window.table.selectRow(0)
    window._forget_selected()
    assert window.table.rowCount() == 0
