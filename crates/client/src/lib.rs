//! LynxRDP client library.
//!
//! [`connection::Client`] implements the protocol without any UI and is
//! what the integration tests use. The GUI in [`app`] is a thin layer on
//! top of it, and [`tunnel`] manages the SSH port forward through which the
//! client reaches the server.

/// Window class and desktop-entry identity.
///
/// Must stay equal to `StartupWMClass` in `packaging/lynxrdp.desktop`: that is
/// what lets a Linux desktop match a running window to its launcher icon.
pub const APP_ID: &str = "lynxrdp";

pub mod app;
pub mod clipchange;
pub mod connection;
pub mod console;
pub mod fileclip;
pub mod icon;
pub mod imageclip;
pub mod keymap;
pub mod launch;
pub mod launcher;
pub mod overlay;
pub mod profiles;
pub mod settings;
pub mod theme;
pub mod tunnel;
pub mod update;

/// Name reported to the server.
pub const CLIENT_NAME: &str = concat!("LynxRDP client/", env!("CARGO_PKG_VERSION"));
