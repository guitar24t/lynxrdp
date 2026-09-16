//! The session core: one thread that owns the X display objects, the
//! current client, and all frame pacing decisions.
//!
//! ## Latency model
//!
//! * Damage events only set a flag; pixels are fetched lazily right before
//!   a frame is sent, so a frame always carries the newest content.
//! * A bounded number of frames may be unacknowledged. When the client is
//!   slow, changes accumulate and are sent as one frame once an ack arrives,
//!   so the client never falls behind the screen by more than one frame's
//!   worth of transmission time. The bound starts at the configured
//!   `max_in_flight` and, unless the operator turned that off, grows towards
//!   the number of frames that actually fit in the measured round trip --
//!   see [`adapt_in_flight`].
//! * Frames are rate limited to `max_fps`.
//! * Input is applied the moment it arrives, ahead of any frame work.
//! * No transfer byte is read or written here. Opening, creating, reading and
//!   writing all go through [`super::fileio`], on a thread of their own,
//!   because this one cannot afford to wait on a disk. Two things are still
//!   done inline, both on the clipboard path: one `stat` per path when the
//!   *session* copies files (see [`Core::on_clipboard_event`]), and the whole
//!   of [`Core::stage_client_files`] when the *client* does -- publishing the
//!   client's copy creates the staging directories and mounts a FUSE view,
//!   which execs `fusermount3`, and the mount it replaces has to be unmounted
//!   and deleted first. Nothing rate limits `Message::FileList`, so that
//!   second one is a hitch a client can ask for as often as it likes.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{after, never, select, Receiver, Sender};
use lynxrdp_proto::codec::{CopyRect, Encoder, FrameUpdate, TileUpdate};
use lynxrdp_proto::frame::{frame_message, HEADER_LEN};
use lynxrdp_proto::message::{clipboard_format, features, reject, CursorImage};
use lynxrdp_proto::transfer::{
    safe_relative_path, Completed, Sink, TransferManager, TransferPolicy, TransferPurpose,
};
use lynxrdp_proto::{
    agreed_version, peer_meets_floor, Framebuffer, Message, Rect, MAX_MESSAGE_SIZE,
    MIN_COMPATIBLE_VERSION, TILE_SIZE,
};
use x11rb::protocol::randr;
use x11rb::protocol::Event;

use super::fileio::{FileIo, FileOpened, FileReader};
use super::listener::spawn_client_reader;
use super::socket::ClientSocket;
use super::{CoreEvent, NewClient, SessionOptions};
use crate::x11::capture::{coalesce, DamageTracker, ScreenCapture};
use crate::x11::clipboard::{Clipboard, ClipboardEvent};
use crate::x11::cursor::CursorTracker;
use crate::x11::input::InputInjector;
use crate::x11::resize::resize_screen;
use crate::x11::XDisplay;
use crate::SERVER_NAME;

/// How long a client may take to send its hello.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Interval between latency probes.
const PING_INTERVAL: Duration = Duration::from_secs(2);
/// A client that has not answered a ping for this long is dropped.
const PONG_TIMEOUT: Duration = Duration::from_secs(30);
/// Housekeeping tick.
const HOUSEKEEPING: Duration = Duration::from_millis(250);
/// Outgoing message queue depth per client.
const WRITE_QUEUE: usize = 256;
/// How long `drop_client` lets the writer finish flushing before it shuts the
/// socket down underneath it. Long enough that a healthy client still receives
/// the queued `Disconnect` reason -- two end-to-end tests assert exactly that --
/// and short enough that a wedged one cannot hold the session core hostage.
const WRITER_FLUSH_GRACE: Duration = Duration::from_millis(500);
/// Hard ceiling on the in-flight window, matching the upper bound `config.rs`
/// accepts for the configured floor. Past this the window is buffering, not
/// pipelining: eight frames of a screen the user is no longer looking at.
const MAX_IN_FLIGHT_CAP: u32 = 8;
/// How far back the base round-trip estimate looks.
///
/// Long enough to keep a genuinely quiet moment in view across a burst of
/// heavy frames, short enough that a path that really did get longer -- a
/// laptop moving to a worse network -- is believed within a few seconds.
const RTT_WINDOW: Duration = Duration::from_secs(10);
/// Samples kept for the base estimate regardless of age. At 240 fps ten
/// seconds is 2400 of them and the minimum is no better for having them all.
const RTT_SAMPLES: usize = 256;
/// Longest run of a peer-supplied string that is quoted back to the peer.
///
/// A `FileRequest` path or a `TransferOffer` name may be as long as the
/// decoder allows, and every reply that quotes one adds a prefix, so quoting
/// a maximal one verbatim produces a string the encoder refuses -- by
/// asserting, on the session's main thread. Nothing a client needs telling
/// about its own path takes more than this to recognise.
const ECHO_LIMIT: usize = 256;
/// Consecutive captures allowed to fail on stale root geometry before the
/// failure is believed.
///
/// A root that shrank between the choice of rectangles and the request is one
/// bad frame, and the next one at the re-read size is fine. A capture that
/// keeps failing the same way at a size just read from the server is not that,
/// and retrying it at the frame rate forever would hide whatever it is.
const STALE_CAPTURE_LIMIT: u32 = 3;
/// Smallest screen edge a client may ask for.
const MIN_SCREEN_DIM: u32 = 64;
/// Bytes a `ScreenUpdate` costs beyond its copies and tiles: kind, frame id,
/// two counts. Pinned against the real encoder by a test below.
const SCREEN_UPDATE_FIXED: usize = 1 + 8 + 4 + 4;
/// Bytes per copy rectangle on the wire: six `u16`s.
const COPY_RECT_BYTES: usize = 12;
/// Bytes per tile beyond its payload: four `u16`s, the encoding, the length.
const TILE_FIXED: usize = 8 + 1 + 4;

/// Supported feature bits.
const SUPPORTED_FEATURES: u32 = features::LOCAL_CURSOR
    | features::CLIPBOARD
    | features::RESIZE
    | features::CLIPBOARD_IMAGE
    | features::FILE_TRANSFER
    | features::CLIPBOARD_FILES
    | features::ATOMIC_FILES
    | features::TARGETED_DROPS;

struct Client {
    generation: u64,
    socket: ClientSocket,
    description: String,
    writer_tx: Sender<Vec<u8>>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    connected_at: Instant,
    ready: bool,
    features: u32,
    /// Frames sent but not yet acknowledged, oldest first, with the moment
    /// each went out. The length is the in-flight count; keeping the two as
    /// one thing means they cannot drift apart.
    frames_in_flight: VecDeque<(u64, Instant)>,
    /// Round-trip samples behind the base estimate the window rule needs.
    rtt: RttWindow,
    /// How many frames may be in flight right now. Starts at the configured
    /// `max_in_flight` and only ever moves above it.
    window: u32,
    next_frame_id: u64,
    last_frame_at: Option<Instant>,
    full_refresh: bool,
    last_ping_at: Instant,
    last_pong_at: Instant,
    ping_nonce: u64,
    /// Last pointer position reported by the client.
    last_client_pointer: (i16, i16),
    /// Last position we told the client about.
    last_sent_pointer: Option<(i16, i16)>,
    /// A message to this client was lost: its queue overflowed, its writer is
    /// gone, or something too large for the wire was refused. Set here, acted
    /// on by [`Core::drop_dead_client`].
    dead: bool,
    bytes_sent: u64,
    frames_sent: u64,
}

impl Client {
    /// Queue a message for the writer thread.
    ///
    /// `false` means the message did not go, and that is the end of this
    /// client: session state has already moved on as if it had -- a transfer's
    /// chunk sequence advanced, the encoder switched to a new size -- so a
    /// client that stays connected past a lost message has a wrong picture of
    /// the session until it eventually times out. The client is marked rather
    /// than dropped here because the callers are deep inside handlers holding
    /// state mid-update; `Core::drop_dead_client` does the dropping at the next
    /// point where nothing is half done. What matters is that the two cannot
    /// come apart, whichever message it was.
    fn send(&mut self, msg: &Message) -> bool {
        if self.dead {
            return false;
        }
        let mut buf = Vec::new();
        frame_message(msg, &mut buf);
        let len = buf.len() - HEADER_LEN;
        // The receiver refuses a frame over the cap without reading it and
        // drops the link -- then reconnects into the same message again,
        // forever. Better refused here, once, with the message named.
        if len > MAX_MESSAGE_SIZE as usize {
            log::error!(
                "{:?} to client {} is {len} bytes, over the {MAX_MESSAGE_SIZE}-byte frame \
                 limit; dropping the client",
                msg.kind(),
                self.description
            );
            self.dead = true;
            return false;
        }
        match self.writer_tx.try_send(buf) {
            Ok(()) => {
                self.bytes_sent += (len + HEADER_LEN) as u64;
                true
            }
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                log::warn!(
                    "client {} is not draining its socket; dropping",
                    self.description
                );
                self.dead = true;
                false
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                self.dead = true;
                false
            }
        }
    }
}

/// Keeps clipboard text from bouncing between the two sides.
///
/// Text crosses the wire on copy, so each side sets its clipboard to what the
/// other sent, and a clipboard manager on either side then announces the new
/// contents as a fresh copy. Without a record of what just crossed, that
/// announcement crosses back, and so on. The record is one value per
/// direction: the text last sent to the client, which the client may echo,
/// and the text last received from it, which the session's own clipboard may.
///
/// Each value is only an echo detector while *both* clipboards still hold that
/// text, and that is the part the first version of this got wrong: it kept
/// each value until the next text in the same direction, so after the session
/// copied an image, a genuine copy of the same text on the client was taken
/// for the echo of a send made an hour before, and the paste produced the
/// image. So a value is dropped as soon as either clipboard is known to hold
/// something else -- text the other way, or a copy of another kind that
/// either side announces.
#[derive(Debug, Default)]
struct ClipboardEcho {
    /// Text last sent to the client, while the session's clipboard still
    /// holds it. A copy of it arriving from the client is an echo.
    sent: Option<String>,
    /// Text last received from the client, while the client's clipboard still
    /// holds it. A copy of it turning up in the session is an echo.
    received: Option<String>,
}

impl ClipboardEcho {
    /// The session's clipboard produced `text`. Whether to send it on.
    fn session_copied(&mut self, text: &str) -> bool {
        if self.received.as_deref() == Some(text) {
            return false;
        }
        self.sent = Some(text.to_owned());
        // The client's clipboard is about to hold this, not what it sent.
        self.received = None;
        true
    }

