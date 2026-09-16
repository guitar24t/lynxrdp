//! Primitive little-endian serialization helpers.
//!
//! All integers are little endian. Strings are UTF-8 with a `u32` byte
//! length prefix. Byte blobs use a `u32` length prefix.

use std::fmt;

/// Error returned when decoding malformed data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Input ended before the field was complete.
    UnexpectedEof {
        /// Bytes needed to decode the field.
        needed: usize,
        /// Bytes remaining in the input.
        remaining: usize,
    },
    /// A string field was not valid UTF-8.
    InvalidUtf8,
    /// A length prefix was larger than allowed.
    LengthTooLarge(usize),
    /// A discriminant / enum tag was unknown.
    InvalidTag(u32),
    /// Payload had trailing bytes after a complete message.
    TrailingBytes(usize),
    /// A field value was outside its allowed range.
    InvalidValue(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEof { needed, remaining } => {
                write!(
                    f,
                    "unexpected end of input: need {needed} bytes, {remaining} remaining"
                )
            }
            DecodeError::InvalidUtf8 => write!(f, "invalid UTF-8 in string field"),
            DecodeError::LengthTooLarge(n) => write!(f, "length prefix too large: {n}"),
            DecodeError::InvalidTag(t) => write!(f, "unknown tag value {t}"),
            DecodeError::TrailingBytes(n) => write!(f, "{n} trailing bytes after message"),
            DecodeError::InvalidValue(what) => write!(f, "invalid value for {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Maximum length accepted for any single string or blob (32 MiB).
///
/// This bounds the payloads that are legitimately large -- a tile's pixels, a
/// transfer chunk, clipboard text. A name, path, reason or message is bounded
/// by [`MAX_TEXT_LEN`] instead.
pub const MAX_BLOB_LEN: usize = 32 * 1024 * 1024;

/// Maximum byte length of a free-text field: a name, path, reason or message
/// (4 KiB).
///
/// Free text is the one kind of field the receiving side routinely puts back
/// on the wire with something prepended -- a `FileRequest` path comes back in
/// the `TransferEnd` that says why it could not be opened -- so a field that
/// was allowed to fill [`MAX_BLOB_LEN`] exactly guaranteed that the echo could
/// not be encoded, and the encoder's assert on that unwound the session's main
/// thread. Capping free text well below the blob limit makes the echo always
/// fit; 4096 is `PATH_MAX` on Linux, so no path the session could actually
/// open is refused. [`Reader::text`] enforces it at decode and [`Writer::text`]
/// truncates rather than fails at encode, so the limit cannot become a panic
/// on either side.
pub const MAX_TEXT_LEN: usize = 4096;

/// The longest prefix of `s` that fits in `max` bytes and ends on a character
/// boundary.
///
/// Truncation is logged rather than silent: it means a caller formatted a
/// peer-supplied string into a field without bounding it first, which is worth
/// knowing about, but a shortened message is the right degradation for that.
fn truncated<'a>(s: &'a str, max: usize, what: &str) -> &'a str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    log::warn!("truncating a {} byte {what} field to {max} bytes", s.len());
    &s[..cut]
}

/// Growable output buffer.
#[derive(Default, Debug)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Create an empty writer.
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Create a writer with reserved capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Consume the writer and return the bytes.
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    /// Borrow the written bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Number of bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Write a single byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Write a boolean as one byte.
    pub fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    /// Write a `u16`.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Write an `i16`.
    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Write a `u32`.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Write an `i32`.
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Write a `u64`.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Write raw bytes without a length prefix.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Write a length-prefixed blob.
    ///
    /// A blob over [`MAX_BLOB_LEN`] is cut to the limit and logged rather than
    /// asserted on. Nothing in the protocol legitimately produces one -- a
    /// chunk is `CHUNK_SIZE` and a tile is 64x64 pixels -- so the log line
    /// reports a bug in the caller; the assert it replaces turned that bug
    /// into an unwind of the session's main thread, reachable from the wire
    /// through any string a peer could fill to the limit.
    pub fn bytes(&mut self, bytes: &[u8]) {
        let bytes = if bytes.len() > MAX_BLOB_LEN {
            log::error!(
                "truncating a {} byte blob to MAX_BLOB_LEN ({MAX_BLOB_LEN})",
                bytes.len()
            );
            &bytes[..MAX_BLOB_LEN]
        } else {
            bytes
        };
        self.u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
    }

    /// Write a length-prefixed UTF-8 string of up to [`MAX_BLOB_LEN`] bytes,
    /// cut on a character boundary if it is longer.
    pub fn string(&mut self, s: &str) {
        self.bytes(truncated(s, MAX_BLOB_LEN, "string").as_bytes());
    }

    /// Write a free-text field: a string of up to [`MAX_TEXT_LEN`] bytes, cut
    /// on a character boundary if it is longer.
    pub fn text(&mut self, s: &str) {
        self.bytes(truncated(s, MAX_TEXT_LEN, "text").as_bytes());
    }
}

