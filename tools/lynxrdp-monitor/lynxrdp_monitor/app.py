"""PySide6 viewer for LynxRDP heartbeat reports.

Listens on a UDP port, shows one row per host, and makes the address easy to
copy so it can be pasted straight into an `ssh` or `lynxrdp` command.

`QUdpSocket` is used rather than a socket on a worker thread: it delivers
datagrams through the Qt event loop, so there is no cross-thread handoff and
no locking around the table.
"""

from __future__ import annotations

import argparse
import sys

from PySide6.QtCore import Qt, QTimer, Slot
from PySide6.QtGui import QAction, QGuiApplication, QKeySequence
from PySide6.QtNetwork import QHostAddress, QUdpSocket
from PySide6.QtWidgets import (
    QAbstractItemView,
    QApplication,
    QHeaderView,
    QMainWindow,
    QMenu,
    QMessageBox,
    QPushButton,
    QStatusBar,
    QTableWidget,
    QTableWidgetItem,
    QVBoxLayout,
    QWidget,
)

from .model import DEFAULT_STALE_AFTER, NodeStore, format_age, format_uptime, parse_report

#: Port the viewer listens on unless told otherwise.
DEFAULT_PORT = 9999

#: Columns, in display order.
COLUMNS = ["Node", "IP address", "Port", "Sessions", "Version", "Uptime", "Last seen"]
COL_NODE, COL_IP, COL_PORT, COL_SESSIONS, COL_VERSION, COL_UPTIME, COL_SEEN = range(7)