    /// The client sent `text`. Whether to put it on the session's clipboard.
    fn client_copied(&mut self, text: &str) -> bool {
        if self.sent.as_deref() == Some(text) {
            return false;
        }
        self.received = Some(text.to_owned());
        // The session's clipboard is about to hold this, not what was sent.
        self.sent = None;
        true
    }

    /// The session's clipboard has a new owner: whatever was sent from it is
    /// no longer what it holds.
    fn session_changed(&mut self) {
        self.sent = None;
    }

    /// The client announced a copy that is not text: whatever it sent is no
    /// longer what it holds.
    fn client_changed(&mut self) {
        self.received = None;
    }
}

/// Decides what the session accepts from the client.
///
/// The session runs as the user, so an upload cannot reach anything the user
/// could not already write. It still refuses path traversal, so a malicious
/// or buggy offer cannot escape the upload directory into, say, `~/.ssh`.
///
/// Note what is and is not decided here. The questions of *whether* we are
/// willing to write -- the path traversal check, and whether a download was
/// solicited -- are the security ones, and they are answered right here, on
/// the spot, out of memory. Only the filesystem work itself is handed to the
/// worker, so a wedged mount can cost a transfer but never a frame.
struct SessionTransferPolicy {
    replace: bool,
    upload_dir: PathBuf,
    /// Downloads the session asked the client for, while staging a clipboard
    /// file copy. Anything else in that direction is refused.
    staging: std::collections::HashMap<u64, PathBuf>,
    fileio: FileIo,
}

impl TransferPolicy for SessionTransferPolicy {
    fn accept(
        &mut self,
        id: u64,
        purpose: TransferPurpose,
        name: &str,
        _size: u64,
    ) -> Result<Sink, String> {
        match purpose {
            TransferPurpose::ClipboardImage => Ok(Sink::Memory(Vec::new())),
            TransferPurpose::FileUpload => {
                let rel = safe_relative_path(name).ok_or_else(|| {
                    format!("refusing unsafe upload path {:?}", echo_prefix(name))
                })?;
                let dest = self.upload_dir.join(&rel);
                log::info!("receiving upload into {}", dest.display());
                // The directory and the file are created on the worker, so a
                // refusal to write is reported when the transfer finishes
                // rather than when it is offered. The peer is told either way.
                Ok(Sink::Stream(Box::new(self.fileio.create(
                    self.upload_dir.clone(),
                    rel,
                    self.replace,
                ))))
            }
            TransferPurpose::FileDownload => {
                // Only files we asked for while staging a clipboard copy.
                let dest = self
                    .staging
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| "unsolicited download offer".to_string())?;
                Ok(Sink::Stream(Box::new(self.fileio.create(
                    dest.parent().unwrap().to_path_buf(),
                    dest.file_name().unwrap().to_string_lossy().into_owned(),
                    false,
                ))))
            }
        }
    }
}

/// Everything the session core owns.
pub struct Core {
    display: Arc<XDisplay>,
    opts: SessionOptions,
    capture: ScreenCapture,
    damage: DamageTracker,
    input: InputInjector,
    cursor: Option<CursorTracker>,
    clipboard: Option<Clipboard>,
    encoder: Encoder,
    screen: Framebuffer,
    current_cursor: Option<CursorImage>,
    client: Option<Client>,
    next_generation: u64,
    events_rx: Receiver<CoreEvent>,
    events_tx: Sender<CoreEvent>,
    min_frame_interval: Duration,
    last_client_seen: Instant,
    /// What clipboard text last crossed, each way, so that neither side's
    /// echo of it crosses back.
    echo: ClipboardEcho,
    /// Transfers in flight in both directions.
    transfers: TransferManager,
    /// Where uploads land.
    upload_dir: PathBuf,
    /// Clipboard formats the client last announced.
    client_formats: u32,
    drops: Option<super::drop::Drops>,
    /// Downloads in flight while staging: transfer id to destination.
    staging_downloads: std::collections::HashMap<u64, PathBuf>,
    // Drop pending replies before mounts, then remove their parent directory.
    staging_results: std::collections::HashMap<
        u64,
        crossbeam_channel::Sender<super::lazy_clipboard::FetchReply>,
    >,
    /// Current metadata-only clipboard offer; reads request contents lazily.
    staging_batch: Option<super::lazy_clipboard::Files>,
    /// Private mounts and content caches for client clipboard references.
    staging_dir: tempfile::TempDir,
    upload_options: Option<(u64, bool)>,
    /// Every file the session opens or writes goes through here, on a thread
    /// of its own.
    fileio: FileIo,
    /// Readers for files a client asked to download, held between asking the
    /// worker to open one and hearing back. Dropping one closes the file.
    pending_downloads: std::collections::HashMap<u64, FileReader>,
    /// The screen size we are actually serving, which is the root size clamped
    /// to what the capture can reach. Everything about framing reads this and
    /// not `display.size()`, because the two can differ -- see
    /// [`Core::adopt_size`].
    served: (u32, u32),
    /// Largest screen the capture can serve. `ScreenCapture` sizes its shared
    /// memory segment once, at construction, and cannot grow it afterwards.
    capture_max: (u32, u32),
    /// Captures in a row that failed on stale root geometry; see
    /// [`STALE_CAPTURE_LIMIT`].
    stale_captures: u32,
}

/// Why the core loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Desktop session ended.
    DesktopExited(String),
    /// Client went away and `exit_on_disconnect` is set.
    ClientDisconnected,
    /// No client for `idle_timeout`.
    IdleTimeout,
    /// Signal or explicit request.
    Shutdown(String),
    /// The X server connection failed.
    XError(String),
}

