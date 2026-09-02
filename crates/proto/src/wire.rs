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
                write!(f, "unexpected end of input: need {needed} bytes, {remaining} remaining")
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
pub const MAX_BLOB_LEN: usize = 32 * 1024 * 1024;

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
        Self { buf: Vec::with_capacity(cap) }
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
    pub fn bytes(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= MAX_BLOB_LEN, "blob exceeds MAX_BLOB_LEN");
        self.u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
    }

    /// Write a length-prefixed UTF-8 string.
    pub fn string(&mut self, s: &str) {
        self.bytes(s.as_bytes());
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
            return Err(DecodeError::UnexpectedEof { needed: n, remaining: self.remaining() });
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

    /// Read a length-prefixed UTF-8 string.
    pub fn string(&mut self) -> Result<String, DecodeError> {
        let b = self.bytes()?;
        std::str::from_utf8(b).map(|s| s.to_owned()).map_err(|_| DecodeError::InvalidUtf8)
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
        assert_eq!(r.u32(), Err(DecodeError::UnexpectedEof { needed: 4, remaining: 3 }));
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
}
