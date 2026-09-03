//! Protocol messages.
//!
//! Every message is encoded as a `u8` kind tag followed by a kind-specific
//! payload. See [`crate::frame`] for how messages are delimited on a
//! stream.
//!
//! ## Handshake
//!
//! 1. Client sends [`Message::ClientHello`].
//! 2. Server answers with [`Message::ServerHello`] (accepted) or
//!    [`Message::Rejected`] followed by closing the connection.
//! 3. Server immediately sends a full [`Message::ScreenUpdate`].
//!
//! ## Flow control
//!
//! The server tags each [`Message::ScreenUpdate`] with a monotonically
//! increasing `frame_id`. The client answers each one with
//! [`Message::FrameAck`] once it has been painted. The server keeps at most a
//! small number of unacknowledged frames in flight and coalesces screen
//! changes in the meantime, so a slow link never builds up a queue of stale
//! frames — that is what keeps interaction latency low.

use crate::codec::{CopyRect, TileEncoding, TileUpdate};
use crate::image::Rect;
use crate::transfer::{FileEntry, TransferPurpose, CHUNK_SIZE, MAX_FILE_LIST, MAX_TRANSFER_SIZE};
use crate::wire::{DecodeError, Reader, Writer};

/// Feature bits advertised in the hello messages.
pub mod features {
    /// Client renders the pointer locally from [`super::Message::CursorShape`].
    pub const LOCAL_CURSOR: u32 = 1 << 0;
    /// Clipboard text synchronisation.
    pub const CLIPBOARD: u32 = 1 << 1;
    /// Dynamic screen resizing.
    pub const RESIZE: u32 = 1 << 2;
    /// Images on the clipboard, transferred on demand as PNG.
    pub const CLIPBOARD_IMAGE: u32 = 1 << 3;
    /// File transfer in either direction.
    pub const FILE_TRANSFER: u32 = 1 << 4;
    /// Files on the clipboard (copy in one place, paste in the other).
    pub const CLIPBOARD_FILES: u32 = 1 << 5;
}

/// Clipboard content types, as a bitmask in [`super::Message::ClipboardOffer`].
pub mod clipboard_format {
    /// UTF-8 text, sent eagerly because it is small.
    pub const TEXT: u32 = 1 << 0;
    /// A PNG image, fetched on demand.
    pub const PNG: u32 = 1 << 1;
    /// One or more files, fetched on demand.
    pub const FILES: u32 = 1 << 2;
}

/// Reasons a server refuses a client.
pub mod reject {
    /// Protocol version mismatch.
    pub const VERSION: u16 = 1;
    /// Peer could not be identified or is not allowed.
    pub const UNAUTHORIZED: u16 = 2;
    /// Session could not be started.
    pub const SESSION_FAILED: u16 = 3;
    /// Server is shutting down or busy.
    pub const UNAVAILABLE: u16 = 4;
}

/// Pointer button identifiers (X11 numbering).
pub mod button {
    /// Left button.
    pub const LEFT: u8 = 1;
    /// Middle button.
    pub const MIDDLE: u8 = 2;
    /// Right button.
    pub const RIGHT: u8 = 3;
    /// Fourth (back) button.
    pub const BACK: u8 = 8;
    /// Fifth (forward) button.
    pub const FORWARD: u8 = 9;
}

/// Kind tags. Client-originated kinds are below 32, server-originated above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// [`Message::ClientHello`]
    ClientHello = 1,
    /// [`Message::KeyEvent`]
    KeyEvent = 2,
    /// [`Message::PointerMotion`]
    PointerMotion = 3,
    /// [`Message::PointerButton`]
    PointerButton = 4,
    /// [`Message::Scroll`]
    Scroll = 5,
    /// [`Message::FrameAck`]
    FrameAck = 6,
    /// [`Message::ResizeRequest`]
    ResizeRequest = 7,
    /// [`Message::ClipboardText`]
    ClipboardText = 8,
    /// [`Message::Pong`]
    Pong = 9,
    /// [`Message::Disconnect`]
    Disconnect = 10,
    /// [`Message::RefreshRequest`]
    RefreshRequest = 11,
    /// [`Message::ServerHello`]
    ServerHello = 32,
    /// [`Message::Rejected`]
    Rejected = 33,
    /// [`Message::ScreenUpdate`]
    ScreenUpdate = 34,
    /// [`Message::ScreenResized`]
    ScreenResized = 35,
    /// [`Message::CursorShape`]
    CursorShape = 36,
    /// [`Message::CursorPosition`]
    CursorPosition = 37,
    /// [`Message::Ping`]
    Ping = 38,
    /// [`Message::Notice`]
    Notice = 39,
    /// [`Message::TransferOffer`]
    TransferOffer = 64,
    /// [`Message::TransferAccept`]
    TransferAccept = 65,
    /// [`Message::TransferData`]
    TransferData = 66,
    /// [`Message::TransferAck`]
    TransferAck = 67,
    /// [`Message::TransferEnd`]
    TransferEnd = 68,
    /// [`Message::FileRequest`]
    FileRequest = 69,
    /// [`Message::FileList`]
    FileList = 70,
    /// [`Message::ClipboardOffer`]
    ClipboardOffer = 71,
    /// [`Message::ClipboardRequest`]
    ClipboardRequest = 72,
}

