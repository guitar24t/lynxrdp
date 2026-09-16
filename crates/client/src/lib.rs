//! LynxRDP client library.
//!
//! [`connection::Client`] implements the protocol without any UI and is
//! what the integration tests use. The GUI in [`app`] is a thin layer on
//! top of it, and [`tunnel`] manages the SSH port forward through which the
//! client reaches the server.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Window class and desktop-entry identity.
///
/// Must stay equal to `StartupWMClass` in `packaging/lynxrdp.desktop`: that is
/// what lets a Linux desktop match a running window to its launcher icon.
pub const APP_ID: &str = "lynxrdp";

/// This executable, as it was when the process started.
///
/// Asked once and kept, because the answer stops being true the moment the
/// updater runs. On Linux the new build is renamed over the running one,
/// which unlinks the inode this process is executing, and from then on
/// `/proc/self/exe` reads `lynxrdp (deleted)` -- a path `Command::new`
/// cannot start and ssh cannot use as an askpass helper. Everything that
/// re-invokes this binary (a session, the askpass helper, the restart after
/// an update, the updater's own plan) comes here, so the one early answer is
/// the answer they all get. `main` asks before it does anything else.
///
/// Canonicalised on Unix: macOS reports the path as it was exec'd, and the
/// release tarball starts the application through a symlink beside the
/// bundle, so planning an update from that path would replace the symlink
/// with a loose binary and leave the bundle on the old version. Windows
/// already reports the module's real path, and `canonicalize` there produces
/// a `\\?\`-prefixed form that not everything handed the path accepts, so it
/// is left as reported.
///
/// `None` only when the operating system cannot say, which callers report
/// rather than guess around.
pub fn exe_path() -> Option<&'static Path> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        #[cfg(unix)]
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        Some(exe)
    })
    .as_deref()
}

pub mod app;
/// SSH authentication prompts, shared by desktop and management connections.
pub mod askpass;
pub mod clipchange;
pub mod connection;
pub mod console;
pub mod fileclip;
mod filedrop;
pub mod icon;
pub mod imageclip;
pub mod keymap;
pub mod launch;
pub mod launcher;
mod outbound;
pub mod overlay;
pub mod profiles;
pub mod settings;
pub mod theme;
pub mod tunnel;
pub mod update;

/// Name reported to the server.
pub const CLIENT_NAME: &str = concat!("LynxRDP client/", env!("CARGO_PKG_VERSION"));

/// Inspect and manage desktops over SSH.
pub mod remote_sessions;

/// Software compositing of graphical widgets over the remote desktop.
pub mod gui_paint;
/// In-session file transfer controls.
pub mod transfer_panel;
