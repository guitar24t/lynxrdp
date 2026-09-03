//! LynxRDP server library.
//!
//! Two binaries are built from this crate:
//!
//! * `lynxrdpd` – the privileged daemon. It listens on loopback only,
//!   identifies the connecting local user (the SSH tunnel endpoint), opens a
//!   PAM login session and hands the connection to that user's
//!   `lynxrdp-session` process.
//! * `lynxrdp-session` – runs as the user. It owns a headless X server
//!   (Xvfb), the desktop session running on it, screen capture, input
//!   injection and the network protocol. It can also run standalone
//!   ("user mode") without the daemon.
//!
//! Only Linux is supported.

#![cfg(target_os = "linux")]

pub mod config;
pub mod daemon;
pub mod fdpass;
pub mod handoff;
pub mod peer;
pub mod reporting;
pub mod session;
pub mod x11;
pub mod xauth;

/// Version string reported to clients.
pub const SERVER_NAME: &str = concat!("LynxRDP/", env!("CARGO_PKG_VERSION"));
