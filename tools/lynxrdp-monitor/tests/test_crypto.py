"""Tests for unsealing reports, and for agreement with the Rust side."""

import pytest

from lynxrdp_monitor.crypto import (
    FORMAT_VERSION,
    HEADER_LEN,
    MAGIC,
    NONCE_LEN,
    TAG_LEN,
    derive_key,
    seal,
    unseal,
)


def test_key_derivation_matches_the_rust_side():
    # The same known answer is asserted in crates/server/src/reporting/seal.rs.
    # If these two ever drift, every deployed viewer goes quiet at once, so it
    # is worth failing loudly here instead.
    assert (
        derive_key().hex()
        == "f34c877322e249221e027336b9945aee555e2bd7a786f81753128971e27dddd8"
    )


def test_roundtrips():
    msg = b'{"node":"desk01","ip":"10.0.0.5"}'
    assert unseal(seal(msg)) == msg


def test_a_real_datagram_from_lynxrdpd_opens():
    # Captured from a running lynxrdpd, so this pins the format against the
    # Rust implementation rather than against this file's own idea of it.
    # Regenerate with: cargo run --bin lynxrdpd -- --config ... (see README)
    import binascii

    raw = binascii.unhexlify(RUST_DATAGRAM_HEX)
    plaintext = unseal(raw)
    assert plaintext is not None, "a datagram from the real server did not open"
    assert b'"node"' in plaintext and b'"ip"' in plaintext


def test_the_payload_is_not_readable_on_the_wire():
    sealed = seal(b'{"node":"secret-host"}')
    assert b"secret-host" not in sealed


def test_every_datagram_uses_a_fresh_nonce():
    a, b = seal(b"same"), seal(b"same")
    assert a[len(MAGIC) + 1 : HEADER_LEN] != b[len(MAGIC) + 1 : HEADER_LEN]
    assert a != b


def test_tampering_is_rejected():
    sealed = seal(b"hello")
    for index in range(len(sealed)):
        bad = bytearray(sealed)
        bad[index] ^= 0x01
        assert unseal(bytes(bad)) is None, f"a flipped bit at {index} was accepted"


def test_truncation_is_rejected():
    sealed = seal(b"hello")
    for cut in range(len(sealed)):
        assert unseal(sealed[:cut]) is None, f"accepted a {cut}-byte prefix"


@pytest.mark.parametrize(
    "bad",
    [b"", b"short", MAGIC, bytes(64), b"XXXX\x01" + bytes(28)],
    ids=["empty", "short", "magic-only", "zeros", "wrong-magic"],
)
def test_junk_is_rejected_without_raising(bad):
    assert unseal(bad) is None


def test_a_future_version_is_refused_rather_than_misread():
    sealed = bytearray(seal(b"hello"))
    sealed[len(MAGIC)] = FORMAT_VERSION + 1
    assert unseal(bytes(sealed)) is None


def test_overhead_is_fixed_and_known():
    assert len(seal(b"")) == HEADER_LEN + TAG_LEN
    assert HEADER_LEN == len(MAGIC) + 1 + NONCE_LEN


def test_a_bad_nonce_length_is_a_programming_error():
    with pytest.raises(ValueError):
        seal(b"x", nonce=b"tooshort")


# A datagram produced by a real lynxrdpd build, hex encoded. Captured with the
# helper documented in the monitor README so it can be regenerated if the wire
# format is ever revised.
RUST_DATAGRAM_HEX = (
    "4c5852310164e433b5708e56d5a44c75babae2aa97020ba973acae658bb58753"
    "82fed1246107a1099b1a5cd2d522e363a1f7e386e0dfb01d31a0a3fea1b7a900"
    "504a9bb62c4e266d7229dc5423f73816b78710a502653445cd45a4b612525ecb"
    "3278868eb8ee37083e26190a04295ae93ab5804591b771a812286bb44c181770"
    "c32988d681587be0a2b91e405682a571fa40f9c4988ca5fe8a5a9990"
)
