//! Protocol client without any user interface.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::Receiver;
use lynxrdp_proto::codec::Decoder;
use lynxrdp_proto::frame::{frame_message, read_message, FrameError};
use lynxrdp_proto::message::{clipboard_format, features, CursorImage};
use lynxrdp_proto::transfer::{Completed, Sink, TransferManager, TransferPolicy, TransferPurpose};
use lynxrdp_proto::{
    can_speak, Framebuffer, Message, Rect, MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION,
};

/// Options for connecting.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// Desired initial screen size (`None` = server default).
    pub size: Option<(u16, u16)>,
    /// Feature bits to request.
    pub features: u32,
    /// Timeout for TCP connect and handshake.
    pub timeout: Duration,
    /// Name sent to the server.
    pub client_name: String,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            size: None,
            features: features::LOCAL_CURSOR
                | features::CLIPBOARD
                | features::RESIZE
                | features::CLIPBOARD_IMAGE
                | features::FILE_TRANSFER
                | features::CLIPBOARD_FILES
                | features::ATOMIC_FILES,
            timeout: Duration::from_secs(15),
            client_name: crate::CLIENT_NAME.to_string(),
        }
    }
}

/// What the server told us in its hello.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    /// Server identification.
    pub server_name: String,
    /// Negotiated feature bits.
    pub features: u32,
    /// Session identifier.
    pub session_id: u64,
    /// User owning the session.
    pub username: String,
}

/// Events produced by [`Client::poll_event`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    /// A frame was applied to the framebuffer; `dirty` is the changed area.
    Frame {
        /// Frame identifier.
        frame_id: u64,
        /// Union of changed rectangles.
        dirty: Rect,
    },
    /// The remote screen size changed (framebuffer already resized).
    Resized {
        /// New width.
        width: u32,
        /// New height.
        height: u32,
    },
    /// The pointer shape changed.
    Cursor(CursorImage),
    /// The pointer was moved by the remote side.
    CursorPosition(u16, u16),
    /// Clipboard text from the remote session.
    Clipboard(String),
    /// A clipboard image (PNG bytes) from the remote session.
    ClipboardImage(Vec<u8>),
    /// The session copied files. Fetch them with
    /// [`Client::request_file`] to put them on the local clipboard.
    ClipboardFiles(Vec<lynxrdp_proto::FileEntry>),
    /// A file finished downloading to this path.
    FileDownloaded {
        /// Transfer identifier, as returned by [`Client::request_file`].
        ///
        /// The id, not the name, is what a caller tracking a batch has to key
        /// on: a session can copy two files called `notes.txt` from different
        /// directories in one go, and names stop being unique the moment a
        /// caller queues transfers across more than one request.
        id: u64,
        /// Where it was written locally.
        path: PathBuf,
        /// Name the server reported.
        name: String,
    },
    /// A file finished uploading into the session.
    FileUploaded {
        /// Transfer identifier, as returned by [`Client::send_file`].
        id: u64,
        /// Name as offered to the server.
        name: String,
    },
    /// A transfer failed.
    TransferFailed {
        /// Transfer identifier.
        id: u64,
        /// Why it failed.
        reason: String,
    },
    /// A notice for the user.
    Notice(String),
    /// Round trip time measured from a ping/pong.
    Rtt(Duration),
    /// The connection ended.
    Disconnected(String),
}

/// What every rejection string says, and the only place it is written down.
const REJECTION_MARK: &str = "rejected (code ";

/// How a rejection from the server is phrased for the user.
///
/// A function rather than a `format!` where it happens, because the code has
/// to be readable back out of the string afterwards: the session window
/// decides whether a dropped link is worth retrying, and a rejection --
/// a version floor, a policy refusal -- is precisely the case where retrying
/// achieves nothing but noise. [`rejection_code`] and [`mentions_rejection`]
/// are the other half, and all three are tested against each other rather
/// than against a literal, so the wording can change without quietly turning
/// every rejection retryable.
pub fn rejection_reason(code: u16, reason: &str) -> String {
    format!("{REJECTION_MARK}{code}): {reason}")
}

/// The code out of a string [`rejection_reason`] produced, if it is one.
pub fn rejection_code(text: &str) -> Option<u16> {
    text.strip_prefix(REJECTION_MARK)?
        .split_once(')')?
        .0
        .parse()
        .ok()
}

/// Whether a failure reports a rejection, wherever in it that is said.
///
/// The question [`rejection_code`] answers is "is this string a rejection",
/// and that is the wrong one to ask of a reconnection failure: by the time
/// one reaches the session window it has been through a `Result` chain, and
/// a single `.context()` anywhere along it turns `rejected (code 3): ...`
/// into `reconnecting: rejected (code 3): ...`. A rejection that stops being
/// recognisable is not a cosmetic loss -- it is the whole retry budget spent
/// re-asking a question whose answer cannot change, and eight refusals in
/// the server's log for an administrator to explain instead of one.
///
/// Nesting does not make it less of a rejection, so searching rather than
/// matching at the front has no false positive to trade against: the parse
/// is still `rejection_code`'s, so the two cannot come to disagree about
/// what the string looks like.
pub fn mentions_rejection(text: &str) -> bool {
    text.match_indices(REJECTION_MARK)
        .any(|(at, _)| rejection_code(&text[at..]).is_some())
}

/// Accepts what the session sends us.
///
/// Downloads only land where this client asked them to: a `FileDownload`
/// offer whose id we did not request is refused, so the session cannot write
/// files onto the client's disk of its own accord.
struct ClientTransferPolicy {
    replacements: std::collections::HashSet<u64>,
    downloads: HashMap<u64, PathBuf>,
}

