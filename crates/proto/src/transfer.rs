//! Chunked binary transfers.
//!
//! Clipboard images and file transfers both need to move blobs that are far
//! too large for a single message, so they share one mechanism rather than
//! growing two chunking implementations to get wrong independently.
//!
//! # Shape of a transfer
//!
//! ```text
//! sender                                receiver
//!   |  TransferOffer { id, size, .. } ----->|
//!   |<---------- TransferAccept { id, ok }  |
//!   |  TransferData { id, seq: 0 } -------->|
//!   |  TransferData { id, seq: 1 } -------->|
//!   |<--------------- TransferAck { seq: 0 }|
//!   |  TransferData { id, seq: 2 } -------->|
//!   |                    ...                |
//!   |  TransferEnd { id, ok: true } ------->|
//! ```
//!
//! Either side may originate a transfer, and either side may end one early
//! with `TransferEnd { ok: false }`.
//!
//! # Why it does not stall the screen
//!
//! Everything shares one TCP connection, so a large transfer could easily
//! starve interactive frames. Two things prevent that: chunks are capped at
//! [`CHUNK_SIZE`] so a frame only ever waits for one chunk to drain, and the
//! sender keeps at most [`WINDOW_CHUNKS`] chunks unacknowledged, which bounds
//! how much transfer data can be queued ahead of a frame.

use std::io::{self, Read, Write};

/// Largest payload carried by a single [`crate::Message::TransferData`].
///
/// Small enough that a screen frame never waits long behind one, large
/// enough that per-message overhead stays negligible.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// How many chunks may be in flight before the sender waits for acks.
/// Bounds transfer data queued ahead of an interactive frame.
pub const WINDOW_CHUNKS: u32 = 8;

/// Largest transfer accepted by default (2 GiB).
pub const MAX_TRANSFER_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Largest transfer either side will hold entirely in memory (64 MiB).
///
/// [`MAX_TRANSFER_SIZE`] is the right bound for something being streamed to a
/// file, and completely the wrong one for a [`Sink::Memory`]: both endpoints'
/// policies took a clipboard image straight into memory while explicitly
/// ignoring the offered size, so a handful of 2 GiB offers were enough to push
/// either peer out of memory. This is the number SECURITY.md already claims for
/// clipboard images, now actually enforced.
pub const MAX_MEMORY_TRANSFER_SIZE: u64 = 64 * 1024 * 1024;

/// How many in-memory transfers may be in flight at once.
///
/// A size cap alone bounds one transfer, not the set of them. This is
/// deliberately *not* a cap on incoming transfers in general: staging a
/// clipboard file copy legitimately fans out one transfer per file, up to
/// `MAX_FILE_LIST`, and those stream to disk.
pub const MAX_CONCURRENT_MEMORY_TRANSFERS: usize = 2;

/// What a transfer is for. Determines how the receiver handles it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TransferPurpose {
    /// A clipboard image, always PNG. Held in memory.
    ClipboardImage = 0,
    /// A file being written into the session (client to server).
    FileUpload = 1,
    /// A file being read out of the session (server to client).
    FileDownload = 2,
}

impl TransferPurpose {
    /// Convert from the wire tag.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(TransferPurpose::ClipboardImage),
            1 => Some(TransferPurpose::FileUpload),
            2 => Some(TransferPurpose::FileDownload),
            _ => None,
        }
    }
}

