//! LynxRDP client library.
//!
//! [`connection::Client`] implements the protocol without any UI and is
//! what the integration tests use. The GUI in [`app`] is a thin layer on
//! top of it, and [`tunnel`] manages the SSH port forward through which the
//! client reaches the server.

pub mod app;
pub mod connection;
pub mod imageclip;
pub mod keymap;
pub mod tunnel;

/// Name reported to the server.
pub const CLIENT_NAME: &str = concat!("LynxRDP client/", env!("CARGO_PKG_VERSION"));
