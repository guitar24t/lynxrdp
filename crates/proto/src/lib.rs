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
pub mod wire;

/// Protocol version spoken by this build. Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Default TCP port the server listens on (loopback only).
pub const DEFAULT_PORT: u16 = 3390;

/// Maximum size of a single framed message payload (64 MiB). Anything larger
/// is treated as a protocol violation and the connection is dropped.
pub const MAX_MESSAGE_SIZE: u32 = 64 * 1024 * 1024;

/// Tile size in pixels used by the screen codec.
pub const TILE_SIZE: u32 = 64;

pub use image::{Framebuffer, Rect};
pub use message::Message;