/// Why a transfer failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    /// A chunk arrived out of order.
    OutOfOrder {
        /// Sequence number that was expected.
        expected: u32,
        /// Sequence number that arrived.
        got: u32,
    },
    /// More bytes arrived than the offer promised.
    TooLong {
        /// Size the offer promised.
        expected: u64,
        /// Bytes received so far including this chunk.
        got: u64,
    },
    /// The transfer ended before the promised number of bytes arrived.
    Truncated {
        /// Size the offer promised.
        expected: u64,
        /// Bytes actually received.
        got: u64,
    },
    /// A chunk exceeded [`CHUNK_SIZE`].
    ChunkTooLarge(usize),
    /// The offer was larger than this side is willing to accept.
    TooLarge(u64),
    /// Writing to (or reading from) the local endpoint failed.
    Io(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::OutOfOrder { expected, got } => {
                write!(f, "chunk out of order: expected {expected}, got {got}")
            }
            TransferError::TooLong { expected, got } => {
                write!(f, "transfer longer than offered: {got} > {expected}")
            }
            TransferError::Truncated { expected, got } => {
                write!(f, "transfer truncated: {got} of {expected} bytes")
            }
            TransferError::ChunkTooLarge(n) => write!(f, "chunk of {n} bytes exceeds the limit"),
            TransferError::TooLarge(n) => write!(f, "transfer of {n} bytes is too large"),
            TransferError::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for TransferError {}

impl From<io::Error> for TransferError {
    fn from(e: io::Error) -> Self {
        TransferError::Io(e.to_string())
    }
}

/// Sending half: reads from a source and hands out chunks, respecting the
/// acknowledgement window.
#[derive(Debug)]
pub struct TransferSender<R> {
    id: u64,
    source: R,
    total: u64,
    sent: u64,
    next_seq: u32,
    acked: u32,
    eof: bool,
}

impl<R: Read> TransferSender<R> {
    /// Begin sending `total` bytes read from `source`.
    pub fn new(id: u64, total: u64, source: R) -> Self {
        Self {
            id,
            source,
            total,
            sent: 0,
            next_seq: 0,
            acked: 0,
            eof: false,
        }
    }

    /// Transfer identifier.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether the window currently allows another chunk.
    pub fn may_send(&self) -> bool {
        !self.eof && self.next_seq - self.acked < WINDOW_CHUNKS
    }

    /// Produce the next chunk, or `None` when the window is full or the
    /// source is exhausted.
    pub fn next_chunk(&mut self) -> Result<Option<(u32, Vec<u8>)>, TransferError> {
        if !self.may_send() {
            return Ok(None);
        }
        // Never read past what the offer promised: if the file grew since it
        // was measured, the extra bytes are not ours to send.
        let remaining = self.total.saturating_sub(self.sent);
        if remaining == 0 {
            self.eof = true;
            return Ok(None);
        }
        let want = CHUNK_SIZE.min(remaining as usize);
        let mut buf = vec![0u8; want];
        let mut filled = 0usize;
        // read() may return short reads; fill the chunk so the stream is not
        // fragmented into tiny messages by an awkward source.
        while filled < want {
            match self.source.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            // The source ended early; the receiver will report a truncated
            // transfer, which is the accurate outcome.
            self.eof = true;
            return Ok(None);
        }
        buf.truncate(filled);
        self.sent += filled as u64;
        let seq = self.next_seq;
        self.next_seq += 1;
        // Knowing the total means EOF needs no extra read that returns zero,
        // so a transfer whose size is an exact multiple of the chunk size
        // still completes as soon as its last chunk is acknowledged.
        if self.sent >= self.total || filled < want {
            self.eof = true;
        }
        Ok(Some((seq, buf)))
    }

    /// Record an acknowledgement, freeing window space.
    pub fn on_ack(&mut self, seq: u32) {
        // Acks are cumulative in effect: a later ack implies the earlier ones.
        if seq >= self.acked && seq < self.next_seq {
            self.acked = seq + 1;
        }
    }

    /// Whether every byte has been read out of the source.
    pub fn is_drained(&self) -> bool {
        self.eof
    }

    /// Whether the source is drained and every chunk acknowledged.
    pub fn is_complete(&self) -> bool {
        self.eof && self.acked == self.next_seq
    }

    /// Bytes handed out so far, and the promised total.
    pub fn progress(&self) -> (u64, u64) {
        (self.sent, self.total)
    }
}

/// Receiving half: validates chunks and writes them to a sink.
#[derive(Debug)]
pub struct TransferReceiver<W> {
    id: u64,
    sink: W,
    expected: u64,
    received: u64,
    next_seq: u32,
}

impl<W: Write> TransferReceiver<W> {
    /// Begin receiving `expected` bytes into `sink`.
    pub fn new(id: u64, expected: u64, sink: W) -> Self {
        Self {
            id,
            sink,
            expected,
            received: 0,
            next_seq: 0,
        }
    }

    /// Transfer identifier.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Accept one chunk. Returns the sequence number to acknowledge.
    pub fn chunk(&mut self, seq: u32, data: &[u8]) -> Result<u32, TransferError> {
        if data.len() > CHUNK_SIZE {
            return Err(TransferError::ChunkTooLarge(data.len()));
        }
        if seq != self.next_seq {
            return Err(TransferError::OutOfOrder {
                expected: self.next_seq,
                got: seq,
            });
        }
        let after = self.received + data.len() as u64;
        if after > self.expected {
            return Err(TransferError::TooLong {
                expected: self.expected,
                got: after,
            });
        }
        self.sink.write_all(data)?;
        self.received = after;
        self.next_seq += 1;
        Ok(seq)
    }

    /// Finish the transfer, checking that everything promised arrived.
    pub fn finish(mut self) -> Result<W, TransferError> {
        if self.received != self.expected {
            return Err(TransferError::Truncated {
                expected: self.expected,
                got: self.received,
            });
        }
        self.sink.flush()?;
        Ok(self.sink)
    }

    /// Bytes received so far, and the promised total.
    pub fn progress(&self) -> (u64, u64) {
        (self.received, self.expected)
    }

    /// Whether every promised byte has arrived.
    pub fn is_complete(&self) -> bool {
        self.received == self.expected
    }
}

/// One entry in a file listing (a clipboard file copy, or a drag-and-drop
/// batch). Directories are expanded to their files before listing, so every
/// entry names a regular file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the root of the batch, using `/` separators.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
}

/// Largest number of files accepted in one listing.
pub const MAX_FILE_LIST: usize = 4096;

/// Allocate transfer identifiers that are unique within a connection.
#[derive(Debug, Default)]
pub struct TransferIds {
    next: u64,
}

impl TransferIds {
    /// Create an allocator. `origin_bit` keeps the two ends of a connection
    /// from ever choosing the same identifier: the client sets it, the
    /// server does not.
    pub fn new(origin_bit: bool) -> Self {
        Self {
            next: u64::from(origin_bit) << 63,
        }
    }

    /// Allocate the next identifier.
    pub fn allocate(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }
}