impl Core {
    /// Build the core for an X display.
    pub fn new(
        display: Arc<XDisplay>,
        opts: SessionOptions,
        events_tx: Sender<CoreEvent>,
        events_rx: Receiver<CoreEvent>,
    ) -> Result<Self> {
        // Ask to hear about root-window resizes we did not make. An `xrandr`
        // run inside the session, or a desktop applying the display layout it
        // saved at last logout, changes the screen under us; without this the
        // core keeps its old size, and the next `GetImage` after a screen that
        // *shrank* asks for pixels outside the root. That is a BadMatch, which
        // becomes `Exit::XError`, which SIGTERMs the user's whole desktop.
        //
        // Before the size is read, not after, and the order is the whole point.
        // Everything below this -- the SHM segment, the damage tracker, the
        // keymap, the clipboard atoms -- is round trips, and it runs at exactly
        // the moment the settings daemon is applying the saved display layout.
        // Selecting first means a change in that window arrives as an event we
        // will process; reading first would mean a change in that window was
        // invisible in both directions at once. The event thread is not running
        // yet, but the connection queues events until it is.
        if display.ext.randr {
            let selected = randr::select_input(
                display.conn(),
                display.root(),
                randr::NotifyMask::SCREEN_CHANGE,
            )
            .map_err(anyhow::Error::from)
            .and_then(|c| c.check().map_err(anyhow::Error::from));
            if let Err(e) = selected {
                log::warn!("cannot subscribe to RANDR screen changes: {e:#}");
            }
        }
        let (w, h) = display.refresh_size()?;
        let capture_max = (opts.max_width.max(w), opts.max_height.max(h));
        let capture = ScreenCapture::new(display.clone(), capture_max.0, capture_max.1)?;
        let damage = DamageTracker::new(display.clone())?;
        let input = InputInjector::new(display.clone())?;
        let cursor = match CursorTracker::new(display.clone()) {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("cursor tracking disabled: {e:#}");
                None
            }
        };
        let clipboard = match Clipboard::new(display.clone()) {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("clipboard disabled: {e:#}");
                None
            }
        };
        let min_frame_interval = Duration::from_secs_f64(1.0 / f64::from(opts.max_fps.max(1)));
        let upload_dir = opts.upload_dir.clone();
        // Clipboard files from the client are staged here so the session can
        // paste them as ordinary local files.
        let staging_dir = tempfile::Builder::new()
            .prefix("lynxrdp-clip-")
            .tempdir()
            .context("creating private clipboard staging")?;
        let drops = super::drop::Drops::new(display.clone(), staging_dir.path().join("drops"))
            .map_err(|e| log::warn!("native drops unavailable: {e:#}"))
            .ok();
        let fileio =
            FileIo::spawn(events_tx.clone()).context("starting the session file worker")?;
        log::info!(
            "session core ready: {w}x{h}, shm={}, cursor={}, clipboard={}, max_fps={}, \
             in_flight={}{}",
            capture.uses_shm(),
            cursor.is_some(),
            clipboard.is_some(),
            opts.max_fps,
            opts.max_in_flight,
            if opts.max_in_flight_auto {
                " (adaptive)"
            } else {
                ""
            }
        );
        Ok(Self {
            display,
            opts,
            capture,
            damage,
            input,
            cursor,
            clipboard,
            encoder: Encoder::new(w, h),
            screen: Framebuffer::new(w, h),
            current_cursor: None,
            client: None,
            next_generation: 1,
            events_rx,
            events_tx,
            min_frame_interval,
            last_client_seen: Instant::now(),
            echo: ClipboardEcho::default(),
            transfers: TransferManager::new(false),
            upload_dir,
            client_formats: 0,
            staging_dir,
            drops,
            staging_downloads: std::collections::HashMap::new(),
            staging_batch: None,
            staging_results: Default::default(),
            upload_options: None,
            fileio,
            pending_downloads: std::collections::HashMap::new(),
            served: (w, h),
            capture_max,
            stale_captures: 0,
        })
    }

    /// Spawn the thread that forwards X events into the core channel.
    pub fn spawn_x_event_thread(display: Arc<XDisplay>, tx: Sender<CoreEvent>) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("x-events".into())
            .spawn(move || loop {
                use x11rb::connection::Connection;
                match display.conn().wait_for_event() {
                    Ok(ev) => {
                        if tx.send(CoreEvent::X(ev)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(CoreEvent::XError(e.to_string()));
                        break;
                    }
                }
            })
            .expect("spawn x event thread")
    }

    /// Run until the session ends.
    pub fn run(&mut self) -> Exit {
        let housekeeping = crossbeam_channel::tick(HOUSEKEEPING);
        loop {
            let frame_wait = self.next_frame_delay();
            let frame_timer = match frame_wait {
                Some(d) => after(d),
                None => never(),
            };
            let outcome: Result<Option<Exit>> = select! {
                recv(self.events_rx) -> ev => match ev {
                    Ok(ev) => self.handle(ev),
                    Err(_) => Ok(Some(Exit::Shutdown("event channel closed".into()))),
                },
                recv(frame_timer) -> _ => Ok(None),
                recv(housekeeping) -> _ => self.housekeeping(),
            };
            if let Some(exit) = Self::settle(outcome, "session core error") {
                return exit;
            }
            // Drain anything else that is already queued before doing frame
            // work, so a burst of input is applied in one go.
            while let Ok(ev) = self.events_rx.try_recv() {
                if let Some(exit) = Self::settle(self.handle(ev), "session core error") {
                    return exit;
                }
            }
            // A send that failed inside any handler above only marked the
            // client; this is the first point at which nothing is half done.
            if let Some(exit) = Self::settle(self.drop_dead_client(), "session core error") {
                return exit;
            }
            self.pump_clipboard_batch();
            let pumped = self.pump().and_then(|()| self.drop_dead_client());
            if let Some(exit) = Self::settle(pumped, "frame pump error") {
                return exit;
            }
        }
    }

    /// The exit a step of the loop asked for, with an error treated as the
    /// loss of the display: by the rule the handlers follow -- anything a
    /// client or the clipboard did is logged, never propagated -- nothing
    /// else arrives here as an error.
    fn settle(outcome: Result<Option<Exit>>, what: &str) -> Option<Exit> {
        match outcome {
            Ok(exit) => exit,
            Err(e) => {
                log::error!("{what}: {e:#}");
                Some(Exit::XError(format!("{e:#}")))
            }
        }
    }

    fn next_frame_delay(&self) -> Option<Duration> {
        let c = self.client.as_ref()?;
        if !c.ready || c.frames_in_flight.len() as u32 >= c.window {
            return None;
        }
        if !(self.damage.is_dirty() || c.full_refresh) {
            return None;
        }
        let last = c.last_frame_at?;
        let next = last + self.min_frame_interval;
        Some(next.saturating_duration_since(Instant::now()))
    }

    fn handle(&mut self, ev: CoreEvent) -> Result<Option<Exit>> {
        match ev {
            CoreEvent::X(ev) => {
                self.handle_x_event(ev)?;
                Ok(None)
            }
            CoreEvent::XError(e) => Ok(Some(Exit::XError(e))),
            CoreEvent::NewClient(nc) => {
                self.accept_client(nc)?;
                Ok(None)
            }
            CoreEvent::ClientMessage(generation, msg) => {
                if self.client.as_ref().map(|c| c.generation) != Some(generation) {
                    return Ok(None);
                }
                self.handle_client_message(msg)
            }
            CoreEvent::ClientClosed(generation, reason) => {
                if self.client.as_ref().map(|c| c.generation) == Some(generation) {
                    log::info!("client disconnected: {reason}");
                    self.drop_client(None)?;
                    return Ok(self.exit_after_disconnect());
                }
                Ok(None)
            }
            CoreEvent::DesktopExited(status) => {
                log::info!("desktop session ended: {status}");
                self.drop_client(Some("The desktop session has ended."))?;
                Ok(Some(Exit::DesktopExited(status)))
            }
            CoreEvent::Shutdown(reason) => {
                self.drop_client(Some("The server is shutting down."))?;
                Ok(Some(Exit::Shutdown(reason)))
            }
            CoreEvent::FileReady => {
                let out = self.transfers.poll();
                self.apply_transfer_outcome(out);
                Ok(None)
            }
            CoreEvent::FileOpened(opened) => {
                self.on_file_opened(*opened);
                Ok(None)
            }
            CoreEvent::ClipboardFetch => {
                self.pump_clipboard_batch();
                Ok(None)
            }
        }
    }

    fn handle_x_event(&mut self, ev: Event) -> Result<()> {
        if let Some(drops) = &mut self.drops {
            let replies = drops.event(&ev);
            self.send_to_client(replies);
        }
        match &ev {
            Event::DamageNotify(_) => {
                self.damage.mark_dirty();
            }
            Event::XfixesCursorNotify(_) => {
                self.refresh_cursor(false)?;
            }
            Event::MappingNotify(_) => {
                if let Err(e) = self.input.reload_keymap() {
                    log::warn!("keymap reload failed: {e:#}");
                }
            }
            Event::RandrScreenChangeNotify(_) => {
                // Somebody resized the root. The size is taken from the server
                // rather than out of the event because `refresh_size` is also
                // what updates the cached size the rest of the code reads; the
                // event's own fields would leave that stale. Not fatal on
                // failure, for the reason spelled out below the match: only a
                // lost connection should be able to end a desktop session, and
                // that arrives by another route.
                let size = self.display.refresh_size();
                match size {
                    Ok((w, h)) => self.adopt_size(w, h),
                    Err(e) => log::warn!("cannot read the root size after a resize: {e:#}"),
                }
            }
            Event::Error(e) => {
                log::debug!("X error: {e:?}");
            }
            _ => {}
        }
        if self.clipboard.is_some() {
            // Clipboard failures are logged, never propagated. `?` here reaches
            // `Core::run` as `Exit::XError`, which SIGTERMs the desktop -- so a
            // conversion this session could not perform ended the user's whole
            // login and their unsaved work with it. Pasting an image larger
            // than one X request is ordinary user action, not an attack.
            //
            // Losing the X connection for real is still fatal, but it arrives
            // as `CoreEvent::XError` from the event reader thread rather than
            // through here, so nothing is being swallowed that should not be.
            // The neighbouring `MappingNotify` arm already takes this view.
            let events = match self.clipboard.as_mut().expect("checked").handle_event(&ev) {
                Ok(events) => events,
                Err(e) => {
                    log::warn!("clipboard: {e:#}");
                    Vec::new()
                }
            };
            for event in events {
                if let Err(e) = self.on_clipboard_event(event) {
                    log::warn!("clipboard: {e:#}");
                }
            }
        }
        // Keep the queue type in use for future ordering needs.
        Ok(())
    }

    /// React to something the session's clipboard did.
    fn on_clipboard_event(&mut self, event: ClipboardEvent) -> Result<()> {
        match event {
            ClipboardEvent::Formats(formats) => {
                // A new owner, so whatever text was last sent from here is no
                // longer what the session holds -- even if this owner turns
                // out to offer the same text, which the fetch below will say.
                self.echo.session_changed();
                // Announce what is available; the client asks for anything
                // large only when it actually wants it.
                let offered = formats & self.client_clipboard_formats();
                if offered != 0 {
                    self.send_to_client(vec![Message::ClipboardOffer { formats: offered }]);
                }
            }
            ClipboardEvent::Text(text) => self.on_session_clipboard(text),
            ClipboardEvent::Image(png) => {
                if self.client_clipboard_formats() & clipboard_format::PNG == 0 {
                    return Ok(());
                }
                log::debug!("clipboard image -> client ({} bytes)", png.len());
                let offer = self.transfers.offer_bytes(
                    TransferPurpose::ClipboardImage,
                    "clipboard.png".to_string(),
                    png,
                );
                self.send_to_client(vec![offer]);
            }
            ClipboardEvent::Files(paths) => {
                if self.client_clipboard_formats() & clipboard_format::FILES == 0 {
                    return Ok(());
                }
                let mut files = Vec::new();
                for p in &paths {
                    // Inline, and a known gap rather than an oversight: an
                    // offer has to carry a size, and answering that from the
                    // worker would mean a second asynchronous round trip for a
                    // path the user just copied by hand. A `stat` per file is
                    // orders of magnitude cheaper than the reads and writes
                    // this module moved off, but on a mount that has stopped
                    // answering it hitches the frame loop just the same.
                    //
                    // Directories would need recursive listing; skip them
                    // rather than offering something we cannot deliver.
                    match std::fs::metadata(p) {
                        Ok(m) if m.is_file() => files.push(lynxrdp_proto::FileEntry {
                            path: p.to_string_lossy().into_owned(),
                            size: m.len(),
                        }),
                        Ok(_) => {
                            log::debug!("skipping non-file {} in a clipboard copy", p.display())
                        }
                        Err(e) => log::debug!("skipping {}: {e}", p.display()),
                    }
                }
                if files.is_empty() {
                    return Ok(());
                }
                log::debug!("clipboard: offering {} file(s) to the client", files.len());
                let id = self.transfers.next_id();
                // This answers ClipboardRequest. Advertising FILES again here
                // would trigger another request and restart the client's batch.
                self.send_to_client(vec![Message::FileList { id, files }]);
            }
            ClipboardEvent::Unavailable(format) => {
                log::debug!("clipboard format {format:#x} turned out to be unavailable");
                // Silence, for both kinds of format that reach here. One the
                // client asked for it cannot have asked for, since the offer
                // above is masked by what it said it understands; text it never
                // asks for at all, being fetched on our own initiative, but a
                // client with clipboard sharing switched off is not receiving
                // that copy either and should not be told one failed.
                if self.client_clipboard_formats() & format == 0 {
                    return Ok(());
                }
                // `ClipboardRequest` has no failure reply, and the client keeps
                // no record of having asked, so a conversion that cannot be
                // completed reaches the user as nothing whatsoever: their
                // clipboard keeps its old contents and the next paste is stale
                // with no sign that anything went wrong. Widening the protocol
                // for that would mean a new message shape carrying something
                // only a person ever reads, so it goes out as the message that
                // already exists for telling a person something.
                self.send_to_client(vec![Message::Notice {
                    text: unavailable_notice(format),
                }]);
            }
        }
        Ok(())
    }

    /// Publish names and sizes immediately. Native file reads during paste
    /// fetch contents, without blocking X11 selection replies or the core loop.
    fn stage_client_files(&mut self, files: Vec<lynxrdp_proto::FileEntry>) {
        if self.clipboard.is_none() || files.is_empty() {
            return;
        }
        for id in self.staging_downloads.keys().copied().collect::<Vec<_>>() {
            if let Some(msg) = self.transfers.cancel(id, "clipboard replaced") {
                self.send_to_client(vec![msg]);
            }
        }
        self.staging_results.clear();
        self.staging_downloads.clear();
        self.staging_batch = None;
        // The batch's fetch queue is not one of the core loop's channels, so
        // without a wake a read in the session waited for the housekeeping
        // tick before its request left -- once per file, one after another.
        let wake = self.events_tx.clone();
        let batch =
            super::lazy_clipboard::Files::with_wake(self.staging_dir.path(), &files, move || {
                let _ = wake.send(CoreEvent::ClipboardFetch);
            });
        match batch {
            Ok(batch) => {
                if let Err(e) = self
                    .clipboard
                    .as_mut()
                    .unwrap()
                    .set_files(batch.paths.clone())
                {
                    self.send_to_client(vec![Message::Notice {
                        text: format!("Could not offer copied files: {e:#}"),
                    }]);
                } else {
                    self.staging_batch = Some(batch);
                    self.send_to_client(vec![Message::Notice {
                        text: format!(
                            "{} copied file(s) ready. Paste into a folder in the remote session.",
                            files.len()
                        ),
                    }]);
                }
            }
            Err(e) => self.send_to_client(vec![Message::Notice {
                text: format!("Could not offer copied files: {e:#}"),
            }]),
        }
    }

    fn pump_clipboard_batch(&mut self) {
        // Before anything new starts, so a read retried after a stall never
        // finds the transfer it gave up on still running beside its own.
        self.report_staging_progress();
        while let Some(fetch) = self
            .staging_batch
            .as_ref()
            .and_then(|b| b.requests.try_recv().ok())
        {
            let id = self.transfers.next_id();
            self.transfers.expect(id);
            self.staging_downloads.insert(id, fetch.destination);
            self.staging_results.insert(id, fetch.result);
            self.send_to_client(vec![Message::FileRequest {
                id,
                path: fetch.remote,
            }]);
        }
    }

    fn settle_clipboard_file(&mut self, id: u64, success: bool) {
        use super::lazy_clipboard::FetchReply;
        if let Some(drops) = &mut self.drops {
            drops.settle(id, success);
        }
        let path = self.staging_downloads.remove(&id);
        if let Some(result) = self.staging_results.remove(&id) {
            let _ = result.send(match path.filter(|_| success) {
                Some(path) => FetchReply::Done(path),
                None => FetchReply::Failed,
            });
        }
    }

    /// Tell each waiting paste how its transfer is going, and cancel the
    /// transfers whose readers have stopped waiting.
    ///
    /// Every tick, and whether or not the count moved: the send is also the
    /// probe. The FUSE worker gives up on a fetch that shows no progress for
    /// `FETCH_IDLE` by dropping its end, and the failed send is the only way
    /// that decision reaches here. Without it the client would keep pushing
    /// a file nobody will read through the tunnel the next paste needs.
    fn report_staging_progress(&mut self) {
        use super::lazy_clipboard::FetchReply;
        let mut abandoned = Vec::new();
        for (id, result) in &self.staging_results {
            let received = self.transfers.progress(*id).map_or(0, |(done, _)| done);
            if let Err(crossbeam_channel::TrySendError::Disconnected(_)) =
                result.try_send(FetchReply::Progress(received))
            {
                abandoned.push(*id);
            }
        }
        for id in abandoned {
            self.staging_results.remove(&id);
            self.staging_downloads.remove(&id);
            if let Some(msg) = self.transfers.cancel(id, "the reader stopped waiting") {
                self.send_to_client(vec![msg]);
            }
        }
    }

    /// Answer a client's request to download a file from the session.
    ///
    /// The `open` and the `stat` both happen on the file worker, because the
    /// offer needs the file's size and a `stat` of a path on a hung mount is
    /// precisely what must not run here. The answer comes back through the
    /// core loop as [`CoreEvent::FileOpened`], so a request for an unreachable
    /// path costs the client its download and costs the desktop nothing.
    fn on_file_request(&mut self, id: u64, path: &str) {
        let Some(generation) = self.client.as_ref().map(|c| c.generation) else {
            return;
        };
        // One open per id at a time. The answer comes back asynchronously and
        // carries only the id, so a second request that reused an id still in
        // flight would be paired with the *first* request's reply: the client
        // would be offered the second file's bytes under the first file's name
        // and length. Refusing costs a client nothing -- it chooses these ids
        // and has no reason to collide with itself -- and it is the only way
        // the reply can be matched to the request that produced it.
        if self.pending_downloads.contains_key(&id)
            || self.pending_downloads.len() >= 64
            || self.transfers.active_ids().contains(&id)
            || self.transfers.active_ids().len() >= 64
        {
            let path = echo_prefix(path);
            log::warn!("transfer id {id} is already opening; refusing {path}");
            self.send_to_client(vec![Message::TransferEnd {
                id,
                ok: false,
                message: format!("{path}: transfer {id} is already in progress"),
            }]);
            return;
        }
        let reader = self
            .fileio
            .open(id, generation, path.to_string(), self.events_tx.clone());
        // Held only until the worker reports back. Dropping it closes the file,
        // which is what happens if the open failed or the client went away.
        self.pending_downloads.insert(id, reader);
    }

    /// The worker has opened (or failed to open) a file a client asked for.
    fn on_file_opened(&mut self, opened: FileOpened) {
        if self.client.as_ref().map(|c| c.generation) != Some(opened.generation) {
            return;
        }
        if !self
            .pending_downloads
            .get(&opened.id)
            .is_some_and(|reader| reader.handle() == opened.handle)
        {
            return;
        }
        let Some(reader) = self.pending_downloads.remove(&opened.id) else {
            return;
        };
        let reply = match opened.result {
            Ok(size) => {
                log::info!("sending {} ({size} bytes) to the client", opened.path);
                let name = std::path::Path::new(&opened.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| opened.path.clone());
                self.transfers.offer_stream_with_id(
                    opened.id,
                    TransferPurpose::FileDownload,
                    name,
                    size,
                    Box::new(reader),
                )
            }
            Err(e) => {
                let path = echo_prefix(&opened.path);
                log::warn!("cannot open {path} for download: {e}");
                Message::TransferEnd {
                    id: opened.id,
                    ok: false,
                    message: format!("{path}: {e}"),
                }
            }
        };
        self.send_to_client(vec![reply]);
    }

    /// Clipboard formats the connected client can handle.
    fn client_clipboard_formats(&self) -> u32 {
        let Some(c) = self.client.as_ref() else {
            return 0;
        };
        let mut f = 0;
        if c.features & features::CLIPBOARD != 0 {
            f |= clipboard_format::TEXT;
        }
        if c.features & features::CLIPBOARD_IMAGE != 0 {
            f |= clipboard_format::PNG;
        }
        if c.features & features::CLIPBOARD_FILES != 0 {
            f |= clipboard_format::FILES;
        }
        f
    }

    /// Send messages to the client. One that cannot be queued marks the
    /// client dead (see [`Client::send`]) and the rest are not attempted.
    fn send_to_client(&mut self, msgs: Vec<Message>) {
        let Some(c) = self.client.as_mut() else {
            return;
        };
        for m in msgs {
            if !c.send(&m) {
                break;
            }
        }
    }

    /// Feed a message to the transfer manager. Returns true if it was one.
    fn handle_transfer_message(&mut self, msg: &Message) -> bool {
        if let Message::TransferEnd { id, ok: false, .. } = msg {
            self.pending_downloads.remove(id);
        }

        if let Message::TransferOptions { id, replace } = msg {
            if self
                .client
                .as_ref()
                .is_some_and(|c| c.features & features::ATOMIC_FILES != 0)
            {
                self.upload_options = Some((*id, *replace));
            }
            return true;
        }
        let replace = if let Message::TransferOffer { id, .. } = msg {
            self.upload_options
                .take()
                .is_some_and(|(offered, replace)| offered == *id && replace)
        } else {
            false
        };
        let mut policy = SessionTransferPolicy {
            replace,
            upload_dir: self.upload_dir.clone(),
            staging: self
                .staging_downloads
                .clone()
                .into_iter()
                .chain(self.drops.iter().flat_map(|d| d.staging.clone()))
                .collect(),
            fileio: self.fileio.clone(),
        };
        let Some(outcome) = self.transfers.handle(msg, &mut policy) else {
            return false;
        };
        self.apply_transfer_outcome(outcome);
        true
    }

    fn apply_transfer_outcome(&mut self, outcome: lynxrdp_proto::transfer::Outcome) {
        for reply in &outcome.replies {
            if let Message::TransferAccept {
                id,
                accepted: false,
                ..
            } = reply
            {
                self.settle_clipboard_file(*id, false);
            }
        }
        self.send_to_client(outcome.replies);
        for (id, reason) in outcome.failed {
            log::warn!("transfer {id} failed: {reason}");
            self.settle_clipboard_file(id, false);
        }
        for done in outcome.completed {
            if let Err(e) = self.on_transfer_complete(done) {
                log::warn!("completed transfer: {e:#}");
            }
        }
        self.pump_clipboard_batch();
        if let Some(drops) = &mut self.drops {
            let replies = drops.pump(&mut self.transfers);
            self.send_to_client(replies);
        }
    }

    fn on_transfer_complete(&mut self, done: Completed) -> Result<()> {
        match done.purpose {
            TransferPurpose::ClipboardImage => {
                let Some(png) = done.data else { return Ok(()) };
                log::debug!("clipboard image <- client ({} bytes)", png.len());
                if let Some(cb) = self.clipboard.as_mut() {
                    if let Err(e) = cb.set_image(png) {
                        log::warn!("clipboard: setting the image failed: {e:#}");
                    }
                }
            }
            TransferPurpose::FileUpload => {
                let name = echo_prefix(&done.name);
                log::info!("upload of {name:?} finished");
                self.send_to_client(vec![Message::Notice {
                    text: format!("Received {name}"),
                }]);
            }
            TransferPurpose::FileDownload => {
                self.settle_clipboard_file(done.id, true);
            }
        }
        Ok(())
    }

    fn on_session_clipboard(&mut self, text: String) {
        let Some(c) = self.client.as_mut() else {
            return;
        };
        if !(c.ready && c.features & features::CLIPBOARD != 0) {
            return;
        }
        if !self.echo.session_copied(&text) {
            return;
        }
        log::debug!("clipboard -> client ({} bytes)", text.len());
        c.send(&Message::ClipboardText { text });
    }

    fn refresh_cursor(&mut self, force: bool) -> Result<()> {
        let Some(tracker) = self.cursor.as_mut() else {
            return Ok(());
        };
        if let Some(img) = tracker.fetch(force)? {
            self.current_cursor = Some(img.clone());
            if let Some(c) = self.client.as_mut() {
                if c.ready && c.features & features::LOCAL_CURSOR != 0 {
                    c.send(&Message::CursorShape { cursor: img });
                }
            }
        }
        Ok(())
    }

    fn accept_client(&mut self, nc: NewClient) -> Result<()> {
        if self.client.is_some() {
            log::info!("replacing existing client with {}", nc.description);
            self.drop_client(Some("Another client connected to this session."))?;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        nc.socket.set_nodelay();
        nc.socket.set_keepalive();
        let (writer_tx, writer_rx) = crossbeam_channel::bounded::<Vec<u8>>(WRITE_QUEUE);
        let mut wsock = nc.socket.clone();
        let desc = nc.description.clone();
        let writer = std::thread::Builder::new()
            .name(format!("client-{generation}-writer"))
            .spawn(move || {
                for buf in writer_rx {
                    if let Err(e) = wsock.write_all(&buf) {
                        log::debug!("write to {desc} failed: {e}");
                        break;
                    }
                }
            })
            .context("spawn writer")?;
        let reader = spawn_client_reader(nc.socket.clone(), generation, self.events_tx.clone());
        let now = Instant::now();
        log::info!(
            "client connected: {} (generation {generation})",
            nc.description
        );
        self.client = Some(Client {
            generation,
            socket: nc.socket,
            description: nc.description,
            writer_tx,
            writer: Some(writer),
            reader: Some(reader),
            connected_at: now,
            ready: false,
            features: 0,
            frames_in_flight: VecDeque::new(),
            rtt: RttWindow::default(),
            window: self.opts.max_in_flight.clamp(1, MAX_IN_FLIGHT_CAP),
            next_frame_id: 1,
            last_frame_at: None,
            full_refresh: false,
            last_ping_at: now,
            last_pong_at: now,
            ping_nonce: 0,
            last_client_pointer: self.input.last_pointer(),
            last_sent_pointer: None,
            dead: false,
            bytes_sent: 0,
            frames_sent: 0,
        });
        self.input.suppress_auto_repeat();
        self.last_client_seen = now;
        Ok(())
    }

    /// Disconnect the current client, optionally with a message.
    fn drop_client(&mut self, notice: Option<&str>) -> Result<()> {
        let Some(mut c) = self.client.take() else {
            return Ok(());
        };
        if let Some(text) = notice {
            c.send(&Message::Disconnect {
                reason: text.to_string(),
            });
        }
        // Close the queue so the writer drains what is already in it -- the
        // Disconnect notice above is the last entry -- and exits by itself.
        drop(c.writer_tx);
        // Then give it a bounded moment, and take the socket down whether or
        // not it finished. The writer is not necessarily parked on the channel:
        // against a peer that has stopped reading (a laptop asleep behind
        // sshd, a stalled forward) it is parked inside `write_all` on a full
        // socket, where closing the queue means nothing to it. Joining first is
        // therefore an unbounded wait *on the session core thread* -- no
        // frames, no input, no housekeeping, no idle timeout, and
        // `accept_client` never runs again, so the user cannot even reconnect
        // to the session they are still paying for. Only the shutdown unblocks
        // that write, so it has to come before the join, not after it.
        let writer = c.writer.take();
        if let Some(w) = &writer {
            let deadline = Instant::now() + WRITER_FLUSH_GRACE;
            while !w.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        c.socket.shutdown();
        if let Some(w) = writer {
            let _ = w.join();
        }
        if let Some(r) = c.reader.take() {
            let _ = r.join();
        }
        log::info!(
            "client {} closed after {:.1}s: {} frames, {} bytes",
            c.description,
            c.connected_at.elapsed().as_secs_f64(),
            c.frames_sent,
            c.bytes_sent
        );
        self.input.release_all()?;
        // Release first, then hand the keyboard back the way we found it: a
        // key still logically held when repeat comes back on would start
        // repeating with nobody left to release it.
        self.input.restore_auto_repeat();
        // Files opened for a download this client never collected. Dropping
        // the readers closes them.
        self.pending_downloads.clear();
        self.transfers.clear();
        self.upload_options = None;
        self.staging_downloads.clear();
        if let Some(drops) = &mut self.drops {
            drops.reset();
        }
        self.staging_results.clear();
        self.staging_batch = None;
        // The next client's clipboard is unknown, so nothing it sends can be
        // an echo of what went to this one.
        self.echo = ClipboardEcho::default();
        self.last_client_seen = Instant::now();
        Ok(())
    }

    /// Drop a client whose queue overflowed or closed since the last check.
    ///
    /// [`Client::send`] only marks; the drop happens here, at a point in the
    /// loop where no handler is mid-update, and with the same consequence a
    /// client leaving any other way has.
    fn drop_dead_client(&mut self) -> Result<Option<Exit>> {
        if !self.client.as_ref().is_some_and(|c| c.dead) {
            return Ok(None);
        }
        self.drop_client(None)?;
        Ok(self.exit_after_disconnect())
    }

    /// What a client's departure means for the session. Every path that
    /// loses a client ends in this, so `--exit-on-disconnect` cannot depend
    /// on *how* the client left.
    fn exit_after_disconnect(&self) -> Option<Exit> {
        self.opts
            .exit_on_disconnect
            .then_some(Exit::ClientDisconnected)
    }

    fn reject(&mut self, code: u16, reason: &str) -> Result<Option<Exit>> {
        log::warn!("rejecting client: {reason}");
        if let Some(c) = self.client.as_mut() {
            c.send(&Message::Rejected {
                code,
                reason: reason.to_string(),
            });
        }
        self.drop_client(None)?;
        Ok(self.exit_after_disconnect())
    }

    fn handle_client_message(&mut self, msg: Message) -> Result<Option<Exit>> {
        let ready = self.client.as_ref().map(|c| c.ready).unwrap_or(false);
        if !ready {
            return match msg {
                Message::ClientHello {
                    version,
                    client_name,
                    features: want,
                    width,
                    height,
                } => self.on_hello(
                    version,
                    client_name,
                    want,
                    u32::from(width),
                    u32::from(height),
                ),
                other => self.reject(
                    reject::VERSION,
                    &format!("expected ClientHello, got {:?}", other.kind()),
                ),
            };
        }
        // Transfers are symmetric and stateless from the core's point of view.
        if self.handle_transfer_message(&msg) {
            return Ok(None);
        }
        match msg {
            Message::KeyEvent { keysym, down } => {
                if let Err(e) = self.input.key(keysym, down) {
                    log::warn!("key injection failed: {e:#}");
                }
            }
            Message::PointerMotion { x, y } => {
                let (w, h) = self.served;
                let x = (u32::from(x).min(w.saturating_sub(1))) as i16;
                let y = (u32::from(y).min(h.saturating_sub(1))) as i16;
                if let Some(c) = self.client.as_mut() {
                    c.last_client_pointer = (x, y);
                    c.last_sent_pointer = Some((x, y));
                }
                self.input.pointer_move(x, y)?;
            }
            Message::PointerButton { button, down } => self.input.button(button, down)?,
            Message::Scroll { dx, dy } => self.input.scroll(dx, dy)?,
            Message::FrameAck { frame_id } => {
                let now = Instant::now();
                let (floor, auto, interval) = (
                    self.opts.max_in_flight,
                    self.opts.max_in_flight_auto,
                    self.min_frame_interval,
                );
                if let Some(c) = self.client.as_mut() {
                    // Acknowledgements arrive in order, so anything still
                    // queued ahead of this frame is never going to be
                    // acknowledged and is dropped with it. That also keeps the
                    // in-flight count honest without a second counter.
                    let found = c
                        .frames_in_flight
                        .iter()
                        .position(|(id, _)| *id == frame_id);
                    if let Some(idx) = found {
                        let (_, sent_at) = c.frames_in_flight[idx];
                        c.frames_in_flight.drain(..=idx);
                        let sample = now.saturating_duration_since(sent_at);
                        let base = c.rtt.record(now, sample);
                        if auto {
                            c.window = adapt_in_flight(c.window, floor, base, sample, interval);
                        }
                    }
                    log::trace!(
                        "ack frame {frame_id}, in flight {} of {}",
                        c.frames_in_flight.len(),
                        c.window
                    );
                }
            }
            Message::ResizeRequest { width, height } => {
                self.on_resize_request(u32::from(width), u32::from(height))?;
            }
            Message::ClipboardText { text } => {
                if self
                    .client
                    .as_ref()
                    .is_some_and(|c| c.features & features::CLIPBOARD == 0)
                {
                    return Ok(None);
                }
                if !self.echo.client_copied(&text) {
                    return Ok(None);
                }
                if let Some(cb) = self.clipboard.as_mut() {
                    log::debug!("clipboard <- client ({} bytes)", text.len());
                    // Non-fatal for the same reason as in `handle_x_event`:
                    // this arrives from the client, and nothing a client sends
                    // should be able to end the user's desktop session.
                    if let Err(e) = cb.set_text(text) {
                        log::warn!("clipboard: setting text failed: {e:#}");
                    }
                }
            }
            Message::Pong { nonce } => {
                if let Some(c) = self.client.as_mut() {
                    if nonce == c.ping_nonce {
                        let now = Instant::now();
                        // The ping round trip is the cleanest sample there is:
                        // a few bytes each way, queued behind nothing. Feeding
                        // it to the same minimum keeps the base estimate
                        // honest on a link whose frames are big enough to hide
                        // the path length, and keeps it fresh while the screen
                        // is idle and no frames are being acknowledged at all.
                        // Only frame acknowledgements move the window itself.
                        c.rtt
                            .record(now, now.saturating_duration_since(c.last_ping_at));
                        c.last_pong_at = now;
                    }
                }
            }
            Message::Ping { nonce } => {
                if let Some(c) = self.client.as_mut() {
                    c.send(&Message::Pong { nonce });
                }
            }
            Message::ClipboardOffer { formats } => {
                // The client copied something. Text arrives on its own; ask
                // for an image only when we can actually use it.
                self.echo.client_changed();
                self.client_formats = formats;
                if formats & clipboard_format::PNG != 0 && self.clipboard.is_some() {
                    self.send_to_client(vec![Message::ClipboardRequest {
                        format: clipboard_format::PNG,
                    }]);
                }
            }
            Message::ClipboardRequest { format } => {
                let events = match self.clipboard.as_mut() {
                    Some(cb) => cb.request_format(format).unwrap_or_else(|e| {
                        log::warn!("clipboard: requesting format {format:#x} failed: {e:#}");
                        Vec::new()
                    }),
                    None => Vec::new(),
                };
                for event in events {
                    if let Err(e) = self.on_clipboard_event(event) {
                        log::warn!("clipboard: {e:#}");
                    }
                }
            }
            Message::FileRequest { id, path } => self.on_file_request(id, &path),
            Message::FileList { files, .. } => self.stage_client_files(files),
            Message::FileDrop { id, x, y, files } => {
                if self
                    .client
                    .as_ref()
                    .is_some_and(|c| c.features & features::TARGETED_DROPS != 0)
                {
                    if let Some(drops) = &mut self.drops {
                        if let Err(e) = drops.begin(id, x, y, files) {
                            self.send_to_client(vec![Message::FileDropResult {
                                id,
                                ok: false,
                                reason: e.to_string(),
                            }]);
                        }
                    }
                }
            }
            Message::RefreshRequest => self.force_full_refresh(),
            Message::Disconnect { reason } => {
                log::info!("client requested disconnect: {reason}");
                self.drop_client(None)?;
                return Ok(self.exit_after_disconnect());
            }
            Message::ClientHello { .. } => {
                return self.reject(reject::VERSION, "duplicate ClientHello");
            }
            other => {
                return self.reject(
                    reject::VERSION,
                    &format!("unexpected message {:?} from client", other.kind()),
                );
            }
        }
        Ok(None)
    }

    fn on_hello(
        &mut self,
        version: u16,
        client_name: String,
        want: u32,
        width: u32,
        height: u32,
    ) -> Result<Option<Exit>> {
        // A floor rather than equality. Refusing anything but our own version
        // makes every release a flag day, which is the wrong trade when server
        // packages are installed by administrators on RHEL 9 while clients
        // update on three platforms. A client *newer* than us is accepted too:
        // it is the newer side's job to decline if it cannot manage, and it
        // knows what changed between the two versions where we cannot.
        if !peer_meets_floor(version) {
            return self.reject(
                reject::VERSION,
                &format!(
                    "protocol version {version} is below the minimum this server \
                     supports ({MIN_COMPATIBLE_VERSION})"
                ),
            );
        }
        let agreed = agreed_version(version);
        let features = want
            & SUPPORTED_FEATURES
            & if self.drops.is_some() {
                u32::MAX
            } else {
                !features::TARGETED_DROPS
            };
        // Apply the requested size (or the default) before the first frame.
        // Compared against the real root, not the size we serve: when the two
        // differ the root is oversized and a client asking for something we
        // can serve should get an actual resize out of it.
        let (cur_w, cur_h) = self.display.size();
        let (req_w, req_h) = if width > 0 && height > 0 {
            (width, height)
        } else if self.client.as_ref().map(|c| c.generation) == Some(1) {
            (self.opts.default_width, self.opts.default_height)
        } else {
            (cur_w, cur_h)
        };
        if (req_w, req_h) != (cur_w, cur_h) && features & features::RESIZE != 0 {
            if let Err(e) = self.apply_resize(req_w, req_h) {
                log::warn!("initial resize to {req_w}x{req_h} failed: {e:#}");
            }
        }
        let (w, h) = self.served;
        let Some(c) = self.client.as_mut() else {
            return Ok(None);
        };
        c.ready = true;
        c.features = features;
        log::info!(
            "client {} ({client_name}) accepted: features 0x{features:x}, screen {w}x{h}",
            c.description
        );
        c.send(&Message::ServerHello {
            // The agreed version, not ours: an older client checks this for
            // equality with its own, so announcing a newer number would have it
            // hang up on a session it could have had.
            version: agreed,
            server_name: SERVER_NAME.to_string(),
            features,
            session_id: self.opts.session_id,
            username: self.opts.username.clone(),
            width: w as u16,
            height: h as u16,
        });
        if features & features::LOCAL_CURSOR != 0 {
            self.refresh_cursor(true)?;
        }
        self.force_full_refresh();
        Ok(None)
    }

    fn on_resize_request(&mut self, width: u32, height: u32) -> Result<()> {
        let Some(c) = self.client.as_ref() else {
            return Ok(());
        };
        if c.features & features::RESIZE == 0 {
            return Ok(());
        }
        if width == 0 || height == 0 {
            return Ok(());
        }
        if (width, height) == self.display.size() {
            return Ok(());
        }
        match self.apply_resize(width, height) {
            Ok(()) => {}
            Err(e) => log::warn!("resize to {width}x{height} failed: {e:#}"),
        }
        Ok(())
    }

    /// Change the X screen size, then adopt whatever the server actually gave
    /// us.
    ///
    /// Split from [`Core::adopt_size`] because a resize is not the only way the
    /// root changes size, and everything after the RANDR call is common to both
    /// causes. `resize_screen` also produces a `ScreenChangeNotify` of its own,
    /// so the adoption below and the one the echo triggers must agree -- which
    /// they do, because adoption is idempotent on an unchanged size.
    fn apply_resize(&mut self, width: u32, height: u32) -> Result<()> {
        let width = clamp_dimension(width, self.opts.max_width);
        let height = clamp_dimension(height, self.opts.max_height);
        resize_screen(&self.display, width, height, self.opts.dpi)?;
        // `resize_screen` refreshes the cached size before it returns, and it
        // is the size the server settled on that matters, not the one we asked
        // for.
        let (w, h) = self.display.size();
        self.adopt_size(w, h);
        Ok(())
    }

    /// Take on a root size, whoever changed it.
    ///
    /// Clamped to what the capture was built for rather than growing the
    /// capture: `ScreenCapture` sizes its shared memory segment once, from the
    /// constructor's maximum, and cannot grow it from here. Height alone would
    /// in fact survive -- `capture_many` bands a tall capture to fit the
    /// segment -- but a root grown wider than one row of that segment fails the
    /// frame outright, and `Framebuffer::new` on a root somebody else controls
    /// is an allocation with no bound of ours. A root larger than the maximum
    /// is only reachable when we attached to somebody else's X server with
    /// `--display`, and cropping there is exactly what this code did before it
    /// noticed resizes at all. The case that actually kills sessions is a root
    /// that *shrank*, and a clamp is never in its way.
    fn adopt_size(&mut self, width: u32, height: u32) {
        let w = width.clamp(1, self.capture_max.0);
        let h = height.clamp(1, self.capture_max.1);
        if (w, h) == self.served {
            return;
        }
        if (w, h) != (width, height) {
            log::warn!(
                "the root window is {width}x{height}, larger than this session can capture \
                 ({}x{}); serving the top left {w}x{h} of it",
                self.capture_max.0,
                self.capture_max.1
            );
        }
        self.served = (w, h);
        self.screen = Framebuffer::new(w, h);
        self.encoder.resize(w, h);
        self.force_full_refresh();
        if let Some(c) = self.client.as_mut() {
            if c.ready {
                c.send(&Message::ScreenResized {
                    width: w as u16,
                    height: h as u16,
                });
            }
        }
        log::info!("screen is now {w}x{h}");
    }

    /// Send a frame if one is due, and sync the pointer position.
    /// Arrange for the next frame to be a complete one.
    ///
    /// Both halves are required and neither is sufficient. `full_refresh` makes
    /// `send_frame` capture the whole screen rather than the damage list;
    /// invalidating the encoder's reference is what makes that capture actually
    /// produce tiles, because the tile diff is against that reference and an
    /// untouched reference matches the screen exactly. Setting only the flag
    /// captures the screen, finds nothing changed and sends nothing -- which is
    /// how `RefreshRequest` used to answer the one client with a reason to ask.
    fn force_full_refresh(&mut self) {
        let (w, h) = self.served;
        self.encoder.invalidate(&Rect::new(0, 0, w, h));
        self.damage.mark_dirty();
        if let Some(c) = self.client.as_mut() {
            c.full_refresh = true;
        }
    }

    fn pump(&mut self) -> Result<()> {
        let Some(c) = self.client.as_ref() else {
            return Ok(());
        };
        if !c.ready || c.frames_in_flight.len() as u32 >= c.window {
            return Ok(());
        }
        if !(self.damage.is_dirty() || c.full_refresh) {
            return Ok(());
        }
        if let Some(last) = c.last_frame_at {
            if last.elapsed() < self.min_frame_interval {
                return Ok(());
            }
        }
        self.send_frame()
    }

    fn send_frame(&mut self) -> Result<()> {
        let (w, h) = self.served;
        let bounds = Rect::new(0, 0, w, h);
        let full = self
            .client
            .as_ref()
            .map(|c| c.full_refresh)
            .unwrap_or(false);
        let started = Instant::now();
        let rects = if full {
            // Drop accumulated damage; everything is resent anyway.
            let _ = self.damage.take()?;
            vec![bounds]
        } else {
            let raw = self.damage.take()?;
            let mut merged = coalesce(&raw, TILE_SIZE, &bounds);
            // Many scattered rectangles cost more round trips than one
            // capture of the whole screen.
            if merged.len() > crate::x11::capture::MAX_CAPTURE_RECTS
                || crate::x11::capture::total_area(&merged) * 2 > bounds.area()
            {
                merged = vec![bounds];
            }
            merged
        };
        if self.screen.width() != w || self.screen.height() != h {
            self.screen = Framebuffer::new(w, h);
        }
        // One call, not a loop: `capture_many` issues every request in a batch
        // before reading any reply, so a frame made of a dozen damage
        // rectangles costs about one round trip instead of a dozen in series.
        // Calling `capture_into` per rectangle is the case its documentation
        // exists to warn against.
        if let Err(e) = self.capture.capture_many(&mut self.screen, &rects) {
            if !crate::x11::capture::is_geometry_error(&e)
                || self.stale_captures >= STALE_CAPTURE_LIMIT
            {
                return Err(e);
            }
            // The root changed size between choosing the rectangles and
            // asking for their pixels: an `xrandr` in the session, or the
            // desktop applying a saved layout while the first frames go out.
            // The RANDR event for it is queued or on its way, but a capture
            // that raced it must not end the session -- that is what the
            // subscription to those events was for. Re-read the size now
            // rather than wait for the event, and send the whole screen at
            // the new size, since whatever landed before the failure is
            // suspect.
            self.stale_captures += 1;
            log::info!("capture raced a root resize ({e:#}); re-reading the root size");
            match self.display.refresh_size() {
                Ok((w, h)) => self.adopt_size(w, h),
                Err(e) => log::warn!("cannot read the root size after a failed capture: {e:#}"),
            }
            self.force_full_refresh();
            return Ok(());
        }
        self.stale_captures = 0;
        // A full refresh resets the reference frame, so there is no previous
        // frame a copy could reference; skip scroll detection in that case.
        let frame = self.encoder.encode_frame(&self.screen, &rects, !full);
        let Some(c) = self.client.as_mut() else {
            return Ok(());
        };
        c.full_refresh = false;
        if frame.is_empty() {
            return Ok(());
        }
        let FrameUpdate { copies, tiles } = frame;
        let bytes: usize = tiles.iter().map(|t| t.data.len()).sum();
        log::trace!(
            "frame {}: {} rects -> {} copies + {} tiles, {} bytes, {:.2} ms",
            c.next_frame_id,
            rects.len(),
            copies.len(),
            tiles.len(),
            bytes,
            started.elapsed().as_secs_f64() * 1000.0
        );
        // Usually one message. A screen large enough, showing content that
        // will not compress, produces more bytes than one frame may carry,
        // and the client refuses such a frame unread -- then reconnects into
        // the same full refresh, forever. Each part is a frame of its own to
        // the client: acknowledged separately, counted in flight separately.
        let mut sent_at = Instant::now();
        for (copies, tiles) in split_frame(copies, tiles) {
            let frame_id = c.next_frame_id;
            c.next_frame_id += 1;
            if !c.send(&Message::ScreenUpdate {
                frame_id,
                copies,
                tiles,
            }) {
                // Marked dead; `run` drops it before the next frame.
                return Ok(());
            }
            sent_at = Instant::now();
            // The send time is what the frame's acknowledgement is measured
            // against, and the queue's length is the in-flight count.
            c.frames_in_flight.push_back((frame_id, sent_at));
            c.frames_sent += 1;
        }
        c.last_frame_at = Some(sent_at);
        self.sync_pointer()
    }

    /// Tell the client where the pointer is if something other than the
    /// client moved it (application warps).
    fn sync_pointer(&mut self) -> Result<()> {
        let Some(c) = self.client.as_mut() else {
            return Ok(());
        };
        if !c.ready {
            return Ok(());
        }
        let pos = self.display.pointer_position()?;
        if pos != c.last_client_pointer && c.last_sent_pointer != Some(pos) {
            c.last_sent_pointer = Some(pos);
            c.send(&Message::CursorPosition {
                x: pos.0.max(0) as u16,
                y: pos.1.max(0) as u16,
            });
        }
        Ok(())
    }

    fn housekeeping(&mut self) -> Result<Option<Exit>> {
        let expired: Vec<_> = self
            .pending_downloads
            .iter()
            .filter(|(_, reader)| reader.open_expired())
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            self.pending_downloads.remove(&id);
            self.send_to_client(vec![Message::TransferEnd {
                id,
                ok: false,
                message: "file open timed out".into(),
            }]);
        }

        let outcome = self.transfers.poll();
        self.apply_transfer_outcome(outcome);

        // A selection owner that has wedged sends nothing, and an idle desktop
        // produces no other X events either, so `handle_event` never runs and
        // the conversion deadline never comes up. That is precisely the state a
        // session is in while its user waits on a paste, which is why the
        // clipboard needs a clock of its own rather than a ride on X traffic.
        let clipboard_events = match self.clipboard.as_mut() {
            Some(cb) => cb.tick().unwrap_or_else(|e| {
                log::warn!("clipboard: {e:#}");
                Vec::new()
            }),
            None => Vec::new(),
        };
        for event in clipboard_events {
            if let Err(e) = self.on_clipboard_event(event) {
                log::warn!("clipboard: {e:#}");
            }
        }

        let now = Instant::now();
        let mut drop_reason: Option<String> = None;
        if let Some(c) = self.client.as_mut() {
            if !c.ready && now.duration_since(c.connected_at) > HANDSHAKE_TIMEOUT {
                drop_reason = Some("handshake timeout".into());
            } else if c.ready {
                if now.duration_since(c.last_pong_at) > PONG_TIMEOUT {
                    drop_reason = Some("no response to pings".into());
                } else if now.duration_since(c.last_ping_at) >= PING_INTERVAL {
                    c.ping_nonce = c.ping_nonce.wrapping_add(1);
                    let nonce = c.ping_nonce;
                    c.last_ping_at = now;
                    c.send(&Message::Ping { nonce });
                }
            }
        }
        if let Some(reason) = drop_reason {
            log::warn!("dropping client: {reason}");
            self.drop_client(None)?;
            if let Some(exit) = self.exit_after_disconnect() {
                return Ok(Some(exit));
            }
        }
        if self.client.is_some() {
            self.sync_pointer()?;
            // The desktop's settings daemon turns auto-repeat back on a few
            // seconds into the first login, which is exactly the runaway-key
            // case the suppression exists for.
            self.input.reassert_auto_repeat_suppression();
        } else if let Some(t) = self.opts.idle_timeout {
            if now.duration_since(self.last_client_seen) > t {
                return Ok(Some(Exit::IdleTimeout));
            }
        }
        Ok(None)
    }

    /// Whether a client is currently connected.
    pub fn has_client(&self) -> bool {
        self.client.is_some()
    }
}