impl Kind {
    /// Convert from the wire tag.
    pub fn from_u8(v: u8) -> Result<Self, DecodeError> {
        Ok(match v {
            1 => Kind::ClientHello,
            2 => Kind::KeyEvent,
            3 => Kind::PointerMotion,
            4 => Kind::PointerButton,
            5 => Kind::Scroll,
            6 => Kind::FrameAck,
            7 => Kind::ResizeRequest,
            8 => Kind::ClipboardText,
            9 => Kind::Pong,
            10 => Kind::Disconnect,
            11 => Kind::RefreshRequest,
            32 => Kind::ServerHello,
            33 => Kind::Rejected,
            34 => Kind::ScreenUpdate,
            35 => Kind::ScreenResized,
            36 => Kind::CursorShape,
            37 => Kind::CursorPosition,
            38 => Kind::Ping,
            39 => Kind::Notice,
            64 => Kind::TransferOffer,
            65 => Kind::TransferAccept,
            66 => Kind::TransferData,
            67 => Kind::TransferAck,
            68 => Kind::TransferEnd,
            69 => Kind::FileRequest,
            70 => Kind::FileList,
            71 => Kind::ClipboardOffer,
            72 => Kind::ClipboardRequest,
            other => return Err(DecodeError::InvalidTag(u32::from(other))),
        })
    }
}

/// Cursor image sent by the server when the pointer shape changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    /// Width in pixels (0 means hidden cursor).
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// Hotspot X.
    pub hot_x: u16,
    /// Hotspot Y.
    pub hot_y: u16,
    /// Premultiplied ARGB pixels, `0xAARRGGBB`, row major.
    pub argb: Vec<u32>,
}

