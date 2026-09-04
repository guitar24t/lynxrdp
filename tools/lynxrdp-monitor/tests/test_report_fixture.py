"""The report payload, pinned against the Rust server.

`test_crypto.py` pins the two implementations to each other on the sealing
*key*, and holds one captured datagram to pin the framing. Neither pins what is
*inside* the envelope, and that is the half that breaks quietly: rename `node`
to `hostname` in `Report::to_json` on the Rust side and every test in this
repository still passes, while `parse_report` starts returning None for every
real report and every deployed viewer shows an empty table. No error, no log
line, nothing to notice until somebody wonders why the monitoring has been
still for a month.

So one sealed datagram is committed, and both sides open it: the Rust suite
asserts its plaintext byte for byte against `Report::to_json`, and this file
asserts the fields the viewer actually reads out of the same bytes. A change on
either side that is not made on the other fails here or there.

The fixture deliberately lives in the *Rust* tree, at
`crates/server/tests/fixtures/report-v1.hex`, and this file reaches across the
repository for it. Do not "fix" that by copying it in here: two copies is
precisely the arrangement that lets the two sides drift, which is the whole
failure this exists to catch. The fixture's own header says the same thing at
greater length.
"""

from __future__ import annotations

import json
from pathlib import Path

from lynxrdp_monitor.crypto import FORMAT_VERSION, MAGIC, unseal
from lynxrdp_monitor.model import parse_report

#: tests -> lynxrdp-monitor -> tools -> the repository root.
FIXTURE = (
    Path(__file__).resolve().parents[3]
    / "crates"
    / "server"
    / "tests"
    / "fixtures"
    / "report-v1.hex"
)

#: What the fixture's plaintext says, field for field. Compared as a whole
#: object rather than field by field so that an *added* field trips this too:
#: the viewer would ignore a new key, but a wire change nobody looked at from
#: this end is exactly the thing worth stopping to read.
EXPECTED = {
    "node": "desk01",
    "ip": "10.0.0.5",
    "port": 3390,
    "version": "LynxRDP/0.1.0",
    "sessions": 2,
    "uptime_secs": 3600,
    "time": 1756900000,
}


def read_fixture() -> bytes:
    """The committed datagram, with the file's comment lines stripped."""
    assert FIXTURE.is_file(), (
        f"{FIXTURE} is missing. It is shared with the Rust test suite on "
        "purpose and lives in that tree; this suite needs a full checkout of "
        "the repository, not just tools/lynxrdp-monitor."
    )
    digits = "".join(
        line.strip()
        for line in FIXTURE.read_text().splitlines()
        if not line.lstrip().startswith("#")
    )
    return bytes.fromhex(digits)


def read_fixture_plaintext() -> bytes:
    """The fixture, unsealed. Fails loudly rather than handing on None."""
    plaintext = unseal(read_fixture())
    assert plaintext is not None, "the committed fixture no longer opens"
    return plaintext


def test_the_fixture_is_one_of_our_datagrams():
    raw = read_fixture()
    assert raw[: len(MAGIC)] == MAGIC
    assert raw[len(MAGIC)] == FORMAT_VERSION


def test_the_fixture_opens_with_the_shared_key():
    # If this fails and test_key_derivation_matches_the_rust_side still passes,
    # the key is fine and the *format* moved.
    assert unseal(read_fixture()) is not None


def test_the_payload_carries_exactly_the_documented_fields():
    plaintext = unseal(read_fixture())
    assert plaintext is not None
    assert json.loads(plaintext) == EXPECTED, (
        "the server's report payload changed. Update this file and the fixture "
        "together, and remember that every viewer already deployed reads the "
        "old shape."
    )


def test_the_viewer_parses_the_fixture_into_a_row():
    # The end an operator sees. parse_report() is what fills the table, and it
    # is where a renamed field turns into an empty window rather than an error.
    report = parse_report(read_fixture_plaintext(), source_ip="10.0.0.5", now=1756900001.0)
    assert report is not None, "the viewer would show nothing for this report"
    assert report.node == EXPECTED["node"]
    assert report.ip == EXPECTED["ip"]
    assert report.port == EXPECTED["port"]
    assert report.version == EXPECTED["version"]
    assert report.sessions == EXPECTED["sessions"]
    assert report.uptime_secs == EXPECTED["uptime_secs"]
    assert report.time == EXPECTED["time"]
    assert report.source_ip == "10.0.0.5"
    assert not report.source_differs
    assert report.received_at == 1756900001.0


def test_a_renamed_field_blanks_the_viewer():
    # Why the fixture exists, demonstrated rather than asserted in a comment:
    # one renamed key and the row disappears, with nothing raised anywhere.
    obj = json.loads(read_fixture_plaintext())
    obj["hostname"] = obj.pop("node")
    assert parse_report(json.dumps(obj).encode()) is None