/// A windowed minimum of round-trip samples.
///
/// A minimum and not an average, deliberately. An exponential average of a slow
/// client's round trips rises with the queue the window itself created, so the
/// rule below would read its own backlog as a longer path and deepen the queue
/// again; on a bandwidth-limited link, where every extra byte in flight adds to
/// the round trip, that has no fixed point short of the cap. The minimum over a
/// recent window tracks the path rather than the queue: it falls only when the
/// path is genuinely shorter, and rises only once every recent sample is worse.
#[derive(Debug, Default)]
struct RttWindow {
    samples: VecDeque<(Instant, Duration)>,
}

impl RttWindow {
    /// Record a sample and return the base estimate that follows from it.
    fn record(&mut self, now: Instant, rtt: Duration) -> Duration {
        self.samples.push_back((now, rtt));
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > RTT_WINDOW)
        {
            self.samples.pop_front();
        }
        while self.samples.len() > RTT_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.iter().map(|(_, d)| *d).min().unwrap_or(rtt)
    }
}

/// One step of the in-flight window rule, applied to each frame acknowledged.
///
/// A fixed `max_in_flight = 2` caps a session at two frames per round trip: 40
/// fps at 50 ms, 20 at 100 ms, 10 at 200 ms -- on a transport that is by design
/// a WAN SSH tunnel. A bigger constant is not the answer, because the right
/// number is the bandwidth-delay product measured in frames and only the link
/// knows what that is.
///
/// * The target is how many frames fit in one round trip at the pace we are
///   allowed to send them: `ceil(base_rtt / min_frame_interval)`. Beyond that
///   the extra frames are queue rather than pipeline -- stale before they are
///   drawn, and paid for twice when the next one supersedes them.
/// * A sample longer than `base + 2 * interval` says the frames already out
///   there are queueing somewhere: a narrow link, or a client that cannot
///   decode as fast as we encode. Give a slot back. Two intervals of slack
///   rather than one, because a frame that merely missed its tick is late by a
///   whole interval through no fault of the window.
/// * Movement is one slot per sample in either direction. This is a guess about
///   a path that changes, and a guess that can leap is a guess that oscillates.
/// * `floor` is the operator's configured `max_in_flight` and always wins, even
///   over the computed target. The documented latency knob has to keep meaning
///   what it says, so this only ever adds frames to what was asked for.
fn adapt_in_flight(
    current: u32,
    floor: u32,
    base_rtt: Duration,
    sample: Duration,
    min_frame_interval: Duration,
) -> u32 {
    let floor = floor.clamp(1, MAX_IN_FLIGHT_CAP);
    // A zero interval would make the target infinite. `max_fps` is validated to
    // be at least 1 and `Core::new` clamps it again, but arithmetic this small
    // should not have to depend on that holding somewhere else.
    let interval = min_frame_interval.max(Duration::from_micros(1));
    let target = frames_per_rtt(base_rtt, interval).clamp(floor, MAX_IN_FLIGHT_CAP);
    let late = base_rtt.saturating_add(interval.saturating_mul(2));
    let next = if sample > late || current > target {
        current.saturating_sub(1)
    } else if current < target {
        current.saturating_add(1)
    } else {
        current
    };
    next.clamp(floor, MAX_IN_FLIGHT_CAP)
}

