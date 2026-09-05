//! Length-prefixed framing of [`Message`]s over a byte stream.
//!
//! Wire layout of one frame:
//!
//! ```text
//! +----------------+---------------------------+
//! | u32 LE length  | length bytes: kind+payload|
//! +----------------+---------------------------+
//! ```
//!
//! `length` counts everything after the prefix. Frames larger than
//! [`MAX_MESSAGE_SIZE`] are rejected without allocating.

use std::io::{self, Read, Write};

use crate::message::Message;
use crate::wire::DecodeError;
use crate::MAX_MESSAGE_SIZE;

/// Size of the length prefix in bytes.
pub const HEADER_LEN: usize = 4;

/// Message tags at or above this are *skippable extensions*.
///
/// `Kind::from_u8` failing is fatal on both sides, so a newer peer sending one
/// new message type drops the connection outright -- and server packages are
/// installed by administrators on RHEL 9 while clients update on three
/// platforms, so some version skew is guaranteed. Reserving a tag range the
/// framing layer simply discards costs nothing, because the length prefix
/// already says exactly how much to throw away.
///
/// This only helps if it is present in the *older* peer, which is why it is
/// worth having before anything uses it and worth very little afterwards.
///
/// Tags below this are structural: a peer that cannot decode one cannot stay in
/// sync, so those stay fatal.
pub const EXTENSION_TAG_MIN: u8 = 128;

/// Whether a frame payload carries a skippable extension message.
fn is_skippable_extension(payload: &[u8]) -> bool {
    // An empty payload has no tag at all; let the decoder report it.
    payload
        .first()
        .is_some_and(|&tag| tag >= EXTENSION_TAG_MIN && crate::message::Kind::from_u8(tag).is_err())
}

/// Errors from framing.
#[derive(Debug)]
pub enum FrameError {
    /// Underlying I/O failure.
    Io(io::Error),
    /// The peer sent a frame larger than [`MAX_MESSAGE_SIZE`].
    TooLarge(u32),
    /// The frame payload could not be decoded.
    Decode(DecodeError),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "i/o error: {e}"),
            FrameError::TooLarge(n) => write!(f, "frame too large: {n} bytes"),
            FrameError::Decode(e) => write!(f, "decode error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameError::Io(e) => Some(e),
            FrameError::Decode(e) => Some(e),
            FrameError::TooLarge(_) => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

impl From<DecodeError> for FrameError {
    fn from(e: DecodeError) -> Self {
        FrameError::Decode(e)
    }
}

impl FrameError {
    /// True if the error indicates the peer closed the connection.
    pub fn is_disconnect(&self) -> bool {
        match self {
            FrameError::Io(e) => matches!(
                e.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
            ),
            _ => false,
        }
    }
}

/// Serialise a message with its length prefix into `out`.
pub fn frame_message(msg: &Message, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    let mut w = crate::wire::Writer::new();
    msg.encode_into(&mut w);
    out.extend_from_slice(w.as_slice());
    let len = (out.len() - start - HEADER_LEN) as u32;
    out[start..start + HEADER_LEN].copy_from_slice(&len.to_le_bytes());
}

/// Write one framed message to a blocking writer and flush it.
pub fn write_message<W: Write>(w: &mut W, msg: &Message) -> io::Result<()> {
    let mut buf = Vec::new();
    frame_message(msg, &mut buf);
    w.write_all(&buf)?;
    w.flush()
}

/// Read one framed message from a blocking reader.
pub fn read_message<R: Read>(r: &mut R) -> Result<Message, FrameError> {
    loop {
        let mut hdr = [0u8; HEADER_LEN];
        r.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes(hdr);
        if len > MAX_MESSAGE_SIZE {
            return Err(FrameError::TooLarge(len));
        }
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload)?;
        if is_skippable_extension(&payload) {
            log::debug!(
                "skipping extension message tag {} ({len} bytes)",
                payload[0]
            );
            continue;
        }
        return Ok(Message::decode(&payload)?);
    }
}

/// Incremental parser for non-blocking or chunked input.
#[derive(Default, Debug)]
pub struct FrameParser {
    buf: Vec<u8>,
    pos: usize,
}

