//! The per-user session process.

pub mod desktop;
pub mod engine;
pub mod fileio;
pub mod listener;
pub mod socket;
pub mod xserver;

use std::path::PathBuf;
use std::time::Duration;

use lynxrdp_proto::Message;
use x11rb::protocol::Event;

use socket::ClientSocket;

/// Tunables for a session.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Upper bound on frames per second.
    pub max_fps: u32,
    /// Frames allowed in flight before waiting for acknowledgements.
    ///
    /// With [`SessionOptions::max_in_flight_auto`] set this is the *floor*:
    /// the session may raise its own window above it when the round trip it
    /// measures justifies more, but never lowers it below.
    pub max_in_flight: u32,
    /// Whether the session may raise `max_in_flight` on its own.
    pub max_in_flight_auto: bool,
    /// Largest screen size a client may request.
    pub max_width: u32,
    /// Largest screen height a client may request.
    pub max_height: u32,
    /// Physical size the X screen reports, in dots per inch.
    ///
    /// This sets the root window's size in millimetres, which is what
    /// `xdpyinfo` prints and what Qt reads to scale itself. It is deliberately
    /// scoped that narrowly: GTK does *not* read it, taking its scaling from
    /// the `Xft.dpi` X resource that the desktop's own settings set, so a GTK
    /// application will not follow this on its own.
    pub dpi: u32,
    /// Size to use when the client does not request one.
    pub default_width: u32,
    /// Height to use when the client does not request one.
    pub default_height: u32,
    /// Name reported to clients.
    pub username: String,
    /// Identifier reported to clients.
    pub session_id: u64,
    /// End the session when the client disconnects.
    pub exit_on_disconnect: bool,
    /// End the session after this long without a client.
    pub idle_timeout: Option<Duration>,
    /// Peer uid that connections must come from (`None` disables the check).
    pub require_uid: Option<u32>,
    /// Directory that uploaded files land in.
    pub upload_dir: PathBuf,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            max_fps: 60,
            max_in_flight: 2,
            max_in_flight_auto: true,
            max_width: 4096,
            max_height: 2160,
            dpi: 96,
            default_width: 1920,
            default_height: 1080,
            username: String::new(),
            session_id: 0,
            exit_on_disconnect: false,
            idle_timeout: None,
            require_uid: Some(crate::peer::own_uid()),
            upload_dir: default_upload_dir(),
        }
    }
}

/// Where uploaded files land by default: the user's `Downloads` directory
/// when it exists, otherwise their home directory.
pub fn default_upload_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let downloads = home.join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        home
    }
}

/// A freshly accepted client connection.
pub struct NewClient {
    /// The connection.
    pub socket: ClientSocket,
    /// Human readable description for logs.
    pub description: String,
}

/// Events delivered to the session core thread.
pub enum CoreEvent {
    /// An X11 event.
    X(Event),
    /// The X connection broke.
    XError(String),
    /// A client connected.
    NewClient(NewClient),
    /// A message from the current client (`generation` identifies which).
    ClientMessage(u64, Message),
    /// The client connection ended.
    ClientClosed(u64, String),
    /// The desktop session process exited.
    DesktopExited(String),
    /// Orderly shutdown request (signal).
    Shutdown(String),
    /// The file worker finished opening a file a client asked to download.
    /// Boxed because every other variant is small and this one need not make
    /// the channel's element wider than the X events that dominate it.
    FileOpened(Box<fileio::FileOpened>),
}