/// How many frames of `interval` fit in one `rtt`, rounded up, at least one and
/// never more than the cap.
fn frames_per_rtt(rtt: Duration, interval: Duration) -> u32 {
    let n = rtt.as_nanos().div_ceil(interval.as_nanos().max(1));
    n.clamp(1, u128::from(MAX_IN_FLIGHT_CAP)) as u32
}

/// What to tell the user when a format the session offered cannot be produced.
///
/// This is read by somebody who copied something in the session and is about
/// to paste it on their own desktop, so it names what they copied rather than
/// the conversion that failed; the format mask and the X-level reason are in
/// the log, for whoever is reading that instead.
fn unavailable_notice(format: u32) -> String {
    let what = match format {
        clipboard_format::PNG => "image",
        clipboard_format::FILES => "files",
        clipboard_format::TEXT => "text",
        // Unreachable while the mask holds only the three known formats, and
        // still better than a sentence with a hole in it.
        _ => "content",
    };
    format!("The {what} copied in the remote session could not be transferred to your clipboard.")
}

/// A peer-supplied string cut to [`ECHO_LIMIT`] characters for quoting back.
fn echo_prefix(s: &str) -> String {
    match s.char_indices().nth(ECHO_LIMIT) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_owned(),
    }
}

/// `value` held between the minimum edge and `max`, whichever way round the
/// two are.
///
/// `u32::clamp` panics when its bounds cross, and `max` is an operator's
/// number: the session binary refuses one below the minimum, but a config
/// file once did not, and `max_width = 48` reached this as the assertion
/// `min <= max` on the session's main thread, from the first client's hello.
/// When the two disagree the operator's maximum wins, because it is the size
/// the X server was started with and asking for more would fail there.
fn clamp_dimension(value: u32, max: u32) -> u32 {
    value.min(max).max(MIN_SCREEN_DIM.min(max))
}