impl TransferPolicy for ClientTransferPolicy {
    fn accept(
        &mut self,
        id: u64,
        purpose: TransferPurpose,
        _name: &str,
        _size: u64,
    ) -> Result<Sink, String> {
        match purpose {
            TransferPurpose::ClipboardImage => Ok(Sink::Memory(Vec::new())),
            TransferPurpose::FileDownload => self.open_download(id),
            // The session never uploads to us.
            TransferPurpose::FileUpload => Err("unexpected upload offer".to_string()),
        }
    }
}

impl ClientTransferPolicy {
    fn open_download(&mut self, id: u64) -> Result<Sink, String> {
        let dest = self
            .downloads
            .get(&id)
            .ok_or_else(|| "unsolicited download".to_string())?;
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
        }
        let f = lynxrdp_proto::atomic_file::AtomicFile::new(dest, self.replacements.contains(&id))
            .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
        Ok(Sink::Stream(Box::new(f)))
    }
}

/// Client-side connection state.
/// Declare the server gone after this long with no message of any kind.
///
/// Matches the server's own `PONG_TIMEOUT`, so both ends give up at the same
/// point rather than one of them holding a connection the other has abandoned.
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest single blocking wait inside `poll_event`.
///
/// A caller asking to wait a minute must still have the liveness deadline
/// checked on the way, so the wait is chopped into pieces rather than trusting
/// the caller to pass something short.
const POLL_SLICE: Duration = Duration::from_millis(250);

pub struct Client {
    writer: Arc<Mutex<TcpStream>>,
    events: Receiver<Message>,
    reader: Option<JoinHandle<()>>,
    decoder: Decoder,
    info: ServerInfo,
    cursor: Option<CursorImage>,
    closed: Option<String>,
    frames_received: u64,
    bytes_received: Arc<Mutex<u64>>,
    last_ping: Option<(u64, Instant)>,
    /// When the last message of any kind arrived from the server.
    ///
    /// The server pings every two seconds from the session core thread, so
    /// "nothing at all for thirty seconds" is an exact test for that thread
    /// being wedged -- and it is the only test the client has. Without it a
    /// stuck session shows a frozen screen, accepts input that goes nowhere and
    /// displays a stale round-trip time indefinitely, because ssh's own
    /// ServerAliveInterval only fires when the *transport* dies, not when the
    /// thing behind it stops.
    last_message_at: Instant,
    /// Events one server message produced but a single `poll_event` call
    /// cannot return.
    ///
    /// One message routinely resolves more than one transfer -- a
    /// `TransferAck` pumps every outgoing transfer and can fail several of
    /// them, and a refusal is reported alongside whatever else the same
    /// message finished. The old code returned the first and dropped the rest,
    /// which is invisible until a caller is tracking a set of transfers and
    /// one of them never resolves.
    queued: std::collections::VecDeque<ClientEvent>,
    transfers: TransferManager,
    /// Destinations for downloads this client asked for.
    downloads: HashMap<u64, PathBuf>,
    replacements: std::collections::HashSet<u64>,
    /// An image copied locally, held until the session asks for it.
    pending_image: Option<Vec<u8>>,
    /// Files this client offered on the clipboard, by the path it advertised.
    /// The session may only read files that appear here.
    offered_files: HashMap<String, PathBuf>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h) = self.size();
        write!(
            f,
            "Client({}@{}, {w}x{h})",
            self.info.username, self.info.server_name
        )
    }
}

