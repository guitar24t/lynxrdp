"""Tests for the parsing and bookkeeping layer.

No Qt is imported here, so these run anywhere, including CI without a display.
"""

import json

import pytest

from lynxrdp_monitor.model import (
    MAX_DATAGRAM,
    NodeStore,
    Report,
    clean_text,
    format_age,
    format_uptime,
    parse_report,
)


def payload(**overrides) -> bytes:
    """A well-formed report, matching what lynxrdpd actually sends."""
    obj = {
        "node": "desk01",
        "ip": "10.0.0.5",
        "port": 3390,
        "version": "LynxRDP/0.1.0",
        "sessions": 2,
        "uptime_secs": 3600,
        "time": 1756900000,
    }
    obj.update(overrides)
    return json.dumps(obj).encode("utf-8")


class TestParsing:
    def test_parses_a_real_server_datagram(self):
        # Captured verbatim from lynxrdpd, so this pins the wire format
        # against the Rust side rather than against my idea of it.
        raw = (
            b'{"node":"test-node","ip":"127.0.0.1","port":33900,'
            b'"version":"LynxRDP/0.1.0","sessions":0,'
            b'"uptime_secs":0,"time":1788463732}'
        )
        r = parse_report(raw, source_ip="127.0.0.1")
        assert r is not None
        assert (r.node, r.ip, r.port) == ("test-node", "127.0.0.1", 33900)
        assert r.version == "LynxRDP/0.1.0"
        assert r.sessions == 0

    def test_round_trips_the_documented_fields(self):
        r = parse_report(payload(), source_ip="10.0.0.5")
        assert r is not None
        assert r.node == "desk01"
        assert r.ip == "10.0.0.5"
        assert r.port == 3390
        assert r.sessions == 2
        assert r.uptime_secs == 3600
        assert not r.source_differs

    def test_notices_when_the_source_address_differs(self):
        # Normal behind NAT, and worth surfacing rather than hiding.
        r = parse_report(payload(), source_ip="203.0.113.9")
        assert r is not None and r.source_differs

    @pytest.mark.parametrize(
        "raw",
        [
            b"",
            b"not json at all",
            b"[]",
            b'"a string"',
            b"123",
            b"null",
            b"{",
            b"\xff\xfe\x00binary",
        ],
        ids=["empty", "garbage", "array", "string", "number", "null", "truncated", "binary"],
    )
    def test_junk_is_ignored_rather_than_raising(self, raw):
        # A UDP port on a network receives scans and strays; none of it may
        # disturb the viewer.
        assert parse_report(raw) is None

    def test_oversized_datagrams_are_refused(self):
        big = json.dumps({"node": "n" * MAX_DATAGRAM, "ip": "1.2.3.4", "port": 1})
        assert parse_report(big.encode()) is None

    @pytest.mark.parametrize(
        "overrides",
        [
            {"node": ""},
            {"node": 42},
            {"ip": ""},
            {"ip": None},
            {"port": 0},
            {"port": 65536},
            {"port": "3390"},
            {"port": True},
        ],
    )
    def test_missing_or_invalid_required_fields_are_refused(self, overrides):
        assert parse_report(payload(**overrides)) is None

    def test_absent_optional_fields_fall_back(self):
        raw = json.dumps({"node": "n", "ip": "1.2.3.4", "port": 3390}).encode()
        r = parse_report(raw)
        assert r is not None
        assert r.sessions == 0 and r.uptime_secs == 0 and r.version == ""

    def test_control_characters_are_stripped_from_display_text(self):
        # A hostile sender must not be able to break the table apart or slip
        # terminal escapes into anything that logs these strings.
        r = parse_report(payload(node="bad\nname\x1b[31m\x00here"))
        assert r is not None
        assert "\n" not in r.node
        assert "\x1b" not in r.node
        assert "\x00" not in r.node
        assert "badname" in r.node.replace("[31m", "")

    def test_long_fields_are_clamped(self):
        # Long enough to be clamped, short enough that the datagram cap (a
        # separate defence, covered above) is not what rejects it.
        r = parse_report(payload(node="n" * 600, version="v" * 600))
        assert r is not None
        assert len(r.node) == 256
        assert len(r.version) == 64

    def test_non_ascii_names_survive(self):
        r = parse_report(payload(node="büro-01"))
        assert r is not None and r.node == "büro-01"


class TestCleanText:
    def test_non_strings_become_empty(self):
        for value in (None, 5, [], {}, True):
            assert clean_text(value) == ""

    def test_printable_text_is_untouched(self):
        assert clean_text("desk-01.example.org") == "desk-01.example.org"


class TestNodeStore:
    def test_reports_replace_rather_than_accumulate(self):
        store = NodeStore()
        assert store.update(parse_report(payload()))
        assert not store.update(parse_report(payload(sessions=5)))
        assert len(store) == 1
        assert store.rows()[0].sessions == 5

    def test_a_host_that_changes_address_moves_rather_than_duplicating(self):
        store = NodeStore()
        store.update(parse_report(payload(ip="10.0.0.5")))
        store.update(parse_report(payload(ip="10.0.0.9")))
        assert len(store) == 1
        assert store.rows()[0].ip == "10.0.0.9"

    def test_rows_are_ordered_by_name_case_insensitively(self):
        store = NodeStore()
        for name in ("zeta", "Alpha", "middle"):
            store.update(parse_report(payload(node=name)))
        assert [r.node for r in store.rows()] == ["Alpha", "middle", "zeta"]

    def test_staleness_uses_our_clock_not_the_senders(self):
        # The sender's clock could be wrong or hostile; only arrival matters.
        store = NodeStore(stale_after=100.0)
        r = parse_report(payload(time=0), now=1000.0)
        assert not store.is_stale(r, now=1050.0)
        assert store.is_stale(r, now=1200.0)
        assert store.age(r, now=1050.0) == pytest.approx(50.0)

    def test_age_never_goes_negative_on_a_clock_step(self):
        store = NodeStore()
        r = parse_report(payload(), now=2000.0)
        assert store.age(r, now=1000.0) == 0.0

    def test_forget_and_clear(self):
        store = NodeStore()
        store.update(parse_report(payload(node="a")))
        store.update(parse_report(payload(node="b")))
        store.forget("a")
        assert [r.node for r in store.rows()] == ["b"]
        store.clear()
        assert len(store) == 0


class TestFormatting:
    @pytest.mark.parametrize(
        ("seconds", "expected"),
        [(0, "0s"), (5, "5s"), (59, "59s"), (60, "1m"), (3599, "59m"),
         (3600, "1h"), (86399, "23h"), (86400, "1d"), (-5, "0s")],
    )
    def test_age_formatting(self, seconds, expected):
        assert format_age(seconds) == expected

    @pytest.mark.parametrize(
        ("seconds", "expected"),
        [(0, "0m"), (90, "1m"), (3600, "1h 0m"), (7260, "2h 1m"),
         (86400, "1d 0h"), (90000, "1d 1h")],
    )
    def test_uptime_formatting(self, seconds, expected):
        assert format_uptime(seconds) == expected