/// Reject a path that would escape the directory it is resolved against,
/// and reduce it to the components that are safe to join.
///
/// Uploads name their own destination, so an offer claiming
/// `../../.ssh/authorized_keys` must not be joined blindly onto the upload
/// directory. Absolute paths are handled separately by the caller, which
/// only honours them when the operator asked for an explicit destination.
pub fn safe_relative_path(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    // Treat both separators as separators: a Windows client may well offer
    // "sub\\file.txt", and it must not become a single odd filename.
    for part in trimmed.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => return None,
            p if p.contains('\0') => return None,
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a whole transfer between a sender and a receiver.
    fn pump(data: &[u8], ack_immediately: bool) -> Vec<u8> {
        let mut tx = TransferSender::new(1, data.len() as u64, io::Cursor::new(data.to_vec()));
        let mut rx = TransferReceiver::new(1, data.len() as u64, Vec::new());
        let mut pending: Vec<u32> = Vec::new();
        loop {
            match tx.next_chunk().unwrap() {
                Some((seq, chunk)) => {
                    let ack = rx.chunk(seq, &chunk).unwrap();
                    if ack_immediately {
                        tx.on_ack(ack);
                    } else {
                        pending.push(ack);
                    }
                }
                None => {
                    if tx.is_drained() {
                        break;
                    }
                    // Window full: flush the acks we were holding.
                    for a in pending.drain(..) {
                        tx.on_ack(a);
                    }
                }
            }
        }
        for a in pending.drain(..) {
            tx.on_ack(a);
        }
        assert!(tx.is_complete());
        assert!(rx.is_complete());
        rx.finish().unwrap()
    }

    #[test]
    fn roundtrips_small_and_large_payloads() {
        for size in [
            0usize,
            1,
            100,
            CHUNK_SIZE - 1,
            CHUNK_SIZE,
            CHUNK_SIZE + 1,
            CHUNK_SIZE * 5 + 7,
        ] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            if size == 0 {
                // An empty transfer sends no chunks and completes at once.
                let rx = TransferReceiver::new(1, 0, Vec::new());
                assert!(rx.is_complete());
                assert_eq!(rx.finish().unwrap(), Vec::<u8>::new());
                continue;
            }
            assert_eq!(pump(&data, true), data, "size {size}");
            assert_eq!(pump(&data, false), data, "size {size} (delayed acks)");
        }
    }

    #[test]
    fn window_limits_chunks_in_flight() {
        let data = vec![7u8; CHUNK_SIZE * (WINDOW_CHUNKS as usize + 4)];
        let mut tx = TransferSender::new(1, data.len() as u64, io::Cursor::new(data));
        let mut issued = 0;
        while tx.next_chunk().unwrap().is_some() {
            issued += 1;
        }
        assert_eq!(issued, WINDOW_CHUNKS, "sender ignored the window");
        assert!(!tx.is_drained(), "should still have data pending");
        // Acking one chunk frees exactly one slot.
        tx.on_ack(0);
        assert!(tx.next_chunk().unwrap().is_some());
        assert!(tx.next_chunk().unwrap().is_none());
    }

    #[test]
    fn out_of_order_chunk_is_rejected() {
        let mut rx = TransferReceiver::new(1, 100, Vec::new());
        assert_eq!(rx.chunk(0, &[1, 2, 3]).unwrap(), 0);
        assert_eq!(
            rx.chunk(2, &[4]),
            Err(TransferError::OutOfOrder {
                expected: 1,
                got: 2
            })
        );
    }

    #[test]
    fn overlong_and_truncated_transfers_are_rejected() {
        let mut rx = TransferReceiver::new(1, 4, Vec::new());
        assert_eq!(
            rx.chunk(0, &[1, 2, 3, 4, 5]),
            Err(TransferError::TooLong {
                expected: 4,
                got: 5
            })
        );

        let mut rx = TransferReceiver::new(1, 10, Vec::new());
        rx.chunk(0, &[1, 2, 3]).unwrap();
        assert_eq!(
            rx.finish(),
            Err(TransferError::Truncated {
                expected: 10,
                got: 3
            })
        );
    }

    #[test]
    fn oversized_chunk_is_rejected() {
        let mut rx = TransferReceiver::new(1, u64::MAX, Vec::new());
        let huge = vec![0u8; CHUNK_SIZE + 1];
        assert_eq!(
            rx.chunk(0, &huge),
            Err(TransferError::ChunkTooLarge(CHUNK_SIZE + 1))
        );
    }

    #[test]
    fn acks_are_effectively_cumulative() {
        let data = vec![1u8; CHUNK_SIZE * 4];
        let mut tx = TransferSender::new(1, data.len() as u64, io::Cursor::new(data));
        for _ in 0..4 {
            tx.next_chunk().unwrap().unwrap();
        }
        // A late ack for seq 2 implies 0 and 1 also arrived.
        tx.on_ack(2);
        assert!(tx.may_send() || tx.is_drained());
        tx.on_ack(3);
        assert!(tx.is_complete());
        // Stale and out-of-range acks are ignored rather than corrupting state.
        tx.on_ack(0);
        tx.on_ack(99);
        assert!(tx.is_complete());
    }

    #[test]
    fn a_source_longer_than_promised_is_truncated_to_the_offer() {
        // The file grew after it was measured: only the promised bytes go.
        let data = vec![3u8; 100];
        let mut tx = TransferSender::new(1, 40, io::Cursor::new(data));
        let mut rx = TransferReceiver::new(1, 40, Vec::new());
        while let Some((seq, c)) = tx.next_chunk().unwrap() {
            tx.on_ack(rx.chunk(seq, &c).unwrap());
        }
        assert!(tx.is_complete());
        assert_eq!(rx.finish().unwrap().len(), 40);
    }

    #[test]
    fn a_source_shorter_than_promised_is_reported_as_truncated() {
        // The file shrank after it was measured: the receiver must notice
        // rather than silently accept a short file.
        let data = vec![3u8; 10];
        let mut tx = TransferSender::new(1, 50, io::Cursor::new(data));
        let mut rx = TransferReceiver::new(1, 50, Vec::new());
        while let Some((seq, c)) = tx.next_chunk().unwrap() {
            tx.on_ack(rx.chunk(seq, &c).unwrap());
        }
        assert!(tx.is_drained());
        assert_eq!(
            rx.finish(),
            Err(TransferError::Truncated {
                expected: 50,
                got: 10
            })
        );
    }

    #[test]
    fn ids_from_the_two_ends_never_collide() {
        let mut client = TransferIds::new(true);
        let mut server = TransferIds::new(false);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(client.allocate()));
            assert!(seen.insert(server.allocate()));
        }
    }

    #[test]
    fn path_traversal_is_refused() {
        assert_eq!(
            safe_relative_path("notes.txt").as_deref(),
            Some("notes.txt")
        );
        assert_eq!(
            safe_relative_path("a/b/c.txt").as_deref(),
            Some("a/b/c.txt")
        );
        assert_eq!(safe_relative_path("./a//b.txt").as_deref(), Some("a/b.txt"));
        // Windows-style separators are separators, not filename characters.
        assert_eq!(
            safe_relative_path("sub\\file.txt").as_deref(),
            Some("sub/file.txt")
        );
        // Anything that climbs out is refused outright.
        assert_eq!(safe_relative_path("../secret"), None);
        assert_eq!(safe_relative_path("a/../../etc/passwd"), None);
        assert_eq!(safe_relative_path("..\\..\\secret"), None);
        assert_eq!(safe_relative_path(""), None);
        assert_eq!(safe_relative_path("   "), None);
        assert_eq!(safe_relative_path("."), None);
        assert_eq!(safe_relative_path("with\0null"), None);
        // A leading slash is stripped rather than honoured, so an "absolute"
        // name in an offer cannot land outside the upload directory.
        assert_eq!(
            safe_relative_path("/etc/passwd").as_deref(),
            Some("etc/passwd")
        );
    }

    #[test]
    fn sender_handles_a_source_that_returns_short_reads() {
        /// A reader that never returns more than 7 bytes at a time.
        struct Dribble(io::Cursor<Vec<u8>>);
        impl Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = buf.len().min(7);
                self.0.read(&mut buf[..n])
            }
        }
        let data: Vec<u8> = (0..CHUNK_SIZE * 2 + 13).map(|i| (i % 97) as u8).collect();
        let mut tx =
            TransferSender::new(1, data.len() as u64, Dribble(io::Cursor::new(data.clone())));
        let mut rx = TransferReceiver::new(1, data.len() as u64, Vec::new());
        let mut chunks = 0;
        loop {
            match tx.next_chunk().unwrap() {
                Some((seq, c)) => {
                    chunks += 1;
                    tx.on_ack(rx.chunk(seq, &c).unwrap());
                }
                None if tx.is_drained() => break,
                None => unreachable!("acks keep the window open"),
            }
        }
        // Short reads must not fragment the stream into tiny chunks.
        assert_eq!(chunks, 3, "expected full-size chunks despite short reads");
        assert_eq!(rx.finish().unwrap(), data);
    }
}