impl Client {
    /// Connect to a LynxRDP server at `addr` (normally the local end of an
    /// SSH tunnel) and perform the handshake.
    ///
    /// `wake` is invoked from the reader thread whenever a message is
    /// queued, so a UI event loop can be nudged to call [`Client::poll_event`].
    pub fn connect(
        addr: SocketAddr,
        opts: &ConnectOptions,
        wake: Option<Box<dyn Fn() + Send>>,
    ) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, opts.timeout)
            .with_context(|| format!("connecting to {addr}"))?;
        Self::from_stream(stream, opts, wake)
    }

    /// Handshake over an already connected stream.
    pub fn from_stream(
        stream: TcpStream,
        opts: &ConnectOptions,
        wake: Option<Box<dyn Fn() + Send>>,
    ) -> Result<Self> {
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(opts.timeout))?;
        // Without this a send into a stalled tunnel blocks forever, and
        // `Client::send` is called from the winit thread -- including on the
        // way out, where `exiting()` sends a Disconnect before the shutdown
        // that would have unblocked it, so the window could not even close.
        stream.set_write_timeout(Some(opts.timeout))?;
        let (w, h) = opts.size.unwrap_or((0, 0));
        let hello = Message::ClientHello {
            version: PROTOCOL_VERSION,
            client_name: opts.client_name.clone(),
            features: opts.features,
            width: w,
            height: h,
        };
        let mut buf = Vec::new();
        frame_message(&hello, &mut buf);
        (&stream).write_all(&buf).context("sending hello")?;

        let mut reader = BufReader::with_capacity(256 * 1024, stream.try_clone()?);
        let (info, width, height) =
            match read_message(&mut reader).map_err(|e| anyhow!("handshake failed: {e}"))? {
                Message::ServerHello {
                    version,
                    server_name,
                    features,
                    session_id,
                    username,
                    width,
                    height,
                } => {
                    // The server answers with the version the two of us agreed on,
                    // which is the older of the pair -- so this is a range check,
                    // not an equality one. `can_speak` refuses a server below our
                    // floor (too old to hold a session with) and one above our own
                    // version (which would mean it ignored the hello we sent).
                    if !can_speak(version) {
                        bail!(
                            "server speaks protocol version {version}; this client supports \
                         {MIN_COMPATIBLE_VERSION} to {PROTOCOL_VERSION}"
                        );
                    }
                    (
                        ServerInfo {
                            server_name,
                            features,
                            session_id,
                            username,
                        },
                        u32::from(width),
                        u32::from(height),
                    )
                }
                // Phrased by `rejection_reason` like every other rejection,
                // because the window reads the code back out of it to decide
                // whether a reconnection attempt is worth repeating.
                Message::Rejected { code, reason } => bail!("{}", rejection_reason(code, &reason)),
                other => bail!("unexpected message {:?} during handshake", other.kind()),
            };
        stream.set_read_timeout(None)?;

        let (tx, rx) = crossbeam_channel::unbounded::<Message>();
        let bytes_received = Arc::new(Mutex::new(0u64));
        let counter = bytes_received.clone();
        let reader_thread = std::thread::Builder::new()
            .name("lynxrdp-reader".into())
            .spawn(move || {
                let mut reader = reader;
                loop {
                    match read_message(&mut reader) {
                        Ok(msg) => {
                            if let Message::ScreenUpdate { copies, tiles, .. } = &msg {
                                let n: usize = tiles.iter().map(|t| t.data.len() + 13).sum();
                                *counter.lock().unwrap() +=
                                    n as u64 + 16 + (copies.len() * 12) as u64;
                            }
                            if tx.send(msg).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let reason = match &e {
                                FrameError::Io(_) if e.is_disconnect() => {
                                    "connection closed".to_string()
                                }
                                other => other.to_string(),
                            };
                            let _ = tx.send(Message::Disconnect { reason });
                            break;
                        }
                    }
                    if let Some(w) = &wake {
                        w();
                    }
                }
                if let Some(w) = &wake {
                    w();
                }
            })
            .context("spawn reader")?;

        Ok(Self {
            writer: Arc::new(Mutex::new(stream)),
            events: rx,
            reader: Some(reader_thread),
            decoder: Decoder::new(width, height),
            info,
            cursor: None,
            closed: None,
            frames_received: 0,
            bytes_received,
            last_ping: None,
            last_message_at: Instant::now(),
            queued: std::collections::VecDeque::new(),
            transfers: TransferManager::new(true),
            downloads: HashMap::new(),
            replacements: Default::default(),
            pending_image: None,
            offered_files: HashMap::new(),
        })
    }

    /// Server information from the handshake.
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    /// The decoded remote screen.
    pub fn framebuffer(&self) -> &Framebuffer {
        self.decoder.framebuffer()
    }

    /// Current remote screen size.
    pub fn size(&self) -> (u32, u32) {
        (
            self.decoder.framebuffer().width(),
            self.decoder.framebuffer().height(),
        )
    }

    /// Latest cursor image, if the server sent one.
    pub fn cursor(&self) -> Option<&CursorImage> {
        self.cursor.as_ref()
    }

    /// Number of frames applied.
    pub fn frames_received(&self) -> u64 {
        self.frames_received
    }

    /// Bytes of screen data received.
    pub fn bytes_received(&self) -> u64 {
        *self.bytes_received.lock().unwrap()
    }

    /// Reason the connection closed, if it has.
    pub fn closed(&self) -> Option<&str> {
        self.closed.as_deref()
    }

    /// How long it has been since the server said anything at all.
    ///
    /// A better liveness signal than a caller's own ping/pong bookkeeping,
    /// and the reason it is exposed: *any* message refreshes it, so a session
    /// that is answering pings but sending nothing else still reads as alive,
    /// and one whose core thread has wedged reads as quiet within a couple of
    /// server ping intervals. It starts at the handshake, which is itself
    /// proof the link was up, so it never lies about a connection too young
    /// to have been probed.
    pub fn quiet_for(&self) -> Duration {
        self.last_message_at.elapsed()
    }

    /// Send a message to the server.
    pub fn send(&self, msg: &Message) -> Result<()> {
        let mut buf = Vec::new();
        frame_message(msg, &mut buf);
        let mut w = self.writer.lock().unwrap();
        w.write_all(&buf).context("sending message")?;
        Ok(())
    }

    /// Send a key event.
    pub fn key(&self, keysym: u32, down: bool) -> Result<()> {
        self.send(&Message::KeyEvent { keysym, down })
    }

    /// Press and release a key.
    pub fn tap_key(&self, keysym: u32) -> Result<()> {
        self.key(keysym, true)?;
        self.key(keysym, false)
    }

    /// Type a string as key events.
    pub fn type_text(&self, text: &str) -> Result<()> {
        for c in text.chars() {
            self.tap_key(lynxrdp_proto::keysym::keysym_from_char(c))?;
        }
        Ok(())
    }

    /// Move the pointer.
    pub fn pointer_move(&self, x: u16, y: u16) -> Result<()> {
        self.send(&Message::PointerMotion { x, y })
    }

    /// Press or release a pointer button.
    pub fn pointer_button(&self, button: u8, down: bool) -> Result<()> {
        self.send(&Message::PointerButton { button, down })
    }

    /// Click a pointer button.
    pub fn click(&self, x: u16, y: u16, button: u8) -> Result<()> {
        self.pointer_move(x, y)?;
        self.pointer_button(button, true)?;
        self.pointer_button(button, false)
    }

    /// Ask for a new screen size.
    pub fn request_resize(&self, width: u16, height: u16) -> Result<()> {
        self.send(&Message::ResizeRequest { width, height })
    }

    /// Send clipboard text to the session.
    pub fn set_clipboard(&self, text: &str) -> Result<()> {
        self.send(&Message::ClipboardText {
            text: text.to_string(),
        })
    }

    /// Offer an image copied locally to the session. The PNG is held until
    /// the session asks for it, so copying an image the user never pastes
    /// costs nothing on the wire.
    pub fn offer_clipboard_image(&mut self, png: Vec<u8>) -> Result<()> {
        if self.info.features & features::CLIPBOARD_IMAGE == 0 {
            return Ok(());
        }
        self.pending_image = Some(png);
        self.send(&Message::ClipboardOffer {
            formats: clipboard_format::PNG,
        })
    }

    /// Offer files copied locally to the session.
    ///
    /// Only these paths become readable by the session, and only until the
    /// next clipboard copy replaces them.
    pub fn offer_clipboard_files(&mut self, paths: &[PathBuf]) -> Result<()> {
        if self.info.features & features::CLIPBOARD_FILES == 0 {
            return Ok(());
        }
        let mut files = Vec::new();
        let mut offered = HashMap::new();
        for p in paths {
            let meta = match std::fs::metadata(p) {
                Ok(m) if m.is_file() => m,
                // Directories would need recursive listing; skip rather than
                // offering something we cannot deliver.
                Ok(_) => continue,
                Err(e) => {
                    log::debug!("skipping {}: {e}", p.display());
                    continue;
                }
            };
            let key = p.to_string_lossy().into_owned();
            files.push(lynxrdp_proto::FileEntry {
                path: key.clone(),
                size: meta.len(),
            });
            offered.insert(key, p.clone());
        }
        if files.is_empty() {
            return Ok(());
        }
        self.offered_files = offered;
        let id = self.transfers.next_id();
        self.send(&Message::FileList { id, files })?;
        self.send(&Message::ClipboardOffer {
            formats: clipboard_format::FILES,
        })
    }

    /// Upload a local file into the session. `dest` is a path relative to the
    /// session's upload directory. Returns the transfer id.
    pub fn send_file(&mut self, local: &Path, dest: &str) -> Result<u64> {
        self.send_file_with_overwrite(local, dest, false)
    }

    /// Upload with an explicit replacement choice. Older servers require explicit overwrite consent.
    pub fn send_file_with_overwrite(
        &mut self,
        local: &Path,
        dest: &str,
        replace: bool,
    ) -> Result<u64> {
        anyhow::ensure!(
            replace || self.info.features & features::ATOMIC_FILES != 0,
            "update the server for safe uploads, or explicitly allow overwriting"
        );
        anyhow::ensure!(
            self.info.features & features::FILE_TRANSFER != 0,
            "the server did not enable file transfer"
        );
        let file =
            std::fs::File::open(local).with_context(|| format!("opening {}", local.display()))?;
        let size = file.metadata()?.len();
        let id = self.transfers.next_id();
        if self.info.features & features::ATOMIC_FILES != 0 {
            self.send(&Message::TransferOptions { id, replace })?;
        }
        let offer = self.transfers.offer_stream_with_id(
            id,
            TransferPurpose::FileUpload,
            dest.to_string(),
            size,
            Box::new(file),
        );
        self.send(&offer)?;
        Ok(id)
    }

    /// Download `remote` out of the session to `local`. Returns the transfer id.
    pub fn request_file(&mut self, remote: &str, local: PathBuf) -> Result<u64> {
        self.request_file_with_overwrite(remote, local, false)
    }

    /// Download atomically; existing files are preserved unless replace is true.
    pub fn request_file_with_overwrite(
        &mut self,
        remote: &str,
        local: PathBuf,
        replace: bool,
    ) -> Result<u64> {
        anyhow::ensure!(
            self.info.features & features::FILE_TRANSFER != 0,
            "the server did not enable file transfer"
        );
        let id = self.transfers.next_id();
        self.transfers.expect(id);
        self.downloads.insert(id, local);
        if replace {
            self.replacements.insert(id);
        }
        self.send(&Message::FileRequest {
            id,
            path: remote.to_string(),
        })?;
        Ok(id)
    }

    /// Abandon transfer `id` in either direction.
    ///
    /// Used when what the transfer was *for* has gone: a clipboard copy
    /// superseded by the next one is still downloading files nobody will ever
    /// paste, and each of them holds an open descriptor and a share of the
    /// global window until it finishes. No event follows -- the caller asked
    /// for this and already knows.
    pub fn cancel_transfer(&mut self, id: u64) {
        self.downloads.remove(&id);
        self.replacements.remove(&id);
        if let Some(end) = self.transfers.cancel(id, "no longer wanted") {
            let _ = self.send(&end);
        }
    }

    /// Active transfer descriptions for a local progress view.
    pub fn transfer_rows(&self) -> Vec<(u64, String)> {
        self.transfers
            .active_ids()
            .into_iter()
            .map(|id| {
                let text = match self.transfers.describe(id) {
                    Some((purpose, name, done, total)) => {
                        format!("{:?} {name}: {done}/{total} bytes", purpose)
                    }
                    None => "Waiting for file offer".into(),
                };
                (id, text)
            })
            .collect()
    }

    /// Drive the connection until transfer `id` finishes.
    ///
    /// Returns the local path for a completed download. Used by the
    /// command line, which has no event loop of its own.
    pub fn run_transfer(&mut self, id: u64, timeout: Duration) -> Result<Option<PathBuf>> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("transfer timed out");
            }
            // Matched by id rather than by "the next completion of that
            // shape": nothing stops a caller from having other transfers in
            // flight, and returning someone else's completion here would
            // report success for a transfer that is still running.
            match self.poll_event(remaining)? {
                Some(ClientEvent::FileUploaded { id: done, .. }) if done == id => return Ok(None),
                Some(ClientEvent::FileDownloaded { id: done, path, .. }) if done == id => {
                    return Ok(Some(path))
                }
                Some(ClientEvent::TransferFailed { id: failed, reason }) if failed == id => {
                    bail!("{reason}")
                }
                Some(ClientEvent::Disconnected(reason)) => bail!("disconnected: {reason}"),
                Some(_) => {}
                None => bail!("transfer timed out"),
            }
        }
    }

    /// Progress of a transfer, as (done, total).
    pub fn transfer_progress(&self, id: u64) -> Option<(u64, u64)> {
        self.transfers.progress(id)
    }

    /// Ask the server to resend the whole screen.
    pub fn request_refresh(&self) -> Result<()> {
        self.send(&Message::RefreshRequest)
    }

    /// Process the next server message, applying frames to the framebuffer
    /// and answering pings. Returns `None` on timeout.
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<ClientEvent>> {
        // Before the closed check, not after: a connection that ends in the
        // same batch of messages that finished a transfer still owes the
        // caller the completion, and a batch reported as neither done nor
        // failed is exactly what leaves a caller waiting forever.
        if let Some(ev) = self.queued.pop_front() {
            return Ok(Some(ev));
        }
        if let Some(reason) = &self.closed {
            return Ok(Some(ClientEvent::Disconnected(reason.clone())));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let msg = match self.events.recv_timeout(remaining.min(POLL_SLICE)) {
                Ok(m) => m,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if self.last_message_at.elapsed() > LIVENESS_TIMEOUT {
                        let reason = format!(
                            "no response from the session for {}s",
                            LIVENESS_TIMEOUT.as_secs()
                        );
                        self.closed = Some(reason.clone());
                        return Ok(Some(ClientEvent::Disconnected(reason)));
                    }
                    if remaining <= POLL_SLICE {
                        return Ok(None);
                    }
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.closed = Some("reader stopped".into());
                    return Ok(Some(ClientEvent::Disconnected("reader stopped".into())));
                }
            };
            self.last_message_at = Instant::now();
            if let Some(ev) = self.handle(msg)? {
                return Ok(Some(ev));
            }
        }
    }

    /// Non-blocking variant of [`Client::poll_event`].
    pub fn try_event(&mut self) -> Result<Option<ClientEvent>> {
        self.poll_event(Duration::ZERO)
    }

    /// Feed a message to the transfer manager, sending any replies.
    ///
    /// Every outcome the message produced is queued, and the first is
    /// returned; the rest come out of later `poll_event` calls. Nothing is
    /// dropped, because a caller waiting on a set of transfers cannot tell an
    /// event that never happened from one that was thrown away.
    fn handle_transfer(&mut self, msg: &Message) -> Option<Result<Option<ClientEvent>>> {
        let mut policy = ClientTransferPolicy {
            replacements: self.replacements.clone(),
            // Moved out and back rather than cloned: this runs for every
            // message on the connection, screen updates included, and staging
            // a clipboard copy can leave hundreds of entries in the map.
            downloads: std::mem::take(&mut self.downloads),
        };
        let outcome = self.transfers.handle(msg, &mut policy);
        self.downloads = std::mem::take(&mut policy.downloads);
        let outcome = outcome?;
        for reply in &outcome.replies {
            // A refusal is read off the reply rather than reported by the
            // policy, because the policy is not the only thing that turns an
            // offer away: `TransferManager` refuses one that is over the size
            // limit, or that would be the third transfer landing in memory,
            // without consulting the policy at all. Every one of those paths
            // -- and the policy's own refusals, of which a local
            // `File::create` failure is the common one, since `a:b.txt`,
            // `what?.png`, `CON` and a trailing dot are ordinary names in an
            // X11 session and none of them can be created on NTFS -- answers
            // on the wire and puts nothing in `Outcome::failed`, because from
            // the manager's point of view no transfer ever existed. The
            // requester is still waiting, and a clipboard batch holding a slot
            // for a file that can never arrive strands the whole paste.
            // `TransferAccept { accepted: false }` is the one thing all of
            // them have in common, so that is what becomes the event.
            if let Message::TransferAccept {
                id,
                accepted: false,
                reason,
            } = reply
            {
                self.fail_transfer(*id, reason.clone());
            }
            if let Err(e) = self.send(reply) {
                return Some(Err(e));
            }
        }
        for (id, reason) in outcome.failed {
            self.fail_transfer(id, reason);
        }
        for (id, purpose, name) in outcome.sent {
            if purpose == TransferPurpose::FileUpload {
                self.queued
                    .push_back(ClientEvent::FileUploaded { id, name });
            }
        }
        for done in outcome.completed {
            match self.on_completed(done) {
                Ok(Some(ev)) => self.queued.push_back(ev),
                Ok(None) => {}
                Err(e) => return Some(Err(e)),
            }
        }
        Some(Ok(self.queued.pop_front()))
    }

    /// Record a transfer as failed and release whatever it was holding.
    fn fail_transfer(&mut self, id: u64, reason: String) {
        self.downloads.remove(&id);
        self.replacements.remove(&id);
        self.queued
            .push_back(ClientEvent::TransferFailed { id, reason });
    }

    fn on_completed(&mut self, done: Completed) -> Result<Option<ClientEvent>> {
        self.replacements.remove(&done.id);
        Ok(match done.purpose {
            TransferPurpose::ClipboardImage => done.data.map(ClientEvent::ClipboardImage),
            TransferPurpose::FileDownload => {
                self.downloads
                    .remove(&done.id)
                    .map(|path| ClientEvent::FileDownloaded {
                        id: done.id,
                        path,
                        name: done.name,
                    })
            }
            TransferPurpose::FileUpload => None,
        })
    }

    fn handle(&mut self, msg: Message) -> Result<Option<ClientEvent>> {
        if let Some(result) = self.handle_transfer(&msg) {
            return result;
        }
        Ok(match msg {
            Message::ScreenUpdate {
                frame_id,
                copies,
                tiles,
            } => {
                let dirty = self
                    .decoder
                    .apply_frame(&copies, &tiles)
                    .context("applying screen update")?;
                self.frames_received += 1;
                // Acknowledge as soon as the frame is decoded; painting is
                // cheap and the server can start on the next frame now.
                self.send(&Message::FrameAck { frame_id })?;
                Some(ClientEvent::Frame { frame_id, dirty })
            }
            Message::ScreenResized { width, height } => {
                self.decoder.resize(u32::from(width), u32::from(height));
                Some(ClientEvent::Resized {
                    width: u32::from(width),
                    height: u32::from(height),
                })
            }
            Message::CursorShape { cursor } => {
                self.cursor = Some(cursor.clone());
                Some(ClientEvent::Cursor(cursor))
            }
            Message::CursorPosition { x, y } => Some(ClientEvent::CursorPosition(x, y)),
            Message::ClipboardText { text } => Some(ClientEvent::Clipboard(text)),
            Message::FileList { files, .. } => Some(ClientEvent::ClipboardFiles(files)),
            Message::FileRequest { id, path } => {
                // Serve only what this client put on the clipboard: the
                // session must not be able to read arbitrary local files.
                match self.offered_files.get(&path).cloned() {
                    Some(local) => match std::fs::File::open(&local)
                        .and_then(|f| f.metadata().map(|m| (f, m.len())))
                    {
                        Ok((file, size)) => {
                            let name = local
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.clone());
                            let offer = self.transfers.offer_stream_with_id(
                                id,
                                TransferPurpose::FileDownload,
                                name,
                                size,
                                Box::new(file),
                            );
                            self.send(&offer)?;
                        }
                        Err(e) => self.send(&Message::TransferEnd {
                            id,
                            ok: false,
                            message: format!("{}: {e}", local.display()),
                        })?,
                    },
                    None => {
                        log::warn!("session asked for {path:?}, which was not offered");
                        self.send(&Message::TransferEnd {
                            id,
                            ok: false,
                            message: "not offered on the clipboard".into(),
                        })?;
                    }
                }
                None
            }
            Message::ClipboardOffer { formats } => {
                // The session copied something; ask for the formats we can
                // actually use. Both are fetched on demand rather than
                // pushed, so a copy the user never pastes costs nothing.
                if formats & clipboard_format::PNG != 0
                    && self.info.features & features::CLIPBOARD_IMAGE != 0
                {
                    self.send(&Message::ClipboardRequest {
                        format: clipboard_format::PNG,
                    })?;
                }
                if formats & clipboard_format::FILES != 0
                    && self.info.features & features::CLIPBOARD_FILES != 0
                {
                    self.send(&Message::ClipboardRequest {
                        format: clipboard_format::FILES,
                    })?;
                }
                None
            }
            Message::ClipboardRequest { format } => {
                if format & clipboard_format::PNG != 0 {
                    if let Some(png) = self.pending_image.take() {
                        let offer = self.transfers.offer_bytes(
                            TransferPurpose::ClipboardImage,
                            "clipboard.png".to_string(),
                            png,
                        );
                        self.send(&offer)?;
                    }
                }
                None
            }
            Message::Notice { text } => Some(ClientEvent::Notice(text)),
            Message::Ping { nonce } => {
                self.send(&Message::Pong { nonce })?;
                None
            }
            Message::Pong { nonce } => {
                let rtt = self.last_ping.take().and_then(|(n, t)| {
                    if n == nonce {
                        Some(t.elapsed())
                    } else {
                        None
                    }
                });
                rtt.map(ClientEvent::Rtt)
            }
            Message::Disconnect { reason } => {
                self.closed = Some(reason.clone());
                Some(ClientEvent::Disconnected(reason))
            }
            Message::Rejected { code, reason } => {
                let r = rejection_reason(code, &reason);
                self.closed = Some(r.clone());
                Some(ClientEvent::Disconnected(r))
            }
            other => {
                log::warn!("ignoring unexpected message {:?} from server", other.kind());
                None
            }
        })
    }

    /// Send a latency probe; the answer arrives as [`ClientEvent::Rtt`].
    pub fn ping(&mut self) -> Result<()> {
        let nonce = self.last_ping.map(|(n, _)| n).unwrap_or(0).wrapping_add(1);
        self.last_ping = Some((nonce, Instant::now()));
        self.send(&Message::Ping { nonce })
    }

    /// Wait until at least `n` frames have been received or `timeout` passes.
    pub fn wait_for_frames(&mut self, n: u64, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        while self.frames_received < n {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            if let Some(ClientEvent::Disconnected(r)) = self.poll_event(remaining)? {
                bail!("disconnected: {r}");
            }
        }
        Ok(true)
    }

    /// Close the connection gracefully.
    pub fn disconnect(&mut self, reason: &str) {
        let _ = self.send(&Message::Disconnect {
            reason: reason.to_string(),
        });
        self.close_socket("closed by client");
    }

    /// Let go of a connection that has already failed, without saying goodbye.
    ///
    /// [`Client::disconnect`] writes a `Disconnect` first, which is right for
    /// a link that is still there and wrong for one that is not: against a
    /// half-open socket -- a sleeping laptop, a forward whose far end has gone
    /// -- that write ends only when the kernel's send buffer fills and the
    /// write timeout expires, fifteen seconds of it, on whatever thread called
    /// in. The session window calls in on the winit thread, and a reconnection
    /// that freezes the window for fifteen seconds before it starts is not a
    /// reconnection anyone would keep.
    ///
    /// The shutdown alone is what actually has to happen: it wakes the reader
    /// thread out of its blocking read so the thread and the descriptor are
    /// released rather than leaked, once per attempt, for as long as the user
    /// keeps trying.
    pub fn abandon(&mut self, reason: &str) {
        self.close_socket(reason);
    }

    /// Take the socket down and collect the reader thread.
    fn close_socket(&mut self, reason: &str) {
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.reader.take() {
            let _ = t.join();
        }
        self.closed.get_or_insert_with(|| reason.to_string());
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.closed.is_none() {
            self.disconnect("client exiting");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lynxrdp_proto::codec::{TileEncoding, TileUpdate};
    use lynxrdp_proto::frame::write_message;
    use lynxrdp_proto::transfer::MAX_CONCURRENT_MEMORY_TRANSFERS;
    use std::net::TcpListener;

    /// A minimal fake server for exercising the client state machine.
    fn fake_server(listener: TcpListener, script: Vec<Message>) -> JoinHandle<Vec<Message>> {
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let hello = read_message(&mut s).unwrap();
            received.push(hello);
            for m in script {
                write_message(&mut s, &m).unwrap();
            }
            // Collect whatever the client sends until it disconnects.
            while let Ok(m) = read_message(&mut s) {
                let done = matches!(m, Message::Disconnect { .. });
                received.push(m);
                if done {
                    break;
                }
            }
            received
        })
    }

    fn hello(w: u16, h: u16) -> Message {
        Message::ServerHello {
            version: PROTOCOL_VERSION,
            server_name: "fake".into(),
            features: 7,
            session_id: 9,
            username: "bob".into(),
            width: w,
            height: h,
        }
    }

    #[test]
    fn handshake_and_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let script = vec![
            hello(100, 50),
            Message::ScreenUpdate {
                frame_id: 1,
                copies: vec![],
                tiles: vec![TileUpdate {
                    rect: Rect::new(10, 10, 5, 5),
                    encoding: TileEncoding::Solid,
                    data: vec![1, 2, 3],
                }],
            },
            Message::ScreenResized {
                width: 20,
                height: 20,
            },
            Message::Ping { nonce: 4 },
            Message::Disconnect {
                reason: "done".into(),
            },
        ];
        let server = fake_server(listener, script);
        let mut c = Client::connect(addr, &ConnectOptions::default(), None).unwrap();
        assert_eq!(c.info().username, "bob");
        assert_eq!(c.size(), (100, 50));
        let ev = c.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(
            ev,
            ClientEvent::Frame {
                frame_id: 1,
                dirty: Rect::new(10, 10, 5, 5)
            }
        );
        assert_eq!(c.framebuffer().get(12, 12), 0x010203);
        assert_eq!(
            c.poll_event(Duration::from_secs(5)).unwrap().unwrap(),
            ClientEvent::Resized {
                width: 20,
                height: 20
            }
        );
        assert_eq!(c.size(), (20, 20));
        assert_eq!(
            c.poll_event(Duration::from_secs(5)).unwrap().unwrap(),
            ClientEvent::Disconnected("done".into())
        );
        c.disconnect("bye");
        let received = server.join().unwrap();
        assert!(matches!(received[0], Message::ClientHello { .. }));
        assert!(received.contains(&Message::FrameAck { frame_id: 1 }));
        assert!(received.contains(&Message::Pong { nonce: 4 }));
    }

    #[test]
    fn rejection_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = fake_server(
            listener,
            vec![Message::Rejected {
                code: 2,
                reason: "no".into(),
            }],
        );
        let err = Client::connect(addr, &ConnectOptions::default(), None).unwrap_err();
        assert!(err.to_string().contains("rejected"), "{err}");
    }

    #[test]
    fn version_mismatch_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut h = hello(1, 1);
        if let Message::ServerHello { version, .. } = &mut h {
            *version = 99;
        }
        let _server = fake_server(listener, vec![h]);
        let err = Client::connect(addr, &ConnectOptions::default(), None).unwrap_err();
        assert!(err.to_string().contains("version"), "{err}");
    }

    /// A server too old to hold a session with is refused; one merely older
    /// than us is not.
    ///
    /// The check used to be plain equality, which made every protocol bump a
    /// flag day -- server packages are installed by administrators on RHEL 9
    /// while clients update on three platforms, so some skew is guaranteed.
    /// The server answers with the version the two sides agreed on, so this is
    /// a range check against the floor rather than an equality test.
    #[test]
    fn a_server_below_the_floor_is_refused_but_an_older_one_is_not() {
        let refuse = |v: u16| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let mut h = hello(1, 1);
            if let Message::ServerHello { version, .. } = &mut h {
                *version = v;
            }
            let _server = fake_server(listener, vec![h]);
            Client::connect(addr, &ConnectOptions::default(), None)
        };

        if MIN_COMPATIBLE_VERSION > 0 {
            let err = refuse(MIN_COMPATIBLE_VERSION - 1)
                .expect_err("a server below the floor must be refused");
            assert!(err.to_string().contains("version"), "{err}");
        }
        // Every version from the floor up to ours inclusive must be accepted;
        // today that is a single value, and it will not stay that way.
        for v in MIN_COMPATIBLE_VERSION..=PROTOCOL_VERSION {
            assert!(refuse(v).is_ok(), "version {v} should have been accepted");
        }
    }

    #[test]
    fn a_refused_offer_is_reported_and_not_merely_declined() {
        // The policy refusing an offer used to be silent: `TransferManager`
        // answers on the wire and forgets, nothing reaches `Outcome::failed`,
        // and the requester waits for a completion that cannot come. A caller
        // counting a batch of downloads then never publishes it.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = fake_server(
            listener,
            vec![
                hello(4, 4),
                // An id this client never asked for: refused by the policy,
                // which is the same code path a local `File::create` failure
                // takes on a name NTFS will not accept.
                Message::TransferOffer {
                    id: 4242,
                    purpose: TransferPurpose::FileDownload,
                    name: "what?.png".into(),
                    size: 9,
                },
            ],
        );
        let mut c = Client::connect(addr, &ConnectOptions::default(), None).unwrap();
        match c.poll_event(Duration::from_secs(5)).unwrap().unwrap() {
            ClientEvent::TransferFailed { id, reason } => {
                assert_eq!(id, 4242);
                assert!(reason.contains("unsolicited"), "{reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        c.disconnect("bye");
        let received = server.join().unwrap();
        assert!(
            received.iter().any(|m| matches!(
                m,
                Message::TransferAccept {
                    id: 4242,
                    accepted: false,
                    ..
                }
            )),
            "the peer must still be told it was refused"
        );
    }

    #[test]
    fn a_refusal_the_policy_never_saw_is_reported_too() {
        // The policy is not the only thing that turns an offer away.
        // `TransferManager` refuses an offer that would be the third landing
        // in memory before it consults the policy at all, so a scheme that
        // reports refusals from inside the policy misses this one entirely --
        // and the requester waits for a completion that cannot come. Reading
        // the refusal off the reply is what covers every path at once.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let image = |id: u64| Message::TransferOffer {
            id,
            purpose: TransferPurpose::ClipboardImage,
            name: "shot.png".into(),
            // Non-zero, so each stays in flight rather than completing on the
            // offer and freeing its slot again.
            size: 16,
        };
        let mut script = vec![hello(4, 4)];
        script.extend((0..=MAX_CONCURRENT_MEMORY_TRANSFERS as u64).map(image));
        let server = fake_server(listener, script);
        let mut c = Client::connect(addr, &ConnectOptions::default(), None).unwrap();
        let refused = MAX_CONCURRENT_MEMORY_TRANSFERS as u64;
        match c.poll_event(Duration::from_secs(5)).unwrap().unwrap() {
            ClientEvent::TransferFailed { id, reason } => {
                assert_eq!(id, refused, "{reason}");
                assert!(reason.contains("in-memory"), "{reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        c.disconnect("bye");
        let received = server.join().unwrap();
        assert!(
            received.iter().any(|m| matches!(
                m,
                Message::TransferAccept {
                    id,
                    accepted: false,
                    ..
                } if *id == refused
            )),
            "the peer must still be told it was refused"
        );
    }

    #[test]
    fn every_outcome_of_one_message_reaches_the_caller() {
        // A `TransferAck` pumps every outgoing transfer at once and a
        // `TransferEnd` resolves whatever the same message finished, so one
        // message can produce several events. Returning the first and dropping
        // the rest leaves a caller tracking a set of transfers waiting on ids
        // that were resolved and thrown away.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = fake_server(listener, vec![hello(4, 4)]);
        let mut c = Client::connect(addr, &ConnectOptions::default(), None).unwrap();
        c.queued.push_back(ClientEvent::TransferFailed {
            id: 1,
            reason: "a".into(),
        });
        c.queued.push_back(ClientEvent::TransferFailed {
            id: 2,
            reason: "b".into(),
        });
        assert!(matches!(
            c.try_event().unwrap(),
            Some(ClientEvent::TransferFailed { id: 1, .. })
        ));
        assert!(matches!(
            c.try_event().unwrap(),
            Some(ClientEvent::TransferFailed { id: 2, .. })
        ));
        // Even after the connection is gone: a queued completion is owed to
        // the caller regardless of what happened to the socket afterwards.
        c.closed = Some("gone".into());
        c.queued.push_back(ClientEvent::FileUploaded {
            id: 3,
            name: "c".into(),
        });
        assert!(matches!(
            c.try_event().unwrap(),
            Some(ClientEvent::FileUploaded { id: 3, .. })
        ));
        assert!(matches!(
            c.try_event().unwrap(),
            Some(ClientEvent::Disconnected(_))
        ));
    }

    #[test]
    fn a_rejection_can_be_read_back_out_of_what_the_user_is_shown() {
        // The pair, not a literal: the session window has to tell a rejection
        // from an ordinary drop to decide whether retrying is worth anything,
        // and it has only this string to go on.
        for code in [0u16, 1, 2, 3, 4, u16::MAX] {
            let text = rejection_reason(code, "no: (parens) and 12345");
            assert_eq!(rejection_code(&text), Some(code), "{text}");
            assert!(mentions_rejection(&text), "{text}");
            // Wrapped in context on the way up, which is what actually
            // reaches the retry classifier.
            let wrapped = format!("reconnecting: {text}");
            assert_eq!(rejection_code(&wrapped), None, "{wrapped}");
            assert!(mentions_rejection(&wrapped), "{wrapped}");
        }
        // Anything else is not a rejection, and must not be mistaken for one.
        for other in [
            "connection closed",
            "no response from the session for 30s",
            "Another client connected to this session.",
            "rejected (code x): not a number",
            "rejected (code ",
            "",
        ] {
            assert_eq!(rejection_code(other), None, "{other}");
            assert!(!mentions_rejection(other), "{other}");
        }
        // A rejection nested inside another failure is still a rejection: the
        // strict parse says no, the question the classifier asks says yes.
        assert_eq!(rejection_code("protocol error: rejected (code 2): x"), None);
        assert!(mentions_rejection("protocol error: rejected (code 2): x"));
    }

    #[test]
    fn abandoning_a_dead_link_does_not_write_to_it() {
        // The point of `abandon`: no `Disconnect` goes out, so a half-open
        // socket cannot hold the caller for the write timeout. The reader
        // thread is still collected, which is the part that must happen.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = fake_server(listener, vec![hello(4, 4)]);
        let mut c = Client::connect(addr, &ConnectOptions::default(), None).unwrap();
        c.abandon("link lost");
        assert_eq!(c.closed(), Some("link lost"));
        // Dropping it must not send either: `Drop` only speaks for a
        // connection nobody has closed.
        drop(c);
        let received = server.join().unwrap();
        assert!(
            !received
                .iter()
                .any(|m| matches!(m, Message::Disconnect { .. })),
            "a goodbye was written down a link that had already failed: {received:?}"
        );
    }

    #[test]
    fn wake_callback_fires() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = fake_server(
            listener,
            vec![hello(4, 4), Message::Notice { text: "hi".into() }],
        );
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let mut c = Client::connect(
            addr,
            &ConnectOptions::default(),
            Some(Box::new(move || {
                let _ = wtx.send(());
            })),
        )
        .unwrap();
        assert!(wrx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert_eq!(
            c.poll_event(Duration::from_secs(5)).unwrap().unwrap(),
            ClientEvent::Notice("hi".into())
        );
    }
}