/// Cursor over an input slice.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Create a reader over `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Number of unread bytes.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Whether every byte has been consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Return an error if any bytes remain.
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes(self.remaining()))
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::UnexpectedEof {
                needed: n,
                remaining: self.remaining(),
            });
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Read a single byte.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    /// Read a boolean; any non-zero byte is `true`.
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.u8()? != 0)
    }

    /// Read a `u16`.
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read an `i16`.
    pub fn i16(&mut self) -> Result<i16, DecodeError> {
        let b = self.take(2)?;
        Ok(i16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a `u32`.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read an `i32`.
    pub fn i32(&mut self) -> Result<i32, DecodeError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a `u64`.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        Ok(u64::from_le_bytes(arr))
    }

    /// Read `n` raw bytes.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        self.take(n)
    }

    /// Read a length-prefixed blob.
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.u32()? as usize;
        if len > MAX_BLOB_LEN {
            return Err(DecodeError::LengthTooLarge(len));
        }
        self.take(len)
    }

    fn string_up_to(&mut self, max: usize) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(DecodeError::LengthTooLarge(len));
        }
        let b = self.take(len)?;
        std::str::from_utf8(b)
            .map(|s| s.to_owned())
            .map_err(|_| DecodeError::InvalidUtf8)
    }

    /// Read a length-prefixed UTF-8 string of up to [`MAX_BLOB_LEN`] bytes.
    pub fn string(&mut self) -> Result<String, DecodeError> {
        self.string_up_to(MAX_BLOB_LEN)
    }

    /// Read a free-text field: a string of up to [`MAX_TEXT_LEN`] bytes.
    pub fn text(&mut self) -> Result<String, DecodeError> {
        self.string_up_to(MAX_TEXT_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_primitives() {
        let mut w = Writer::new();
        w.u8(0xAB);
        w.bool(true);
        w.u16(0x1234);
        w.i16(-2);
        w.u32(0xDEADBEEF);
        w.i32(-100000);
        w.u64(0x0102030405060708);
        w.bytes(&[1, 2, 3]);
        w.string("héllo");
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 0xAB);
        assert!(r.bool().unwrap());
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.i16().unwrap(), -2);
        assert_eq!(r.u32().unwrap(), 0xDEADBEEF);
        assert_eq!(r.i32().unwrap(), -100000);
        assert_eq!(r.u64().unwrap(), 0x0102030405060708);
        assert_eq!(r.bytes().unwrap(), &[1, 2, 3]);
        assert_eq!(r.string().unwrap(), "héllo");
        assert!(r.finish().is_ok());
    }

    #[test]
    fn eof_is_detected() {
        let mut r = Reader::new(&[1, 2, 3]);
        assert_eq!(
            r.u32(),
            Err(DecodeError::UnexpectedEof {
                needed: 4,
                remaining: 3
            })
        );
    }

    #[test]
    fn oversized_blob_rejected() {
        let mut w = Writer::new();
        w.u32(u32::MAX);
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        assert!(matches!(r.bytes(), Err(DecodeError::LengthTooLarge(_))));
    }

    #[test]
    fn invalid_utf8_rejected() {
        let mut w = Writer::new();
        w.bytes(&[0xff, 0xfe]);
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.string(), Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn trailing_bytes_detected() {
        let r = Reader::new(&[1]);
        assert_eq!(r.finish(), Err(DecodeError::TrailingBytes(1)));
    }

    /// The assert this replaced was reachable from the wire: a peer could fill
    /// a string to exactly `MAX_BLOB_LEN`, and any prefix this side added to
    /// it before sending it back put the result over the limit.
    #[test]
    fn an_oversized_blob_is_cut_rather_than_asserted_on() {
        let mut w = Writer::new();
        w.bytes(&vec![7u8; MAX_BLOB_LEN + 1]);
        assert_eq!(w.len(), 4 + MAX_BLOB_LEN);
        let mut r = Reader::new(w.as_slice());
        assert_eq!(r.bytes().unwrap().len(), MAX_BLOB_LEN);
    }

    #[test]
    fn text_is_bounded_on_both_sides_and_cut_on_a_character_boundary() {
        // Three-byte characters, so the limit itself is not a boundary and a
        // byte-level cut would produce invalid UTF-8.
        let s = "\u{20ac}".repeat(MAX_TEXT_LEN);
        let mut w = Writer::new();
        w.text(&s);
        let back = Reader::new(w.as_slice()).text().unwrap();
        assert_eq!(back.len(), MAX_TEXT_LEN - 1);
        assert!(s.starts_with(&back));

        // Exactly the limit passes; one byte more is refused by the reader...
        let at_limit = "a".repeat(MAX_TEXT_LEN);
        let mut w = Writer::new();
        w.text(&at_limit);
        assert_eq!(Reader::new(w.as_slice()).text().unwrap(), at_limit);
        let mut w = Writer::new();
        w.u32((MAX_TEXT_LEN + 1) as u32);
        w.raw(&vec![b'a'; MAX_TEXT_LEN + 1]);
        assert_eq!(
            Reader::new(w.as_slice()).text(),
            Err(DecodeError::LengthTooLarge(MAX_TEXT_LEN + 1))
        );
        // ...while `string` still admits it: clipboard text is legitimately
        // larger than any name or path.
        assert!(Reader::new(w.as_slice()).string().is_ok());
    }
}