// ---------------------------------------------------------------------------
// Transfer manager
// ---------------------------------------------------------------------------

use crate::message::Message;
use std::collections::{HashMap, HashSet};

/// Where the bytes of an incoming transfer should go.
pub enum Sink {
    /// Accumulate in memory and hand the buffer back on completion. Used for
    /// clipboard images, which both sides must hold anyway.
    Memory(Vec<u8>),
    /// Stream somewhere else, typically a file being written.
    Stream(Box<dyn Write + Send>),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Memory(v) => v.write(buf),
            Sink::Stream(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Memory(v) => v.flush(),
            Sink::Stream(w) => w.flush(),
        }
    }
}

impl std::fmt::Debug for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sink::Memory(v) => write!(f, "Sink::Memory({} bytes)", v.len()),
            Sink::Stream(_) => write!(f, "Sink::Stream"),
        }
    }
}

/// Decides what to do with transfers the peer offers.
pub trait TransferPolicy {
    /// Accept an offer and say where the bytes go, or refuse with a reason.
    fn accept(
        &mut self,
        id: u64,
        purpose: TransferPurpose,
        name: &str,
        size: u64,
    ) -> Result<Sink, String>;
}

/// An incoming transfer that finished successfully.
#[derive(Debug)]
pub struct Completed {
    /// Transfer identifier.
    pub id: u64,
    /// What it was for.
    pub purpose: TransferPurpose,
    /// Name from the offer.
    pub name: String,
    /// Bytes, for [`Sink::Memory`] transfers only.
    pub data: Option<Vec<u8>>,
}

/// What handling a transfer message produced.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Messages to send to the peer.
    pub replies: Vec<Message>,
    /// Incoming transfers that completed.
    pub completed: Vec<Completed>,
    /// Transfers that failed, with a reason (either direction).
    pub failed: Vec<(u64, String)>,
    /// Outgoing transfers the peer confirmed it received in full.
    pub sent: Vec<(u64, TransferPurpose, String)>,
}

struct Outgoing {
    sender: TransferSender<Box<dyn Read + Send>>,
    purpose: TransferPurpose,
    name: String,
    accepted: bool,
}

struct Incoming {
    receiver: TransferReceiver<Sink>,
    purpose: TransferPurpose,
    name: String,
    /// Whether this one accumulates in memory. Tracked at insertion rather
    /// than derived later, so every path that drops an `Incoming` releases the
    /// budget by simply removing the record -- a separate counter would have to
    /// be decremented on each of them, and missing one silently wedges
    /// clipboard images after a few failed transfers.
    in_memory: bool,
}

/// Tracks every transfer in flight on one connection, in both directions.
///
/// Both endpoints run one of these; it is symmetric, because either side may
/// originate a transfer.
pub struct TransferManager {
    ids: TransferIds,
    outgoing: HashMap<u64, Outgoing>,
    incoming: HashMap<u64, Incoming>,
    /// Identifiers we asked the peer to fill, before any offer arrives. A
    /// refusal names one of these, and without them the failure would have
    /// nothing to resolve against and the requester would wait forever.
    pending: HashSet<u64>,
    max_size: u64,
}

impl std::fmt::Debug for TransferManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TransferManager({} out, {} in)",
            self.outgoing.len(),
            self.incoming.len()
        )
    }
}

impl TransferManager {
    /// Create a manager. `client_side` partitions the identifier space so the
    /// two ends of a connection never choose the same id.
    pub fn new(client_side: bool) -> Self {
        Self {
            ids: TransferIds::new(client_side),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            pending: HashSet::new(),
            max_size: MAX_TRANSFER_SIZE,
        }
    }

    /// Refuse offers larger than this.
    pub fn set_max_size(&mut self, max: u64) {
        self.max_size = max;
    }

    /// Number of transfers in flight, outgoing and incoming.
    pub fn in_flight(&self) -> (usize, usize) {
        (self.outgoing.len(), self.incoming.len())
    }

