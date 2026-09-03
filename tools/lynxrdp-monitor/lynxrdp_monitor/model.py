"""Parsing and bookkeeping for LynxRDP heartbeat reports.

Deliberately free of Qt so it can be tested without a display, and because
this is the part that touches untrusted input: every datagram arrives from
the network from whoever cares to send one. Nothing here trusts its input.
"""

from __future__ import annotations

import json
import time as _time
from dataclasses import dataclass, field

#: Longest datagram we will even look at. The server caps its own reports far
#: below this; anything larger is not ours.
MAX_DATAGRAM = 4096

#: Longest string we will keep from a report, per field. Bounds what a hostile
#: sender can put in the table.
MAX_FIELD = 256

#: A node not heard from for this long is shown as stale.
DEFAULT_STALE_AFTER = 150.0


def clean_text(value: object, limit: int = MAX_FIELD) -> str:
    """Return `value` as a single-line string safe to put in a widget.

    Control characters are dropped rather than escaped: they have no business
    in a hostname, and letting them through would let a sender break up the
    table display or smuggle terminal escapes into logs.
    """
    if not isinstance(value, str):
        return ""
    out = "".join(ch for ch in value if ch.isprintable())
    return out[:limit]


def _as_int(value: object, low: int, high: int) -> int | None:
    """Coerce to an int within bounds, or None. Rejects bools and floats."""
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    return value if low <= value <= high else None


@dataclass
class Report:
    """One heartbeat, already validated."""

    node: str
    ip: str
    port: int
    version: str
    sessions: int
    uptime_secs: int
    time: int
    #: Address the datagram actually came from, which is not necessarily the
    #: address inside it -- they differ across NAT, and a mismatch is worth
    #: showing rather than hiding.
    source_ip: str = ""
    #: Local clock when we received it. Staleness is measured against this,
    #: never against the sender's clock, which we have no reason to trust.
    received_at: float = field(default_factory=_time.time)

    @property
    def source_differs(self) -> bool:
        return bool(self.source_ip) and self.source_ip != self.ip


def parse_report(data: bytes, source_ip: str = "", now: float | None = None) -> Report | None:
    """Turn one datagram into a `Report`, or None if it is not one.

    Returning None rather than raising is deliberate: a monitoring viewer
    sitting on a UDP port will receive scans, stray traffic and malformed
    packets, and none of that should disturb it.
    """
    if not data or len(data) > MAX_DATAGRAM:
        return None
    try:
        obj = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return None
    if not isinstance(obj, dict):
        return None

    node = clean_text(obj.get("node"))
    ip = clean_text(obj.get("ip"), limit=64)
    if not node or not ip:
        return None

    port = _as_int(obj.get("port"), 1, 65535)
    if port is None:
        return None

    return Report(
        node=node,
        ip=ip,
        port=port,
        version=clean_text(obj.get("version"), limit=64),
        sessions=_as_int(obj.get("sessions"), 0, 1_000_000) or 0,
        uptime_secs=_as_int(obj.get("uptime_secs"), 0, 2**63 - 1) or 0,
        time=_as_int(obj.get("time"), 0, 2**63 - 1) or 0,
        source_ip=clean_text(source_ip, limit=64),
        received_at=_time.time() if now is None else now,
    )


class NodeStore:
    """The set of hosts we have heard from, newest report per host.

    Keyed by node name: a machine that changes address should move, not
    appear twice. Two machines sharing a name will collide, which is a naming
    problem worth noticing rather than papering over.
    """

    def __init__(self, stale_after: float = DEFAULT_STALE_AFTER) -> None:
        self.stale_after = stale_after
        self._by_node: dict[str, Report] = {}

    def update(self, report: Report) -> bool:
        """Record a report. Returns True if this host was not already known."""
        new = report.node not in self._by_node
        self._by_node[report.node] = report
        return new

    def rows(self) -> list[Report]:
        """Every known host, ordered by name so the table does not jump."""
        return sorted(self._by_node.values(), key=lambda r: r.node.lower())

    def is_stale(self, report: Report, now: float | None = None) -> bool:
        now = _time.time() if now is None else now
        return (now - report.received_at) > self.stale_after

    def age(self, report: Report, now: float | None = None) -> float:
        now = _time.time() if now is None else now
        return max(0.0, now - report.received_at)

    def forget(self, node: str) -> None:
        self._by_node.pop(node, None)

    def clear(self) -> None:
        self._by_node.clear()

    def __len__(self) -> int:
        return len(self._by_node)


def format_age(seconds: float) -> str:
    """Short human form for 'last seen', e.g. '4s', '3m', '2h', '5d'."""
    seconds = int(max(0.0, seconds))
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m"
    if seconds < 86400:
        return f"{seconds // 3600}h"
    return f"{seconds // 86400}d"


def format_uptime(seconds: int) -> str:
    """Uptime as days/hours/minutes, which is how an operator reads it."""
    seconds = max(0, int(seconds))
    days, rem = divmod(seconds, 86400)
    hours, rem = divmod(rem, 3600)
    minutes = rem // 60
    if days:
        return f"{days}d {hours}h"
    if hours:
        return f"{hours}h {minutes}m"
    return f"{minutes}m"