impl FrameParser {
    /// Create an empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append received bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.pos > 0 && self.pos >= self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Extract the next complete message, if any.
    pub fn next_message(&mut self) -> Result<Option<Message>, FrameError> {
        loop {
            let avail = &self.buf[self.pos..];
            if avail.len() < HEADER_LEN {
                return Ok(None);
            }
            let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]);
            if len > MAX_MESSAGE_SIZE {
                return Err(FrameError::TooLarge(len));
            }
            let total = HEADER_LEN + len as usize;
            if avail.len() < total {
                return Ok(None);
            }
            let payload = &avail[HEADER_LEN..total];
            if is_skippable_extension(payload) {
                log::debug!(
                    "skipping extension message tag {} ({len} bytes)",
                    payload[0]
                );
                self.pos += total;
                continue;
            }
            let msg = Message::decode(payload)?;
            self.pos += total;
            return Ok(Some(msg));
        }
    }

    /// Bytes buffered but not yet parsed.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn write_then_read() {
        let mut buf = Vec::new();
        write_message(&mut buf, &Message::Ping { nonce: 7 }).unwrap();
        write_message(&mut buf, &Message::RefreshRequest).unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(read_message(&mut c).unwrap(), Message::Ping { nonce: 7 });
        assert_eq!(read_message(&mut c).unwrap(), Message::RefreshRequest);
        let err = read_message(&mut c).unwrap_err();
        assert!(err.is_disconnect());
    }

    #[test]
    fn too_large_frame_rejected_before_alloc() {
        let mut bytes = (MAX_MESSAGE_SIZE + 1).to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0; 8]);
        let mut c = Cursor::new(bytes.clone());
        assert!(matches!(read_message(&mut c), Err(FrameError::TooLarge(_))));
        let mut p = FrameParser::new();
        p.feed(&bytes);
        assert!(matches!(p.next_message(), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn parser_handles_chunks() {
        let mut buf = Vec::new();
        for i in 0..50u64 {
            frame_message(&Message::FrameAck { frame_id: i }, &mut buf);
        }
        let mut p = FrameParser::new();
        let mut got = Vec::new();
        for chunk in buf.chunks(7) {
            p.feed(chunk);
            while let Some(m) = p.next_message().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got.len(), 50);
        assert_eq!(got[49], Message::FrameAck { frame_id: 49 });
        assert_eq!(p.pending(), 0);
    }

    #[test]
    fn parser_reports_decode_errors() {
        let mut p = FrameParser::new();
        // Deliberately just below EXTENSION_TAG_MIN. An unknown *structural*
        // tag is still fatal; this test used to use 0xEE, which now falls in
        // the skippable range and would be discarded rather than reported.
        p.feed(&[1, 0, 0, 0, EXTENSION_TAG_MIN - 1]);
        assert!(matches!(p.next_message(), Err(FrameError::Decode(_))));
    }

    /// An unknown message in the extension range is skipped, not fatal.
    ///
    /// This is the whole point of the range: a newer peer can send an optional
    /// message an older one has never heard of, and the older one keeps the
    /// connection instead of dropping it. The value is entirely in the *older*
    /// peer, so it has to be here before anything sends one.
    #[test]
    fn extension_messages_are_skipped_not_fatal() {
        let mut p = FrameParser::new();
        // An extension frame with a payload, then an ordinary message. The
        // real message must still arrive, in one call.
        p.feed(&[5, 0, 0, 0, EXTENSION_TAG_MIN, 1, 2, 3, 4]);
        let mut buf = Vec::new();
        frame_message(&Message::FrameAck { frame_id: 7 }, &mut buf);
        p.feed(&buf);
        assert_eq!(
            p.next_message().unwrap(),
            Some(Message::FrameAck { frame_id: 7 })
        );
        assert_eq!(p.pending(), 0);

        // The blocking reader must skip it too, not just the incremental one.
        let mut wire = vec![5, 0, 0, 0, 0xEE, 9, 9, 9, 9];
        wire.extend_from_slice(&buf);
        let mut cur = Cursor::new(wire);
        assert_eq!(
            read_message(&mut cur).unwrap(),
            Message::FrameAck { frame_id: 7 }
        );
    }
}
