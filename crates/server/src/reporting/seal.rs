//! Obfuscation of monitoring reports.
//!
//! # What this is, and what it is not
//!
//! The key below is compiled into `lynxrdpd` and written in this file, which
//! is public. Anyone who can read the source, or run `strings` on the binary,
//! can recover it and then decrypt or forge any report. This is **not**
//! confidentiality in the cryptographic sense and must not be relied on as
//! though it were.
//!
//! What it does buy: reports no longer sit on the wire as readable JSON, so a
//! packet capture, an IDS log or a curious colleague's `tcpdump` does not hand
//! over an inventory of hostnames and addresses at a glance. That is the
//! threat this was asked to address, and it addresses it.
//!
//! If reports ever need to survive an attacker who has the software, this has
//! to become a per-deployment key that is not in the repository. The wire
//! format carries a version byte so that change does not have to be a
//! flag day.
//!
//! # Wire format
//!
//! ```text
//! magic   4 bytes  "LXR1"
//! version 1 byte   FORMAT_VERSION
//! nonce  12 bytes  random per datagram
//! body    n bytes  ChaCha20-Poly1305 ciphertext, 16-byte tag included
//! ```
//!
//! The magic and version are authenticated as associated data, so flipping
//! either invalidates the tag rather than silently changing how the body is
//! read.

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use sha2::{Digest, Sha256};

/// Marks a datagram as ours, so the viewer can dismiss strays cheaply.
pub const MAGIC: &[u8; 4] = b"LXR1";

/// Bumped if the format or the key derivation ever changes.
pub const FORMAT_VERSION: u8 = 1;

/// Bytes before the ciphertext: magic, version, nonce.
pub const HEADER_LEN: usize = 4 + 1 + NONCE_LEN;

/// ChaCha20-Poly1305 nonce length.
pub const NONCE_LEN: usize = 12;

/// Poly1305 tag length, included in the ciphertext by the AEAD.
pub const TAG_LEN: usize = 16;

/// Input keying material, baked in deliberately. See the module docs: this is
/// obfuscation, not a secret. Anything that changes here must change in
/// `tools/lynxrdp-monitor/lynxrdp_monitor/crypto.py` in the same commit.
const KEY_MATERIAL: &[u8] = b"lynxrdp-monitor-report-key-v1";

/// Domain separation for the derivation, so this key cannot collide with a
/// key derived from the same material for some other purpose later.
const SALT: &[u8] = b"lynxrdp.reporting.salt.v1";

/// Derive the datagram key: `SHA-256(SALT || 0x00 || KEY_MATERIAL)`.
///
/// The separator matters. Without it, a different (salt, material) split that
/// concatenates to the same bytes would derive the same key, and a zero byte
/// cannot appear in either constant.
pub fn derive_key() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SALT);
    hasher.update([0u8]);
    hasher.update(KEY_MATERIAL);
    hasher.finalize().into()
}

/// Associated data: the header bytes that precede the nonce.
fn associated_data() -> [u8; 5] {
    [MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], FORMAT_VERSION]
}

/// Wrap `plaintext` into a datagram the viewer will accept.
pub fn seal(plaintext: &[u8]) -> Result<Vec<u8>> {
    let key = derive_key();
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    // A fresh random nonce per datagram. Reports go out once per interval, so
    // the 96-bit space is nowhere near a birthday collision.
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aad = associated_data();
    let body = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("sealing the report failed"))?;

    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Unwrap a datagram produced by [`seal`].
///
/// Present so the format can be tested from both directions in one place; the
/// server itself never opens a report.
pub fn open(datagram: &[u8]) -> Result<Vec<u8>> {
    if datagram.len() < HEADER_LEN + TAG_LEN {
        bail!("datagram is too short to be a report");
    }
    if &datagram[..4] != MAGIC {
        bail!("not a LynxRDP report");
    }
    let version = datagram[4];
    if version != FORMAT_VERSION {
        bail!("report format version {version} is not supported");
    }
    let nonce = Nonce::from_slice(&datagram[5..HEADER_LEN]);
    let aad = associated_data();
    let key = derive_key();
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &datagram[HEADER_LEN..],
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("report failed authentication"))
        .context("opening a report")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_pinned() {
        // A known answer, so a careless edit to the constants is caught here
        // rather than by every deployed viewer going quiet at once. The same
        // value is asserted on the Python side.
        let key = derive_key();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "f34c877322e249221e027336b9945aee555e2bd7a786f81753128971e27dddd8"
        );
    }

    #[test]
    fn roundtrips() {
        let msg = br#"{"node":"desk01","ip":"10.0.0.5"}"#;
        let sealed = seal(msg).unwrap();
        assert_eq!(open(&sealed).unwrap(), msg);
    }

    #[test]
    fn the_payload_is_not_readable_on_the_wire() {
        // The whole point: a capture must not show the hostname.
        let sealed = seal(br#"{"node":"secret-host"}"#).unwrap();
        assert!(
            !sealed.windows(11).any(|w| w == b"secret-host"),
            "plaintext leaked into the datagram"
        );
    }

    #[test]
    fn every_datagram_uses_a_fresh_nonce() {
        // Reusing a nonce with the same key would leak the XOR of two
        // plaintexts, so this is worth pinning rather than assuming.
        let a = seal(b"same").unwrap();
        let b = seal(b"same").unwrap();
        assert_ne!(a[5..HEADER_LEN], b[5..HEADER_LEN], "nonce repeated");
        assert_ne!(a, b);
    }

    #[test]
    fn tampering_is_rejected() {
        let sealed = seal(b"hello").unwrap();
        for index in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[index] ^= 0x01;
            assert!(open(&bad).is_err(), "a flipped bit at {index} was accepted");
        }
    }

    #[test]
    fn truncation_is_rejected() {
        let sealed = seal(b"hello").unwrap();
        for cut in 0..sealed.len() {
            assert!(
                open(&sealed[..cut]).is_err(),
                "accepted a {cut}-byte prefix"
            );
        }
    }

    #[test]
    fn junk_is_rejected_without_panicking() {
        for bad in [
            &b""[..],
            b"short",
            b"LXR1",
            &[0u8; 64][..],
            b"XXXX\x01aaaaaaaaaaaabbbbbbbbbbbbbbbb",
        ] {
            assert!(open(bad).is_err());
        }
    }

    #[test]
    fn a_future_version_is_refused_rather_than_misread() {
        let mut sealed = seal(b"hello").unwrap();
        sealed[4] = FORMAT_VERSION + 1;
        let err = open(&sealed).unwrap_err().to_string();
        assert!(err.contains("version"), "{err}");
    }

    #[test]
    fn overhead_is_what_the_size_budget_assumes() {
        let sealed = seal(b"").unwrap();
        assert_eq!(sealed.len(), HEADER_LEN + TAG_LEN);
    }
}