/// Cut one frame's update into `ScreenUpdate` messages that each fit the
/// receiver's frame limit.
fn split_frame(
    copies: Vec<CopyRect>,
    tiles: Vec<TileUpdate>,
) -> Vec<(Vec<CopyRect>, Vec<TileUpdate>)> {
    split_frame_within(copies, tiles, MAX_MESSAGE_SIZE as usize)
}

/// [`split_frame`] against an explicit payload budget.
///
/// The copies all go in the first part: a message's copies are applied before
/// its tiles, and every later part holds tiles only, so the order of
/// application is exactly what one message would have given. Tiles keep their
/// order. A part is never empty unless the whole frame was, and a first tile
/// that does not fit beside the copies goes in over budget rather than
/// nowhere -- a tile is at most 64x64 pixels, so at the real limit that
/// cannot happen.
fn split_frame_within(
    copies: Vec<CopyRect>,
    tiles: Vec<TileUpdate>,
    budget: usize,
) -> Vec<(Vec<CopyRect>, Vec<TileUpdate>)> {
    let mut parts = Vec::new();
    let mut copies = Some(copies);
    let mut part: Vec<TileUpdate> = Vec::new();
    let mut size = SCREEN_UPDATE_FIXED + copies.as_ref().map_or(0, |c| c.len() * COPY_RECT_BYTES);
    for tile in tiles {
        let cost = TILE_FIXED + tile.data.len();
        if !part.is_empty() && size + cost > budget {
            parts.push((copies.take().unwrap_or_default(), std::mem::take(&mut part)));
            size = SCREEN_UPDATE_FIXED;
        }
        size += cost;
        part.push(tile);
    }
    parts.push((copies.take().unwrap_or_default(), part));
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100 fps, so a round trip in whole milliseconds is a whole number of
    /// frames and the arithmetic in these tests is readable.
    const INTERVAL: Duration = Duration::from_millis(10);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_window_ramps_one_slot_per_clean_ack() {
        // 50 ms of path at 10 ms a frame is five frames in flight.
        let base = ms(50);
        let mut window = 2;
        for expected in [3, 4, 5, 5, 5] {
            window = adapt_in_flight(window, 2, base, base, INTERVAL);
            assert_eq!(window, expected);
        }
    }

    #[test]
    fn a_late_sample_gives_a_slot_back() {
        let base = ms(50);
        // Two intervals of slack is still on time; a hair over it is not.
        assert_eq!(adapt_in_flight(5, 2, base, ms(70), INTERVAL), 5);
        assert_eq!(adapt_in_flight(5, 2, base, ms(71), INTERVAL), 4);
    }

    #[test]
    fn the_configured_floor_is_never_undercut() {
        let base = ms(50);
        let awful = Duration::from_secs(2);
        assert_eq!(adapt_in_flight(2, 2, base, awful, INTERVAL), 2);
        assert_eq!(adapt_in_flight(1, 1, base, awful, INTERVAL), 1);
        // Even from above the floor, a run of bad samples stops there.
        let mut window = 6;
        for _ in 0..20 {
            window = adapt_in_flight(window, 2, base, awful, INTERVAL);
        }
        assert_eq!(window, 2);
    }

    #[test]
    fn the_window_is_capped_however_long_the_path_is() {
        // Half a second of path would be fifty frames; eight is the cap.
        let base = ms(500);
        let mut window = 2;
        for _ in 0..30 {
            window = adapt_in_flight(window, 2, base, base, INTERVAL);
        }
        assert_eq!(window, MAX_IN_FLIGHT_CAP);
    }

    #[test]
    fn a_configured_floor_above_the_computed_target_still_wins() {
        // 5 ms of path computes a target of one frame; the operator asked for
        // four, and the knob has to keep meaning what it says.
        let base = ms(5);
        assert_eq!(adapt_in_flight(4, 4, base, base, INTERVAL), 4);
        assert_eq!(adapt_in_flight(2, 4, base, base, INTERVAL), 4);
        assert_eq!(adapt_in_flight(4, 4, base, ms(300), INTERVAL), 4);
    }

    #[test]
    fn the_window_walks_back_down_when_the_path_gets_shorter() {
        // 20 ms of path is two frames, and the walk down is one slot a time.
        let short = ms(20);
        let mut window = MAX_IN_FLIGHT_CAP;
        for expected in [7, 6, 5, 4, 3, 2, 2] {
            window = adapt_in_flight(window, 2, short, short, INTERVAL);
            assert_eq!(window, expected);
        }
    }

    #[test]
    fn the_base_rtt_is_the_minimum_over_the_window_not_the_last_sample() {
        let mut rtt = RttWindow::default();
        let t0 = Instant::now();
        assert_eq!(rtt.record(t0, ms(50)), ms(50));
        // A spike must not move the base...
        assert_eq!(rtt.record(t0 + ms(1), ms(400)), ms(50));
        // ...but a genuinely shorter path must.
        assert_eq!(rtt.record(t0 + ms(2), ms(20)), ms(20));
        // And once the window has rolled past those, the estimate follows the
        // path up rather than clinging to a minimum that no longer happens.
        let later = t0 + RTT_WINDOW + Duration::from_secs(1);
        assert_eq!(rtt.record(later, ms(300)), ms(300));
    }

    #[test]
    fn the_sample_window_is_bounded_by_count_as_well_as_age() {
        let mut rtt = RttWindow::default();
        let t0 = Instant::now();
        for i in 0..(RTT_SAMPLES as u64 * 3) {
            rtt.record(t0 + Duration::from_micros(i), ms(50));
        }
        assert!(rtt.samples.len() <= RTT_SAMPLES);
    }

    #[test]
    fn an_undeliverable_format_is_named_in_words_the_user_copied_in() {
        // The failure this guards is silence: the whole answer to an
        // unavailable format used to be a debug line, so the person waiting to
        // paste was told nothing and pasted something stale instead.
        for (format, word) in [
            (clipboard_format::TEXT, "text"),
            (clipboard_format::PNG, "image"),
            (clipboard_format::FILES, "files"),
        ] {
            let notice = unavailable_notice(format);
            assert!(
                notice.contains(word),
                "format {format:#x} produced {notice:?}, which does not say what did not arrive"
            );
            assert!(notice.contains("clipboard"), "{notice:?}");
        }
    }

    #[test]
    fn an_unrecognised_format_still_produces_a_whole_sentence() {
        let notice = unavailable_notice(1 << 20);
        assert!(
            notice.contains("clipboard") && notice.ends_with('.'),
            "{notice:?}"
        );
        // The hole this is really guarding: an empty word from the fallback
        // arm leaves "The  copied in the remote session ...", which still
        // mentions the clipboard and still ends in a full stop, so the two
        // assertions above would not notice it.
        assert!(!notice.contains("The  "), "{notice:?}");
    }

    #[test]
    fn a_maximal_peer_string_is_quoted_short_and_between_characters() {
        // The longest string the decoder accepts, which is also the longest
        // the encoder does: quoting it with any prefix at all used to trip
        // the encoder's assertion on the session's main thread.
        let path = "a".repeat(lynxrdp_proto::wire::MAX_BLOB_LEN);
        let quoted = echo_prefix(&path);
        assert!(quoted.chars().count() <= ECHO_LIMIT + 1);
        assert!(quoted.ends_with('…'));
        // Multi-byte characters are cut between, never through.
        let accented = "é".repeat(ECHO_LIMIT * 2);
        assert_eq!(echo_prefix(&accented).chars().count(), ECHO_LIMIT + 1);
        assert_eq!(echo_prefix("short"), "short");
        assert_eq!(echo_prefix(""), "");
        let mut buf = Vec::new();
        frame_message(
            &Message::TransferEnd {
                id: 1,
                ok: false,
                message: format!("{}: transfer 1 is already in progress", echo_prefix(&path)),
            },
            &mut buf,
        );
        assert!(buf.len() < 1024, "{} bytes", buf.len());
    }

    #[test]
    fn a_dimension_clamp_cannot_invert() {
        assert_eq!(clamp_dimension(1920, 4096), 1920);
        assert_eq!(clamp_dimension(10, 4096), MIN_SCREEN_DIM);
        assert_eq!(clamp_dimension(9000, 4096), 4096);
        // Below the minimum the operator's maximum is what the X server was
        // started with, so it wins -- and `u32::clamp` used to panic here.
        assert_eq!(clamp_dimension(1920, 48), 48);
        assert_eq!(clamp_dimension(10, 48), 48);
        assert_eq!(clamp_dimension(1920, 0), 0);
    }

    #[test]
    fn a_frame_over_the_message_limit_is_split_into_messages_under_it() {
        use lynxrdp_proto::codec::TileEncoding;
        let tiles: Vec<TileUpdate> = (0..40u32)
            .map(|i| TileUpdate {
                rect: Rect::new(i * 64, 0, 64, 64),
                encoding: TileEncoding::Raw,
                data: vec![i as u8; 100],
            })
            .collect();
        let copies = vec![CopyRect {
            src_x: 0,
            src_y: 0,
            dest: Rect::new(0, 64, 64, 64),
        }];
        let budget = 400;
        let parts = split_frame_within(copies.clone(), tiles.clone(), budget);
        assert!(parts.len() > 1);
        let mut seen = Vec::new();
        for (i, (c, t)) in parts.iter().enumerate() {
            let mut buf = Vec::new();
            frame_message(
                &Message::ScreenUpdate {
                    frame_id: i as u64,
                    copies: c.clone(),
                    tiles: t.clone(),
                },
                &mut buf,
            );
            let payload = buf.len() - HEADER_LEN;
            assert!(payload <= budget, "part {i} is {payload} bytes");
            // The per-item costs the split is computed from are the wire's.
            assert_eq!(
                payload,
                SCREEN_UPDATE_FIXED
                    + c.len() * COPY_RECT_BYTES
                    + t.iter().map(|t| TILE_FIXED + t.data.len()).sum::<usize>()
            );
            assert_eq!(c.is_empty(), i != 0, "copies belong in the first part only");
            assert!(!t.is_empty());
            seen.extend(t.iter().cloned());
        }
        assert_eq!(seen, tiles);
        // A frame that fits stays one message, copies and all.
        let whole = split_frame(copies.clone(), tiles.clone());
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0], (copies, tiles));
    }

    #[test]
    fn a_later_copy_of_the_same_text_is_not_mistaken_for_an_echo() {
        // The session copies T, then an image; the client then copies T.
        let mut echo = ClipboardEcho::default();
        assert!(echo.session_copied("T"));
        assert!(
            !echo.client_copied("T"),
            "the client's echo of T is still an echo"
        );
        echo.session_changed();
        assert!(
            echo.client_copied("T"),
            "T copied on the client after the session moved on is news"
        );
        // The mirror: the client copies T, then an image; the session copies T.
        let mut echo = ClipboardEcho::default();
        assert!(echo.client_copied("T"));
        assert!(!echo.session_copied("T"));
        echo.client_changed();
        assert!(echo.session_copied("T"));
    }

    #[test]
    fn text_the_other_way_replaces_the_echo_record() {
        // The session sends U; the client copies T; the client copies U
        // again. U is news now: the session holds T.
        let mut echo = ClipboardEcho::default();
        assert!(echo.session_copied("U"));
        assert!(echo.client_copied("T"));
        assert!(echo.client_copied("U"));
        // A session-side manager re-offering the client's T is still an echo,
        // and the client echoing what the session sent still is too.
        let mut echo = ClipboardEcho::default();
        assert!(echo.client_copied("T"));
        assert!(!echo.session_copied("T"));
        assert!(echo.session_copied("U"));
        assert!(!echo.client_copied("U"));
    }
}
