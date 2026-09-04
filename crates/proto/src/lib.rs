//! LynxRDP protocol crate.
//!
//! This crate is shared between the server (Linux) and the client
//! (Windows / macOS / Linux). It contains:
//!
//! * [`wire`]  – primitive little-endian encode/decode helpers.
//! * [`message`] – all protocol messages and their binary encoding.
//! * [`frame`] – length-prefixed framing over any byte stream.
//! * [`image`] – framebuffer and rectangle helpers.
//! * [`codec`] – tile based screen diffing and compression.
//! * [`keysym`] – X11 keysym constants and helpers used by both sides.
//!
//! The protocol is deliberately small. It is designed to be carried inside
//! an SSH tunnel and therefore does not implement its own encryption or
//! authentication: those are provided by SSH and by the server's peer
//! identification (see the server crate).
//!
//! Everything in this crate is `#![forbid(unsafe_code)]` and covered by
//! unit and property tests.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codec;
pub mod frame;
pub mod image;
pub mod keysym;
pub mod message;
pub mod transfer;
pub mod urilist;
pub mod wire;

/// Protocol version spoken by this build.
///
/// Bumped whenever the protocol gains anything. [`MIN_COMPATIBLE_VERSION`] is
/// the one that moves only for a change an older peer cannot survive, and the
/// two are not the same event: adding an optional message raises this constant
/// alone, which costs nobody a session.
pub const PROTOCOL_VERSION: u16 = 3;

/// Oldest peer version this build will still hold a session with.
///
/// Comparing a single version for equality makes every release a flag day, and
/// the two ends are routinely built from different commits: server packages are
/// installed by administrators on RHEL 9 while clients update themselves on
/// three platforms. A floor names the pairings that are known to work and turns
/// the rest into a refusal at the handshake, with a reason a user can act on,
/// instead of leaving an old peer to fail somewhere in the middle of a session
/// on the first message it cannot parse.
///
/// See the `Versioning` section of [`message`] for the handshake rule the two
/// hello handlers implement, and for the one thing this floor does *not* cover.
///
/// # The obligation, which is the expensive half
///
/// While the floor stays at this number, **no message that existed at this
/// version may change shape**. A peer compiled from an older commit decodes the
/// new bytes with the layout it was built with: a field that moved, changed
/// width or changed meaning is not an error to that peer, it is a plausible
/// wrong value it will act on. That is worse than the flag day the floor was
/// meant to replace, and it fails in a deployed session rather than in CI.
///
/// `crates/proto/tests/wire_corpus.rs` is what keeps the promise honest. It
/// holds the exact bytes of every message as checked-in hex and fails if any of
/// them move, and its regeneration step refuses to rewrite an existing entry
/// unless this constant has actually risen. Raising the floor without
/// regenerating fails; regenerating to silence a failure does not work.
pub const MIN_COMPATIBLE_VERSION: u16 = 3;

// A floor above the version we speak would refuse every peer including
// ourselves, and the mistake is easy to make when bumping one and not the
// other. Catch it at compile time rather than in a test nobody ran.
const _: () = assert!(MIN_COMPATIBLE_VERSION <= PROTOCOL_VERSION);

/// Whether a peer announcing `peer` is one this build is willing to accept.
///
/// Deliberately one-sided: a peer *newer* than this build passes. See
/// [`agreed_version`] for why that is not the oversight it looks like.
pub const fn peer_meets_floor(peer: u16) -> bool {
    peer >= MIN_COMPATIBLE_VERSION
}

/// The version two builds settle on: the older of the two.
///
/// The server must put this, not its own [`PROTOCOL_VERSION`], in its
/// `ServerHello`, which is what lets an old client's plain equality check pass
/// unchanged — it sees exactly the version it sent. The newer side is then the
/// one that decides whether the pairing is workable, with [`can_speak`], and it
/// is the only side that can: it knows both version numbers *and* its own
/// floor, where the older side knows neither of the other's.
///
/// That asymmetry only works if the older build already declines to refuse a
/// higher version, so it has to ship before the first build that needs it —
/// the same argument, and the same short window, as
/// [`frame::EXTENSION_TAG_MIN`].
pub const fn agreed_version(peer: u16) -> u16 {
    if peer < PROTOCOL_VERSION {
        peer
    } else {
        PROTOCOL_VERSION
    }
}

/// Whether this build can hold a session at protocol version `version`.
///
/// The check for the side reading an [`agreed_version`] back out of a
/// `ServerHello`. A server answering with something *above* our own version has
/// not clamped and is broken, so that fails here too.
pub const fn can_speak(version: u16) -> bool {
    peer_meets_floor(version) && version <= PROTOCOL_VERSION
}

/// Default TCP port the server listens on (loopback only).
pub const DEFAULT_PORT: u16 = 3390;

/// Maximum size of a single framed message payload (64 MiB). Anything larger
/// is treated as a protocol violation and the connection is dropped.
pub const MAX_MESSAGE_SIZE: u32 = 64 * 1024 * 1024;

/// Tile size in pixels used by the screen codec.
pub const TILE_SIZE: u32 = 64;

pub use codec::{CopyRect, FrameUpdate};
pub use image::{Framebuffer, Rect};
pub use message::Message;
pub use transfer::{FileEntry, TransferPurpose};

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn a_peer_below_the_floor_is_refused() {
        assert!(!peer_meets_floor(MIN_COMPATIBLE_VERSION - 1));
        assert!(!can_speak(MIN_COMPATIBLE_VERSION - 1));
        assert!(peer_meets_floor(MIN_COMPATIBLE_VERSION));
    }

    /// A newer peer is *not* refused by this side. It is answered with the
    /// version we can speak, and left to decide for itself -- it is the only
    /// side that knows both numbers and its own floor.
    #[test]
    fn a_newer_peer_is_clamped_rather_than_refused() {
        let newer = PROTOCOL_VERSION + 7;
        assert!(peer_meets_floor(newer));
        assert_eq!(agreed_version(newer), PROTOCOL_VERSION);
        // ...but a version above ours must never be *spoken*: a server that
        // answered with one failed to clamp, and we have no idea what it will
        // send next.
        assert!(!can_speak(newer));
    }

    /// Every pairing the floor promises to serve, checked end to end: the peer
    /// is accepted, it is answered with exactly the version it announced (an
    /// old build compares that for equality, so anything else and the
    /// compatibility is theoretical), and the answer is one we can speak.
    ///
    /// One entry while the floor sits at the current version; the loop is the
    /// point, because raising `PROTOCOL_VERSION` alone is what fills it in.
    #[test]
    fn every_version_the_floor_promises_is_actually_workable() {
        for peer in MIN_COMPATIBLE_VERSION..=PROTOCOL_VERSION {
            assert!(peer_meets_floor(peer));
            assert_eq!(
                agreed_version(peer),
                peer,
                "peer {peer} would see a mismatch"
            );
            assert!(can_speak(agreed_version(peer)));
        }
    }
}