    /// Allocate an identifier, for a request the peer will answer.
    pub fn next_id(&mut self) -> u64 {
        self.ids.allocate()
    }

    /// Note that we have asked the peer for `id`, so that a refusal is
    /// reported rather than silently ignored.
    pub fn expect(&mut self, id: u64) {
        self.pending.insert(id);
    }

    /// Offer a blob held in memory. Returns the offer to send.
    pub fn offer_bytes(
        &mut self,
        purpose: TransferPurpose,
        name: String,
        data: Vec<u8>,
    ) -> Message {
        let id = self.ids.allocate();
        self.offer_stream_with_id(
            id,
            purpose,
            name,
            data.len() as u64,
            Box::new(io::Cursor::new(data)),
        )
    }

    /// Offer a stream of known length using a specific identifier. Answering
    /// a [`Message::FileRequest`] reuses the requester's id.
    pub fn offer_stream_with_id(
        &mut self,
        id: u64,
        purpose: TransferPurpose,
        name: String,
        size: u64,
        source: Box<dyn Read + Send>,
    ) -> Message {
        self.outgoing.insert(
            id,
            Outgoing {
                sender: TransferSender::new(id, size, source),
                purpose,
                name: name.clone(),
                accepted: false,
            },
        );
        Message::TransferOffer {
            id,
            purpose,
            name,
            size,
        }
    }

    /// Abandon a transfer in either direction.
    pub fn cancel(&mut self, id: u64, reason: &str) -> Option<Message> {
        let known = self.outgoing.remove(&id).is_some()
            | self.incoming.remove(&id).is_some()
            | self.pending.remove(&id);
        known.then(|| Message::TransferEnd {
            id,
            ok: false,
            message: reason.to_string(),
        })
    }

    /// Handle a transfer-related message. Returns `None` for messages that
    /// are not part of the transfer protocol, so callers can fall through.
    pub fn handle(&mut self, msg: &Message, policy: &mut dyn TransferPolicy) -> Option<Outcome> {
        let mut out = Outcome::default();
        match msg {
            Message::TransferOffer {
                id,
                purpose,
                name,
                size,
            } => {
                if *size > self.max_size {
                    out.replies.push(Message::TransferAccept {
                        id: *id,
                        accepted: false,
                        reason: format!("{size} bytes exceeds the limit"),
                    });
                    return Some(out);
                }
                self.pending.remove(id);
                match policy.accept(*id, *purpose, name, *size) {
                    Ok(sink) => {
                        // Memory limits are enforced here rather than in each
                        // policy. There are two policies, both of which took a
                        // clipboard image straight into memory while ignoring
                        // the size they were handed, and a rule that has to be
                        // repeated on both sides of a protocol is a rule that
                        // will eventually only be applied on one.
                        let in_memory = matches!(sink, Sink::Memory(_));
                        if in_memory {
                            if *size > MAX_MEMORY_TRANSFER_SIZE {
                                out.replies.push(Message::TransferAccept {
                                    id: *id,
                                    accepted: false,
                                    reason: format!(
                                        "{size} bytes is more than this side will hold in memory"
                                    ),
                                });
                                return Some(out);
                            }
                            let live = self.incoming.values().filter(|i| i.in_memory).count();
                            if live >= MAX_CONCURRENT_MEMORY_TRANSFERS {
                                out.replies.push(Message::TransferAccept {
                                    id: *id,
                                    accepted: false,
                                    reason: "too many in-memory transfers already in flight"
                                        .to_string(),
                                });
                                return Some(out);
                            }
                        }
                        self.incoming.insert(
                            *id,
                            Incoming {
                                receiver: TransferReceiver::new(*id, *size, sink),
                                purpose: *purpose,
                                name: name.clone(),
                                in_memory,
                            },
                        );
                        out.replies.push(Message::TransferAccept {
                            id: *id,
                            accepted: true,
                            reason: String::new(),
                        });
                        // A zero-length transfer has no chunks at all.
                        if *size == 0 {
                            self.complete_incoming(*id, &mut out);
                        }
                    }
                    Err(reason) => out.replies.push(Message::TransferAccept {
                        id: *id,
                        accepted: false,
                        reason,
                    }),
                }
            }
            Message::TransferAccept {
                id,
                accepted,
                reason,
            } => {
                let Some(o) = self.outgoing.get_mut(id) else {
                    return Some(out);
                };
                if !*accepted {
                    let reason = reason.clone();
                    self.outgoing.remove(id);
                    out.failed.push((*id, format!("peer declined: {reason}")));
                    return Some(out);
                }
                o.accepted = true;
                self.pump_one(*id, &mut out);
            }
            Message::TransferData { id, seq, data } => {
                let Some(i) = self.incoming.get_mut(id) else {
                    return Some(out);
                };
                match i.receiver.chunk(*seq, data) {
                    Ok(ack) => {
                        out.replies.push(Message::TransferAck { id: *id, seq: ack });
                        if i.receiver.is_complete() {
                            self.complete_incoming(*id, &mut out);
                        }
                    }
                    Err(e) => {
                        self.incoming.remove(id);
                        out.replies.push(Message::TransferEnd {
                            id: *id,
                            ok: false,
                            message: e.to_string(),
                        });
                        out.failed.push((*id, e.to_string()));
                    }
                }
            }
            Message::TransferAck { id, seq } => {
                if let Some(o) = self.outgoing.get_mut(id) {
                    o.sender.on_ack(*seq);
                }
                self.pump_one(*id, &mut out);
            }
            Message::TransferEnd { id, ok, message } => {
                let incoming = self.incoming.remove(id);
                let was_pending = self.pending.remove(id);
                let outgoing = self.outgoing.remove(id);
                if *ok {
                    // "ok" is the peer's claim, not a fact, and both directions
                    // have to check it against what actually moved.
                    //
                    // Still holding an incoming record here means the sender
                    // declared the transfer finished before its chunks arrived.
                    // Dropping the receiver at this point skipped `finish()`
                    // altogether, so the truncation check never ran: a short
                    // file was left at exactly the path the user asked for, no
                    // event was emitted, and the id stayed registered, so the
                    // requester waited on a transfer that had already ended.
                    if let Some(i) = incoming {
                        let (purpose, name) = (i.purpose, i.name);
                        match i.receiver.finish() {
                            Ok(_) => out.completed.push(Completed {
                                id: *id,
                                purpose,
                                name,
                                data: None,
                            }),
                            Err(e) => out.failed.push((*id, e.to_string())),
                        }
                    }
                    if let Some(o) = outgoing {
                        // Symmetrically: a peer that acknowledges more than we
                        // managed to send has not received what it thinks.
                        if o.sender.is_complete() {
                            out.sent.push((*id, o.purpose, o.name));
                        } else {
                            out.failed.push((
                                *id,
                                "peer reported success before the transfer finished".to_string(),
                            ));
                        }
                    }
                } else if incoming.is_some() || was_pending || outgoing.is_some() {
                    out.failed.push((*id, message.clone()));
                }
            }
            _ => return None,
        }
        Some(out)
    }