/// A protocol message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// First message from the client.
    ClientHello {
        /// [`crate::PROTOCOL_VERSION`] of the client.
        version: u16,
        /// Human readable client identification.
        client_name: String,
        /// Feature bits ([`features`]).
        features: u32,
        /// Desired screen width (0 = server default).
        width: u16,
        /// Desired screen height (0 = server default).
        height: u16,
    },
    /// Server accepts the client.
    ServerHello {
        /// Server protocol version.
        version: u16,
        /// Human readable server identification.
        server_name: String,
        /// Feature bits the server will use.
        features: u32,
        /// Session identifier (stable across reconnects).
        session_id: u64,
        /// Name of the user the session belongs to.
        username: String,
        /// Current screen width.
        width: u16,
        /// Current screen height.
        height: u16,
    },
    /// Server refuses the client and will close the connection.
    Rejected {
        /// One of [`reject`].
        code: u16,
        /// Human readable explanation.
        reason: String,
    },
    /// Keyboard key press or release.
    KeyEvent {
        /// X11 keysym.
        keysym: u32,
        /// True for press, false for release.
        down: bool,
    },
    /// Absolute pointer motion.
    PointerMotion {
        /// X coordinate on the remote screen.
        x: u16,
        /// Y coordinate on the remote screen.
        y: u16,
    },
    /// Pointer button press or release.
    PointerButton {
        /// X11 button number ([`button`]).
        button: u8,
        /// True for press.
        down: bool,
    },
    /// Scroll wheel movement in whole detents.
    Scroll {
        /// Horizontal detents (positive = right).
        dx: i16,
        /// Vertical detents (positive = down / away from the user).
        dy: i16,
    },
    /// The client has painted the given frame.
    FrameAck {
        /// Frame being acknowledged.
        frame_id: u64,
    },
    /// The client asks the server to change the screen size.
    ResizeRequest {
        /// Requested width.
        width: u16,
        /// Requested height.
        height: u16,
    },
    /// Clipboard text in either direction.
    ClipboardText {
        /// UTF-8 text.
        text: String,
    },
    /// Latency probe from the server.
    Ping {
        /// Opaque value echoed by the client.
        nonce: u64,
    },
    /// Reply to [`Message::Ping`].
    Pong {
        /// Echoed nonce.
        nonce: u64,
    },
    /// Graceful shutdown of the connection by either side.
    Disconnect {
        /// Human readable reason.
        reason: String,
    },
    /// The client asks for a full screen retransmission.
    RefreshRequest,
    /// A batch of screen changes: regions moved from elsewhere in the
    /// previous frame, then freshly encoded tiles.
    ScreenUpdate {
        /// Monotonic frame identifier.
        frame_id: u64,
        /// Regions copied from the previous frame. Applied before `tiles`.
        copies: Vec<CopyRect>,
        /// Changed rectangles.
        tiles: Vec<TileUpdate>,
    },
    /// The screen size changed; a full update follows.
    ScreenResized {
        /// New width.
        width: u16,
        /// New height.
        height: u16,
    },
    /// The pointer shape changed.
    CursorShape {
        /// The new shape.
        cursor: CursorImage,
    },
    /// The pointer moved on the server without client input (or the
    /// client should warp its pointer).
    CursorPosition {
        /// X coordinate.
        x: u16,
        /// Y coordinate.
        y: u16,
    },
    /// Informational text for the user (shown by the client).
    Notice {
        /// Text.
        text: String,
    },

    // ---- transfers (either side may originate one) ----
    /// Offer to send a blob. The receiver answers [`Message::TransferAccept`].
    TransferOffer {
        /// Identifier, unique within the connection.
        id: u64,
        /// What the blob is for.
        purpose: TransferPurpose,
        /// Suggested name; for uploads, the destination path.
        name: String,
        /// Total size in bytes.
        size: u64,
    },
    /// Answer to a [`Message::TransferOffer`].
    TransferAccept {
        /// Transfer being answered.
        id: u64,
        /// Whether the receiver wants it.
        accepted: bool,
        /// Why not, when declined.
        reason: String,
    },
    /// One chunk of a transfer.
    TransferData {
        /// Transfer this belongs to.
        id: u64,
        /// Chunk index, starting at zero.
        seq: u32,
        /// Payload, at most `CHUNK_SIZE` bytes.
        data: Vec<u8>,
    },
    /// Acknowledge a chunk, freeing a slot in the sender's window.
    TransferAck {
        /// Transfer being acknowledged.
        id: u64,
        /// Chunk index that arrived.
        seq: u32,
    },
    /// End a transfer, successfully or not. Either side may send this.
    TransferEnd {
        /// Transfer being ended.
        id: u64,
        /// Whether it completed successfully.
        ok: bool,
        /// Explanation when it did not.
        message: String,
    },
    /// Ask the peer to send a file. It replies with a
    /// [`Message::TransferOffer`] carrying the same `id`, or a
    /// [`Message::TransferEnd`] explaining why not.
    FileRequest {
        /// Identifier the peer should use for the resulting transfer.
        id: u64,
        /// Path to read, as the session user.
        path: String,
    },
    /// The files behind a clipboard file copy or a drag-and-drop batch.
    FileList {
        /// Batch identifier; individual files are fetched with
        /// [`Message::FileRequest`] using paths from this listing.
        id: u64,
        /// The files.
        files: Vec<FileEntry>,
    },
    /// Announce which clipboard formats are now available. Large formats
    /// are not sent until the peer asks with [`Message::ClipboardRequest`].
    ClipboardOffer {
        /// Bitmask of [`clipboard_format`].
        formats: u32,
    },
    /// Ask for one clipboard format previously announced.
    ClipboardRequest {
        /// A single bit of [`clipboard_format`].
        format: u32,
    },
}

/// Maximum number of tiles accepted in one screen update.
pub const MAX_TILES_PER_UPDATE: usize = 65_536;
/// Maximum number of copy rectangles accepted in one screen update.
pub const MAX_COPIES_PER_UPDATE: usize = 4_096;
/// Maximum cursor dimension accepted.
pub const MAX_CURSOR_DIM: u16 = 256;

