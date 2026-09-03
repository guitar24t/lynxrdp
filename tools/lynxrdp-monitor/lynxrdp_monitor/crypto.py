"""Unsealing monitoring reports.

The mirror of `crates/server/src/reporting/seal.rs`. The constants and the
derivation below MUST match that file exactly; both sides assert the same
known-answer key, so a change to one without the other fails a test rather
than silently going quiet in the field.

What this is, and what it is not
--------------------------------

The key is derived from constants written in this file and compiled into
`lynxrdpd`. Anyone who can read the source, or run `strings` on the binary,
can recover it and then decrypt or forge any report. This is **obfuscation,
not confidentiality**: it stops a packet capture from reading as a list of
hostnames and addresses, which is what it was asked to do, and it stops
nothing else. See the project's SECURITY.md.

Wire format
-----------

    magic   4 bytes  b"LXR1"
    version 1 byte   FORMAT_VERSION
    nonce  12 bytes  random per datagram
    body    n bytes  ChaCha20-Poly1305 ciphertext, 16-byte tag included

Magic and version are authenticated as associated data, so altering either
fails the tag rather than changing how the body is read.
"""

from __future__ import annotations

import hashlib

from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305

#: Marks a datagram as ours. Must match seal.rs MAGIC.
MAGIC = b"LXR1"

#: Must match seal.rs FORMAT_VERSION.
FORMAT_VERSION = 1

NONCE_LEN = 12
TAG_LEN = 16
HEADER_LEN = len(MAGIC) + 1 + NONCE_LEN

#: Must match seal.rs KEY_MATERIAL.
_KEY_MATERIAL = b"lynxrdp-monitor-report-key-v1"

#: Must match seal.rs SALT.
_SALT = b"lynxrdp.reporting.salt.v1"

#: Associated data: the header bytes before the nonce.
_AAD = MAGIC + bytes([FORMAT_VERSION])


def derive_key() -> bytes:
    """Derive the datagram key: ``SHA-256(SALT || 0x00 || KEY_MATERIAL)``.

    The zero separator matches seal.rs; without it a different split of the
    same concatenated bytes would derive the same key.
    """
    return hashlib.sha256(_SALT + b"\x00" + _KEY_MATERIAL).digest()


#: Derived once. Deriving per datagram would be wasteful and no safer.
_KEY = derive_key()
_CIPHER = ChaCha20Poly1305(_KEY)


def unseal(datagram: bytes) -> bytes | None:
    """Return the plaintext of `datagram`, or None if it is not a valid report.

    None rather than an exception: a UDP port receives scans, strays and
    outright garbage, and none of that should reach the caller as an error to
    handle. A wrong key, a forged packet and a random port scan all look the
    same here, which is the correct outcome for all three.
    """
    if len(datagram) < HEADER_LEN + TAG_LEN:
        return None
    if datagram[: len(MAGIC)] != MAGIC:
        return None
    if datagram[len(MAGIC)] != FORMAT_VERSION:
        return None
    nonce = datagram[len(MAGIC) + 1 : HEADER_LEN]
    try:
        return _CIPHER.decrypt(nonce, datagram[HEADER_LEN:], _AAD)
    except InvalidTag:
        return None


def seal(plaintext: bytes, nonce: bytes | None = None) -> bytes:
    """Produce a datagram the viewer will accept.

    Only the tests need this -- the viewer never sends -- but having both
    directions here lets the format be exercised without a running server.
    """
    if nonce is None:
        import os

        nonce = os.urandom(NONCE_LEN)
    if len(nonce) != NONCE_LEN:
        raise ValueError(f"nonce must be {NONCE_LEN} bytes")
    body = _CIPHER.encrypt(nonce, plaintext, _AAD)
    return MAGIC + bytes([FORMAT_VERSION]) + nonce + body