    fn complete_incoming(&mut self, id: u64, out: &mut Outcome) {
        let Some(i) = self.incoming.remove(&id) else {
            return;
        };
        let (purpose, name) = (i.purpose, i.name);
        match i.receiver.finish() {
            Ok(sink) => {
                let data = match sink {
                    Sink::Memory(v) => Some(v),
                    Sink::Stream(_) => None,
                };
                out.replies.push(Message::TransferEnd {
                    id,
                    ok: true,
                    message: String::new(),
                });
                out.completed.push(Completed {
                    id,
                    purpose,
                    name,
                    data,
                });
            }
            Err(e) => {
                out.replies.push(Message::TransferEnd {
                    id,
                    ok: false,
                    message: e.to_string(),
                });
                out.failed.push((id, e.to_string()));
            }
        }
    }

    /// Emit as many chunks for one transfer as its window allows.
    fn pump_one(&mut self, id: u64, out: &mut Outcome) {
        let Some(o) = self.outgoing.get_mut(&id) else {
            return;
        };
        if !o.accepted {
            return;
        }
        loop {
            match o.sender.next_chunk() {
                Ok(Some((seq, data))) => out.replies.push(Message::TransferData { id, seq, data }),
                Ok(None) => break,
                Err(e) => {
                    self.outgoing.remove(&id);
                    out.replies.push(Message::TransferEnd {
                        id,
                        ok: false,
                        message: e.to_string(),
                    });
                    out.failed.push((id, e.to_string()));
                    return;
                }
            }
        }
        // The record stays until the peer confirms with TransferEnd: that is
        // what tells us the bytes actually landed, and lets a late failure be
        // reported against the right transfer.
    }

    /// Describe a transfer in flight: its purpose, name and progress.
    /// Used to render progress for uploads and downloads.
    pub fn describe(&self, id: u64) -> Option<(TransferPurpose, &str, u64, u64)> {
        if let Some(o) = self.outgoing.get(&id) {
            let (done, total) = o.sender.progress();
            return Some((o.purpose, o.name.as_str(), done, total));
        }
        let i = self.incoming.get(&id)?;
        let (done, total) = i.receiver.progress();
        Some((i.purpose, i.name.as_str(), done, total))
    }