class MonitorWindow(QMainWindow):
    """The whole application: a table, a copy button and a status line."""

    def __init__(self, bind: str, port: int, stale_after: float) -> None:
        super().__init__()
        self.store = NodeStore(stale_after=stale_after)
        self.setWindowTitle("LynxRDP Monitor")
        self.resize(900, 460)

        self.table = QTableWidget(0, len(COLUMNS))
        self.table.setHorizontalHeaderLabels(COLUMNS)
        self.table.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self.table.setSelectionMode(QAbstractItemView.SelectionMode.SingleSelection)
        self.table.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self.table.verticalHeader().setVisible(False)
        self.table.setAlternatingRowColors(True)
        header = self.table.horizontalHeader()
        header.setSectionResizeMode(COL_NODE, QHeaderView.ResizeMode.Stretch)
        for col in (COL_IP, COL_PORT, COL_SESSIONS, COL_VERSION, COL_UPTIME, COL_SEEN):
            header.setSectionResizeMode(col, QHeaderView.ResizeMode.ResizeToContents)
        # Double-clicking a row is the fastest way to grab an address.
        self.table.itemDoubleClicked.connect(lambda _item: self.copy_selected_ip())
        self.table.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
        self.table.customContextMenuRequested.connect(self._show_context_menu)

        self.copy_button = QPushButton("Copy IP address")
        self.copy_button.setEnabled(False)
        self.copy_button.clicked.connect(self.copy_selected_ip)
        self.table.itemSelectionChanged.connect(self._selection_changed)

        layout = QVBoxLayout()
        layout.addWidget(self.table)
        layout.addWidget(self.copy_button)
        central = QWidget()
        central.setLayout(layout)
        self.setCentralWidget(central)

        self.setStatusBar(QStatusBar())

        # Ctrl+C copies the selected address, which is what any operator will
        # try first. The table is read-only, so nothing else wants that key.
        copy_action = QAction("Copy IP address", self)
        copy_action.setShortcut(QKeySequence.StandardKey.Copy)
        copy_action.triggered.connect(self.copy_selected_ip)
        self.addAction(copy_action)

        self.socket = QUdpSocket(self)
        self.socket.readyRead.connect(self._read_datagrams)
        self._bind(bind, port)

        # "Last seen" has to keep counting up between reports, so redraw on a
        # timer rather than only when a datagram lands.
        self.tick = QTimer(self)
        self.tick.setInterval(1000)
        self.tick.timeout.connect(self.refresh)
        self.tick.start()

    # ---- networking -----------------------------------------------------

    def _bind(self, bind: str, port: int) -> None:
        address = QHostAddress(bind) if bind else QHostAddress.SpecialAddress.Any
        if not self.socket.bind(address, port):
            reason = self.socket.errorString()
            QMessageBox.critical(
                self,
                "Cannot listen",
                f"Could not listen on {bind or '0.0.0.0'}:{port}.\n\n{reason}\n\n"
                "Another program may already have the port.",
            )
            self.listening = ""
            self.statusBar().showMessage(f"not listening: {reason}")
            return
        self.listening = f"{bind or '0.0.0.0'}:{port}"
        self._update_status()

    @Slot()
    def _read_datagrams(self) -> None:
        while self.socket.hasPendingDatagrams():
            datagram = self.socket.receiveDatagram()
            data = bytes(datagram.data())
            sender = datagram.senderAddress().toString()
            # Qt renders IPv4-mapped IPv6 as "::ffff:10.0.0.5"; show the plain
            # form so it matches what the host reports about itself.
            if sender.startswith("::ffff:"):
                sender = sender[len("::ffff:") :]
            report = parse_report(data, source_ip=sender)
            if report is None:
                # Scans and strays are normal on an open UDP port.
                continue
            self.store.update(report)
        self.refresh()

    # ---- display --------------------------------------------------------

    @Slot()
    def refresh(self) -> None:
        """Redraw the table, keeping the selected host selected."""
        selected = self.selected_node()
        rows = self.store.rows()
        self.table.setRowCount(len(rows))
        for row, report in enumerate(rows):
            stale = self.store.is_stale(report)
            values = [
                report.node,
                report.ip,
                str(report.port),
                str(report.sessions),
                report.version,
                format_uptime(report.uptime_secs),
                format_age(self.store.age(report)) + (" (stale)" if stale else ""),
            ]
            for col, text in enumerate(values):
                item = self.table.item(row, col)
                if item is None:
                    item = QTableWidgetItem()
                    self.table.setItem(row, col, item)
                item.setText(text)
                font = item.font()
                font.setItalic(stale)
                item.setFont(font)
            node_item = self.table.item(row, COL_NODE)
            # The name lives in the item's data, not its text: the text may
            # carry a NAT marker, and parsing identity back out of a label is
            # the kind of thing that breaks the first time a name contains an
            # asterisk.
            node_item.setData(Qt.ItemDataRole.UserRole, report.node)
            if report.source_differs:
                # Worth surfacing: normal behind NAT, but also what a spoofed
                # report would look like.
                node_item.setText(f"{report.node} *")
                node_item.setToolTip(
                    f"Reports its address as {report.ip}, but the datagram came "
                    f"from {report.source_ip}. Normal behind NAT."
                )
            else:
                node_item.setToolTip("")
            if selected == report.node:
                self.table.selectRow(row)
        self._update_status()

    def _update_status(self) -> None:
        where = self.listening or "nothing"
        total = len(self.store)
        stale = sum(1 for r in self.store.rows() if self.store.is_stale(r))
        message = f"Listening on {where} - {total} host(s)"
        if stale:
            message += f", {stale} stale"
        self.statusBar().showMessage(message)

    # ---- copying --------------------------------------------------------

    def selected_node(self) -> str | None:
        rows = self.table.selectionModel().selectedRows() if self.table.selectionModel() else []
        if not rows:
            return None
        item = self.table.item(rows[0].row(), COL_NODE)
        if item is None:
            return None
        return item.data(Qt.ItemDataRole.UserRole)

    def selected_ip(self) -> str | None:
        node = self.selected_node()
        if node is None:
            return None
        for report in self.store.rows():
            if report.node == node:
                return report.ip
        return None

    @Slot()
    def copy_selected_ip(self) -> None:
        ip = self.selected_ip()
        if not ip:
            return
        QGuiApplication.clipboard().setText(ip)
        self.statusBar().showMessage(f"Copied {ip}", 2000)

    @Slot()
    def _selection_changed(self) -> None:
        self.copy_button.setEnabled(self.selected_ip() is not None)

    def _show_context_menu(self, pos) -> None:
        if self.selected_ip() is None:
            return
        menu = QMenu(self)
        menu.addAction("Copy IP address", self.copy_selected_ip)
        menu.addAction("Copy ssh command", self._copy_ssh)
        menu.addSeparator()
        menu.addAction("Remove from list", self._forget_selected)
        menu.exec(self.table.viewport().mapToGlobal(pos))

    @Slot()
    def _copy_ssh(self) -> None:
        """Copy a ready-to-paste lynxrdp command for this host."""
        ip = self.selected_ip()
        if not ip:
            return
        text = f"lynxrdp {ip}"
        QGuiApplication.clipboard().setText(text)
        self.statusBar().showMessage(f"Copied: {text}", 2000)

    @Slot()
    def _forget_selected(self) -> None:
        node = self.selected_node()
        if node:
            self.store.forget(node)
            self.refresh()


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="lynxrdp-monitor",
        description="Watch LynxRDP servers reporting in, and copy their addresses.",
    )
    parser.add_argument(
        "--bind",
        default="",
        help="address to listen on (default: every interface)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=DEFAULT_PORT,
        help=f"UDP port to listen on (default: {DEFAULT_PORT})",
    )
    parser.add_argument(
        "--stale-after",
        type=float,
        default=DEFAULT_STALE_AFTER,
        metavar="SECONDS",
        help=f"mark a host stale after this long (default: {DEFAULT_STALE_AFTER:g})",
    )
    args = parser.parse_args(argv)
    if not 1 <= args.port <= 65535:
        parser.error("--port must be between 1 and 65535")
    if args.stale_after <= 0:
        parser.error("--stale-after must be positive")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    app = QApplication(sys.argv[:1])
    window = MonitorWindow(args.bind, args.port, args.stale_after)
    window.show()
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