impl Message {
    /// Kind tag of this message.
    pub fn kind(&self) -> Kind {
        match self {
            Message::ClientHello { .. } => Kind::ClientHello,
            Message::ServerHello { .. } => Kind::ServerHello,
            Message::Rejected { .. } => Kind::Rejected,
            Message::KeyEvent { .. } => Kind::KeyEvent,
            Message::PointerMotion { .. } => Kind::PointerMotion,
            Message::PointerButton { .. } => Kind::PointerButton,
            Message::Scroll { .. } => Kind::Scroll,
            Message::FrameAck { .. } => Kind::FrameAck,
            Message::ResizeRequest { .. } => Kind::ResizeRequest,
            Message::ClipboardText { .. } => Kind::ClipboardText,
            Message::Ping { .. } => Kind::Ping,
            Message::Pong { .. } => Kind::Pong,
            Message::Disconnect { .. } => Kind::Disconnect,
            Message::RefreshRequest => Kind::RefreshRequest,
            Message::ScreenUpdate { .. } => Kind::ScreenUpdate,
            Message::ScreenResized { .. } => Kind::ScreenResized,
            Message::CursorShape { .. } => Kind::CursorShape,
            Message::CursorPosition { .. } => Kind::CursorPosition,
            Message::Notice { .. } => Kind::Notice,
            Message::TransferOffer { .. } => Kind::TransferOffer,
            Message::TransferAccept { .. } => Kind::TransferAccept,
            Message::TransferData { .. } => Kind::TransferData,
            Message::TransferAck { .. } => Kind::TransferAck,
            Message::TransferEnd { .. } => Kind::TransferEnd,
            Message::FileRequest { .. } => Kind::FileRequest,
            Message::FileList { .. } => Kind::FileList,
            Message::ClipboardOffer { .. } => Kind::ClipboardOffer,
            Message::ClipboardRequest { .. } => Kind::ClipboardRequest,
        }
    }