    /// Progress of one transfer, as (done, total).
    pub fn progress(&self, id: u64) -> Option<(u64, u64)> {
        if let Some(o) = self.outgoing.get(&id) {
            return Some(o.sender.progress());
        }
        self.incoming.get(&id).map(|i| i.receiver.progress())
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;

    /// Accepts everything into memory.
    struct MemoryPolicy;
    impl TransferPolicy for MemoryPolicy {
        fn accept(&mut self, _: u64, _: TransferPurpose, _: &str, _: u64) -> Result<Sink, String> {
            Ok(Sink::Memory(Vec::new()))
        }
    }

    /// Refuses everything.
    struct RefusePolicy;
    impl TransferPolicy for RefusePolicy {
        fn accept(&mut self, _: u64, _: TransferPurpose, _: &str, _: u64) -> Result<Sink, String> {
            Err("not accepting".into())
        }
    }

    /// Run both managers against each other until the queues drain.
    fn exchange(
        a: &mut TransferManager,
        b: &mut TransferManager,
        first: Message,
    ) -> (Vec<Completed>, Vec<Completed>, Vec<(u64, String)>) {
        let (mut done_a, mut done_b, mut failed) = (Vec::new(), Vec::new(), Vec::new());
        // (message, goes_to_b)
        let mut queue: Vec<(Message, bool)> = vec![(first, true)];
        let mut guard = 0;
        while let Some((msg, to_b)) = queue.pop() {
            guard += 1;
            assert!(guard < 100_000, "transfer did not settle");
            let (target, done): (&mut TransferManager, &mut Vec<Completed>) = if to_b {
                (b, &mut done_b)
            } else {
                (a, &mut done_a)
            };
            let mut policy = MemoryPolicy;
            if let Some(outcome) = target.handle(&msg, &mut policy) {
                done.extend(outcome.completed);
                failed.extend(outcome.failed);
                for reply in outcome.replies.into_iter().rev() {
                    queue.push((reply, !to_b));
                }
            }
        }
        (done_a, done_b, failed)
    }

    #[test]
    fn a_blob_moves_end_to_end() {
        let payload: Vec<u8> = (0..CHUNK_SIZE * 3 + 11).map(|i| (i % 253) as u8).collect();
        let mut client = TransferManager::new(true);
        let mut server = TransferManager::new(false);
        let offer = client.offer_bytes(
            TransferPurpose::ClipboardImage,
            "shot.png".into(),
            payload.clone(),
        );
        let (_, done_server, failed) = exchange(&mut client, &mut server, offer);
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(done_server.len(), 1);
        assert_eq!(done_server[0].data.as_deref(), Some(&payload[..]));
        assert_eq!(done_server[0].purpose, TransferPurpose::ClipboardImage);
        assert_eq!(done_server[0].name, "shot.png");
        // Both sides forgot about it.
        assert_eq!(client.in_flight(), (0, 0));
        assert_eq!(server.in_flight(), (0, 0));
    }

    #[test]
    fn the_sender_learns_when_the_peer_has_it_all() {
        let mut client = TransferManager::new(true);
        let mut server = TransferManager::new(false);
        let offer = client.offer_bytes(
            TransferPurpose::FileUpload,
            "report.pdf".into(),
            vec![5; 5000],
        );
        let Message::TransferOffer { id, .. } = offer.clone() else {
            unreachable!()
        };
        let mut policy = MemoryPolicy;
        // Drive both ends until they settle, collecting what the client saw.
        let mut queue = vec![(offer, true)];
        let mut sent = Vec::new();
        let mut guard = 0;
        while let Some((msg, to_server)) = queue.pop() {
            guard += 1;
            assert!(guard < 10_000);
            let target = if to_server { &mut server } else { &mut client };
            if let Some(out) = target.handle(&msg, &mut policy) {
                if !to_server {
                    sent.extend(out.sent);
                }
                for reply in out.replies.into_iter().rev() {
                    queue.push((reply, !to_server));
                }
            }
        }
        assert_eq!(sent.len(), 1, "upload completion was not reported");
        assert_eq!(sent[0].0, id);
        assert_eq!(sent[0].1, TransferPurpose::FileUpload);
        assert_eq!(sent[0].2, "report.pdf");
    }

    #[test]
    fn an_empty_blob_completes_without_chunks() {
        let mut client = TransferManager::new(true);
        let mut server = TransferManager::new(false);
        let offer = client.offer_bytes(TransferPurpose::ClipboardImage, "empty".into(), Vec::new());
        let (_, done_server, failed) = exchange(&mut client, &mut server, offer);
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(done_server.len(), 1);
        assert_eq!(done_server[0].data.as_deref(), Some(&[][..]));
    }

    #[test]
    fn a_declined_offer_is_reported_and_forgotten() {
        let mut client = TransferManager::new(true);
        let mut server = TransferManager::new(false);
        let offer = client.offer_bytes(TransferPurpose::FileUpload, "x".into(), vec![1, 2, 3]);
        let mut refuse = RefusePolicy;
        let reply = server.handle(&offer, &mut refuse).unwrap();
        assert_eq!(reply.replies.len(), 1);
        let mut policy = MemoryPolicy;
        let back = client.handle(&reply.replies[0], &mut policy).unwrap();
        assert_eq!(back.failed.len(), 1);
        assert!(back.failed[0].1.contains("declined"), "{:?}", back.failed);
        assert_eq!(client.in_flight(), (0, 0));
    }

    /// Neither side will hold an unbounded amount of a transfer in memory.
    ///
    /// Both endpoints' policies accepted a `ClipboardImage` straight into
    /// `Sink::Memory` while explicitly ignoring the offered size, and nothing
    /// capped how many were in flight, so a handful of 2 GiB offers pushed the
    /// peer out of memory. The limits live in the manager because a rule
    /// repeated in two policies on two sides of a protocol is one that
    /// eventually gets applied on only one of them.
    #[test]
    fn in_memory_transfers_are_bounded_in_size_and_number() {
        let mut m = TransferManager::new(true);
        let mut policy = MemoryPolicy;

        let offer = |id: u64, size: u64| Message::TransferOffer {
            id,
            purpose: TransferPurpose::ClipboardImage,
            name: "clip.png".into(),
            size,
        };
        let accepted = |out: &Outcome| {
            matches!(
                out.replies.as_slice(),
                [Message::TransferAccept { accepted: true, .. }]
            )
        };

        // Too large for memory, but well under MAX_TRANSFER_SIZE, so the old
        // size check let it straight through.
        let out = m
            .handle(&offer(1, MAX_MEMORY_TRANSFER_SIZE + 1), &mut policy)
            .unwrap();
        assert!(!accepted(&out), "an oversized in-memory offer was accepted");

        // The permitted number may be in flight at once...
        for id in 0..MAX_CONCURRENT_MEMORY_TRANSFERS as u64 {
            let out = m.handle(&offer(100 + id, 1024), &mut policy).unwrap();
            assert!(accepted(&out), "offer {id} within the budget was refused");
        }
        // ...and one more is not.
        let out = m.handle(&offer(200, 1024), &mut policy).unwrap();
        assert!(!accepted(&out), "the concurrency budget was not enforced");

        // Finishing one releases the budget again, on the ordinary path.
        m.handle(
            &Message::TransferData {
                id: 100,
                seq: 0,
                data: vec![0u8; 1024],
            },
            &mut policy,
        )
        .unwrap();
        let out = m.handle(&offer(201, 1024), &mut policy).unwrap();
        assert!(
            accepted(&out),
            "the budget was not released when a transfer completed"
        );
    }

    /// A peer claiming success mid-transfer must not produce a silent
    /// truncated file.
    ///
    /// `TransferEnd { ok: true }` used to drop the receiver without calling
    /// `finish()`, so the truncation check never ran. The short file was left
    /// at exactly the path the user asked for, nothing was reported, and the
    /// requester went on waiting for a transfer that had already ended.
    #[test]
    fn a_premature_success_is_reported_as_a_failure() {
        // `true` = the initiating side; irrelevant here, we only receive.
        let mut m = TransferManager::new(true);
        let mut policy = MemoryPolicy;
        let id = 1;
        let out = m
            .handle(
                &Message::TransferOffer {
                    id,
                    purpose: TransferPurpose::FileDownload,
                    name: "f.bin".into(),
                    size: 4096,
                },
                &mut policy,
            )
            .expect("offer handled");
        assert!(matches!(
            out.replies.as_slice(),
            [Message::TransferAccept { accepted: true, .. }]
        ));

        // One short chunk, then the sender claims it is done.
        m.handle(
            &Message::TransferData {
                id,
                seq: 0,
                data: vec![0u8; 16],
            },
            &mut policy,
        )
        .expect("chunk handled");
        let out = m
            .handle(
                &Message::TransferEnd {
                    id,
                    ok: true,
                    message: String::new(),
                },
                &mut policy,
            )
            .expect("end handled");

        assert!(
            out.completed.is_empty(),
            "a 16-byte prefix of a 4096-byte file was reported as complete"
        );
        assert_eq!(out.failed.len(), 1, "truncation was not reported: {out:?}");
    }

    #[test]
    fn an_oversized_offer_is_refused_before_any_data() {
        let mut server = TransferManager::new(false);
        server.set_max_size(1024);
        let offer = Message::TransferOffer {
            id: 1,
            purpose: TransferPurpose::FileUpload,
            name: "big".into(),
            size: 10_000,
        };
        let mut policy = MemoryPolicy;
        let out = server.handle(&offer, &mut policy).unwrap();
        assert!(matches!(
            out.replies.as_slice(),
            [Message::TransferAccept {
                accepted: false,
                ..
            }]
        ));
        assert_eq!(server.in_flight(), (0, 0));
    }

    #[test]
    fn a_corrupt_chunk_ends_the_transfer() {
        let mut server = TransferManager::new(false);
        let mut policy = MemoryPolicy;
        server
            .handle(
                &Message::TransferOffer {
                    id: 5,
                    purpose: TransferPurpose::ClipboardImage,
                    name: "x".into(),
                    size: 10,
                },
                &mut policy,
            )
            .unwrap();
        // Sequence 1 arrives before sequence 0.
        let out = server
            .handle(
                &Message::TransferData {
                    id: 5,
                    seq: 1,
                    data: vec![0; 4],
                },
                &mut policy,
            )
            .unwrap();
        assert!(matches!(
            out.replies.as_slice(),
            [Message::TransferEnd { ok: false, .. }]
        ));
        assert_eq!(out.failed.len(), 1);
        assert_eq!(server.in_flight(), (0, 0));
    }

    #[test]
    fn a_refused_request_is_reported_to_the_requester() {
        // The peer cannot produce what we asked for and answers with a bare
        // TransferEnd. Without expect(), that would resolve against nothing
        // and the requester would wait forever.
        let mut client = TransferManager::new(true);
        let id = client.next_id();
        client.expect(id);
        let mut policy = MemoryPolicy;
        let out = client
            .handle(
                &Message::TransferEnd {
                    id,
                    ok: false,
                    message: "/nope.txt: No such file or directory".into(),
                },
                &mut policy,
            )
            .unwrap();
        assert_eq!(out.failed.len(), 1);
        assert!(out.failed[0].1.contains("No such file"), "{:?}", out.failed);
        // A duplicate is not reported twice.
        let again = client
            .handle(
                &Message::TransferEnd {
                    id,
                    ok: false,
                    message: "again".into(),
                },
                &mut policy,
            )
            .unwrap();
        assert!(again.failed.is_empty());
    }

    #[test]
    fn unrelated_messages_fall_through() {
        let mut m = TransferManager::new(true);
        let mut policy = MemoryPolicy;
        assert!(m.handle(&Message::RefreshRequest, &mut policy).is_none());
        assert!(m.handle(&Message::Ping { nonce: 1 }, &mut policy).is_none());
    }

    #[test]
    fn cancel_reports_only_known_transfers() {
        let mut m = TransferManager::new(true);
        assert!(m.cancel(42, "gone").is_none());
        let offer = m.offer_bytes(TransferPurpose::FileUpload, "f".into(), vec![1, 2, 3]);
        let Message::TransferOffer { id, .. } = offer else {
            unreachable!()
        };
        assert!(m.cancel(id, "user cancelled").is_some());
        assert_eq!(m.in_flight(), (0, 0));
    }

    #[test]
    fn describe_reports_both_directions() {
        let mut client = TransferManager::new(true);
        let mut server = TransferManager::new(false);
        let offer = client.offer_bytes(
            TransferPurpose::FileUpload,
            "notes.txt".into(),
            vec![1; 4096],
        );
        let Message::TransferOffer { id, .. } = offer.clone() else {
            unreachable!()
        };
        // Sender side knows the name and total before anything is acked.
        let (purpose, name, done, total) = client.describe(id).unwrap();
        assert_eq!(purpose, TransferPurpose::FileUpload);
        assert_eq!(name, "notes.txt");
        assert_eq!((done, total), (0, 4096));
        // Receiver side knows them once the offer is accepted.
        let mut policy = MemoryPolicy;
        server.handle(&offer, &mut policy).unwrap();
        let (purpose, name, done, total) = server.describe(id).unwrap();
        assert_eq!(purpose, TransferPurpose::FileUpload);
        assert_eq!(name, "notes.txt");
        assert_eq!((done, total), (0, 4096));
        assert!(client.describe(9999).is_none());
    }

    #[test]
    fn data_for_an_unknown_transfer_is_ignored() {
        let mut m = TransferManager::new(false);
        let mut policy = MemoryPolicy;
        let out = m
            .handle(
                &Message::TransferData {
                    id: 999,
                    seq: 0,
                    data: vec![1],
                },
                &mut policy,
            )
            .unwrap();
        assert!(out.replies.is_empty());
        assert!(out.failed.is_empty());
    }
}