    /// Serialise to bytes: kind tag followed by payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(self.size_hint());
        self.encode_into(&mut w);
        w.into_inner()
    }

    fn size_hint(&self) -> usize {
        match self {
            Message::ScreenUpdate { copies, tiles, .. } => {
                16 + copies.len() * 12 + tiles.iter().map(|t| t.data.len() + 16).sum::<usize>()
            }
            Message::CursorShape { cursor } => 16 + cursor.argb.len() * 4,
            Message::ClipboardText { text } => 8 + text.len(),
            Message::TransferData { data, .. } => 24 + data.len(),
            Message::FileList { files, .. } => 16 + files.len() * 64,
            _ => 32,
        }
    }

    /// Serialise into an existing writer.
    pub fn encode_into(&self, w: &mut Writer) {
        w.u8(self.kind() as u8);
        match self {
            Message::ClientHello {
                version,
                client_name,
                features,
                width,
                height,
            } => {
                w.u16(*version);
                w.string(client_name);
                w.u32(*features);
                w.u16(*width);
                w.u16(*height);
            }
            Message::ServerHello {
                version,
                server_name,
                features,
                session_id,
                username,
                width,
                height,
            } => {
                w.u16(*version);
                w.string(server_name);
                w.u32(*features);
                w.u64(*session_id);
                w.string(username);
                w.u16(*width);
                w.u16(*height);
            }
            Message::Rejected { code, reason } => {
                w.u16(*code);
                w.string(reason);
            }
            Message::KeyEvent { keysym, down } => {
                w.u32(*keysym);
                w.bool(*down);
            }
            Message::PointerMotion { x, y } => {
                w.u16(*x);
                w.u16(*y);
            }
            Message::PointerButton { button, down } => {
                w.u8(*button);
                w.bool(*down);
            }
            Message::Scroll { dx, dy } => {
                w.i16(*dx);
                w.i16(*dy);
            }
            Message::FrameAck { frame_id } => w.u64(*frame_id),
            Message::ResizeRequest { width, height } => {
                w.u16(*width);
                w.u16(*height);
            }
            Message::ClipboardText { text } => w.string(text),
            Message::Ping { nonce } | Message::Pong { nonce } => w.u64(*nonce),
            Message::Disconnect { reason } => w.string(reason),
            Message::RefreshRequest => {}
            Message::ScreenUpdate {
                frame_id,
                copies,
                tiles,
            } => {
                w.u64(*frame_id);
                w.u32(copies.len() as u32);
                for c in copies {
                    w.u16(c.src_x as u16);
                    w.u16(c.src_y as u16);
                    w.u16(c.dest.x as u16);
                    w.u16(c.dest.y as u16);
                    w.u16(c.dest.width as u16);
                    w.u16(c.dest.height as u16);
                }
                w.u32(tiles.len() as u32);
                for t in tiles {
                    w.u16(t.rect.x as u16);
                    w.u16(t.rect.y as u16);
                    w.u16(t.rect.width as u16);
                    w.u16(t.rect.height as u16);
                    w.u8(t.encoding as u8);
                    w.bytes(&t.data);
                }
            }
            Message::ScreenResized { width, height } => {
                w.u16(*width);
                w.u16(*height);
            }
            Message::CursorShape { cursor } => {
                w.u16(cursor.width);
                w.u16(cursor.height);
                w.u16(cursor.hot_x);
                w.u16(cursor.hot_y);
                w.u32(cursor.argb.len() as u32);
                for &p in &cursor.argb {
                    w.u32(p);
                }
            }
            Message::CursorPosition { x, y } => {
                w.u16(*x);
                w.u16(*y);
            }
            Message::Notice { text } => w.string(text),
            Message::TransferOffer {
                id,
                purpose,
                name,
                size,
            } => {
                w.u64(*id);
                w.u8(*purpose as u8);
                w.string(name);
                w.u64(*size);
            }
            Message::TransferAccept {
                id,
                accepted,
                reason,
            } => {
                w.u64(*id);
                w.bool(*accepted);
                w.string(reason);
            }
            Message::TransferData { id, seq, data } => {
                w.u64(*id);
                w.u32(*seq);
                w.bytes(data);
            }
            Message::TransferAck { id, seq } => {
                w.u64(*id);
                w.u32(*seq);
            }
            Message::TransferEnd { id, ok, message } => {
                w.u64(*id);
                w.bool(*ok);
                w.string(message);
            }
            Message::FileRequest { id, path } => {
                w.u64(*id);
                w.string(path);
            }
            Message::FileList { id, files } => {
                w.u64(*id);
                w.u32(files.len() as u32);
                for f in files {
                    w.string(&f.path);
                    w.u64(f.size);
                }
            }
            Message::ClipboardOffer { formats } => w.u32(*formats),
            Message::ClipboardRequest { format } => w.u32(*format),
        }
    }

    /// Parse a message from bytes produced by [`Message::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Message, DecodeError> {
        let mut r = Reader::new(bytes);
        let kind = Kind::from_u8(r.u8()?)?;
        let msg = match kind {
            Kind::ClientHello => Message::ClientHello {
                version: r.u16()?,
                client_name: r.string()?,
                features: r.u32()?,
                width: r.u16()?,
                height: r.u16()?,
            },
            Kind::ServerHello => Message::ServerHello {
                version: r.u16()?,
                server_name: r.string()?,
                features: r.u32()?,
                session_id: r.u64()?,
                username: r.string()?,
                width: r.u16()?,
                height: r.u16()?,
            },
            Kind::Rejected => Message::Rejected {
                code: r.u16()?,
                reason: r.string()?,
            },
            Kind::KeyEvent => Message::KeyEvent {
                keysym: r.u32()?,
                down: r.bool()?,
            },
            Kind::PointerMotion => Message::PointerMotion {
                x: r.u16()?,
                y: r.u16()?,
            },
            Kind::PointerButton => Message::PointerButton {
                button: r.u8()?,
                down: r.bool()?,
            },
            Kind::Scroll => Message::Scroll {
                dx: r.i16()?,
                dy: r.i16()?,
            },
            Kind::FrameAck => Message::FrameAck { frame_id: r.u64()? },
            Kind::ResizeRequest => Message::ResizeRequest {
                width: r.u16()?,
                height: r.u16()?,
            },
            Kind::ClipboardText => Message::ClipboardText { text: r.string()? },
            Kind::Ping => Message::Ping { nonce: r.u64()? },
            Kind::Pong => Message::Pong { nonce: r.u64()? },
            Kind::Disconnect => Message::Disconnect {
                reason: r.string()?,
            },
            Kind::RefreshRequest => Message::RefreshRequest,
            Kind::ScreenUpdate => {
                let frame_id = r.u64()?;
                let n_copies = r.u32()? as usize;
                if n_copies > MAX_COPIES_PER_UPDATE {
                    return Err(DecodeError::LengthTooLarge(n_copies));
                }
                if n_copies.saturating_mul(12) > r.remaining() {
                    return Err(DecodeError::UnexpectedEof {
                        needed: n_copies * 12,
                        remaining: r.remaining(),
                    });
                }
                let mut copies = Vec::with_capacity(n_copies);
                for _ in 0..n_copies {
                    let src_x = u32::from(r.u16()?);
                    let src_y = u32::from(r.u16()?);
                    let x = u32::from(r.u16()?);
                    let y = u32::from(r.u16()?);
                    let width = u32::from(r.u16()?);
                    let height = u32::from(r.u16()?);
                    if width == 0 || height == 0 {
                        return Err(DecodeError::InvalidValue("copy size"));
                    }
                    copies.push(CopyRect {
                        src_x,
                        src_y,
                        dest: Rect::new(x, y, width, height),
                    });
                }
                let n = r.u32()? as usize;
                if n > MAX_TILES_PER_UPDATE {
                    return Err(DecodeError::LengthTooLarge(n));
                }
                // Each tile needs at least 13 bytes; refuse absurd counts early.
                if n.saturating_mul(13) > r.remaining() {
                    return Err(DecodeError::UnexpectedEof {
                        needed: n * 13,
                        remaining: r.remaining(),
                    });
                }
                let mut tiles = Vec::with_capacity(n);
                for _ in 0..n {
                    let x = u32::from(r.u16()?);
                    let y = u32::from(r.u16()?);
                    let width = u32::from(r.u16()?);
                    let height = u32::from(r.u16()?);
                    if width == 0 || height == 0 {
                        return Err(DecodeError::InvalidValue("tile size"));
                    }
                    let encoding = TileEncoding::from_u8(r.u8()?)?;
                    let data = r.bytes()?.to_vec();
                    tiles.push(TileUpdate {
                        rect: Rect::new(x, y, width, height),
                        encoding,
                        data,
                    });
                }
                Message::ScreenUpdate {
                    frame_id,
                    copies,
                    tiles,
                }
            }
            Kind::ScreenResized => Message::ScreenResized {
                width: r.u16()?,
                height: r.u16()?,
            },
            Kind::CursorShape => {
                let width = r.u16()?;
                let height = r.u16()?;
                let hot_x = r.u16()?;
                let hot_y = r.u16()?;
                if width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
                    return Err(DecodeError::InvalidValue("cursor size"));
                }
                let n = r.u32()? as usize;
                if n != usize::from(width) * usize::from(height) {
                    return Err(DecodeError::InvalidValue("cursor pixel count"));
                }
                let mut argb = Vec::with_capacity(n);
                for _ in 0..n {
                    argb.push(r.u32()?);
                }
                Message::CursorShape {
                    cursor: CursorImage {
                        width,
                        height,
                        hot_x,
                        hot_y,
                        argb,
                    },
                }
            }
            Kind::CursorPosition => Message::CursorPosition {
                x: r.u16()?,
                y: r.u16()?,
            },
            Kind::Notice => Message::Notice { text: r.string()? },
            Kind::TransferOffer => {
                let id = r.u64()?;
                let purpose = TransferPurpose::from_u8(r.u8()?)
                    .ok_or(DecodeError::InvalidValue("transfer purpose"))?;
                let name = r.string()?;
                let size = r.u64()?;
                if size > MAX_TRANSFER_SIZE {
                    return Err(DecodeError::InvalidValue("transfer size"));
                }
                Message::TransferOffer {
                    id,
                    purpose,
                    name,
                    size,
                }
            }
            Kind::TransferAccept => Message::TransferAccept {
                id: r.u64()?,
                accepted: r.bool()?,
                reason: r.string()?,
            },
            Kind::TransferData => {
                let id = r.u64()?;
                let seq = r.u32()?;
                let data = r.bytes()?;
                if data.len() > CHUNK_SIZE {
                    return Err(DecodeError::InvalidValue("transfer chunk size"));
                }
                Message::TransferData {
                    id,
                    seq,
                    data: data.to_vec(),
                }
            }
            Kind::TransferAck => Message::TransferAck {
                id: r.u64()?,
                seq: r.u32()?,
            },
            Kind::TransferEnd => Message::TransferEnd {
                id: r.u64()?,
                ok: r.bool()?,
                message: r.string()?,
            },
            Kind::FileRequest => Message::FileRequest {
                id: r.u64()?,
                path: r.string()?,
            },
            Kind::FileList => {
                let id = r.u64()?;
                let n = r.u32()? as usize;
                if n > MAX_FILE_LIST {
                    return Err(DecodeError::LengthTooLarge(n));
                }
                // Each entry needs at least 12 bytes; refuse absurd counts
                // before allocating for them.
                if n.saturating_mul(12) > r.remaining() {
                    return Err(DecodeError::UnexpectedEof {
                        needed: n * 12,
                        remaining: r.remaining(),
                    });
                }
                let mut files = Vec::with_capacity(n);
                for _ in 0..n {
                    files.push(FileEntry {
                        path: r.string()?,
                        size: r.u64()?,
                    });
                }
                Message::FileList { id, files }
            }
            Kind::ClipboardOffer => Message::ClipboardOffer { formats: r.u32()? },
            Kind::ClipboardRequest => Message::ClipboardRequest { format: r.u32()? },
        };
        r.finish()?;
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn all_samples() -> Vec<Message> {
        vec![
            Message::ClientHello {
                version: 1,
                client_name: "test".into(),
                features: 7,
                width: 1920,
                height: 1080,
            },
            Message::ServerHello {
                version: 1,
                server_name: "srv".into(),
                features: 3,
                session_id: 42,
                username: "alice".into(),
                width: 800,
                height: 600,
            },
            Message::Rejected {
                code: reject::VERSION,
                reason: "nope".into(),
            },
            Message::KeyEvent {
                keysym: 0xff0d,
                down: true,
            },
            Message::PointerMotion { x: 1, y: 2 },
            Message::PointerButton {
                button: button::LEFT,
                down: false,
            },
            Message::Scroll { dx: -1, dy: 3 },
            Message::FrameAck { frame_id: u64::MAX },
            Message::ResizeRequest {
                width: 1,
                height: 1,
            },
            Message::ClipboardText {
                text: "clip ✂".into(),
            },
            Message::Ping { nonce: 9 },
            Message::Pong { nonce: 9 },
            Message::Disconnect {
                reason: "bye".into(),
            },
            Message::RefreshRequest,
            Message::ScreenUpdate {
                frame_id: 3,
                copies: vec![
                    CopyRect {
                        src_x: 0,
                        src_y: 40,
                        dest: Rect::new(0, 0, 320, 200),
                    },
                    CopyRect {
                        src_x: 5,
                        src_y: 5,
                        dest: Rect::new(9, 9, 1, 1),
                    },
                ],
                tiles: vec![
                    TileUpdate {
                        rect: Rect::new(0, 0, 2, 2),
                        encoding: TileEncoding::Solid,
                        data: vec![1, 2, 3],
                    },
                    TileUpdate {
                        rect: Rect::new(64, 64, 1, 1),
                        encoding: TileEncoding::Raw,
                        data: vec![9, 9, 9],
                    },
                ],
            },
            Message::ScreenResized {
                width: 10,
                height: 20,
            },
            Message::CursorShape {
                cursor: CursorImage {
                    width: 2,
                    height: 1,
                    hot_x: 0,
                    hot_y: 1,
                    argb: vec![1, 2],
                },
            },
            Message::CursorPosition { x: 5, y: 6 },
            Message::Notice {
                text: "hello".into(),
            },
            Message::TransferOffer {
                id: 7,
                purpose: TransferPurpose::ClipboardImage,
                name: "screenshot.png".into(),
                size: 4096,
            },
            Message::TransferAccept {
                id: 7,
                accepted: true,
                reason: String::new(),
            },
            Message::TransferAccept {
                id: 8,
                accepted: false,
                reason: "too large".into(),
            },
            Message::TransferData {
                id: 7,
                seq: 3,
                data: vec![9; 128],
            },
            Message::TransferAck { id: 7, seq: 3 },
            Message::TransferEnd {
                id: 7,
                ok: true,
                message: String::new(),
            },
            Message::FileRequest {
                id: 9,
                path: "/home/alice/notes.md".into(),
            },
            Message::FileList {
                id: 9,
                files: vec![
                    FileEntry {
                        path: "a.txt".into(),
                        size: 12,
                    },
                    FileEntry {
                        path: "sub/b.bin".into(),
                        size: u64::MAX / 2,
                    },
                ],
            },
            Message::ClipboardOffer {
                formats: clipboard_format::TEXT | clipboard_format::PNG,
            },
            Message::ClipboardRequest {
                format: clipboard_format::PNG,
            },
        ]
    }

    #[test]
    fn roundtrip_all_messages() {
        for m in all_samples() {
            let bytes = m.encode();
            let back = Message::decode(&bytes).unwrap();
            assert_eq!(m, back, "roundtrip of {:?}", m.kind());
            assert_eq!(back.kind(), m.kind());
        }
    }

    #[test]
    fn kind_tags_roundtrip() {
        for m in all_samples() {
            assert_eq!(Kind::from_u8(m.kind() as u8).unwrap(), m.kind());
        }
        assert!(Kind::from_u8(0).is_err());
        assert!(Kind::from_u8(200).is_err());
    }

    #[test]
    fn truncated_messages_fail_cleanly() {
        for m in all_samples() {
            let bytes = m.encode();
            for cut in 0..bytes.len() {
                assert!(
                    Message::decode(&bytes[..cut]).is_err(),
                    "cut {cut} of {:?}",
                    m.kind()
                );
            }
        }
    }

    #[test]
    fn trailing_bytes_rejected() {
        let mut bytes = Message::RefreshRequest.encode();
        bytes.push(0);
        assert_eq!(Message::decode(&bytes), Err(DecodeError::TrailingBytes(1)));
    }

    #[test]
    fn zero_sized_copy_rejected() {
        let m = Message::ScreenUpdate {
            frame_id: 1,
            copies: vec![CopyRect {
                src_x: 0,
                src_y: 0,
                dest: Rect::new(0, 0, 4, 0),
            }],
            tiles: vec![],
        };
        assert_eq!(
            Message::decode(&m.encode()),
            Err(DecodeError::InvalidValue("copy size"))
        );
    }

    #[test]
    fn absurd_copy_count_rejected() {
        let mut w = Writer::new();
        w.u8(Kind::ScreenUpdate as u8);
        w.u64(1);
        w.u32(100_000);
        assert!(matches!(
            Message::decode(w.as_slice()),
            Err(DecodeError::LengthTooLarge(_))
        ));
        let mut w = Writer::new();
        w.u8(Kind::ScreenUpdate as u8);
        w.u64(1);
        w.u32(1_000);
        assert!(matches!(
            Message::decode(w.as_slice()),
            Err(DecodeError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn zero_sized_tile_rejected() {
        let m = Message::ScreenUpdate {
            frame_id: 1,
            copies: vec![],
            tiles: vec![TileUpdate {
                rect: Rect::new(0, 0, 0, 1),
                encoding: TileEncoding::Solid,
                data: vec![0, 0, 0],
            }],
        };
        assert_eq!(
            Message::decode(&m.encode()),
            Err(DecodeError::InvalidValue("tile size"))
        );
    }

    #[test]
    fn absurd_tile_count_rejected() {
        let mut w = Writer::new();
        w.u8(Kind::ScreenUpdate as u8);
        w.u64(1);
        w.u32(0);
        w.u32(1_000_000);
        assert!(Message::decode(w.as_slice()).is_err());
        let mut w = Writer::new();
        w.u8(Kind::ScreenUpdate as u8);
        w.u64(1);
        w.u32(0);
        w.u32(10_000);
        assert!(matches!(
            Message::decode(w.as_slice()),
            Err(DecodeError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn oversized_cursor_rejected() {
        let mut w = Writer::new();
        w.u8(Kind::CursorShape as u8);
        w.u16(1000);
        w.u16(1000);
        w.u16(0);
        w.u16(0);
        w.u32(1_000_000);
        assert_eq!(
            Message::decode(w.as_slice()),
            Err(DecodeError::InvalidValue("cursor size"))
        );
    }

    proptest! {
        #[test]
        fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = Message::decode(&bytes);
        }

        #[test]
        fn key_event_roundtrip(keysym in any::<u32>(), down in any::<bool>()) {
            let m = Message::KeyEvent { keysym, down };
            prop_assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }

        #[test]
        fn clipboard_roundtrip(text in ".{0,200}") {
            let m = Message::ClipboardText { text: text.clone() };
            prop_assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }
    }
}
