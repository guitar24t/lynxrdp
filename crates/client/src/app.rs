//! The graphical client: a winit window painted with softbuffer.
//!
//! The window shows the remote framebuffer at a whole-number magnification --
//! 1:1 on an ordinary display, 2:1 on a 2x one, so a 96-DPI remote desktop
//! comes out the size it would be on the machine it is being viewed from
//! rather than half of it. Magnifying by a whole number with nearest
//! neighbour duplicates whole pixels and invents none, so text is still
//! exactly what the server drew; nothing here ever resamples. When the window
//! is resized the remote screen is asked to follow (debounced), at the size
//! that divides by the magnification. The pointer is drawn locally from the
//! cursor images the server sends, which makes it feel instantaneous even
//! on slow links.
//!
//! The window also outlives the connection. A link that drops leaves the last
//! frame on screen, dimmed, with what happened written across the bottom,
//! while a worker thread tries to reconnect -- because the desktop those
//! pixels came from is still running on the server, and closing the window is
//! the one part of this that cannot be undone.
//!
//! There is no widget toolkit here: the window owns raw pixels. The connection
//! bar in [`crate::overlay`] is composited into the *presented* buffer after
//! the blit, never into the decoded framebuffer, because the server sends
//! incremental frames that diff against the pixels it believes we hold.

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lynxrdp_proto::message::{button, features, CursorImage};
use lynxrdp_proto::{keysym, Framebuffer, Message, Rect};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::clipchange::ClipboardWatcher;
use crate::connection::{Client, ClientEvent, ConnectOptions};
use crate::keymap;
use crate::overlay::{self, Overlay};
use crate::profiles::MAX_SCALE;
use crate::tunnel::Endpoint;

/// How long to wait after the last resize before asking the server.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(200);
/// How often the clipboard watcher is consulted while focused.
///
/// This is the *decision* interval, not the read interval:
/// [`ClipboardWatcher`] turns most of these ticks into nothing at all, and
/// what it does not turn away is paced by the platform's own change counter
/// or by its backoff.
const CLIPBOARD_POLL: Duration = Duration::from_millis(700);
/// RTT probe interval.
const PING_INTERVAL: Duration = Duration::from_secs(3);
/// How long the link may stay silent before the bar says so. Two probe
/// intervals: one missed pong is a slow link, two is a link that has stopped.
const STALL_AFTER: Duration = Duration::from_secs(2 * PING_INTERVAL.as_secs());
/// How often the window repaints while the bar is up, so the round-trip and
/// upload figures on it are not stale.
const OVERLAY_TICK: Duration = Duration::from_millis(100);

/// How many frames of presented damage are remembered, for buffer age.
///
/// Three is one more than any backend we ship against actually needs -- X11
/// and Win32 keep a single shadow buffer and report age 1 forever, and
/// softbuffer's Wayland backend double-buffers, so age settles at 2. The spare
/// entry costs a few rectangles and means an age we did not predict is served
/// from the log rather than falling back to a full repaint.
const PRESENT_LOG_DEPTH: usize = 3;

/// Pixels of trackpad scroll that make one wheel detent.
///
/// The protocol has no sub-detent scroll -- [`Message::Scroll`] counts whole
/// button-4/5 clicks, which is all an X11 session understands -- so a pixel
/// delta has to be divided by something. 24 is at the calm end of what X
/// clients move for one wheel click (a three-line jump at a typical line
/// height), and it is what [`ScrollCarry`] accumulates towards.
///
/// The old code divided by 40 and rounded, which made 20 px a whole detent
/// (twice the gain this constant gives) and threw everything below 20 px away
/// -- and a macOS trackpad emits ~3 px per event, so it threw away everything.
/// Truncating with a carry never discards a pixel, so the divisor can be the
/// honest one.
pub const PX_PER_DETENT: f32 = 24.0;

/// How long a lost link waits before each automatic attempt, by attempt
/// number; the last entry repeats.
///
/// The first wait is short because the common causes are instantaneous from
/// the user's side -- a laptop lid, a train tunnel, a VPN reconnecting -- and
/// the session is sitting there on the server the whole time. It lengthens
/// because the uncommon causes are not: a server that is being restarted, or
/// a network that is gone for the afternoon, must not be dialled once a second
/// for as long as the window is open.
const RETRY_BACKOFF: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

/// How many automatic attempts a lost link gets before it waits to be asked.
///
/// The budget exists because an automatic retry is only polite while there is
/// reason to think it will work. Eight attempts spans about a minute of
/// backoff, after which the window says what happened and waits: a person who
/// wants another go presses a key, and a person who has walked away is not
/// leaving a process dialling a dead host until the battery runs out.
const RETRY_BUDGET: u32 = 8;

/// How far the last frame is dimmed while the link is not up.
///
/// Dark enough that the window is plainly not live, light enough that the
/// desktop underneath is still recognisable -- which is the point of keeping
/// it at all: what is on screen is what the session still has, and it is the
/// user's evidence that nothing was lost.
const DIM_ALPHA: u32 = 140;

/// Options for the GUI.
#[derive(Clone, Debug)]
pub struct AppOptions {
    /// Start in fullscreen.
    pub fullscreen: bool,
    /// Window title prefix.
    pub title: String,
    /// Follow the window size with remote resizes.
    pub dynamic_resize: bool,
    /// Sync the clipboard.
    pub clipboard: bool,
    /// Client-side magnification, or `None` to follow the display.
    ///
    /// `Some` is a user who asked for a number and gets that number on every
    /// monitor; `None` follows the window's scale factor, which is what makes
    /// a 2x display show a 96-DPI remote desktop at the size it would be on a
    /// 1x one instead of half of it.
    pub scale: Option<u8>,
}

/// What a session window needs in order to come back after the link drops.
///
/// Held by the window rather than by `main`, which is the change that makes
/// reconnection possible at all: the tunnel used to be a local in `run()` and
/// the GUI was handed a `Client` and nothing else, so it could not so much as
/// ask whether `ssh` was still alive, let alone dial through it again.
pub struct Session {
    /// How to reach the server again, when there is a way.
    pub endpoint: Option<Endpoint>,
    /// The options the first connection was made with. A reconnection asks
    /// for the same features, or it would be a different session.
    pub connect: ConnectOptions,
    /// Where the event loop proxy lives, so a reconnected client's reader
    /// thread can wake this loop the way the first one does.
    pub waker: WakerSlot,
}

/// What has become of the connection to the session.
///
/// The window outlives the connection now. Every state but [`Link::Up`] keeps
/// the last frame on screen, dimmed, with what happened written across it --
/// because the desktop those pixels came from is still running on the server,
/// untouched, and closing the window is the one thing that cannot be undone.
#[derive(Debug)]
enum Link {
    /// Messages are flowing.
    Up,
    /// The link failed, and nothing is being done about it: either the retry
    /// budget is spent or the reason was not one to retry automatically.
    Lost {
        /// What ended it.
        reason: String,
        /// When it ended.
        since: Instant,
    },
    /// An attempt is in flight, or scheduled for `next_try`.
    Reconnecting {
        /// What ended the link in the first place.
        reason: String,
        /// When it ended -- not when this attempt began.
        since: Instant,
        /// Which attempt this is, counting from one.
        attempt: u32,
        /// When the next attempt starts, if one is not already running.
        next_try: Instant,
    },
    /// The link failed for a reason no reconnection can fix.
    Gone {
        /// What ended it.
        reason: String,
        /// When it ended.
        since: Instant,
    },
}

impl Link {
    /// Whether anything may be sent to the session.
    fn is_up(&self) -> bool {
        matches!(self, Self::Up)
    }

    /// When the link went down. `None` while it is up.
    fn since(&self) -> Option<Instant> {
        match self {
            Self::Up => None,
            Self::Lost { since, .. } | Self::Gone { since, .. } => Some(*since),
            Self::Reconnecting { since, .. } => Some(*since),
        }
    }

    /// What ended the link.
    fn reason(&self) -> &str {
        match self {
            Self::Up => "",
            Self::Lost { reason, .. }
            | Self::Reconnecting { reason, .. }
            | Self::Gone { reason, .. } => reason,
        }
    }

    /// Two words for the window title and the taskbar.
    fn headline(&self) -> &'static str {
        match self {
            Self::Up => "connected",
            Self::Lost { .. } => "connection lost",
            Self::Reconnecting { .. } => "reconnecting",
            Self::Gone { .. } => "session ended",
        }
    }
}

/// Whether a lost link is worth trying again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Reconnect, with backoff.
    Retry,
    /// Stop, and say why.
    Fatal,
}

/// Decide whether the reason a link ended is one a reconnection can fix.
///
/// Retrying is the default, because the ordinary ways a link dies -- a
/// sleeping laptop, a changed network, an `ssh` that was killed, a forward
/// whose far end went away -- all arrive as an I/O error and all recover. The
/// list below is the set where reconnecting is not merely useless but actively
/// worse:
///
/// * **Another client connected.** The server replaces the attached client
///   deliberately; that is what makes picking a session up on another machine
///   work at all. Retry it and two devices evict each other forever, each one
///   flickering back for the second before the other takes it again, and the
///   desktop is unusable on both until somebody quits. This case is why the
///   classifier exists.
/// * **A rejection.** A version floor, a uid the policy refuses, a session
///   that could not be started: the answer is a function of the two ends, so
///   it will be identical every time, and for the policy cases every attempt
///   is another refusal in the server's log for an administrator to puzzle
///   over.
/// * **A protocol error.** The two ends disagree about the wire format. A
///   fresh connection disagrees in exactly the same way.
/// * **The desktop ended.** Reconnecting would start a *new* desktop, which
///   is not what anybody means by reconnecting.
///
/// Matching on the server's own wording is not as fragile as it looks: these
/// strings are the user-visible text of a protocol both halves of this
/// repository ship together, and the one that is not -- the rejection -- is
/// read back through [`crate::connection::mentions_rejection`] rather than by
/// eye. That looks for the marker anywhere in the string rather than at the
/// front, because what arrives here is a whole `Result` chain formatted with
/// `{:#}`: one `.context()` added to the connect in [`App::start_attempt`]
/// would otherwise make every policy refusal retryable again, silently.
/// `a_refusal_is_still_a_refusal_by_the_time_the_window_sees_it` drives a
/// really-refusing server through the real worker so that stays true.
///
/// A reason that changes wording and is not caught here degrades to a retry
/// that fails a few times and gives up, which is the safe direction.
fn classify(reason: &str) -> Fate {
    if crate::connection::mentions_rejection(reason) {
        return Fate::Fatal;
    }
    let lower = reason.to_ascii_lowercase();
    const FATAL: [&str; 3] = [
        "another client connected",
        "protocol error",
        "desktop session has ended",
    ];
    if FATAL.iter().any(|f| lower.contains(f)) {
        return Fate::Fatal;
    }
    Fate::Retry
}

/// What the dimmed window says while the link is not up.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    /// Two or three words, in the state's colour.
    headline: String,
    /// The reason string the connection gave us, verbatim.
    detail: String,
    /// What the user can do about it.
    hint: &'static str,
    /// Headline colour.
    colour: u32,
}

/// A reconnection running on a worker thread.
///
/// On a thread because it is not quick and must not be: dialling a live tunnel
/// takes a moment, but starting `ssh` again can take as long as the user takes
/// to answer a passphrase prompt. Doing that on the winit thread would freeze
/// the very window that is supposed to be showing what is going on.
struct Attempt {
    /// The endpoint comes back whatever happens, so a failed attempt does not
    /// take the tunnel with it.
    done: crossbeam_channel::Receiver<(Endpoint, Result<Client>)>,
}

/// Event sent through the winit proxy to wake the loop.
#[derive(Debug)]
pub struct Wake;

struct Gfx {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
}

/// The application state.
pub struct App {
    client: Client,
    opts: AppOptions,
    gfx: Option<Gfx>,
    cursor: Option<CursorImage>,
    pointer: Option<(u32, u32)>,
    modifiers: ModifiersState,
    focused: bool,
    last_resize_event: Option<Instant>,
    pending_size: Option<(u32, u32)>,
    clipboard: Option<arboard::Clipboard>,
    last_clipboard: Option<String>,
    /// Hash of the last image seen on the local clipboard, so an image we
    /// received from the session is not immediately offered back to it.
    last_image: Option<u64>,
    /// Decides when the clipboard is worth reading at all.
    clipwatch: ClipboardWatcher,
    /// Uploads in flight: (transfer id, destination name).
    uploads: Vec<(u64, String)>,
    /// Uploads waiting for a slot: (local path, destination name).
    ///
    /// Dropped files are queued rather than offered all at once because each
    /// live transfer holds an open descriptor until the peer confirms it, and
    /// `MAX_DROPPED_FILES` bounds one dropped *path*, not the drop: five
    /// folders arrive as five separate events. See [`MAX_CONCURRENT_UPLOADS`].
    upload_queue: VecDeque<(PathBuf, String)>,
    /// Uploads finished (or given up on) since the queue was last empty, so
    /// progress can be reported across a whole drop rather than per file.
    upload_done: usize,
    /// The clipboard file copy being staged, if any.
    clipboard_batch: Option<ClipBatch>,
    /// Number of clipboard batches this process has started, which is what
    /// gives each one its own directory.
    clipboard_batches: u64,
    last_clipboard_poll: Instant,
    last_ping: Instant,
    rtt: Option<Duration>,
    dirty: Option<Rect>,
    /// Where the local cursor was drawn last frame, so the pixels it is
    /// leaving are presented along with the ones it is entering.
    last_cursor: Option<Rect>,
    /// Where the bar was painted last frame.
    ///
    /// Tracked here as well as inside [`Overlay`] because the rectangle is
    /// needed *before* the blit -- see [`App::redraw`] on why the bar's own
    /// pixels have to be repainted from the framebuffer every time it is
    /// drawn.
    last_bar: Option<Rect>,
    /// What each of the last few frames presented, newest first.
    ///
    /// This is what makes a buffer's age usable: age `n` means the buffer we
    /// were handed holds the picture from `n` frames ago, so everything those
    /// `n - 1` frames since changed has to be repainted along with this
    /// frame's own damage.
    present_log: VecDeque<Vec<Rect>>,
    /// Window size at the last present, so a resize is caught here as well as
    /// in the event that caused it.
    presented_size: Option<(u32, u32)>,
    full_redraw: bool,
    /// Whether the next `RedrawRequested` is one we asked for.
    ///
    /// A redraw nobody asked for is an expose: X11 reports one as a bare
    /// `RedrawRequested` with nothing else to distinguish it, and it means
    /// the window's pixels are gone. A damage list built from our own state
    /// would then present the cursor rectangle and leave the rest of the
    /// window showing whatever uncovered it, because `present_with_damage`
    /// copies only the rectangles it is given.
    redraw_asked: bool,
    exit_reason: Option<String>,
    fullscreen: bool,
    pressed_keys: Vec<u32>,
    /// Keysyms consumed as part of a bar accelerator. Their release is
    /// consumed too: by then the user may have let go of Ctrl, so the
    /// modifier test that recognised the press would no longer fire.
    swallowed: Vec<u32>,
    /// The connection bar.
    overlay: Overlay,
    /// The pointer is over the bar, so its events are ours and not the
    /// session's.
    pointer_on_bar: bool,
    /// A press that landed on the bar; its release belongs to the bar too,
    /// wherever it happens.
    bar_press: bool,
    /// Which pointer buttons are held down on the remote screen, one bit per
    /// button. The bar stays out of the way until every one of them is up, or
    /// dragging a window whose title bar sits under the top edge would break
    /// the moment the bar appeared -- and releasing the second button of a
    /// two-button drag must not be mistaken for the end of the drag.
    remote_buttons: u8,
    /// Wheel remainder, so a trackpad's few-pixel events add up instead of
    /// rounding to nothing.
    scroll: ScrollCarry,
    /// Whether the link was answering at the last check, so a change can show
    /// the bar without documentation.
    link_ok: bool,
    /// What has become of the connection.
    link: Link,
    /// How to make another connection, and what to make it with.
    session: Session,
    /// A reconnection in flight.
    attempt: Option<Attempt>,
    /// Automatic attempts spent since the link was last up.
    attempts: u32,
    /// What the notice said last frame, so it is repainted when it changes
    /// and not four times a second because it might have.
    notice_shown: Option<Notice>,
    /// Whole-number magnification of the remote screen.
    scale: u32,
    /// Whether `--scale` fixed it, in which case moving between monitors must
    /// not change it.
    scale_pinned: bool,
    /// One expanded row of the magnified blit, reused across frames.
    row: Vec<u32>,
    /// When the bar was last repainted for its own sake.
    ///
    /// `about_to_wait` runs again as soon as a redraw it asked for has been
    /// served, so an unconditional request there is a spin at 100% of a core.
    /// This paces the bar's own repaints instead.
    last_overlay_frame: Instant,
    frames: u64,
    last_title_update: Instant,
}

impl App {
    /// Wrap a connected client.
    pub fn new(client: Client, opts: AppOptions, session: Session) -> Self {
        let clipboard = if opts.clipboard {
            match arboard::Clipboard::new() {
                Ok(c) => Some(c),
                Err(e) => {
                    log::warn!("clipboard unavailable: {e}");
                    None
                }
            }
        } else {
            None
        };
        let cursor = client.cursor().cloned();
        let now = Instant::now();
        Self {
            client,
            fullscreen: opts.fullscreen,
            // A pinned scale is known now; an unpinned one is not known until
            // there is a window to ask which display it opened on.
            scale: opts.scale.map_or(1, u32::from),
            scale_pinned: opts.scale.is_some(),
            opts,
            gfx: None,
            cursor,
            pointer: None,
            modifiers: ModifiersState::empty(),
            focused: false,
            last_resize_event: None,
            pending_size: None,
            clipboard,
            last_clipboard: None,
            last_image: None,
            clipwatch: ClipboardWatcher::new(now),
            uploads: Vec::new(),
            upload_queue: VecDeque::new(),
            upload_done: 0,
            clipboard_batch: None,
            clipboard_batches: 0,
            last_clipboard_poll: now,
            last_ping: now,
            rtt: None,
            dirty: None,
            last_cursor: None,
            last_bar: None,
            present_log: VecDeque::new(),
            presented_size: None,
            full_redraw: true,
            redraw_asked: false,
            exit_reason: None,
            pressed_keys: Vec::new(),
            swallowed: Vec::new(),
            overlay: Overlay::new(now),
            pointer_on_bar: false,
            bar_press: false,
            remote_buttons: 0,
            scroll: ScrollCarry::default(),
            link_ok: true,
            link: Link::Up,
            session,
            attempt: None,
            attempts: 0,
            notice_shown: None,
            row: Vec::new(),
            last_overlay_frame: now,
            frames: 0,
            last_title_update: now,
        }
    }

    /// Run the event loop until the window closes or the connection ends.
    ///
    /// `waker` is the slot returned by [`make_waker`]; it is filled with the
    /// event loop proxy so the network reader thread can wake the loop.
    pub fn run(client: Client, opts: AppOptions, session: Session) -> Result<Option<String>> {
        let event_loop = EventLoop::<Wake>::with_user_event()
            .build()
            .context("creating event loop")?;
        *session.waker.lock().unwrap() = Some(event_loop.create_proxy());
        let mut app = App::new(client, opts, session);
        event_loop.run_app(&mut app).context("event loop")?;
        Ok(app.exit_reason.take())
    }

    fn title(&self) -> String {
        let info = self.client.info();
        let (w, h) = self.client.size();
        let mut t = format!(
            "{} - {}@{} {}x{}",
            self.opts.title, info.username, info.server_name, w, h
        );
        // A link that is down displaces the figures rather than joining them:
        // a round-trip time from before the drop is not a measurement any
        // more, and the window list is where someone with six windows open
        // looks first to find out which one stopped.
        if !self.link.is_up() {
            t.push_str(&format!(" - {}", self.link.headline()));
            return t;
        }
        if let Some(rtt) = self.rtt {
            t.push_str(&format!(" - {:.0} ms", rtt.as_secs_f64() * 1000.0));
        }
        if let Some((files, pct)) = self.upload_progress() {
            t.push_str(&format!(" - uploading {files} file(s) {pct}%"));
        }
        t
    }

    /// Files still to upload and how far through the whole drop we are.
    ///
    /// The count spans the queue as well as the transfers actually running,
    /// because a user who dropped a folder wants to know how much of *it* is
    /// left, not how many descriptors we happen to be holding.
    ///
    /// The percentage counts files finished, plus the byte fraction of the
    /// slice that is moving now -- which is why the slice is weighted by how
    /// many files it holds. Byte progress alone would say 5% for a drop that
    /// is nearly half done, and file progress alone would sit at 0% for the
    /// whole of a single large file.
    fn upload_progress(&self) -> Option<(usize, u64)> {
        let remaining = self.uploads.len() + self.upload_queue.len();
        if remaining == 0 {
            return None;
        }
        let total = (self.upload_done + remaining) as u64;
        let (done, size) = self.uploads.iter().fold((0u64, 0u64), |(d, t), (id, _)| {
            let (a, b) = self.client.transfer_progress(*id).unwrap_or((0, 0));
            (d + a, t + b)
        });
        let live = (self.uploads.len() as u64 * done * 100)
            .checked_div(size)
            .unwrap_or(0);
        Some((remaining, (self.upload_done as u64 * 100 + live) / total))
    }

    fn init_window(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let (w, h) = self.client.size();
        // There is no window yet to ask which display it will open on, so the
        // primary monitor stands in and the answer is checked below. Getting
        // it right the first time matters more than it looks: the window is
        // created at the remote screen's size times the magnification, and a
        // window created at half the size it wanted is one the user has to
        // resize by hand.
        if !self.scale_pinned {
            self.scale = event_loop
                .primary_monitor()
                .map_or(1, |m| display_scale(m.scale_factor()));
        }
        let mut attrs = Window::default_attributes()
            .with_title(self.title())
            .with_inner_size(PhysicalSize::new(w * self.scale, h * self.scale))
            .with_resizable(true);
        if self.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        // The same WM_CLASS the launcher uses, so a session window groups
        // with it and picks up the .desktop entry's icon and name.
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            use winit::platform::x11::WindowAttributesExtX11;
            attrs = WindowAttributesExtX11::with_name(attrs, crate::APP_ID, crate::APP_ID);
            attrs = WindowAttributesExtWayland::with_name(attrs, crate::APP_ID, crate::APP_ID);
        }
        if let Some(icon) = crate::icon::load() {
            // `from_rgba` only fails on a length mismatch, which `load` has
            // already ruled out; either way a missing icon is not fatal.
            attrs = attrs.with_window_icon(
                winit::window::Icon::from_rgba(icon.rgba, icon.width, icon.height).ok(),
            );
        }
        let window = Arc::new(event_loop.create_window(attrs).context("creating window")?);
        // The window may not have opened on the monitor that was guessed at.
        if !self.scale_pinned {
            let actual = display_scale(window.scale_factor());
            if actual != self.scale {
                self.scale = actual;
                let _ = window.request_inner_size(PhysicalSize::new(w * actual, h * actual));
            }
        }
        let context = softbuffer::Context::new(window.clone())
            .map_err(|e| anyhow::anyhow!("softbuffer context: {e}"))?;
        let surface = softbuffer::Surface::new(&context, window.clone())
            .map_err(|e| anyhow::anyhow!("softbuffer surface: {e}"))?;
        window.set_cursor_visible(!self.uses_local_cursor());
        self.gfx = Some(Gfx { window, surface });
        self.full_redraw = true;
        Ok(())
    }

    fn uses_local_cursor(&self) -> bool {
        self.client.info().features & features::LOCAL_CURSOR != 0
    }

    /// Whether the pointer on screen right now is ours to draw.
    ///
    /// It stops being ours the moment the link goes: the session's pointer is
    /// not where the session thinks it is any more, nothing the user does with
    /// it reaches anywhere, and the window has become an ordinary window with
    /// a notice on it that they need a real cursor to click.
    fn draws_own_cursor(&self) -> bool {
        self.uses_local_cursor() && self.link.is_up()
    }

    /// Whether anything may be sent to the session.
    fn link_up(&self) -> bool {
        self.link.is_up()
    }

    /// Send a message, if there is a link to send it down.
    ///
    /// Every send in this file goes through here or [`App::send_key`], and the
    /// guard is not tidiness. `Client::send` writes straight into the socket
    /// with a fifteen-second write timeout, from this thread; against a link
    /// that has half-died -- a sleeping laptop, a forward whose far end is
    /// gone -- the write succeeds into the kernel buffer until the buffer
    /// fills and then blocks for the whole timeout. One keystroke would freeze
    /// the window for fifteen seconds, and the window is the thing that is
    /// supposed to be telling the user what happened.
    fn send(&self, msg: &Message) {
        if self.link_up() {
            let _ = self.client.send(msg);
        }
    }

    /// Send a key event, if there is a link to send it down.
    fn send_key(&self, keysym: u32, down: bool) {
        self.send(&Message::KeyEvent { keysym, down });
    }

    /// Repaint the window and present what actually changed.
    ///
    /// Both halves are now damage-driven: the same list of rectangles is
    /// blitted out of the framebuffer and handed to `present_with_damage`, so
    /// a blinking terminal cursor costs a few hundred pixels of copy and a few
    /// hundred pixels of upload instead of two full-window passes.
    ///
    /// That is only sound because the buffer we are handed back is not blank.
    /// `Buffer::age` says how many frames ago its contents were presented, and
    /// [`stale_regions`] turns that into the extra rectangles the last few
    /// frames changed and this buffer therefore has not seen. Where the age
    /// cannot be trusted -- a fresh buffer (`age == 0`), an age deeper than the
    /// log, an expose we did not ask for, or any resize -- the whole window is
    /// repainted, which is exactly what this code did unconditionally before.
    /// A full blit is a cost; a stale rectangle is a visible artefact.
    fn redraw(&mut self) -> Result<()> {
        let Some(size) = self.gfx.as_ref().map(|g| g.window.inner_size()) else {
            return Ok(());
        };
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };
        let now = Instant::now();
        let win = Rect::new(0, 0, size.width, size.height);
        // Two different scales, and they are not the same thing. `bar_s` is
        // the display's, so the bar's 5x9 font is legible on a 2x screen;
        // `self.scale` is the remote screen's magnification. They agree by
        // default and part company the moment `--scale` is given.
        let bar_s = self.overlay_scale();
        let status = self.overlay_status(now);
        let notice = self.link_notice(now);
        // While the pointer is on the bar it is the OS cursor the user is
        // steering, not the session's, so ours is neither drawn nor moved.
        let cursor_now = match (
            self.draws_own_cursor() && !self.pointer_on_bar,
            &self.cursor,
        ) {
            (true, Some(cur)) => self.pointer.map(|(px, py)| {
                cursor_bounds_scaled(cur, px * self.scale, py * self.scale, self.scale)
            }),
            _ => None,
        };
        // The bar is blended over what is underneath it, so the pixels it
        // covers have to come fresh out of the framebuffer every time it is
        // painted -- otherwise the scrim is laid over a scrim and the strip
        // darkens frame by frame. That means the rectangle has to be in the
        // list *before* the blit, which is why it is predicted here rather
        // than taken from what `Overlay::draw` returns. The prediction is
        // exact: the bar is always the full window width and a fixed height.
        let bar_now = (self.overlay.visible())
            .then(|| Rect::new(0, 0, size.width, overlay::bar_height(bar_s)));
        // A resize invalidates every buffer the surface holds, and
        // softbuffer's Wayland backend reallocates the mapping without
        // resetting `age`, so this check has to be ours rather than the
        // backend's.
        if self.presented_size != Some((size.width, size.height)) {
            self.full_redraw = true;
        }
        // Snapshot before `gfx` is borrowed; it is at most three short lists.
        let log: Vec<Vec<Rect>> = self.present_log.iter().cloned().collect();
        // A frame with a notice on it is repainted whole, and that is the
        // whole of the damage bookkeeping for the dimmed state: the dim is
        // blended over the blit, so a rectangle repainted without being
        // dimmed again would come back bright against the rest, and the panel
        // is blended too. Repainting everything is cheap here precisely
        // because nothing is arriving to repaint -- these frames happen once a
        // second, when the wording changes.
        let asked_full = self.full_redraw || notice.is_some();

        let Some(gfx) = self.gfx.as_mut() else {
            return Ok(());
        };
        gfx.surface
            .resize(w, h)
            .map_err(|e| anyhow::anyhow!("surface resize: {e}"))?;
        let mut buf = gfx
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("surface buffer: {e}"))?;

        let stale = stale_regions(buf.age(), &log);
        let full = asked_full || stale.is_none();
        let own = [
            self.dirty.unwrap_or_default(),
            self.last_cursor.unwrap_or_default(),
            cursor_now.unwrap_or_default(),
            bar_now.unwrap_or_default(),
            // What the bar covered last frame but does not now: the blit
            // restores those pixels, but nothing else would present them.
            self.last_bar.unwrap_or_default(),
        ];
        // Two lists, not one: `regions` is painted and presented, `changed` is
        // what a later frame is told this one altered. See [`present_plan`] --
        // conflating them makes a buffer age of 2 repaint the whole window for
        // ever.
        let (regions, changed) = present_plan(win, full, &own, &stale.unwrap_or_default());
        for r in &regions {
            blit_rect_scaled(
                &mut buf,
                size.width,
                size.height,
                self.client.framebuffer(),
                *r,
                self.scale,
                &mut self.row,
            );
            // Per region rather than over the window, so this stays correct
            // whatever the damage list turns out to be: every pixel painted
            // this frame is dimmed exactly once, immediately after the blit
            // that produced it.
            if notice.is_some() {
                blend_rect(
                    &mut buf,
                    size.width,
                    size.height,
                    *r,
                    0x0000_0000,
                    DIM_ALPHA,
                );
            }
        }
        if let Some(n) = &notice {
            draw_notice(&mut buf, size.width, size.height, bar_s, n);
        }
        // After the blit and before the cursor: the bar sits over the remote
        // screen and under the pointer, and it is drawn into this buffer
        // rather than into the framebuffer the next frame will diff against.
        let (bar, bar_was) = self
            .overlay
            .draw(&mut buf, size.width, size.height, bar_s, &status);
        debug_assert_eq!(
            bar, bar_now,
            "the predicted bar rectangle must match the painted one, or the \
             blit did not cover the pixels the scrim was blended onto"
        );
        debug_assert_eq!(bar_was, self.last_bar, "the bar's own history drifted");
        if cursor_now.is_some() {
            if let (Some(cur), Some((px, py))) = (&self.cursor, self.pointer) {
                draw_cursor_scaled(
                    &mut buf,
                    size.width,
                    size.height,
                    cur,
                    px * self.scale,
                    py * self.scale,
                    self.scale,
                );
            }
        }
        let damage: Vec<softbuffer::Rect> =
            regions.iter().copied().filter_map(surface_rect).collect();
        let presented = buf.present_with_damage(&damage);

        // Recorded before the error is propagated, and unconditionally: what
        // the bar painted is what the *next* frame has to blit back, whether
        // or not this one reached the screen.
        self.last_cursor = cursor_now;
        self.last_bar = bar;
        self.dirty = None;
        self.presented_size = Some((size.width, size.height));
        if presented.is_err() {
            // A present that failed says nothing about what the surface now
            // holds, so the next frame starts from nothing known.
            self.present_log.clear();
            self.full_redraw = true;
        } else {
            // A full redraw clears the log rather than appending to it:
            // whatever forced the full redraw may also have replaced the
            // buffers behind our back, and an entry from before that says
            // nothing about what they hold now.
            if full {
                self.present_log.clear();
            }
            self.present_log.push_front(changed);
            while self.present_log.len() > PRESENT_LOG_DEPTH {
                self.present_log.pop_back();
            }
            self.full_redraw = false;
        }
        presented.map_err(|e| anyhow::anyhow!("present: {e}"))?;
        Ok(())
    }

    /// The integer pixel scale the bar is drawn at.
    fn overlay_scale(&self) -> u32 {
        overlay::pixel_scale(self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor()))
    }

    /// Everything the bar is allowed to say, and nothing else: the handshake
    /// identity, the remote size, the last real round-trip sample and real
    /// upload progress. No rate is offered because `Client::bytes_received` is
    /// a cumulative counter, and a counter is not a rate.
    fn overlay_status(&self, now: Instant) -> overlay::Status {
        let info = self.client.info();
        let mut st = overlay::Status::new(
            &format!("{}@{}", info.username, info.server_name),
            self.client.size(),
            self.rtt,
        );
        st.stalled = self.stalled_for(now);
        st.uploads = self.upload_progress();
        st
    }

    /// What the dimmed window says while the link is not up.
    fn link_notice(&self, now: Instant) -> Option<Notice> {
        let notice = |headline: &str, detail: String, hint, colour| {
            Some(Notice {
                headline: headline.to_string(),
                detail,
                hint,
                colour,
            })
        };
        match &self.link {
            Link::Up => None,
            Link::Lost { reason, .. } => notice(
                "Connection lost",
                reason.clone(),
                "Ctrl+Alt+R or a click reconnects  -  Ctrl+Alt+Q closes this window",
                overlay::colour::WARN,
            ),
            Link::Reconnecting {
                reason,
                attempt,
                next_try,
                ..
            } => {
                // The wording distinguishes the two halves of the state on
                // purpose: "trying" is a moment the user waits through, and
                // "in 8s" is a moment they may not want to wait through, so it
                // is exactly when the click that skips the wait is worth
                // offering.
                // The accelerator and the count lead, because they are what
                // is still true when a narrow window cuts the line short.
                let detail = if self.attempt.is_some() {
                    format!("attempt {attempt} - {reason}")
                } else {
                    let wait = next_try.saturating_duration_since(now).as_secs() + 1;
                    format!("attempt {} in {wait}s - {reason}", attempt + 1)
                };
                notice(
                    "Reconnecting",
                    detail,
                    "Ctrl+Alt+R or a click tries now  -  Ctrl+Alt+Q closes this window",
                    overlay::colour::WARN,
                )
            }
            Link::Gone { reason, .. } => notice(
                "Session ended",
                reason.clone(),
                "Ctrl+Alt+Q closes this window  -  the desktop may still be on the server",
                overlay::colour::DANGER,
            ),
        }
    }

    /// How long the link has been silent, when that is long enough to say so.
    ///
    /// Measured from [`Client::quiet_for`] rather than from our own probe,
    /// because *any* message refreshes that -- so a session whose core thread
    /// has wedged reads as quiet within a couple of server ping intervals,
    /// while one that is merely not being pinged by us does not read as
    /// anything at all. It starts at the handshake, which is itself proof the
    /// link was alive, so a three-second-old session is never called stalled
    /// because the first probe has not come back yet.
    fn stalled_for(&self, now: Instant) -> Option<Duration> {
        match self.link.since() {
            // A link that has actually failed is not "quiet", it is down, and
            // the bar says so from the moment it happens rather than six
            // seconds later when a probe that was never sent fails to return.
            Some(since) => Some(now.saturating_duration_since(since)),
            None => {
                let quiet = self.client.quiet_for();
                (quiet > STALL_AFTER).then_some(quiet)
            }
        }
    }

    fn request_redraw(&mut self) {
        if let Some(g) = &self.gfx {
            self.redraw_asked = true;
            g.window.request_redraw();
        }
    }

    /// Whether a pointer button is down on the remote screen.
    fn remote_drag(&self) -> bool {
        self.remote_buttons != 0
    }

    fn drain_network(&mut self) {
        // A closed `Client` answers every poll with the same `Disconnected`,
        // so draining one that has already been reported is an infinite loop
        // dressed as a state machine.
        if self.exit_reason.is_some() || !self.link_up() {
            return;
        }
        loop {
            match self.client.try_event() {
                Ok(Some(ev)) => match ev {
                    ClientEvent::Frame { dirty, .. } => {
                        self.frames += 1;
                        // Into window pixels here, once, at the boundary: from
                        // this point on every rectangle in this file is in the
                        // coordinates the window is presented in.
                        let dirty = scale_rect(dirty, self.scale);
                        self.dirty = Some(self.dirty.map(|d| d.union(&dirty)).unwrap_or(dirty));
                        self.request_redraw();
                    }
                    ClientEvent::Resized { width, height } => {
                        log::info!("remote screen is now {width}x{height}");
                        self.full_redraw = true;
                        self.match_window_to_remote(width, height);
                        self.update_title();
                        self.request_redraw();
                    }
                    ClientEvent::Cursor(c) => {
                        self.cursor = Some(c);
                        if let Some(g) = &self.gfx {
                            g.window.set_cursor_visible(false);
                        }
                        self.request_redraw();
                    }
                    ClientEvent::CursorPosition(x, y) => {
                        // The application warped the pointer; mirror it locally
                        // by moving our drawn cursor (we cannot move the OS
                        // pointer portably, and doing so would fight the user).
                        self.pointer = Some((u32::from(x), u32::from(y)));
                        self.request_redraw();
                    }
                    ClientEvent::Clipboard(text) => {
                        if let Some(cb) = self.clipboard.as_mut() {
                            if let Err(e) = cb.set_text(text.clone()) {
                                log::warn!("setting clipboard failed: {e}");
                            }
                            self.last_clipboard = Some(text);
                        }
                    }
                    ClientEvent::ClipboardImage(png) => self.on_remote_image(png),
                    ClientEvent::ClipboardFiles(files) => self.on_remote_files(files),
                    ClientEvent::FileDownloaded { id, path, name } => {
                        log::info!("downloaded {name} to {}", path.display());
                        self.on_clipboard_file(id, Some(path));
                    }
                    ClientEvent::FileUploaded { id, name } => {
                        log::info!("uploaded {name}");
                        self.finish_upload(id);
                    }
                    ClientEvent::TransferFailed { id, reason } => {
                        // Either direction, and the id says which: a failure
                        // has to reach the batch as well as the upload list,
                        // or a refused download leaves a paste waiting on a
                        // file that is never coming.
                        log::warn!("transfer {id} failed: {reason}");
                        self.finish_upload(id);
                        self.on_clipboard_file(id, None);
                    }
                    ClientEvent::Notice(text) => log::info!("server: {text}"),
                    ClientEvent::Rtt(rtt) => self.rtt = Some(rtt),
                    ClientEvent::Disconnected(reason) => {
                        self.on_link_lost(reason);
                        return;
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    self.on_link_lost(format!("protocol error: {e:#}"));
                    return;
                }
            }
        }
    }

    /// Size the window to a remote screen of `width` by `height`.
    fn match_window_to_remote(&mut self, width: u32, height: u32) {
        let (Some(g), false) = (self.gfx.as_ref(), self.fullscreen) else {
            return;
        };
        let want = (width * self.scale, height * self.scale);
        let cur = g.window.inner_size();
        if (cur.width, cur.height) != want {
            let _ = g
                .window
                .request_inner_size(PhysicalSize::new(want.0, want.1));
        }
    }

    /// The link ended. Keep the window, keep the last frame, say what happened.
    ///
    /// This used to exit the event loop, which is the bug: the desktop those
    /// pixels came from is still running on the server, and a laptop that
    /// slept for a minute closed the window on a session that was in no
    /// trouble at all.
    fn on_link_lost(&mut self, reason: String) {
        if !self.link_up() {
            return;
        }
        let now = Instant::now();
        log::warn!("connection lost: {reason}");
        // Not `disconnect`: there is nothing to say goodbye to, and saying it
        // into a half-open socket costs the write timeout on this thread.
        self.client.abandon(&reason);
        self.forget_transfers();
        let fate = classify(&reason);
        self.attempts = 0;
        self.link = match (fate, self.session.endpoint.is_some()) {
            (Fate::Fatal, _) => Link::Gone { reason, since: now },
            // Nothing to reconnect through is not a state to offer a retry
            // from; the window still stays up and still says what happened.
            (Fate::Retry, false) => Link::Gone {
                reason: format!("{reason} (this session has no way to reconnect)"),
                since: now,
            },
            (Fate::Retry, true) => Link::Reconnecting {
                reason,
                since: now,
                attempt: 0,
                next_try: now + retry_delay(0),
            },
        };
        // The OS pointer comes back: ours belonged to a session that is not
        // listening, and the notice asks for a click.
        self.restore_cursor();
        self.full_redraw = true;
        self.overlay.flash(now);
        self.update_title();
        self.request_redraw();
    }

    /// Let go of transfers that belonged to the connection that just died.
    ///
    /// Every id was issued by that connection, and a reconnected client
    /// numbers its own from one again: a completion arriving on the new link
    /// would otherwise retire an upload from the old one, and a clipboard
    /// batch would sit forever holding slots for files that can never arrive.
    fn forget_transfers(&mut self) {
        let pending = self.uploads.len() + self.upload_queue.len();
        if pending > 0 {
            log::warn!("{pending} upload(s) abandoned with the link");
        }
        self.uploads.clear();
        self.upload_queue.clear();
        self.upload_done = 0;
        if self.clipboard_batch.take().is_some() {
            log::warn!("a clipboard file copy was abandoned with the link");
        }
        // The mirrors describe a session this side can no longer see. Clearing
        // them means a reconnected session is told what is on this clipboard
        // rather than having the change suppressed as one it already knows
        // about.
        self.last_clipboard = None;
        self.last_image = None;
    }

    /// Ask for a reconnection now, whatever the backoff was going to say.
    fn reconnect_now(&mut self) {
        if self.attempt.is_some() || matches!(self.link, Link::Up | Link::Gone { .. }) {
            return;
        }
        // A person asking is not one of the automatic attempts, and it puts
        // the budget back: they may have just plugged the network in.
        self.attempts = 0;
        self.start_attempt(Instant::now());
    }

    /// Start a reconnection on a worker thread.
    fn start_attempt(&mut self, now: Instant) {
        let Some(since) = self.link.since() else {
            return;
        };
        let reason = self.link.reason().to_string();
        let Some(endpoint) = self.session.endpoint.take() else {
            // Only reachable if a worker went away holding it. Saying so beats
            // a window that offers a retry and then does nothing when asked.
            self.link = Link::Gone {
                reason: format!("{reason} (there is no way left to reconnect)"),
                since,
            };
            return;
        };
        self.attempts += 1;
        self.link = Link::Reconnecting {
            reason,
            since,
            attempt: self.attempts,
            next_try: now,
        };
        let opts = self.session.connect.clone();
        let reader_waker = waker_for(&self.session.waker);
        let nudge = waker_for(&self.session.waker);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("lynxrdp-reconnect".into())
            .spawn(move || {
                let mut endpoint = endpoint;
                let result = endpoint
                    .connect()
                    .and_then(|s| Client::from_stream(s.into_tcp(), &opts, Some(reader_waker)));
                // The endpoint goes back whatever happened, so a failed
                // attempt does not take a live tunnel with it.
                let _ = tx.send((endpoint, result));
                nudge();
            });
        match spawned {
            Ok(_) => self.attempt = Some(Attempt { done: rx }),
            Err(e) => {
                // The endpoint went with the closure. There is no way back
                // from here, and pretending otherwise would leave the window
                // counting down to attempts it cannot make.
                self.link = Link::Gone {
                    reason: format!("could not start a reconnection: {e}"),
                    since,
                };
            }
        }
        self.update_title();
        self.request_redraw();
    }

    /// Collect a finished attempt, or start the one that is due.
    fn poll_reconnect(&mut self, now: Instant) {
        if let Some(a) = &self.attempt {
            match a.done.try_recv() {
                Ok((endpoint, result)) => {
                    self.attempt = None;
                    self.session.endpoint = Some(endpoint);
                    match result {
                        Ok(client) => self.adopt(client, now),
                        Err(e) => self.attempt_failed(format!("{e:#}"), now),
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // The worker went away without answering, which takes the
                    // endpoint with it: there is nothing left to dial.
                    self.attempt = None;
                    self.attempt_failed("the reconnection stopped unexpectedly".into(), now);
                }
            }
            return;
        }
        if let Link::Reconnecting { next_try, .. } = &self.link {
            if now >= *next_try {
                self.start_attempt(now);
            }
        }
    }

    /// One attempt did not work.
    fn attempt_failed(&mut self, reason: String, now: Instant) {
        log::warn!("reconnection attempt {} failed: {reason}", self.attempts);
        let Some(since) = self.link.since() else {
            return;
        };
        // Whatever the attempt hit is more use than the original reason by
        // now: "connection refused" says the tunnel is up and the session is
        // not, and "no route to host" says the network still has not come
        // back.
        //
        // It is also classified in its own right. An attempt can fail for a
        // reason of its own that no repetition will change -- a rejection is the
        // one that matters, because every repeat of it is another refusal in
        // the server's log for an administrator to explain.
        if classify(&reason) == Fate::Fatal {
            self.link = Link::Gone { reason, since };
            self.update_title();
            self.request_redraw();
            return;
        }
        if self.attempts >= RETRY_BUDGET || self.session.endpoint.is_none() {
            self.link = Link::Lost {
                reason: format!("{reason} (gave up after {} attempts)", self.attempts),
                since,
            };
        } else {
            let wait = retry_delay(self.attempts);
            self.link = Link::Reconnecting {
                reason,
                since,
                attempt: self.attempts,
                next_try: now + wait,
            };
        }
        self.update_title();
        self.request_redraw();
    }

    /// Take over a freshly connected client.
    fn adopt(&mut self, client: Client, now: Instant) {
        let (w, h) = client.size();
        log::info!(
            "reconnected to {} as {} (session {}), screen {w}x{h}",
            client.info().server_name,
            client.info().username,
            client.info().session_id
        );
        // The old one was abandoned when the link failed, so dropping it here
        // neither writes nor blocks.
        self.client = client;
        self.link = Link::Up;
        self.attempts = 0;
        self.rtt = None;
        self.last_ping = now;
        self.link_ok = true;
        self.cursor = self.client.cursor().cloned();
        // A new client is a new framebuffer, and the server invalidates its
        // encoder for an arriving client precisely so that the first frame
        // after this is a whole screen. Nothing that was in flight before
        // describes it.
        self.dirty = None;
        self.present_log.clear();
        self.full_redraw = true;
        self.resync_input();
        self.restore_cursor();
        if self.opts.dynamic_resize && self.client.info().features & features::RESIZE != 0 {
            // The window may have been resized while the link was down, and
            // the session still has the size it had. Ask through the ordinary
            // debounced path rather than inventing a second one.
            if let Some(g) = &self.gfx {
                self.pending_size = Some(remote_size_for(g.window.inner_size(), self.scale));
                self.last_resize_event = Some(now);
            }
        } else {
            self.match_window_to_remote(w, h);
        }
        self.overlay.flash(now);
        self.update_title();
        self.request_redraw();
    }

    /// Put the session's idea of the keyboard and pointer back where ours is.
    ///
    /// The server releases everything a client was holding when that client
    /// goes away, and again when a new one replaces it -- but *when* it
    /// noticed the old link was gone is not something this side can know: a
    /// half-open socket is only reaped by a timeout. Sending the releases
    /// ourselves makes the two agree whichever way round it happened, and the
    /// alternative is a Ctrl the desktop believes is held down for the rest of
    /// the session.
    fn resync_input(&mut self) {
        self.swallowed.clear();
        // A wheel remainder is part of a gesture that ended when the link did.
        self.scroll.reset();
        self.release_all_keys();
        for b in [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
            MouseButton::Back,
            MouseButton::Forward,
        ] {
            if let Some((button, bit)) = button_codes(b) {
                if self.remote_buttons & bit != 0 {
                    self.send(&Message::PointerButton {
                        button,
                        down: false,
                    });
                }
            }
        }
        self.remote_buttons = 0;
        // And where the pointer is, after the buttons are up rather than
        // before: warping first would drag whatever the old link was holding
        // across the desktop and drop it somewhere new.
        //
        // The session's pointer is where the *old* link left it, and ours is
        // wherever the mouse has moved to since -- over a dimmed window a
        // motion event updates our copy and sends nothing. Without this the
        // next click is delivered to whatever is under the pre-drop position,
        // and "the user will move the mouse first" does not save it: a motion
        // that does not change the remote pixel sends nothing, so at a
        // magnification of two a whole pixel of movement can still leave the
        // two ends disagreeing.
        if let Some((x, y)) = self.pointer {
            // Clamped to the new screen, which may not be the size the old
            // link had -- the desktop can have been resized by whoever else
            // attached while this window was dark.
            let (w, h) = self.client.size();
            let (x, y) = (x.min(w.saturating_sub(1)), y.min(h.saturating_sub(1)));
            self.pointer = Some((x, y));
            self.send(&Message::PointerMotion {
                x: x as u16,
                y: y as u16,
            });
        }
    }

    fn housekeeping(&mut self) {
        let now = Instant::now();
        self.poll_reconnect(now);
        if let (Some(t), Some((w, h))) = (self.last_resize_event, self.pending_size) {
            if now.duration_since(t) >= RESIZE_DEBOUNCE {
                self.last_resize_event = None;
                self.pending_size = None;
                if (w, h) != self.client.size() && w >= 64 && h >= 64 {
                    log::debug!("requesting remote resize to {w}x{h}");
                    self.send(&Message::ResizeRequest {
                        width: w.min(u16::MAX as u32) as u16,
                        height: h.min(u16::MAX as u32) as u16,
                    });
                }
            }
        }
        if self.link_up() && now.duration_since(self.last_ping) >= PING_INTERVAL {
            self.last_ping = now;
            let _ = self.client.ping();
        }
        if self.link_up()
            && self.focused
            && now.duration_since(self.last_clipboard_poll) >= CLIPBOARD_POLL
        {
            self.last_clipboard_poll = now;
            // Ask before reading. On Windows and macOS this is a change
            // counter and almost every tick stops here; elsewhere the text
            // read goes ahead as it always did and only the image read, which
            // is the one that copies a whole screenshot out of the window
            // system, is paced.
            let look = self.clipwatch.tick(now);
            if look.text {
                self.poll_clipboard();
            }
            if look.image {
                let found = self.poll_clipboard_image();
                self.clipwatch.image_read(found, now);
            }
        }
        if now.duration_since(self.last_title_update) >= Duration::from_secs(1) {
            self.last_title_update = now;
            if let Some(g) = &self.gfx {
                g.window.set_title(&self.title());
            }
        }
        // A link that stops answering, or starts again, shows the bar for a
        // moment: it is the one status change a user needs to see without
        // having gone looking for it.
        let ok = self.stalled_for(now).is_none();
        if ok != self.link_ok {
            self.link_ok = ok;
            self.overlay.flash(now);
        }
        // The notice is repainted when its wording changes and not otherwise.
        // It carries a countdown, so "has it changed" is asked four times a
        // second and answered yes about once -- and a repaint while the link
        // is down is a whole window, which is worth asking before doing.
        let notice = self.link_notice(now);
        if notice != self.notice_shown {
            self.notice_shown = notice;
            self.full_redraw = true;
            self.request_redraw();
        }
        if self.overlay.tick(now) {
            // The bar covers remote pixels, so both showing and hiding it are
            // a full repaint; the blit restores what was underneath.
            self.full_redraw = true;
            if !self.overlay.visible() && self.pointer_on_bar {
                // It went away under a stationary pointer (a lost focus, an
                // unpin). Hand the pointer back rather than leaving the
                // session unable to see it.
                self.pointer_on_bar = false;
                self.bar_press = false;
                self.restore_cursor();
            }
            self.request_redraw();
        } else if self.overlay.visible()
            && now.duration_since(self.last_overlay_frame) >= OVERLAY_TICK
        {
            // The round-trip and upload figures move while it is up.
            self.last_overlay_frame = now;
            self.request_redraw();
        }
    }

    /// Show or hide the OS pointer according to who is drawing it now.
    fn restore_cursor(&self) {
        if let Some(g) = &self.gfx {
            g.window
                .set_cursor_visible(!self.draws_own_cursor() || self.cursor.is_none());
        }
    }

    /// Do what a bar button or its accelerator asks.
    fn run_overlay_action(&mut self, action: overlay::Action, event_loop: &ActiveEventLoop) {
        match action {
            overlay::Action::Fullscreen => self.toggle_fullscreen(),
            overlay::Action::SecureAttention => self.send_secure_attention(),
            overlay::Action::Disconnect => self.close(event_loop, "disconnected by the user"),
        }
    }

    /// End the window. The one exit, whatever asked for it.
    ///
    /// A link that failed keeps its own reason: it is what the command line
    /// reports, and it is what the user was reading on the dimmed window when
    /// they closed it. "Window closed" would replace the only account of what
    /// happened with a description of the click that dismissed it.
    fn close(&mut self, event_loop: &ActiveEventLoop, reason: &str) {
        let reason = match &self.link {
            Link::Up => reason.to_string(),
            other => other.reason().to_string(),
        };
        self.exit_reason.get_or_insert(reason);
        event_loop.exit();
    }

    /// Send Ctrl+Alt+Del into the session.
    ///
    /// The modifiers are synthesised only when they are not already physically
    /// down. From the bar's button nothing is held, so all three keys are
    /// pressed and released in order; from Ctrl+Alt+End the session has
    /// already seen the modifiers go down, and pressing them again -- or
    /// releasing them afterwards while the user is still holding them --
    /// would leave our idea of the keyboard and the session's disagreeing.
    ///
    /// Whatever is synthesised goes on `pressed_keys` for the duration, so a
    /// focus loss in the middle still releases it, and comes off again at the
    /// end so `release_all_keys` does not send a second release for a key that
    /// is already up.
    fn send_secure_attention(&mut self) {
        if !self.link_up() {
            return;
        }
        let held = |a: u32, b: u32| self.pressed_keys.iter().any(|&k| k == a || k == b);
        let mut synth = Vec::new();
        if !held(keysym::CONTROL_L, keysym::CONTROL_R) {
            synth.push(keysym::CONTROL_L);
        }
        if !held(keysym::ALT_L, keysym::ALT_R) {
            synth.push(keysym::ALT_L);
        }
        for &ks in &synth {
            self.pressed_keys.push(ks);
            self.send_key(ks, true);
        }
        self.send_key(keysym::DELETE, true);
        self.send_key(keysym::DELETE, false);
        for &ks in synth.iter().rev() {
            self.pressed_keys.retain(|&k| k != ks);
            self.send_key(ks, false);
        }
    }

    /// Upload a file or directory dropped onto the window.
    ///
    /// The drop is queued, not started. `MAX_DROPPED_FILES` bounds the walk of
    /// one dropped *path*, and a drop of five folders arrives as five separate
    /// `DroppedFile` events -- so the bound that mattered was never applied to
    /// what the user actually dropped. The budget below is checked against
    /// everything already outstanding, and a path that does not fit is refused
    /// whole: uploading the first two hundred files of a folder and silently
    /// dropping the rest produces a directory in the session that looks
    /// complete and is not.
    fn on_dropped_file(&mut self, path: &std::path::Path) {
        if !self.link_up() {
            log::warn!("the link is down; ignoring dropped file");
            return;
        }
        if self.client.info().features & features::FILE_TRANSFER == 0 {
            log::warn!("the server did not enable file transfer; ignoring dropped file");
            return;
        }
        let files = match collect_dropped_files(path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("cannot read {}: {e:#}", path.display());
                return;
            }
        };
        if files.is_empty() {
            log::warn!("nothing to upload from {}", path.display());
            return;
        }
        let outstanding = self.uploads.len() + self.upload_queue.len();
        if outstanding + files.len() > MAX_PENDING_UPLOADS {
            log::warn!(
                "refusing {}: its {} file(s) on top of the {outstanding} already queued would \
                 pass the {MAX_PENDING_UPLOADS} file limit; drop it again once the current \
                 uploads have finished",
                path.display(),
                files.len()
            );
            return;
        }
        log::info!("queued {} file(s) from {}", files.len(), path.display());
        self.upload_queue.extend(files);
        self.pump_uploads();
    }

    /// Start as many queued uploads as the concurrency limit allows.
    fn pump_uploads(&mut self) {
        while self.link_up() && self.uploads.len() < MAX_CONCURRENT_UPLOADS {
            let Some((local, dest)) = self.upload_queue.pop_front() else {
                break;
            };
            match self.client.send_file(&local, &dest) {
                Ok(id) => {
                    log::debug!("uploading {} as {dest}", local.display());
                    self.uploads.push((id, dest));
                }
                Err(e) => {
                    // One unreadable file does not stop the drop; it is
                    // counted as finished so the progress figure still reaches
                    // the end.
                    log::warn!("uploading {} failed: {e:#}", local.display());
                    self.upload_done += 1;
                }
            }
        }
        if self.uploads.is_empty() && self.upload_queue.is_empty() {
            self.upload_done = 0;
        }
        self.update_title();
    }

    /// The session copied files: download them, then offer them locally.
    fn on_remote_files(&mut self, files: Vec<lynxrdp_proto::FileEntry>) {
        if files.is_empty() || !self.link_up() {
            return;
        }
        // A copy in the session replaces whatever the last one was staging.
        // The old transfers are cancelled rather than left to finish: nobody
        // will paste them, and each one holds a descriptor and a share of the
        // connection's global transfer window until it ends.
        if let Some(old) = self.clipboard_batch.take() {
            for id in old.live_ids() {
                self.client.cancel_transfer(id);
            }
        }
        let dir = match clipboard_staging_dir()
            .and_then(|root| new_batch_dir(&root, &mut self.clipboard_batches))
        {
            Ok(d) => d,
            Err(e) => {
                log::warn!("cannot prepare the clipboard staging directory: {e:#}");
                return;
            }
        };
        log::info!(
            "fetching {} file(s) copied in the session into {}",
            files.len(),
            dir.display()
        );
        self.clipboard_batch = Some(ClipBatch::new(dir, &files));
        self.pump_clipboard_batch();
    }

    /// Issue what the batch is allowed to have in flight, and publish it once
    /// every file in it has either arrived or failed.
    fn pump_clipboard_batch(&mut self) {
        if !self.link_up() {
            // Nothing can be asked for and nothing can arrive. The batch goes
            // with the link rather than being published half empty.
            return;
        }
        while let Some((remote, dest, slot)) = self
            .clipboard_batch
            .as_mut()
            .and_then(ClipBatch::next_request)
        {
            match self.client.request_file(&remote, dest) {
                Ok(id) => {
                    if let Some(b) = self.clipboard_batch.as_mut() {
                        b.requested(slot, id);
                    }
                }
                // Nothing to record: taking it off the queue already made the
                // batch one file closer to finishing, and its slot stays empty
                // so the file is simply missing from what gets published.
                Err(e) => log::warn!("cannot fetch {remote}: {e:#}"),
            }
        }
        if self.clipboard_batch.as_ref().is_some_and(ClipBatch::done) {
            if let Some(b) = self.clipboard_batch.take() {
                self.publish_clipboard_batch(b);
            }
        }
    }

    /// One staged file resolved, one way or the other.
    fn on_clipboard_file(&mut self, id: u64, path: Option<PathBuf>) {
        let Some(b) = self.clipboard_batch.as_mut() else {
            return;
        };
        if !b.resolve(id, path) {
            // Not ours: an upload, or a leftover from a superseded batch.
            return;
        }
        self.pump_clipboard_batch();
    }

    /// Put whatever arrived on the local clipboard.
    ///
    /// Whatever *arrived*, not everything that was asked for: one file the
    /// session offered and this side could not create used to strand the
    /// entire copy, because the batch was a count that only ever went down on
    /// success. The user pressed Ctrl+V, got their old clipboard back, and the
    /// explanation went to a terminal a windowed client does not have.
    fn publish_clipboard_batch(&mut self, batch: ClipBatch) {
        let asked = batch.total();
        let dir = batch.dir().to_path_buf();
        let files = batch.into_files();
        if files.is_empty() {
            log::warn!("none of the {asked} file(s) copied in the session could be staged");
        } else {
            if files.len() < asked {
                log::warn!(
                    "{} of {asked} file(s) could not be staged; offering the {} that could",
                    asked - files.len(),
                    files.len()
                );
            }
            match crate::fileclip::write_files(&files) {
                Ok(()) => log::info!("{} file(s) are on the clipboard", files.len()),
                Err(e) => {
                    // The files are still on disk, so say where rather than
                    // leaving the user with nothing.
                    log::warn!("could not put the files on the clipboard: {e:#}");
                    for f in &files {
                        log::info!("downloaded to {}", f.display());
                    }
                }
            }
        }
        // Pruning happens here and not at startup. A staged file has to outlive
        // the paste that reads it -- a file manager opens it when the user
        // pastes, which may be minutes later -- so directories are removed only
        // once a newer batch has replaced them, and the last few are kept
        // because the paste of the previous copy may still be in progress.
        // Doing it at startup instead would delete a directory a *running*
        // download is writing into and then recreate it empty, which is the
        // one ordering that loses data.
        if let Some(parent) = dir.parent() {
            prune_batches(parent, KEEP_STAGED_BATCHES);
        }
    }

    /// Forget an upload that finished or failed, and start the next.
    ///
    /// Matched by id, not by name: a queue that spans several drops can hold
    /// two files with the same destination name -- `project/README.md` from
    /// two projects -- and the first completion would then retire the wrong
    /// transfer, leaving one in the list forever and its slot never reused.
    fn finish_upload(&mut self, id: u64) {
        let before = self.uploads.len();
        self.uploads.retain(|(uid, _)| *uid != id);
        if self.uploads.len() < before {
            self.upload_done += 1;
        }
        self.pump_uploads();
    }

    fn update_title(&self) {
        if let Some(g) = &self.gfx {
            g.window.set_title(&self.title());
        }
    }

    /// Put an image from the session onto the local clipboard.
    fn on_remote_image(&mut self, png: Vec<u8>) {
        let Some(cb) = self.clipboard.as_mut() else {
            return;
        };
        match crate::imageclip::decode_png(&png) {
            Ok(img) => {
                self.last_image = Some(image_digest(&img.bytes));
                let data = arboard::ImageData {
                    width: img.width,
                    height: img.height,
                    bytes: std::borrow::Cow::Owned(img.bytes),
                };
                if let Err(e) = cb.set_image(data) {
                    log::warn!("setting the clipboard image failed: {e}");
                }
            }
            Err(e) => log::warn!("undecodable clipboard image from the session: {e:#}"),
        }
    }

    /// Look for an image on the local clipboard and offer it to the session.
    ///
    /// Returns whether the clipboard held an image this side had not seen
    /// before, which is what tells [`ClipboardWatcher`] whether looking was
    /// worth it.
    fn poll_clipboard_image(&mut self) -> bool {
        if !self.link_up() || self.client.info().features & features::CLIPBOARD_IMAGE == 0 {
            return false;
        }
        let Some(cb) = self.clipboard.as_mut() else {
            return false;
        };
        let img = match cb.get_image() {
            Ok(i) => i,
            // No image on the clipboard is the common case, not an error.
            Err(arboard::Error::ContentNotAvailable) => return false,
            Err(e) => {
                log::debug!("reading the clipboard image failed: {e}");
                return false;
            }
        };
        let digest = image_digest(&img.bytes);
        if self.last_image == Some(digest) {
            return false;
        }
        self.last_image = Some(digest);
        let rgba = match crate::imageclip::Rgba::new(img.width, img.height, img.bytes.into_owned())
        {
            Ok(r) => r,
            Err(e) => {
                log::debug!("clipboard image was not usable: {e:#}");
                // Still a change: something new was on the clipboard, it was
                // just not something we can send. Reporting it as "nothing
                // found" would back the watcher off for as long as it sat
                // there.
                return true;
            }
        };
        match crate::imageclip::encode_png(&rgba) {
            Ok(png) => {
                log::debug!(
                    "offering a {}x{} clipboard image to the session",
                    rgba.width,
                    rgba.height
                );
                if let Err(e) = self.client.offer_clipboard_image(png) {
                    log::warn!("offering the clipboard image failed: {e:#}");
                }
            }
            Err(e) => log::warn!("encoding the clipboard image failed: {e:#}"),
        }
        true
    }

    fn poll_clipboard(&mut self) {
        if !self.link_up() {
            return;
        }
        let Some(cb) = self.clipboard.as_mut() else {
            return;
        };
        if self.client.info().features & features::CLIPBOARD == 0 {
            return;
        }
        match cb.get_text() {
            Ok(text) => {
                if !text.is_empty() && self.last_clipboard.as_deref() != Some(text.as_str()) {
                    if text.len() <= 4 * 1024 * 1024 {
                        self.send(&Message::ClipboardText { text: text.clone() });
                    }
                    self.last_clipboard = Some(text);
                }
            }
            Err(arboard::Error::ContentNotAvailable) => {}
            Err(e) => log::debug!("clipboard read failed: {e}"),
        }
    }

    fn next_wake(&self) -> Duration {
        let mut d = Duration::from_millis(250);
        if self.pending_size.is_some() {
            d = d.min(RESIZE_DEBOUNCE / 2);
        }
        // A dwell in progress is timed against the same 100 ms tick the bar's
        // figures use, so the reveal lands within a tick of `REVEAL_DELAY`
        // rather than within the 250 ms idle wake -- the difference between a
        // delay that feels chosen and one that feels random.
        if self.overlay.visible() || self.overlay.revealing() {
            d = d.min(OVERLAY_TICK);
        }
        d
    }

    /// The accelerators this window keeps for itself.
    ///
    /// A closed list, deliberately: a raw framebuffer has no focus model, so
    /// the bar cannot own keys the way a widget would, and every key not named
    /// here -- Esc, Tab, the function keys, everything -- belongs to the
    /// session. The bar prints these next to the matching button, which is the
    /// whole of its keyboard story.
    fn accelerator(&self, logical: &Key, physical: PhysicalKey) -> Option<Accelerator> {
        let acc = accelerator_for(self.modifiers, logical, physical)?;
        // Ctrl+Alt+R is ours only while there is nothing to send it to. Taking
        // a shortcut away from the desktop for the whole session, to run a
        // command that does nothing for all but a few seconds of it, is a
        // worse trade than the other four make -- and while the link is down
        // the session is not receiving keys anyway, so nothing is lost.
        if acc == Accelerator::Reconnect && self.link_up() {
            return None;
        }
        Some(acc)
    }

    fn on_key(&mut self, event: KeyEvent, event_loop: &ActiveEventLoop) {
        let Some(ks) = keymap::keysym_for(&event.logical_key, event.location) else {
            return;
        };
        if event.state == ElementState::Pressed {
            if let Some(acc) = self.accelerator(&event.logical_key, event.physical_key) {
                self.swallowed.push(ks);
                match acc {
                    Accelerator::Fullscreen => self.toggle_fullscreen(),
                    Accelerator::SecureAttention => self.send_secure_attention(),
                    Accelerator::Pin => {
                        self.overlay.toggle_pin();
                        // Flash either way: pinning wants confirmation and
                        // unpinning wants a last look before it goes.
                        self.overlay.flash(Instant::now());
                    }
                    Accelerator::Reconnect => self.reconnect_now(),
                    Accelerator::Disconnect => {
                        self.run_overlay_action(overlay::Action::Disconnect, event_loop)
                    }
                }
                return;
            }
        } else if let Some(i) = self.swallowed.iter().position(|&k| k == ks) {
            self.swallowed.swap_remove(i);
            return;
        }
        // Nothing else reaches the session while the link is down, and
        // nothing is recorded either: `pressed_keys` is this side's copy of
        // what the *session* believes is held, and the session believes what
        // it was told before the link failed. Recording a press it never saw
        // would have `resync_input` release a key it is not holding, and
        // dropping a release it never saw is what leaves a modifier stuck.
        if !self.link_up() {
            return;
        }
        // Numpad digits should arrive as KP_ keysyms.
        let ks = match (&event.logical_key, event.location) {
            (Key::Character(s), winit::keyboard::KeyLocation::Numpad) => s
                .chars()
                .next()
                .and_then(keymap::numpad_keysym)
                .unwrap_or(ks),
            _ => ks,
        };
        // Do not send a key press if we think it is already down, unless it is
        // an auto-repeat which the server should see as repeated presses.
        let down = event.state == ElementState::Pressed;
        if down {
            if !self.pressed_keys.contains(&ks) {
                self.pressed_keys.push(ks);
            }
        } else {
            self.pressed_keys.retain(|&k| k != ks);
        }
        self.send_key(ks, down);
    }

    fn release_all_keys(&mut self) {
        for ks in std::mem::take(&mut self.pressed_keys) {
            self.send_key(ks, false);
        }
        // The releases for these will arrive with the window unfocused, or not
        // at all; either way there is nothing left to match them against.
        self.swallowed.clear();
    }

    fn toggle_fullscreen(&mut self) {
        let Some(g) = &self.gfx else { return };
        self.fullscreen = !self.fullscreen;
        if self.fullscreen {
            g.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            g.window.set_fullscreen(None);
        }
    }

    fn on_pointer_moved(&mut self, pos: PhysicalPosition<f64>) {
        // A drag that started on the remote screen keeps the pointer even when
        // it crosses the top edge, and the bar does not come up under it --
        // not by claiming the pointer, and not by completing a dwell either.
        let claimed = if self.remote_drag() {
            self.overlay.pointer_taken();
            false
        } else {
            self.overlay.track(
                pos.x.max(0.0) as u32,
                pos.y.max(0.0) as u32,
                self.overlay_scale(),
                Instant::now(),
            )
        };
        if claimed {
            if !self.pointer_on_bar {
                self.pointer_on_bar = true;
                // Hand the pointer back to the OS: ours is the session's
                // cursor and it has no business over our own controls.
                if let Some(g) = &self.gfx {
                    g.window.set_cursor_visible(true);
                }
            }
            self.request_redraw();
            return;
        }
        // Leaving the bar resyncs the session's pointer even when our idea of
        // the position has not changed, because it has been moving under the
        // bar without the session hearing about it.
        let resync = std::mem::take(&mut self.pointer_on_bar);
        if resync {
            self.restore_cursor();
        }
        // Down into remote pixels: the pointer is kept in the session's
        // coordinates because that is what is sent, what arrives back in a
        // `CursorPosition`, and what has to be clamped to the remote screen.
        // Window pixels are re-derived where it is drawn.
        let (w, h) = self.client.size();
        let s = f64::from(self.scale);
        let x = (pos.x.max(0.0) / s).min(f64::from(w.saturating_sub(1))) as u32;
        let y = (pos.y.max(0.0) / s).min(f64::from(h.saturating_sub(1))) as u32;
        if resync || self.pointer != Some((x, y)) {
            self.pointer = Some((x, y));
            self.send(&Message::PointerMotion {
                x: x as u16,
                y: y as u16,
            });
            if self.draws_own_cursor() {
                self.request_redraw();
            }
        }
    }
}

/// The protocol button code and our own held-buttons bit for a winit button.
///
/// The bit is not the protocol code: `button::BACK` and `FORWARD` are 8 and 9
/// and would not fit in a byte of flags. It has to be one bit per button so
/// that letting go of the second button of a two-button drag is not mistaken
/// for the end of the drag.
fn button_codes(b: MouseButton) -> Option<(u8, u8)> {
    Some(match b {
        MouseButton::Left => (button::LEFT, 1),
        MouseButton::Middle => (button::MIDDLE, 2),
        MouseButton::Right => (button::RIGHT, 4),
        MouseButton::Back => (button::BACK, 8),
        MouseButton::Forward => (button::FORWARD, 16),
        MouseButton::Other(_) => return None,
    })
}

/// Match a key event against the bar's accelerators.
///
/// The layout's answer wins where it gives one: on Dvorak the key labelled B
/// is what should pin, whatever physical code it sits on. The physical code is
/// consulted only when the layout produced no letter at all, which is exactly
/// the macOS case -- Option is applied before winit sees the event, so
/// Ctrl+Alt+B arrives as the character U+222B and Ctrl+Alt+Q as U+0153.
///
/// Falling through to the physical code unconditionally would be worse than
/// not having it: on a Dvorak layout the physical B key types `x`, so
/// Ctrl+Alt+X would be quietly swallowed instead of reaching the session. The
/// price of the guard is that the two letter accelerators are unreachable on
/// macOS with a non-QWERTY layout; both buttons are still on the bar.
fn accelerator_for(
    modifiers: ModifiersState,
    logical: &Key,
    physical: PhysicalKey,
) -> Option<Accelerator> {
    if !(modifiers.control_key() && modifiers.alt_key()) {
        return None;
    }
    match logical {
        Key::Named(NamedKey::Enter) => return Some(Accelerator::Fullscreen),
        Key::Named(NamedKey::End) => return Some(Accelerator::SecureAttention),
        Key::Character(c) if c.eq_ignore_ascii_case("b") => return Some(Accelerator::Pin),
        Key::Character(c) if c.eq_ignore_ascii_case("r") => return Some(Accelerator::Reconnect),
        Key::Character(c) if c.eq_ignore_ascii_case("q") => return Some(Accelerator::Disconnect),
        // A letter the layout produced that is not one of ours is the
        // layout's final word; the session gets it.
        Key::Character(c) if c.chars().all(|c| c.is_ascii_alphanumeric()) => return None,
        _ => {}
    }
    match physical {
        PhysicalKey::Code(KeyCode::KeyB) => Some(Accelerator::Pin),
        PhysicalKey::Code(KeyCode::KeyR) => Some(Accelerator::Reconnect),
        PhysicalKey::Code(KeyCode::KeyQ) => Some(Accelerator::Disconnect),
        _ => None,
    }
}

/// An accelerator this window keeps rather than forwarding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Accelerator {
    /// Ctrl+Alt+Enter.
    Fullscreen,
    /// Ctrl+Alt+End.
    SecureAttention,
    /// Ctrl+Alt+B.
    Pin,
    /// Ctrl+Alt+R, and only while the link is down -- see
    /// [`App::accelerator`].
    Reconnect,
    /// Ctrl+Alt+Q.
    Disconnect,
}

/// Turn a wheel event into detents, before any accumulation.
///
/// Winit's y grows as the content moves up; [`Message::Scroll`]'s grows
/// downwards, like X11 button 5, so the sign flips here and only here.
pub fn scroll_units(delta: MouseScrollDelta) -> (f32, f32) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (x, -y),
        MouseScrollDelta::PixelDelta(p) => {
            (p.x as f32 / PX_PER_DETENT, -(p.y as f32) / PX_PER_DETENT)
        }
    }
}

/// The fractional part of a scroll gesture, held between events.
///
/// Without this a macOS trackpad cannot scroll at all: it emits a stream of
/// two- and three-pixel deltas, each of which is a fraction of a detent on its
/// own, and rounding each one independently rounds every one of them to zero.
/// Keeping the remainder turns the stream into whole detents at the rate the
/// finger actually moved.
///
/// The remainder is dropped when the axis reverses, so flicking back does not
/// have to repay the fraction the previous direction left behind, and when the
/// window loses focus, because a remainder is part of one gesture.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScrollCarry {
    carry: (f32, f32),
}

impl ScrollCarry {
    /// Forget the remainder on both axes.
    pub fn reset(&mut self) {
        self.carry = (0.0, 0.0);
    }

    /// Add a delta in detents and take out whole ones.
    pub fn feed(&mut self, dx: f32, dy: f32) -> (i16, i16) {
        (
            Self::axis(&mut self.carry.0, dx),
            Self::axis(&mut self.carry.1, dy),
        )
    }

    fn axis(carry: &mut f32, delta: f32) -> i16 {
        if !delta.is_finite() {
            return 0;
        }
        if delta != 0.0 && *carry != 0.0 && delta.is_sign_positive() != carry.is_sign_positive() {
            *carry = 0.0;
        }
        *carry += delta;
        // `trunc`, not `round`: rounding would emit a detent for half a
        // detent's movement and then owe the carry the other half back.
        let whole = carry.trunc();
        *carry -= whole;
        whole.clamp(f32::from(-i16::MAX), f32::from(i16::MAX)) as i16
    }
}

/// The window rectangle a cursor image covers at `(px, py)`, clamped to the
/// positive quadrant. Used to present the pixels the pointer leaves behind.
pub fn cursor_bounds(cur: &CursorImage, px: u32, py: u32) -> Rect {
    cursor_bounds_scaled(cur, px, py, 1)
}

/// What [`draw_cursor_scaled`] will touch, in window pixels.
///
/// Exactly the same arithmetic as the drawing, because a rectangle that is
/// even one pixel short of what was painted leaves a line of the old pointer
/// on screen until something else happens to repaint it.
pub fn cursor_bounds_scaled(cur: &CursorImage, px: u32, py: u32, scale: u32) -> Rect {
    let s = i64::from(scale.max(1));
    let x = i64::from(px) - i64::from(cur.hot_x) * s;
    let y = i64::from(py) - i64::from(cur.hot_y) * s;
    let (x0, y0) = (x.max(0), y.max(0));
    Rect::new(
        x0 as u32,
        y0 as u32,
        (x + i64::from(cur.width) * s - x0).max(0) as u32,
        (y + i64::from(cur.height) * s - y0).max(0) as u32,
    )
}

/// A rectangle of the remote screen, in window pixels.
///
/// The only conversion between the two coordinate systems, and the reason it
/// is a named function: everything the window presents -- decoded damage, the
/// cursor's box, the bar, the notice -- has to be in *window* pixels by the
/// time it reaches the damage list, and a damage rectangle left in remote
/// pixels does not fail, it just presents a quarter of the area that changed
/// and leaves stale bands on screen wherever the rest of it was.
pub fn scale_rect(r: Rect, scale: u32) -> Rect {
    let s = scale.max(1);
    Rect::new(
        r.x.saturating_mul(s),
        r.y.saturating_mul(s),
        r.width.saturating_mul(s),
        r.height.saturating_mul(s),
    )
}

/// Work out what to present.
///
/// `parts` are candidate damaged rectangles in window coordinates; empty ones
/// are ignored and the rest are clipped to `win`. A rectangle already covered
/// by another is dropped, so a full-window part collapses the list rather than
/// presenting the same pixels twice.
///
/// An empty result means the whole window: it is what a compositor-driven
/// expose looks like, where nothing in our own state changed but the pixels
/// still have to go out. Presenting too much is slow; presenting too little
/// leaves the user looking at pixels that are not there any more.
pub fn damage_regions(win: Rect, full: bool, parts: &[Rect]) -> Vec<Rect> {
    if full {
        return vec![win];
    }
    let mut out: Vec<Rect> = Vec::new();
    for p in parts {
        let r = p.intersect(&win);
        if r.is_empty() || out.iter().any(|o| o.contains(&r)) {
            continue;
        }
        out.retain(|o| !r.contains(o));
        out.push(r);
    }
    if out.is_empty() {
        vec![win]
    } else {
        out
    }
}

/// What a buffer of this age is still showing out of date.
///
/// `log` holds what each of the last few frames painted, newest first. An age
/// of `n` means this buffer's contents are the picture from `n` frames ago, so
/// the `n - 1` frames presented since then have changed pixels it has never
/// seen; those are what come back. An age of 1 is the common case and needs
/// nothing: the buffer is last frame's, and last frame's damage is already in
/// it.
///
/// `None` means "no idea, repaint everything". That covers `age == 0`, which
/// softbuffer documents as a buffer with unspecified contents, and an age
/// deeper than the log -- including the case where the log was just cleared,
/// because a cleared log is precisely the statement that nothing is known
/// about what the buffers hold.
pub fn stale_regions(age: u8, log: &[Vec<Rect>]) -> Option<Vec<Rect>> {
    if age == 0 || log.is_empty() {
        return None;
    }
    let back = usize::from(age) - 1;
    if back > log.len() {
        return None;
    }
    Some(log[..back].iter().flatten().copied().collect())
}

/// What to paint this frame, and what to remember of it.
///
/// `own` are the rectangles this frame's own state changed -- decoded damage,
/// the cursor's old box and its new one, the bar's. `stale` is what
/// [`stale_regions`] says the buffer we were handed has not seen. The paint
/// list is both together; the remembered list is `own` alone.
///
/// That difference is the whole point of the function, and it is not
/// cosmetic. What a buffer from `n` frames ago is missing is what *changed on
/// screen* since, not what those frames happened to repaint -- and a log of
/// the latter never decays where the age is 2. softbuffer's Wayland backend
/// settles there permanently: each frame is handed the buffer from two frames
/// back, so it repaints what the previous frame painted, which was in turn
/// what the frame before that painted. Seed one full redraw into a log of
/// painted rectangles -- and start-up seeds two -- and the whole window is
/// blitted *and presented* for the rest of the session, which is exactly the
/// cost the damage list exists to avoid. Remembering only what changed lets
/// the list shrink back to the real damage on the second frame.
///
/// `own` is passed through [`damage_regions`], so an idle frame with nothing
/// changed at all is remembered as the whole window rather than as nothing.
/// That is the conservative direction (repainting more than needed is a cost,
/// not an artefact), and `about_to_wait` does not ask for a redraw with
/// nothing to draw, so it does not arise in practice.
pub fn present_plan(win: Rect, full: bool, own: &[Rect], stale: &[Rect]) -> (Vec<Rect>, Vec<Rect>) {
    let changed = damage_regions(win, full, own);
    if full || stale.is_empty() {
        return (changed.clone(), changed);
    }
    let mut all = own.to_vec();
    all.extend_from_slice(stale);
    (damage_regions(win, false, &all), changed)
}

/// Convert to softbuffer's rectangle, dropping empty ones.
fn surface_rect(r: Rect) -> Option<softbuffer::Rect> {
    Some(softbuffer::Rect {
        x: r.x,
        y: r.y,
        width: NonZeroU32::new(r.width)?,
        height: NonZeroU32::new(r.height)?,
    })
}

/// Shared slot through which the reader thread reaches the event loop proxy.
pub type WakerSlot = Arc<std::sync::Mutex<Option<EventLoopProxy<Wake>>>>;

/// A wake callback for [`Client::connect`] that nudges the event loop.
/// Safe to call from any thread; it is a no-op until [`App::run`] has
/// installed the proxy in the returned slot.
pub fn make_waker() -> (Box<dyn Fn() + Send>, WakerSlot) {
    let slot: WakerSlot = Arc::new(std::sync::Mutex::new(None));
    let f = waker_for(&slot);
    (f, slot)
}

/// Another wake callback for a slot that already exists.
///
/// A reconnection needs two: one for the new client's reader thread and one
/// for the worker itself, so the answer is noticed when it arrives rather than
/// at the next quarter-second tick.
pub fn waker_for(slot: &WakerSlot) -> Box<dyn Fn() + Send> {
    let slot = slot.clone();
    Box::new(move || {
        if let Ok(guard) = slot.lock() {
            if let Some(p) = guard.as_ref() {
                let _ = p.send_event(Wake);
            }
        }
    })
}

/// How long to wait before the attempt after `spent` of them.
///
/// The last entry of [`RETRY_BACKOFF`] repeats, so the wait grows and then
/// stops growing rather than running off into hours -- the budget, not the
/// delay, is what ends the attempts.
fn retry_delay(spent: u32) -> Duration {
    RETRY_BACKOFF[(spent as usize).min(RETRY_BACKOFF.len() - 1)]
}

/// The remote screen size a window of this size can show at `scale`.
///
/// Floored, and that is a requirement rather than a rounding preference: the
/// remote screen has to be a whole number of magnified pixels, or the session
/// is asked for a size the window cannot show without scaling something. The
/// few pixels the division loses are the black margin the blit already paints
/// down the right and bottom edges.
fn remote_size_for(size: PhysicalSize<u32>, scale: u32) -> (u32, u32) {
    let s = scale.max(1);
    (size.width / s, size.height / s)
}

/// The magnification a display of this scale factor asks for.
///
/// Rounded, because the magnification is a whole number or it is resampling;
/// clamped to [`MAX_SCALE`], which is the same ceiling the saved connections
/// use, so a profile and a display cannot disagree about what is possible.
pub fn display_scale(window_scale: f64) -> u32 {
    if !window_scale.is_finite() {
        return 1;
    }
    window_scale.round().clamp(1.0_f64, f64::from(MAX_SCALE)) as u32
}

/// Blend a colour over a rectangle of the window buffer.
///
/// The same integer form as the cursor compositor and the bar's, so all three
/// agree pixel for pixel over the same background.
pub fn blend_rect(dst: &mut [u32], dst_w: u32, dst_h: u32, r: Rect, colour: u32, alpha: u32) {
    if alpha == 0 || dst.len() < (dst_w as usize) * (dst_h as usize) {
        return;
    }
    let r = r.intersect(&Rect::new(0, 0, dst_w, dst_h));
    let inv = 255 - alpha.min(255);
    let a = alpha.min(255);
    for y in r.y..r.bottom() {
        let row = (y as usize) * (dst_w as usize);
        for x in r.x..r.right() {
            let i = row + x as usize;
            let d = dst[i];
            let ch = |shift: u32| -> u32 {
                let sc = (colour >> shift) & 0xff;
                let dc = (d >> shift) & 0xff;
                ((sc * a + dc * inv + 127) / 255).min(255)
            };
            dst[i] = (ch(16) << 16) | (ch(8) << 8) | ch(0);
        }
    }
}

/// How many glyphs of the bar's font fit in `w` pixels at scale `s`.
///
/// The font is fixed width: five columns and a one-column gap, all at `s`,
/// with no gap after the last glyph. Arithmetic rather than a decision, which
/// is why it is repeated here instead of exported from [`overlay`] -- and
/// `the_text_metric_matches_the_font_that_paints_it` checks it against real
/// painted pixels, so the two cannot drift apart quietly.
fn chars_fitting(w: u32, s: u32) -> usize {
    ((w + s) / (6 * s)) as usize
}

/// Height of the strip the link notice is drawn in.
fn notice_height(s: u32) -> u32 {
    44 * s
}

/// Draw one line of text with the bar's font, at `(x, y)` in window pixels.
///
/// [`overlay`] owns the only bitmap font in this window and exports no text
/// call of its own -- but `paint` becomes one when it is handed a layout with
/// no bar, no state square and no buttons: every rectangle it fills is then
/// empty and clips away, and the spans are all that reach the buffer.
/// Borrowing it keeps one font in the window; a second copy of a 96-entry
/// glyph table here would be a second thing to keep in step for no gain.
fn draw_text_line(
    dst: &mut [u32],
    w: u32,
    h: u32,
    s: u32,
    at: (u32, u32),
    text: &str,
    colour: u32,
) {
    let layout = overlay::Layout {
        bar: Rect::default(),
        dot: Rect::default(),
        dot_colour: 0,
        spans: vec![overlay::Span {
            x: at.0,
            text: text.to_string(),
            colour,
        }],
        buttons: Vec::new(),
        s,
        text_y: at.1,
    };
    overlay::paint(dst, w, h, &layout, None, None);
}

/// Shorten `text` to what will fit in `w` pixels, marking it if it was cut.
///
/// Marked because a silently clipped reason is a different reason: "no route
/// to host" and "no route" are not the same sentence, and the second one looks
/// deliberate.
fn fit_text(text: &str, w: u32, s: u32) -> String {
    let room = chars_fitting(w, s);
    if text.chars().count() <= room {
        return text.to_string();
    }
    if room < 3 {
        return String::new();
    }
    let mut out: String = text.chars().take(room - 2).collect();
    out.push_str("..");
    out
}

/// Draw the strip that says what happened to the link.
///
/// Along the bottom edge, which is not decoration: the bar lives along the top
/// and the two must never fight over the same pixels, and the bottom is also
/// where a full-screen session's own panel usually is not.
fn draw_notice(dst: &mut [u32], w: u32, h: u32, s: u32, n: &Notice) -> Rect {
    let height = notice_height(s).min(h);
    let panel = Rect::new(0, h.saturating_sub(height), w, height);
    blend_rect(
        dst,
        w,
        h,
        panel,
        overlay::colour::SCRIM,
        overlay::colour::SCRIM_ALPHA,
    );
    // A hairline along the top edge, the way the bar has one along its bottom:
    // over a pale desktop the scrim alone does not read as an edge.
    blend_rect(
        dst,
        w,
        h,
        Rect::new(panel.x, panel.y, panel.width, s),
        overlay::colour::HAIRLINE,
        overlay::colour::HAIRLINE_ALPHA,
    );
    let pad = 4 * s;
    let avail = w.saturating_sub(2 * pad);
    for (i, (text, colour)) in [
        (n.headline.as_str(), n.colour),
        (n.detail.as_str(), overlay::colour::TEXT),
        (n.hint, overlay::colour::DIM),
    ]
    .iter()
    .enumerate()
    {
        let y = panel.y + 6 * s + i as u32 * 12 * s;
        draw_text_line(dst, w, h, s, (pad, y), &fit_text(text, avail, s), *colour);
    }
    panel
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            if let Err(e) = self.init_window(event_loop) {
                log::error!("{e:#}");
                self.exit_reason = Some(format!("{e:#}"));
                event_loop.exit();
            }
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            Instant::now() + self.next_wake(),
        ));
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: Wake) {
        self.drain_network();
        // A wake also arrives when a reconnection worker has an answer, and
        // the answer is worth having before the next quarter-second tick.
        self.poll_reconnect(Instant::now());
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::DroppedFile(path) => self.on_dropped_file(&path),
            WindowEvent::CloseRequested => self.close(event_loop, "window closed"),
            WindowEvent::RedrawRequested => {
                // A redraw nobody asked for is an expose. Our own state says
                // nothing changed, so the damage list would name at most the
                // cursor -- and `present_with_damage` copies only what it is
                // given, leaving the rest of the window showing whatever
                // uncovered it. Repaint the lot instead.
                if !std::mem::take(&mut self.redraw_asked) {
                    self.full_redraw = true;
                }
                self.drain_network();
                if let Err(e) = self.redraw() {
                    log::error!("redraw failed: {e:#}");
                }
            }
            WindowEvent::Resized(size) => {
                self.full_redraw = true;
                if self.opts.dynamic_resize && self.client.info().features & features::RESIZE != 0 {
                    self.pending_size = Some(remote_size_for(size, self.scale));
                    self.last_resize_event = Some(Instant::now());
                }
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Dragged to a monitor with a different DPI. The window keeps
                // its logical size, so its physical size has just changed by
                // the same ratio as the magnification is about to -- which
                // means the remote screen size works out unchanged and the
                // desktop stays the size it looks. A pinned `--scale` is left
                // alone: it was an instruction, not a guess.
                if !self.scale_pinned {
                    let want = display_scale(scale_factor);
                    if want != self.scale {
                        log::info!("display scale is now {scale_factor}; magnifying {want}x");
                        self.scale = want;
                    }
                }
                self.full_redraw = true;
                self.request_redraw();
            }
            WindowEvent::Focused(f) => {
                self.focused = f;
                self.overlay.set_focused(f);
                if f {
                    // Coming back to the window is the moment someone is about
                    // to paste what they copied elsewhere, so the image poll's
                    // backoff is cleared here rather than waited out. Where
                    // there is a change counter this costs nothing: the
                    // counter already knows whether anything was copied.
                    self.clipwatch.wake(Instant::now());
                    self.poll_clipboard();
                } else {
                    // Only while there is a link to release them on. With one
                    // down, `pressed_keys` is the record of what the session
                    // was left holding and the note `resync_input` replays
                    // when a new link comes up; clearing it here would throw
                    // that away and leave the modifier held.
                    if self.link_up() {
                        self.release_all_keys();
                    }
                    // A wheel remainder from before the window lost focus is
                    // not part of whatever gesture comes next.
                    self.scroll.reset();
                    // Nor is a drag: the release may be delivered to whoever
                    // took the pointer instead of to us, and a button left
                    // recorded as held would keep the bar down for the rest
                    // of the session.
                    self.remote_buttons = 0;
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => self.on_key(event, event_loop),
            WindowEvent::CursorMoved { position, .. } => self.on_pointer_moved(position),
            WindowEvent::CursorLeft { .. } => {
                self.overlay.pointer_left();
                self.pointer_on_bar = false;
                if let Some(g) = &self.gfx {
                    g.window.set_cursor_visible(true);
                }
            }
            WindowEvent::CursorEntered { .. } => self.restore_cursor(),
            WindowEvent::MouseInput {
                state, button: b, ..
            } => {
                let down = state == ElementState::Pressed;
                // A press that started on the bar owns its release wherever
                // that lands, or the session would see a release it never saw
                // a press for.
                if !down && std::mem::take(&mut self.bar_press) {
                    if let Some(action) = self.overlay.release() {
                        self.run_overlay_action(action, event_loop);
                    }
                    self.request_redraw();
                    return;
                }
                if self.pointer_on_bar {
                    if down && b == MouseButton::Left {
                        self.bar_press = true;
                        self.overlay.press();
                        self.request_redraw();
                    }
                    return;
                }
                // A click on a dimmed window is not input for a session that
                // cannot hear it; it is the most obvious way there is to ask
                // for the connection back, and the notice says so.
                if !self.link_up() {
                    if down && b == MouseButton::Left {
                        self.reconnect_now();
                    }
                    return;
                }
                let Some((btn, bit)) = button_codes(b) else {
                    return;
                };
                if down {
                    self.remote_buttons |= bit;
                    // A press in the hot zone is a press: it stops the dwell
                    // from raising the bar over whatever was clicked, even if
                    // the pointer then never moves.
                    self.overlay.pointer_taken();
                } else {
                    self.remote_buttons &= !bit;
                }
                self.send(&Message::PointerButton { button: btn, down });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.pointer_on_bar || !self.link_up() {
                    return;
                }
                let (dx, dy) = scroll_units(delta);
                let (sx, sy) = self.scroll.feed(dx, dy);
                if sx != 0 || sy != 0 {
                    self.send(&Message::Scroll { dx: sx, dy: sy });
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_network();
        self.housekeeping();
        if self.dirty.is_some() || self.full_redraw {
            self.request_redraw();
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            Instant::now() + self.next_wake(),
        ));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // A reconnection worker holds the endpoint, and an SSH endpoint owns
        // the `ssh` child: leaving while it is out there orphans a process
        // that would go on forwarding a port with no window behind it. Waiting
        // a moment gets it back in the ordinary case, where the attempt is one
        // connection to a tunnel that is already up; anything slower than that
        // is ssh authenticating, and the exit does not wait on a passphrase.
        if let Some(a) = self.attempt.take() {
            let _ = a.done.recv_timeout(Duration::from_millis(250));
        }
        // Nothing is said down a link that has already failed. `Client::send`
        // would block for the write timeout against a half-open socket, and
        // this is the path a user takes when they close a window that is
        // *already* telling them the connection is gone -- so the freeze would
        // land exactly where it is least deserved.
        if !self.link_up() {
            return;
        }
        self.release_all_keys();
        // Release Shift/Control keysym remnants explicitly for safety.
        self.send_key(keysym::SHIFT_L, false);
        self.client
            .disconnect(self.exit_reason.as_deref().unwrap_or("client exiting"));
    }
}

/// Where files copied in the session are downloaded before being offered on
/// the local clipboard. A file manager pasting them reads them from here, so
/// they outlive the paste rather than living in a directory we delete.
///
/// This is the root; each copy gets a numbered subdirectory of its own from
/// [`new_batch_dir`]. It is per process so that two sessions cannot fight over
/// the same names, and the process id is reused by the operating system, which
/// is exactly why `new_batch_dir` refuses to reuse a directory it finds.
pub fn clipboard_staging_dir() -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("lynxrdp-clipboard-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Prefix of a per-copy staging directory under [`clipboard_staging_dir`].
const BATCH_PREFIX: &str = "batch-";

/// How many staged copies are kept before the oldest are deleted.
///
/// More than one, because a file manager reads a pasted file when the *paste*
/// happens: copying something new in the session while the previous paste is
/// still being written would otherwise delete the bytes out from under it.
/// Four is a couple of copies' worth of grace and still a bounded amount of
/// temporary space.
const KEEP_STAGED_BATCHES: usize = 4;

/// Largest number of files one drop may upload, so dropping a huge tree
/// does not queue thousands of transfers by accident.
pub const MAX_DROPPED_FILES: usize = 512;

/// How many uploads may be in flight at once.
///
/// Each live transfer holds the source file open until the peer confirms it,
/// and the whole point of a queue is that this number, not the size of the
/// drop, is what the process's descriptor limit has to accommodate. macOS
/// inherits `RLIMIT_NOFILE` from launchd, commonly 256, so a ~300 file folder
/// used to fail partway through and take every other open in the process down
/// with it.
///
/// Eight also costs nothing in throughput: `GLOBAL_WINDOW_CHUNKS` caps the
/// data in flight across *all* transfers at sixteen chunks, so past about
/// eight concurrent transfers each one is down to less than a chunk of window
/// and the connection moves no faster for having more of them open.
pub const MAX_CONCURRENT_UPLOADS: usize = 8;

/// Largest number of files that may be queued and in flight together.
///
/// [`MAX_DROPPED_FILES`] bounds one dropped path; this bounds the drop. Four
/// full folders' worth, after which a further drop is refused whole and said
/// so, rather than accepted and quietly truncated.
pub const MAX_PENDING_UPLOADS: usize = 4 * MAX_DROPPED_FILES;

/// How many files of one clipboard copy are fetched at once.
///
/// The same reasoning as [`MAX_CONCURRENT_UPLOADS`], in the other direction:
/// the receiving side creates a file per accepted offer and holds it open
/// until the transfer ends, and a session may copy up to `MAX_FILE_LIST`
/// (4096) files in one go.
pub const MAX_CONCURRENT_CLIPBOARD_FILES: usize = 8;

/// Longest staged file name, in bytes.
///
/// Comfortably inside the 255-byte component limit that ext4, APFS and NTFS
/// all share, with room for the disambiguating suffix and the extension that
/// are appended after the truncation.
const MAX_STAGED_NAME: usize = 200;

/// Create a fresh directory for one clipboard copy.
///
/// `next` is bumped past whatever was used, so the numbering is monotonic
/// within a process and [`prune_batches`] can order by it. Creation is
/// exclusive rather than `create_dir_all`, because the operating system reuses
/// process ids: a directory already there belongs to a dead process that had
/// this pid, and reusing it would mix its files into this copy.
pub fn new_batch_dir(root: &Path, next: &mut u64) -> anyhow::Result<PathBuf> {
    for _ in 0..1024 {
        let dir = root.join(format!("{BATCH_PREFIX}{}", *next));
        *next += 1;
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context(format!("creating {}", dir.display())),
        }
    }
    anyhow::bail!("no free staging directory under {}", root.display())
}

/// Delete all but the `keep` newest staged copies under `root`.
///
/// Ordered by the number in the name rather than by a timestamp: the number is
/// what this process assigned, and a modification time can be identical across
/// two copies made in the same second. Anything that is not a `batch-<n>`
/// directory is left alone -- this runs against a directory in the system
/// temporary area and has no business deleting something it does not recognise.
pub fn prune_batches(root: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(n) = name
            .to_str()
            .and_then(|n| n.strip_prefix(BATCH_PREFIX))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        found.push((n, entry.path()));
    }
    // Newest batch first, so `skip(keep)` drops the oldest.
    found.sort_unstable_by_key(|(n, _)| std::cmp::Reverse(*n));
    for (_, path) in found.into_iter().skip(keep) {
        if let Err(e) = std::fs::remove_dir_all(&path) {
            log::debug!("could not remove {}: {e}", path.display());
        }
    }
}

/// Reduce a name from the session to one every platform can create.
///
/// Sanitising happens on all three platforms, not under `cfg(windows)`, for
/// two reasons. The obvious one is that the staged file has to be creatable
/// here; the less obvious one is that the *result* has to be predictable,
/// because the caller deduplicates names and a rule that differs per platform
/// gives a different set of collisions per platform -- so the case that
/// silently overwrites a file would only ever appear on the platform nobody
/// tested on.
///
/// What is removed: the path separators and the characters Windows rejects
/// outright, control characters (a newline in a file name is legal on Linux
/// and a menace everywhere else), trailing dots and spaces, which Windows
/// strips when it resolves a name so that `report.` and `report` are the same
/// file, and the device names that are reserved whatever the extension.
pub fn safe_file_name(raw: &str) -> String {
    const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    // `CON.txt` opens the console on Windows, not a file. The check is on the
    // part before the first dot, which is how Windows resolves them.
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || FORBIDDEN.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();

    if name.len() > MAX_STAGED_NAME {
        // Keep the extension: it is what a file manager uses to decide what
        // the pasted file is, and losing it turns a spreadsheet into a blob.
        let ext = Path::new(&name)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .filter(|e| e.len() <= 16)
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let mut cut = MAX_STAGED_NAME - ext.len();
        while cut > 0 && !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
        name.push_str(&ext);
    }

    // After the truncation, not before: cutting a name can expose a dot that
    // was in the middle of it.
    let trimmed = name.trim_end_matches([' ', '.']);
    // This is also what rules out "." and "..", which trim to nothing.
    let mut name = if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    };
    let stem = name.split('.').next().unwrap_or_default();
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        name.insert(0, '_');
    }
    name
}

/// Make `name` unique among the names already `taken`, recording it.
///
/// Two files called `notes.txt` from different directories in the session used
/// to be staged over each other -- the second `File::create` truncated the
/// first and the paste delivered one file where the user copied two, with no
/// error anywhere. Comparison is case-insensitive because NTFS and a default
/// APFS volume both resolve `Notes.txt` and `notes.txt` to the same file, so
/// on those platforms the collision is real even when the names differ.
pub fn unique_name(taken: &mut HashSet<String>, name: &str) -> String {
    if taken.insert(name.to_lowercase()) {
        return name.to_string();
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    // Terminates because `taken` is finite and every candidate is distinct.
    let mut n = 2u32;
    loop {
        let candidate = format!("{stem} ({n}){ext}");
        if taken.insert(candidate.to_lowercase()) {
            return candidate;
        }
        n += 1;
    }
}

/// One clipboard file copy, from the session's list to the local clipboard.
///
/// A batch cannot be tracked by a count, which is what it was: the counter
/// only came down when a file arrived, so a single file that could not be
/// created locally left it stuck above zero and the copy was never published
/// at all. That is disproportionately a Windows failure -- `a:b.txt`,
/// `what?.png`, `CON` and a trailing dot are all ordinary names in an X11
/// session and none of them can be created on NTFS -- and the user's only
/// symptom was Ctrl+V returning their old clipboard.
///
/// Here every file has a slot and every transfer has an id, each id is
/// resolved exactly once whether it arrived or failed, and the batch finishes
/// when there is nothing left outstanding. What did not arrive is simply
/// missing from the published list.
pub struct ClipBatch {
    dir: PathBuf,
    /// Not yet requested: (remote path, local destination, slot).
    queued: VecDeque<(String, PathBuf, usize)>,
    /// Requested and unresolved: transfer id to slot.
    live: HashMap<u64, usize>,
    /// Where each file landed, in the order the session listed them. Order is
    /// kept because it is the order the user selected, and a file manager
    /// shows a paste in the order it is given.
    slots: Vec<Option<PathBuf>>,
}

impl ClipBatch {
    /// Plan a copy: one slot per file, with a staged name that is safe on this
    /// platform and unique within the batch.
    pub fn new(dir: PathBuf, files: &[lynxrdp_proto::FileEntry]) -> Self {
        let mut taken = HashSet::new();
        let mut queued = VecDeque::with_capacity(files.len());
        for (slot, f) in files.iter().enumerate() {
            let base = f.path.rsplit(['/', '\\']).next().unwrap_or_default();
            let name = unique_name(&mut taken, &safe_file_name(base));
            queued.push_back((f.path.clone(), dir.join(name), slot));
        }
        Self {
            dir,
            queued,
            live: HashMap::new(),
            slots: vec![None; files.len()],
        }
    }

    /// Where this batch is staged.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How many files the session offered.
    pub fn total(&self) -> usize {
        self.slots.len()
    }

    /// The next file to request, if the concurrency limit leaves room.
    pub fn next_request(&mut self) -> Option<(String, PathBuf, usize)> {
        if self.live.len() >= MAX_CONCURRENT_CLIPBOARD_FILES {
            return None;
        }
        self.queued.pop_front()
    }

    /// Record the transfer id a request was given.
    pub fn requested(&mut self, slot: usize, id: u64) {
        self.live.insert(id, slot);
    }

    /// Resolve a transfer: `Some(path)` if it arrived, `None` if it failed.
    /// Returns whether the id belonged to this batch.
    pub fn resolve(&mut self, id: u64, path: Option<PathBuf>) -> bool {
        let Some(slot) = self.live.remove(&id) else {
            return false;
        };
        if let Some(p) = path {
            self.slots[slot] = Some(p);
        }
        true
    }

    /// Transfers still in flight, for cancelling a superseded copy.
    pub fn live_ids(&self) -> Vec<u64> {
        self.live.keys().copied().collect()
    }

    /// Whether every file has arrived, failed or been given up on.
    pub fn done(&self) -> bool {
        self.queued.is_empty() && self.live.is_empty()
    }

    /// The files that actually landed, in the order they were copied.
    pub fn into_files(self) -> Vec<PathBuf> {
        self.slots.into_iter().flatten().collect()
    }
}

/// Work out what to upload for a dropped path.
///
/// Returns (local path, destination path relative to the session's upload
/// directory) pairs. A dropped directory keeps its structure, rooted at the
/// directory's own name; symlinks are not followed, so a link into `/` cannot
/// turn one drop into a copy of the filesystem.
pub fn collect_dropped_files(
    path: &std::path::Path,
) -> anyhow::Result<Vec<(std::path::PathBuf, String)>> {
    let mut out = Vec::new();
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_file() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow::anyhow!("dropped path has no file name"))?;
        out.push((path.to_path_buf(), name));
        return Ok(out);
    }
    if !meta.is_dir() {
        // A symlink or special file: nothing sensible to upload.
        return Ok(out);
    }
    let root = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dropped".to_string());
    let mut stack = vec![(path.to_path_buf(), root)];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            // Note both DirEntry::file_type and DirEntry::metadata describe the
            // entry itself rather than a symlink's target, unlike fs::metadata.
            // The walk depends on that: following a link would upload files
            // outside the dropped tree, and a link to an ancestor would loop.
            let kind = entry.file_type()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let dest = format!("{prefix}/{name}");
            if kind.is_dir() {
                stack.push((entry.path(), dest));
            } else if kind.is_file() {
                if out.len() >= MAX_DROPPED_FILES {
                    anyhow::bail!("more than {MAX_DROPPED_FILES} files in the dropped directory");
                }
                out.push((entry.path(), dest));
            }
        }
    }
    Ok(out)
}

/// A cheap digest of image bytes, used only to notice that the clipboard
/// image changed. Collisions would merely skip one redundant offer.
fn image_digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^ bytes.len() as u64
}

/// Copy the framebuffer into the window buffer (top-left anchored, black
/// where the window is larger than the remote screen).
///
/// [`redraw`](App::redraw) goes through [`blit_rect`] now, but this stays as
/// the statement of what a full repaint means, and as what `blit_rect` is
/// tested against: the two must agree over the whole window or the damage
/// path is painting something the full path would not.
pub fn blit(dst: &mut [u32], dst_w: u32, dst_h: u32, fb: &Framebuffer) {
    let w = dst_w.min(fb.width());
    let h = dst_h.min(fb.height());
    for y in 0..dst_h {
        let row = &mut dst[(y * dst_w) as usize..((y + 1) * dst_w) as usize];
        if y < h {
            row[..w as usize].copy_from_slice(fb.row(y, 0, w));
            row[w as usize..].fill(0);
        } else {
            row.fill(0);
        }
    }
}

/// Copy one rectangle of the framebuffer into the window buffer.
///
/// Row for row this does exactly what [`blit`] does to those rows: framebuffer
/// pixels where the remote screen reaches, black where it does not. The clamp
/// is the entire point of the function. `Framebuffer::row` panics past its end
/// and the window is routinely larger than the remote screen -- a window
/// maximised before the session has resized to match is the ordinary case, not
/// a corner -- so a rectangle that runs off the right or bottom edge of the
/// remote screen has to be split into the part that is there and the part that
/// is padding, per row, exactly as `blit` splits it.
pub fn blit_rect(dst: &mut [u32], dst_w: u32, dst_h: u32, fb: &Framebuffer, rect: Rect) {
    if dst.len() < (dst_w as usize) * (dst_h as usize) {
        return;
    }
    let r = rect.intersect(&Rect::new(0, 0, dst_w, dst_h));
    if r.is_empty() {
        return;
    }
    // Columns of this rectangle the framebuffer actually covers. Saturating,
    // because a rectangle can start beyond the right edge of the remote screen
    // and then every column of it is padding.
    let copy_w = r.right().min(fb.width()).saturating_sub(r.x);
    for y in r.y..r.bottom() {
        let start = (y as usize) * (dst_w as usize) + (r.x as usize);
        let row = &mut dst[start..start + r.width as usize];
        if y < fb.height() && copy_w > 0 {
            row[..copy_w as usize].copy_from_slice(fb.row(y, r.x, copy_w));
            row[copy_w as usize..].fill(0);
        } else {
            row.fill(0);
        }
    }
}

/// Copy one rectangle of the framebuffer into the window buffer, magnified.
///
/// `rect` is in *window* pixels; each remote pixel becomes a `scale`-by-`scale`
/// block of itself. Nearest-neighbour at a whole factor is the only
/// magnification this client does, and the reason is the promise at the top of
/// this file: duplicating a pixel invents nothing, so text stays exactly as
/// the server drew it and a screenful of 9 px terminal type is still the same
/// glyphs, four times the area. Anything fractional would resample, and a
/// resampled remote desktop is a blurry one.
///
/// `row` is scratch space, reused across calls. Each *source* row is expanded
/// once into it and then memcpy'd into the `scale` window rows that share it,
/// so the total writes stay proportional to the output rather than being
/// re-derived per pixel per row; at scale 4 that is one expansion instead of
/// four and three memcpys instead of three more passes of division.
///
/// Padding follows [`blit_rect`] exactly: black wherever the window reaches
/// past the magnified remote screen, which is the ordinary state of a window
/// that has been resized and whose session has not caught up yet.
pub fn blit_rect_scaled(
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
    fb: &Framebuffer,
    rect: Rect,
    scale: u32,
    row: &mut Vec<u32>,
) {
    if scale <= 1 {
        blit_rect(dst, dst_w, dst_h, fb, rect);
        return;
    }
    if dst.len() < (dst_w as usize) * (dst_h as usize) {
        return;
    }
    let r = rect.intersect(&Rect::new(0, 0, dst_w, dst_h));
    if r.is_empty() {
        return;
    }
    row.clear();
    row.resize(r.width as usize, 0);
    let mut y = r.y;
    while y < r.bottom() {
        let sy = y / scale;
        // The window rows this source row covers, clipped to the rectangle:
        // one expansion serves all of them.
        let band = ((sy + 1) * scale).min(r.bottom());
        if sy < fb.height() {
            let mut x = r.x;
            while x < r.right() {
                let sx = x / scale;
                let end = ((sx + 1) * scale).min(r.right());
                let v = if sx < fb.width() { fb.get(sx, sy) } else { 0 };
                row[(x - r.x) as usize..(end - r.x) as usize].fill(v);
                x = end;
            }
        } else {
            row.fill(0);
        }
        for yy in y..band {
            let start = (yy as usize) * (dst_w as usize) + (r.x as usize);
            dst[start..start + r.width as usize].copy_from_slice(row);
        }
        y = band;
    }
}

/// Alpha-blend a premultiplied ARGB cursor onto the buffer, 1:1.
pub fn draw_cursor(dst: &mut [u32], dst_w: u32, dst_h: u32, cur: &CursorImage, px: u32, py: u32) {
    draw_cursor_scaled(dst, dst_w, dst_h, cur, px, py, 1);
}

/// Alpha-blend a premultiplied ARGB cursor, magnified `scale` times.
///
/// `(px, py)` is the hotspot in *window* pixels, and the image is magnified
/// about it: at scale 2 a cursor whose hotspot is 4 px into its own image
/// starts 8 window pixels left of the pointer. Anything else would put the
/// point of the arrow somewhere other than where the user is pointing, which
/// is the one thing a pointer has to get right.
///
/// The magnification is the remote screen's, not the display's: the cursor is
/// part of the picture the session drew, so it grows with the rest of it. Each
/// source pixel becomes a `scale`-by-`scale` block of the same colour --
/// nothing is resampled and no colour appears that the server did not send.
pub fn draw_cursor_scaled(
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
    cur: &CursorImage,
    px: u32,
    py: u32,
    scale: u32,
) {
    if cur.width == 0 || cur.height == 0 || scale == 0 {
        return;
    }
    let s = i64::from(scale);
    let ox = i64::from(px) - i64::from(cur.hot_x) * s;
    let oy = i64::from(py) - i64::from(cur.hot_y) * s;
    for cy in 0..i64::from(cur.height) {
        for cx in 0..i64::from(cur.width) {
            let src = cur.argb[(cy as usize) * usize::from(cur.width) + cx as usize];
            let a = src >> 24;
            if a == 0 {
                continue;
            }
            for by in 0..s {
                let y = oy + cy * s + by;
                if y < 0 || y >= i64::from(dst_h) {
                    continue;
                }
                for bx in 0..s {
                    let x = ox + cx * s + bx;
                    if x < 0 || x >= i64::from(dst_w) {
                        continue;
                    }
                    let idx = (y as usize) * (dst_w as usize) + x as usize;
                    if a == 255 {
                        dst[idx] = src & 0x00FF_FFFF;
                        continue;
                    }
                    let d = dst[idx];
                    let inv = 255 - a;
                    let blend =
                        |sc: u32, dc: u32| -> u32 { (sc + (dc * inv + 127) / 255).min(255) };
                    let r = blend((src >> 16) & 0xff, (d >> 16) & 0xff);
                    let g = blend((src >> 8) & 0xff, (d >> 8) & 0xff);
                    let b = blend(src & 0xff, d & 0xff);
                    dst[idx] = (r << 16) | (g << 8) | b;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn dropping_a_file_uploads_it_under_its_own_name() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("notes.txt");
        std::fs::write(&f, b"x").unwrap();
        let got = collect_dropped_files(&f).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "notes.txt");
    }

    #[test]
    fn dropping_a_directory_keeps_its_structure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("README.md"), b"r").unwrap();
        std::fs::write(root.join("src/main.rs"), b"m").unwrap();
        let mut got: Vec<String> = collect_dropped_files(&root)
            .unwrap()
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        got.sort();
        assert_eq!(got, vec!["project/README.md", "project/src/main.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_dropped_directory_is_not_followed() {
        // Following links here would upload files the user never dropped, and
        // a link back to an ancestor would make the walk loop forever. This
        // holds today because DirEntry does not resolve links; the test pins
        // it so a switch to fs::metadata cannot quietly reintroduce either.
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"s").unwrap();

        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("real.txt"), b"r").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link-to-outside")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link-to-file")).unwrap();
        // A link back to the directory being walked: the loop case.
        std::os::unix::fs::symlink(&root, root.join("link-to-self")).unwrap();

        let got: Vec<String> = collect_dropped_files(&root)
            .unwrap()
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(got, vec!["project/real.txt"]);
    }

    #[test]
    fn dropping_something_unreadable_is_an_error_not_a_panic() {
        let missing = std::path::Path::new("/definitely/not/here");
        assert!(collect_dropped_files(missing).is_err());
    }

    #[test]
    fn destinations_from_a_drop_are_safe_to_join() {
        // Whatever a drop produces must survive the session's traversal check.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/c.txt"), b"c").unwrap();
        for (_, dest) in collect_dropped_files(&root).unwrap() {
            assert!(
                lynxrdp_proto::transfer::safe_relative_path(&dest).is_some(),
                "{dest} would be refused by the session"
            );
        }
    }

    /// A framebuffer whose pixels all differ, so a misplaced copy shows up.
    fn ramp(w: u32, h: u32) -> Framebuffer {
        let mut fb = Framebuffer::new(w, h);
        for y in 0..h {
            for x in 0..w {
                fb.set(x, y, 0x11_0000 | (y * w + x));
            }
        }
        fb
    }

    #[test]
    fn a_full_window_blit_rect_is_the_full_blit() {
        // The equivalence the damage path rests on: when everything is
        // repainted, repainting it rectangle by rectangle has to produce the
        // pixels the old unconditional blit produced. The window sizes cover
        // both directions of over- and under-hang.
        let fb = ramp(7, 5);
        for (w, h) in [(7u32, 5u32), (10, 8), (4, 3), (10, 3), (4, 8)] {
            let mut whole = vec![0xDEADu32; (w * h) as usize];
            let mut piece = whole.clone();
            blit(&mut whole, w, h, &fb);
            blit_rect(&mut piece, w, h, &fb, Rect::new(0, 0, w, h));
            assert_eq!(whole, piece, "{w}x{h} window");
        }
    }

    #[test]
    fn blit_rect_pads_exactly_where_blit_pads() {
        // The window is wider and taller than the remote screen, and the
        // rectangle straddles both edges. `Framebuffer::row` panics past its
        // end, so getting this wrong is a crash rather than an artefact.
        let fb = ramp(4, 3);
        let (w, h) = (8u32, 6u32);
        let mut whole = vec![0xDEADu32; (w * h) as usize];
        blit(&mut whole, w, h, &fb);
        for rect in [
            Rect::new(2, 1, 4, 4),   // straddles the right and bottom edges
            Rect::new(5, 0, 3, 6),   // entirely past the right edge
            Rect::new(0, 4, 8, 2),   // entirely below the bottom edge
            Rect::new(0, 0, 8, 6),   // the lot
            Rect::new(3, 2, 100, 9), // running off the window, to be clipped
        ] {
            let mut piece = vec![0xDEADu32; (w * h) as usize];
            blit_rect(&mut piece, w, h, &fb, Rect::new(0, 0, w, h));
            // Scribble over the rectangle, then repaint just it: the result
            // must be indistinguishable from the full blit.
            let clipped = rect.intersect(&Rect::new(0, 0, w, h));
            for y in clipped.y..clipped.bottom() {
                for x in clipped.x..clipped.right() {
                    piece[(y * w + x) as usize] = 0xBADD;
                }
            }
            blit_rect(&mut piece, w, h, &fb, rect);
            assert_eq!(whole, piece, "{rect}");
        }
    }

    #[test]
    fn blit_rect_touches_nothing_outside_itself() {
        let fb = ramp(6, 6);
        let (w, h) = (6u32, 6u32);
        let mut buf = vec![0xBADDu32; (w * h) as usize];
        blit_rect(&mut buf, w, h, &fb, Rect::new(2, 2, 2, 2));
        for y in 0..h {
            for x in 0..w {
                let inside = (2..4).contains(&x) && (2..4).contains(&y);
                let got = buf[(y * w + x) as usize];
                if inside {
                    assert_eq!(got, fb.get(x, y), "({x},{y})");
                } else {
                    assert_eq!(got, 0xBADD, "({x},{y}) was overwritten");
                }
            }
        }
        // An empty rectangle, and one wholly off the window, do nothing.
        let before = buf.clone();
        blit_rect(&mut buf, w, h, &fb, Rect::default());
        blit_rect(&mut buf, w, h, &fb, Rect::new(20, 20, 4, 4));
        assert_eq!(buf, before);
    }

    #[test]
    fn buffer_age_names_the_frames_the_buffer_missed() {
        let log = vec![
            vec![Rect::new(1, 1, 2, 2)],
            vec![Rect::new(5, 5, 3, 3)],
            vec![Rect::new(9, 9, 4, 4)],
        ];
        // A fresh buffer holds nothing we can reason about.
        assert_eq!(stale_regions(0, &log), None);
        // Age 1 is last frame's buffer: last frame's damage is already in it.
        assert_eq!(stale_regions(1, &log), Some(vec![]));
        assert_eq!(stale_regions(2, &log), Some(vec![Rect::new(1, 1, 2, 2)]));
        assert_eq!(
            stale_regions(3, &log),
            Some(vec![Rect::new(1, 1, 2, 2), Rect::new(5, 5, 3, 3)])
        );
        // Deeper than the log: nothing is known, so everything is repainted.
        assert_eq!(stale_regions(5, &log), None);
        // And an empty log is exactly the statement that nothing is known --
        // it is what a full redraw leaves behind.
        assert_eq!(stale_regions(1, &[]), None);
        assert_eq!(stale_regions(2, &[]), None);
    }

    #[test]
    fn a_double_buffered_backend_settles_back_to_the_real_damage() {
        // softbuffer's Wayland backend settles at a buffer age of 2 for the
        // life of the surface: every frame is handed the buffer from two
        // frames back and has to repaint what the frame in between changed.
        // Remember what was *painted* instead of what changed and that is a
        // fixed point -- the full redraws at start-up put the whole window in
        // the log, the next frame repaints the whole window because of it and
        // logs that, and the window is blitted and presented in full for the
        // rest of the session. This walks the loop the way `redraw` does.
        let win = Rect::new(0, 0, 100, 100);
        let d = Rect::new(10, 10, 4, 4);
        let mut log: VecDeque<Vec<Rect>> = VecDeque::new();
        let mut painted: Vec<Vec<Rect>> = Vec::new();
        for frame in 0..6 {
            // Age 0 on the first frame (nothing has been presented yet), 2
            // from then on, which is what the backend actually reports.
            let age = if frame == 0 { 0 } else { 2 };
            let snapshot: Vec<Vec<Rect>> = log.iter().cloned().collect();
            let stale = stale_regions(age, &snapshot);
            let full = stale.is_none();
            let (paint, changed) = present_plan(win, full, &[d], &stale.unwrap_or_default());
            painted.push(paint);
            if full {
                log.clear();
            }
            log.push_front(changed);
            while log.len() > PRESENT_LOG_DEPTH {
                log.pop_back();
            }
        }
        assert_eq!(painted[0], vec![win], "a fresh buffer has to be filled");
        // The only frame between this buffer and the one it holds was the full
        // one, so this frame genuinely does owe the whole window.
        assert_eq!(painted[1], vec![win]);
        for (i, p) in painted.iter().enumerate().skip(2) {
            assert_eq!(p, &vec![d], "frame {i} never stopped repainting the lot");
        }
    }

    #[test]
    fn blit_clips_and_pads() {
        let mut fb = Framebuffer::new(3, 2);
        fb.fill(&fb.bounds(), 0x123456);
        let mut dst = vec![0xFFu32; 4 * 3];
        blit(&mut dst, 4, 3, &fb);
        assert_eq!(dst[0], 0x123456);
        assert_eq!(dst[2], 0x123456);
        assert_eq!(dst[3], 0);
        assert_eq!(dst[8], 0);
        let mut small = vec![0u32; 2];
        blit(&mut small, 2, 1, &fb);
        assert_eq!(small, vec![0x123456, 0x123456]);
    }

    #[test]
    fn a_trackpads_small_deltas_add_up_instead_of_vanishing() {
        // The bug this replaces: (3.0 / 40.0).round() == 0, for every event
        // forever, so a macOS trackpad could not scroll at all.
        let mut c = ScrollCarry::default();
        let mut sent = 0i32;
        for _ in 0..8 {
            let (dx, dy) = scroll_units(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                0.0, -3.0,
            )));
            let (sx, sy) = c.feed(dx, dy);
            assert_eq!(sx, 0);
            sent += i32::from(sy);
        }
        // Eight three-pixel events are 24 px, which is exactly one detent.
        assert_eq!(sent, 1);
    }

    #[test]
    fn a_detent_is_never_rounded_up() {
        // Truncate-and-carry, not round: 23 px is not yet a detent, and the
        // 23 px are still owed rather than discarded.
        let mut c = ScrollCarry::default();
        let (dx, dy) = scroll_units(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0, -23.0,
        )));
        assert_eq!(c.feed(dx, dy), (0, 0));
        let (dx, dy) = scroll_units(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0, -1.0,
        )));
        assert_eq!(c.feed(dx, dy), (0, 1));
    }

    #[test]
    fn reversing_direction_drops_the_carry() {
        // Otherwise a flick back up would first have to repay the fraction
        // the downward flick left behind, and the first event of the new
        // gesture would do nothing.
        let mut c = ScrollCarry::default();
        assert_eq!(c.feed(0.0, 0.9), (0, 0));
        assert_eq!(c.feed(0.0, -1.0), (0, -1));
    }

    #[test]
    fn a_line_delta_and_a_pixel_delta_scroll_the_same_way() {
        // Winit\'s y grows as the content moves up; the protocol\'s grows down.
        // Both arms must flip it, which is what the old code got right for
        // lines and the new code has to keep right for both.
        let (_, up_lines) = scroll_units(MouseScrollDelta::LineDelta(0.0, 1.0));
        let (_, up_pixels) = scroll_units(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0, 48.0,
        )));
        assert!(up_lines < 0.0 && up_pixels < 0.0);
        assert_eq!(up_pixels, -2.0);
        let (dx, _) = scroll_units(MouseScrollDelta::LineDelta(1.0, 0.0));
        assert_eq!(dx, 1.0);
    }

    #[test]
    fn a_wild_delta_cannot_overflow_the_wire_type() {
        let mut c = ScrollCarry::default();
        let (sx, sy) = c.feed(1e12, -1e12);
        assert_eq!((sx, sy), (i16::MAX, -i16::MAX));
        assert_eq!(c.feed(f32::NAN, f32::INFINITY), (0, 0));
    }

    #[test]
    fn focus_loss_forgets_the_remainder() {
        let mut c = ScrollCarry::default();
        c.feed(0.0, 0.9);
        c.reset();
        assert_eq!(c.feed(0.0, 0.5), (0, 0));
    }

    #[test]
    fn damage_names_only_what_moved() {
        let win = Rect::new(0, 0, 800, 600);
        let got = damage_regions(
            win,
            false,
            &[
                Rect::new(10, 10, 20, 20),
                Rect::default(),
                Rect::new(700, 500, 32, 32),
            ],
        );
        assert_eq!(
            got,
            vec![Rect::new(10, 10, 20, 20), Rect::new(700, 500, 32, 32)]
        );
    }

    #[test]
    fn damage_is_clipped_and_deduplicated() {
        let win = Rect::new(0, 0, 800, 600);
        // A rectangle running off the edge is clipped, not refused: a cursor
        // half off the window still has to be presented.
        let got = damage_regions(win, false, &[Rect::new(790, 590, 32, 32)]);
        assert_eq!(got, vec![Rect::new(790, 590, 10, 10)]);
        // A part covering another collapses the list rather than uploading
        // the same pixels twice.
        let got = damage_regions(
            win,
            false,
            &[Rect::new(10, 10, 4, 4), win, Rect::new(0, 0, 8, 8)],
        );
        assert_eq!(got, vec![win]);
    }

    #[test]
    fn nothing_to_present_still_presents_everything() {
        // A redraw we did not ask for is an expose: our own state says nothing
        // changed, but the pixels still have to go out.
        let win = Rect::new(0, 0, 800, 600);
        assert_eq!(damage_regions(win, false, &[]), vec![win]);
        assert_eq!(damage_regions(win, false, &[Rect::default()]), vec![win]);
        assert_eq!(
            damage_regions(win, true, &[Rect::new(1, 1, 2, 2)]),
            vec![win]
        );
    }

    #[test]
    fn a_cursor_rectangle_covers_every_pixel_the_cursor_draws() {
        let cur = CursorImage {
            width: 16,
            height: 16,
            hot_x: 4,
            hot_y: 4,
            argb: vec![0xFF00_0000; 256],
        };
        assert_eq!(cursor_bounds(&cur, 100, 100), Rect::new(96, 96, 16, 16));
        // Clamped at the top-left corner: the visible part only.
        assert_eq!(cursor_bounds(&cur, 1, 2), Rect::new(0, 0, 13, 14));
        // Wholly off the top-left: nothing to present.
        assert!(
            cursor_bounds(&cur, 0, 0)
                .intersect(&Rect::new(0, 0, 800, 600))
                .area()
                <= 144
        );
    }

    #[test]
    fn the_layout_has_the_last_word_on_which_letter_is_which() {
        use winit::keyboard::SmolStr;
        let ctrl_alt = ModifiersState::CONTROL | ModifiersState::ALT;
        let ch = |c: &str| Key::Character(SmolStr::new(c));

        // The ordinary case: the layout says `b`, and it sits on the physical
        // B key.
        assert_eq!(
            accelerator_for(ctrl_alt, &ch("b"), PhysicalKey::Code(KeyCode::KeyB)),
            Some(Accelerator::Pin)
        );
        // macOS applies Option first, so the letter never arrives; the
        // physical code is all there is left to go on.
        assert_eq!(
            accelerator_for(ctrl_alt, &ch("\u{222b}"), PhysicalKey::Code(KeyCode::KeyB)),
            Some(Accelerator::Pin)
        );
        assert_eq!(
            accelerator_for(ctrl_alt, &ch("\u{153}"), PhysicalKey::Code(KeyCode::KeyQ)),
            Some(Accelerator::Disconnect)
        );
        // Dvorak: the key labelled B is physically N, and it still pins...
        assert_eq!(
            accelerator_for(ctrl_alt, &ch("b"), PhysicalKey::Code(KeyCode::KeyN)),
            Some(Accelerator::Pin)
        );
        // ...while the physical B key types `x` there, and Ctrl+Alt+X belongs
        // to the session. Falling through to the physical code here would
        // swallow it, which is the whole reason for the guard.
        assert_eq!(
            accelerator_for(ctrl_alt, &ch("x"), PhysicalKey::Code(KeyCode::KeyB)),
            None
        );
        // The named keys do not depend on the layout at all.
        assert_eq!(
            accelerator_for(
                ctrl_alt,
                &Key::Named(NamedKey::End),
                PhysicalKey::Code(KeyCode::End)
            ),
            Some(Accelerator::SecureAttention)
        );
        // And nothing at all fires without both modifiers.
        for m in [
            ModifiersState::empty(),
            ModifiersState::CONTROL,
            ModifiersState::ALT,
        ] {
            assert_eq!(
                accelerator_for(
                    m,
                    &Key::Named(NamedKey::Enter),
                    PhysicalKey::Code(KeyCode::Enter)
                ),
                None
            );
        }
    }

    #[test]
    fn every_pointer_button_gets_its_own_bit() {
        // Sharing a bit would mean releasing Back cleared Forward's flag, and
        // the bar would come up in the middle of a two-button drag.
        let buttons = [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
            MouseButton::Back,
            MouseButton::Forward,
        ];
        let mut seen = 0u8;
        for b in buttons {
            let (_, bit) = button_codes(b).expect("a named button has a code");
            assert_eq!(bit.count_ones(), 1, "{b:?} is not a single bit");
            assert_eq!(seen & bit, 0, "{b:?} shares a bit with an earlier button");
            seen |= bit;
        }
        assert!(button_codes(MouseButton::Other(9)).is_none());
    }

    #[test]
    fn the_bar_leaves_no_trace_in_the_framebuffer() {
        // The single correctness point of the overlay: it is composited into
        // the presented buffer, so the next frame\'s blit erases it whole. If
        // it were ever drawn into the decoded framebuffer instead, the
        // server\'s next incremental frame would diff against pixels it never
        // sent and the smear would be permanent.
        let mut fb = Framebuffer::new(200, 80);
        fb.fill(&fb.bounds(), 0x0033_6699);
        let before = fb.clone();

        let (w, h) = (200u32, 80u32);
        let mut clean = vec![0u32; (w * h) as usize];
        blit(&mut clean, w, h, &fb);

        let mut buf = vec![0u32; (w * h) as usize];
        blit(&mut buf, w, h, &fb);
        let status = crate::overlay::Status::new("a@b", (200, 80), None);
        crate::overlay::paint(
            &mut buf,
            w,
            h,
            &crate::overlay::bar_layout(w, 2, &status),
            None,
            None,
        );
        assert_ne!(buf, clean, "the bar should have drawn something");
        assert_eq!(fb, before, "the framebuffer must be untouched");

        blit(&mut buf, w, h, &fb);
        assert_eq!(buf, clean, "the next blit must erase every trace of it");
    }

    fn entries(paths: &[&str]) -> Vec<lynxrdp_proto::FileEntry> {
        paths
            .iter()
            .map(|p| lynxrdp_proto::FileEntry {
                path: (*p).to_string(),
                size: 1,
            })
            .collect()
    }

    /// Issue every request the batch will give out, numbering the transfers
    /// 1, 2, 3... as a real connection would. Returns (id, destination).
    fn issue_all(batch: &mut ClipBatch, from: u64) -> Vec<(u64, PathBuf)> {
        let mut out = Vec::new();
        let mut next = from;
        while let Some((_, dest, slot)) = batch.next_request() {
            batch.requested(slot, next);
            out.push((next, dest));
            next += 1;
        }
        out
    }

    #[test]
    fn one_file_that_cannot_be_staged_no_longer_strands_the_paste() {
        // The defect this replaces: the batch was a count decremented only on
        // success, so a file that failed left it above zero forever. The user
        // pressed Ctrl+V, got their old clipboard back, and the warning went
        // to a terminal a windowed client does not have.
        let mut b = ClipBatch::new(
            PathBuf::from("/staging"),
            &entries(&["/home/a/notes.txt", "/home/a/what?.png"]),
        );
        let issued = issue_all(&mut b, 1);
        assert_eq!(issued.len(), 2);
        assert!(!b.done());
        assert!(b.resolve(issued[0].0, Some(issued[0].1.clone())));
        assert!(!b.done());
        // The second one failed -- a refused offer, or a local `File::create`
        // that could not make the name. It still resolves.
        assert!(b.resolve(issued[1].0, None));
        assert!(b.done());
        assert_eq!(b.into_files(), vec![issued[0].1.clone()]);
    }

    #[test]
    fn a_transfer_that_is_not_ours_is_ignored() {
        // Uploads and the tail of a superseded copy both arrive as ids this
        // batch has never heard of, and resolving one of those against a slot
        // would finish the batch early.
        let mut b = ClipBatch::new(PathBuf::from("/staging"), &entries(&["/a/one.txt"]));
        let issued = issue_all(&mut b, 10);
        assert!(!b.resolve(999, Some(PathBuf::from("/elsewhere"))));
        assert!(!b.done());
        assert!(b.resolve(issued[0].0, Some(issued[0].1.clone())));
        assert!(b.done());
    }

    #[test]
    fn two_files_with_one_name_get_two_files() {
        // Flattening to the basename is unavoidable -- a clipboard file list
        // is a list of names -- but flattening two of them onto one path is
        // not: the second `File::create` truncated the first and the paste
        // delivered one file where the user copied two, silently.
        let mut b = ClipBatch::new(
            PathBuf::from("/staging"),
            &entries(&["/a/notes.txt", "/b/notes.txt", "/c/NOTES.TXT", "/d/notes"]),
        );
        let dests: Vec<PathBuf> = issue_all(&mut b, 1).into_iter().map(|(_, d)| d).collect();
        let mut seen: Vec<String> = dests
            .iter()
            .map(|d| d.file_name().unwrap().to_string_lossy().to_lowercase())
            .collect();
        let before = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            before,
            "two files landed on one name: {dests:?}"
        );
        assert_eq!(dests[0].file_name().unwrap().to_string_lossy(), "notes.txt");
        assert_eq!(
            dests[1].file_name().unwrap().to_string_lossy(),
            "notes (2).txt"
        );
    }

    #[test]
    fn a_batch_keeps_only_a_few_files_open_at_a_time() {
        // A session may copy up to MAX_FILE_LIST files, and each accepted
        // offer creates a file this side holds open until the transfer ends.
        // macOS inherits an RLIMIT_NOFILE of 256 from launchd.
        let paths: Vec<String> = (0..64).map(|i| format!("/a/f{i}.txt")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut b = ClipBatch::new(PathBuf::from("/staging"), &entries(&refs));
        let first = issue_all(&mut b, 1);
        assert_eq!(first.len(), MAX_CONCURRENT_CLIPBOARD_FILES);
        // A slot only frees when a transfer resolves.
        assert!(b.next_request().is_none());
        b.resolve(first[0].0, Some(first[0].1.clone()));
        assert!(b.next_request().is_some());
    }

    #[test]
    fn the_published_order_is_the_order_the_session_copied_in() {
        // A file manager shows a paste in the order it is handed, and files
        // finish in whatever order the network delivers them.
        let mut b = ClipBatch::new(
            PathBuf::from("/staging"),
            &entries(&["/a/one.txt", "/a/two.txt", "/a/three.txt"]),
        );
        let issued = issue_all(&mut b, 1);
        for i in [2usize, 0, 1] {
            b.resolve(issued[i].0, Some(issued[i].1.clone()));
        }
        assert!(b.done());
        assert_eq!(
            b.into_files(),
            vec![
                issued[0].1.clone(),
                issued[1].1.clone(),
                issued[2].1.clone()
            ]
        );
    }

    #[test]
    fn names_the_session_allows_and_a_filesystem_does_not() {
        // Every one of these is an ordinary name in an X11 session and none of
        // them can be created on NTFS. Sanitising happens on all platforms so
        // that the deduplication above sees the same collisions everywhere.
        for (raw, want) in [
            ("notes.txt", "notes.txt"),
            ("a:b.txt", "a_b.txt"),
            ("what?.png", "what_.png"),
            ("re<port>.txt", "re_port_.txt"),
            ("pipe|it", "pipe_it"),
            ("star*", "star_"),
            ("quote\"d", "quote_d"),
            ("line\nbreak", "line_break"),
            ("report.", "report"),
            ("report...", "report"),
            ("trailing  ", "trailing"),
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("LPT9.log", "_LPT9.log"),
            ("nul", "_nul"),
            // Not reserved: the device names are exact, not prefixes.
            ("console.log", "console.log"),
            (".", "file"),
            ("..", "file"),
            ("", "file"),
            ("   ", "file"),
            // Separators cannot survive: the name is joined onto the staging
            // directory and must not be able to point anywhere else.
            ("a/b.txt", "a_b.txt"),
            ("a\\b.txt", "a_b.txt"),
            ("../../.ssh/authorized_keys", ".._.._.ssh_authorized_keys"),
        ] {
            assert_eq!(safe_file_name(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn a_staged_name_is_always_a_single_safe_component() {
        // The property, rather than the table: whatever the session says, the
        // result is one component the session's own traversal check accepts.
        for raw in [
            "notes.txt",
            "../..",
            "/etc/passwd",
            "C:\\Windows\\System32",
            "\u{0}embedded",
            "CON",
            &"x".repeat(4000),
            &format!("{}.tar.gz", "y".repeat(4000)),
            &"\u{e9}".repeat(500),
        ] {
            let got = safe_file_name(raw);
            assert!(!got.is_empty(), "{raw:?} produced nothing");
            assert!(!got.contains(['/', '\\', '\0']), "{raw:?} produced {got:?}");
            assert!(got != "." && got != "..", "{raw:?} produced {got:?}");
            assert!(
                got.len() <= MAX_STAGED_NAME + 1,
                "{raw:?} is {} bytes",
                got.len()
            );
            assert_eq!(
                lynxrdp_proto::transfer::safe_relative_path(&got).as_deref(),
                Some(got.as_str()),
                "{raw:?} produced {got:?}, which the traversal check changes"
            );
        }
        // A long name keeps its extension, which is what a file manager uses
        // to decide what the pasted file is.
        assert!(safe_file_name(&format!("{}.pdf", "z".repeat(4000))).ends_with(".pdf"));
    }

    #[test]
    fn unique_name_disambiguates_case_insensitively() {
        // NTFS and a default APFS volume both resolve Notes.txt and notes.txt
        // to the same file, so a case-sensitive check would miss the
        // collision on exactly the platforms where it matters.
        let mut taken = HashSet::new();
        assert_eq!(unique_name(&mut taken, "notes.txt"), "notes.txt");
        assert_eq!(unique_name(&mut taken, "NOTES.TXT"), "NOTES (2).TXT");
        assert_eq!(unique_name(&mut taken, "notes.txt"), "notes (3).txt");
        assert_eq!(unique_name(&mut taken, "README"), "README");
        assert_eq!(unique_name(&mut taken, "README"), "README (2)");
        assert_eq!(unique_name(&mut taken, "a.tar.gz"), "a.tar.gz");
        assert_eq!(unique_name(&mut taken, "a.tar.gz"), "a.tar (2).gz");
    }

    #[test]
    fn each_copy_gets_a_directory_of_its_own() {
        // Two copies of `notes.txt` must not share a path, and a second copy
        // must not overwrite bytes a still-running paste of the first is
        // reading. One directory per copy is what buys both.
        let root = tempfile::tempdir().unwrap();
        let mut next = 0u64;
        let a = new_batch_dir(root.path(), &mut next).unwrap();
        let b = new_batch_dir(root.path(), &mut next).unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        // A directory left by a dead process that had this pid is never
        // adopted: its files would be mixed into this copy.
        let mut reused = 0u64;
        let c = new_batch_dir(root.path(), &mut reused).unwrap();
        assert_ne!(c, a);
        assert_ne!(c, b);
    }

    #[test]
    fn pruning_keeps_the_newest_and_leaves_strangers_alone() {
        let root = tempfile::tempdir().unwrap();
        let mut next = 0u64;
        let dirs: Vec<PathBuf> = (0..6)
            .map(|_| new_batch_dir(root.path(), &mut next).unwrap())
            .collect();
        for d in &dirs {
            std::fs::write(d.join("f.txt"), b"x").unwrap();
        }
        // Something we did not create. This runs against a directory in the
        // system temporary area; deleting what it does not recognise is not
        // its business.
        let stranger = root.path().join("not-a-batch");
        std::fs::create_dir(&stranger).unwrap();
        std::fs::write(root.path().join("batch-notanumber"), b"x").unwrap();

        prune_batches(root.path(), 2);
        assert!(!dirs[0].exists() && !dirs[3].exists());
        assert!(dirs[4].exists() && dirs[5].exists());
        assert!(stranger.exists());
        assert!(root.path().join("batch-notanumber").exists());
        // Pruning an absent directory is not an error.
        prune_batches(&root.path().join("missing"), 2);
    }

    // ---------------------------------------------- magnification (B3)

    /// What a magnified window must hold, pixel for pixel: every window pixel
    /// is the remote pixel under it at this factor, and black past the edge.
    fn reference_scaled(dst_w: u32, dst_h: u32, fb: &Framebuffer, s: u32) -> Vec<u32> {
        let mut out = vec![0u32; (dst_w * dst_h) as usize];
        for y in 0..dst_h {
            for x in 0..dst_w {
                let (sx, sy) = (x / s, y / s);
                out[(y * dst_w + x) as usize] = if sx < fb.width() && sy < fb.height() {
                    fb.get(sx, sy)
                } else {
                    0
                };
            }
        }
        out
    }

    #[test]
    fn magnifying_by_one_is_the_unmagnified_blit() {
        // The scaled path is the only one `redraw` calls now, so at scale 1 it
        // has to be indistinguishable from the code it replaced -- which is
        // itself tested against the full blit above.
        let fb = ramp(7, 5);
        let mut row = Vec::new();
        for (w, h) in [(7u32, 5u32), (10, 8), (4, 3)] {
            for rect in [
                Rect::new(0, 0, w, h),
                Rect::new(2, 1, 4, 4),
                Rect::new(5, 0, 3, 6),
                Rect::new(3, 2, 100, 9),
                Rect::default(),
            ] {
                let mut a = vec![0xDEADu32; (w * h) as usize];
                let mut b = a.clone();
                blit_rect(&mut a, w, h, &fb, rect);
                blit_rect_scaled(&mut b, w, h, &fb, rect, 1, &mut row);
                assert_eq!(a, b, "{w}x{h} {rect}");
            }
        }
    }

    #[test]
    fn a_magnified_window_holds_whole_duplicated_pixels() {
        // Nearest neighbour at a whole factor: every output pixel is a pixel
        // the server sent, never a blend of two. That is the promise the top
        // of this file makes, and the reason the option is an integer.
        let fb = ramp(5, 3);
        let mut row = Vec::new();
        for s in 2..=4u32 {
            // A window both larger and smaller than the magnified screen, so
            // the padding and the clipping are both covered.
            for (w, h) in [
                (5 * s, 3 * s),
                (5 * s + 7, 3 * s + 5),
                (5 * s - 3, 3 * s - 1),
            ] {
                let want = reference_scaled(w, h, &fb, s);
                let mut got = vec![0xBADDu32; (w * h) as usize];
                blit_rect_scaled(&mut got, w, h, &fb, Rect::new(0, 0, w, h), s, &mut row);
                assert_eq!(got, want, "scale {s}, window {w}x{h}");
            }
        }
    }

    #[test]
    fn a_damage_rectangle_repaints_exactly_its_own_pixels() {
        // The half of the mapping that is easy to get wrong and invisible in a
        // screenshot: a rectangle that is not on a multiple of the scale, and
        // one that straddles the edge of the remote screen. Painting the whole
        // window and painting it rectangle by rectangle must agree, or a
        // damaged present leaves a stale band behind.
        let fb = ramp(5, 3);
        let mut row = Vec::new();
        let s = 3u32;
        let (w, h) = (5 * s + 4, 3 * s + 4);
        let want = reference_scaled(w, h, &fb, s);
        for rect in [
            Rect::new(0, 0, w, h),
            Rect::new(1, 1, 4, 4),
            Rect::new(2, 2, s, s),
            Rect::new(s * 4, 0, s * 3, h),
            Rect::new(0, s * 2 + 1, w, s + 1),
            Rect::new(w - 2, h - 2, 50, 50),
        ] {
            let mut got = want.clone();
            let clipped = rect.intersect(&Rect::new(0, 0, w, h));
            for y in clipped.y..clipped.bottom() {
                for x in clipped.x..clipped.right() {
                    got[(y * w + x) as usize] = 0xBADD;
                }
            }
            blit_rect_scaled(&mut got, w, h, &fb, rect, s, &mut row);
            assert_eq!(got, want, "{rect} at scale {s}");
        }
    }

    #[test]
    fn a_magnified_blit_touches_nothing_outside_itself() {
        let fb = ramp(6, 6);
        let s = 2u32;
        let (w, h) = (12u32, 12u32);
        let mut row = Vec::new();
        let mut buf = vec![0xBADDu32; (w * h) as usize];
        let rect = Rect::new(3, 5, 4, 3);
        blit_rect_scaled(&mut buf, w, h, &fb, rect, s, &mut row);
        for y in 0..h {
            for x in 0..w {
                let got = buf[(y * w + x) as usize];
                if rect.contains(&Rect::new(x, y, 1, 1)) {
                    assert_eq!(got, fb.get(x / s, y / s), "({x},{y})");
                } else {
                    assert_eq!(got, 0xBADD, "({x},{y}) was overwritten");
                }
            }
        }
    }

    #[test]
    fn remote_damage_becomes_window_damage() {
        assert_eq!(scale_rect(Rect::new(3, 4, 5, 6), 1), Rect::new(3, 4, 5, 6));
        assert_eq!(
            scale_rect(Rect::new(3, 4, 5, 6), 2),
            Rect::new(6, 8, 10, 12)
        );
        // A scale of zero is not a thing, and must not turn damage into
        // nothing at all.
        assert_eq!(scale_rect(Rect::new(3, 4, 5, 6), 0), Rect::new(3, 4, 5, 6));
    }

    #[test]
    fn a_magnified_cursor_stays_inside_the_rectangle_that_presents_it() {
        // The pixels the pointer leaves behind are presented by naming this
        // rectangle, so anything drawn outside it stays on screen until
        // something else happens to repaint that part of the window.
        let cur = CursorImage {
            width: 3,
            height: 2,
            hot_x: 1,
            hot_y: 1,
            argb: vec![0xFF00_FF00; 6],
        };
        let (w, h) = (40u32, 30u32);
        for s in 1..=4u32 {
            for (px, py) in [(20, 15), (0, 0), (1, 1), (39, 29), (2, 2)] {
                let mut buf = vec![0u32; (w * h) as usize];
                draw_cursor_scaled(&mut buf, w, h, &cur, px, py, s);
                let bounds =
                    cursor_bounds_scaled(&cur, px, py, s).intersect(&Rect::new(0, 0, w, h));
                let mut painted = 0u32;
                for y in 0..h {
                    for x in 0..w {
                        if buf[(y * w + x) as usize] != 0 {
                            painted += 1;
                            assert!(
                                bounds.contains(&Rect::new(x, y, 1, 1)),
                                "({x},{y}) painted outside {bounds} at scale {s}"
                            );
                        }
                    }
                }
                // And tight, when the whole thing is on screen: a rectangle
                // that is too large is a cost, one that is too small is an
                // artefact, and only the exact one is neither.
                if bounds.area() == u64::from(3 * 2 * s * s) {
                    assert_eq!(painted, 3 * 2 * s * s, "at scale {s}");
                }
            }
        }
    }

    #[test]
    fn the_magnification_follows_the_display_and_stays_whole() {
        // A 2x screen gets 2, which is the whole point: the window is sized in
        // physical pixels against a fixed 96 DPI server, so 1:1 there is a
        // window half the size the desktop should be.
        assert_eq!(display_scale(1.0), 1);
        assert_eq!(display_scale(2.0), 2);
        assert_eq!(display_scale(3.0), 3);
        // Fractional factors round rather than resampling.
        assert_eq!(display_scale(1.25), 1);
        assert_eq!(display_scale(1.5), 2);
        assert_eq!(display_scale(2.75), 3);
        // Clamped at both ends, and a nonsense factor is not a crash.
        assert_eq!(display_scale(0.5), 1);
        assert_eq!(display_scale(0.0), 1);
        assert_eq!(display_scale(99.0), u32::from(MAX_SCALE));
        // A factor that is not a number at all falls back to 1:1 rather than
        // to the largest window the machine cannot draw.
        assert_eq!(display_scale(f64::NAN), 1);
        assert_eq!(display_scale(f64::INFINITY), 1);
    }

    #[test]
    fn a_window_size_divides_by_the_magnification() {
        // What the resize path asks the session for. A size that does not
        // divide is one the window cannot show without scaling something, and
        // the session would be serving pixels nobody can present.
        for (w, h, s, want) in [
            (1920u32, 1080u32, 1u32, (1920u32, 1080u32)),
            (1920, 1080, 2, (960, 540)),
            // The awkward sizes: an odd window at 2x, and one that divides
            // into nothing at all.
            (1921, 1081, 2, (960, 540)),
            (100, 60, 3, (33, 20)),
            (10, 10, 4, (2, 2)),
            // A scale of zero is not a thing, and must not divide by it.
            (800, 600, 0, (800, 600)),
        ] {
            let got = remote_size_for(PhysicalSize::new(w, h), s);
            assert_eq!(got, want, "{w}x{h} at scale {s}");
            assert!(got.0 * s.max(1) <= w && got.1 * s.max(1) <= h);
        }
    }

    // ------------------------------------------------ a dropped link (B1)

    /// A server that completes the handshake `connections` times over and
    /// records everything each client sends afterwards.
    ///
    /// Two connections is the whole of the reconnection story from the far
    /// end's point of view, and it is what the real server does: the second
    /// one replaces the first on a session that never stopped running.
    fn fake_session(connections: usize) -> (SocketAddr, Arc<std::sync::Mutex<Vec<Message>>>) {
        use lynxrdp_proto::frame::{read_message, write_message};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let theirs = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().take(connections) {
                let Ok(mut s) = conn else { break };
                if read_message(&mut s).is_err() {
                    break;
                }
                let hello = Message::ServerHello {
                    version: lynxrdp_proto::PROTOCOL_VERSION,
                    server_name: "fake".into(),
                    features: features::LOCAL_CURSOR,
                    session_id: 7,
                    username: "bob".into(),
                    width: 64,
                    height: 48,
                };
                if write_message(&mut s, &hello).is_err() {
                    break;
                }
                while let Ok(m) = read_message(&mut s) {
                    theirs.lock().unwrap().push(m);
                }
            }
        });
        (addr, seen)
    }

    /// A server that holds one session and refuses everyone after it: a
    /// policy that changed, or a version floor, seen from the client side.
    fn fake_session_then_rejecting(code: u16, reason: &str) -> SocketAddr {
        use lynxrdp_proto::frame::{read_message, write_message};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reason = reason.to_string();
        std::thread::spawn(move || {
            let mut first = true;
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { break };
                if read_message(&mut s).is_err() {
                    break;
                }
                let reply = if std::mem::take(&mut first) {
                    Message::ServerHello {
                        version: lynxrdp_proto::PROTOCOL_VERSION,
                        server_name: "fake".into(),
                        features: 0,
                        session_id: 7,
                        username: "bob".into(),
                        width: 64,
                        height: 48,
                    }
                } else {
                    Message::Rejected {
                        code,
                        reason: reason.clone(),
                    }
                };
                if write_message(&mut s, &reply).is_err() {
                    break;
                }
                // Held open until the client lets go, so the first session
                // ends when this side is told to end it and not before.
                while read_message(&mut s).is_ok() {}
            }
        });
        addr
    }

    /// A window's state without a window: `App::new` opens nothing, so every
    /// path below that touches `gfx` is a no-op and the state machine is what
    /// is left.
    fn test_app(addr: SocketAddr, endpoint: Option<Endpoint>) -> App {
        let connect = ConnectOptions::default();
        let client = Client::connect(addr, &connect, None).expect("handshake");
        App::new(
            client,
            AppOptions {
                fullscreen: false,
                title: "LynxRDP".into(),
                dynamic_resize: false,
                // Not the system clipboard: these tests have no business
                // reading or writing the one the developer is using.
                clipboard: false,
                scale: Some(1),
            },
            Session {
                endpoint,
                connect,
                waker: make_waker().1,
            },
        )
    }

    /// A loopback address with nothing behind it: every connection is refused
    /// at once, which is what a tunnel that is up and a session that is not
    /// looks like.
    fn refusing_addr() -> SocketAddr {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        addr
    }

    #[test]
    fn a_dropped_link_keeps_the_window_and_says_what_happened() {
        // The bug this replaces: any `Disconnected` left the event loop, so a
        // laptop that slept for a minute closed the window on a desktop that
        // was still running on the server, untouched.
        let (addr, _seen) = fake_session(1);
        let mut app = test_app(addr, Some(Endpoint::direct(addr).unwrap()));
        assert!(app.link_up());
        app.on_link_lost("connection closed".into());

        assert!(!app.link_up());
        assert!(
            app.exit_reason.is_none(),
            "the window must outlive the link"
        );
        // The last frame is still there to dim.
        assert_eq!(app.client.size(), (64, 48));
        let notice = app.link_notice(Instant::now()).expect("a notice");
        assert!(
            notice.detail.contains("connection closed"),
            "the reason that arrived has to be the reason shown: {notice:?}"
        );
        // And the bar is told the real state rather than our ping bookkeeping.
        assert!(app.overlay_status(Instant::now()).stalled.is_some());
        assert!(!app.draws_own_cursor(), "the pointer is the user's again");
    }

    #[test]
    fn a_fatal_reason_is_not_retried_by_hand_either() {
        let (addr, _seen) = fake_session(1);
        let mut app = test_app(addr, Some(Endpoint::direct(addr).unwrap()));
        app.on_link_lost("Another client connected to this session.".into());
        assert!(matches!(app.link, Link::Gone { .. }));
        assert!(app.attempt.is_none(), "nothing may be scheduled");
        // Even asked directly. Two devices taking turns at evicting each other
        // is worse than the disconnection they are each recovering from.
        app.reconnect_now();
        assert!(app.attempt.is_none());
        assert!(matches!(app.link, Link::Gone { .. }));
    }

    #[test]
    fn a_link_that_dies_takes_its_transfers_with_it() {
        // Transfer ids belong to the connection that issued them and a
        // reconnected client numbers its own from one again, so a completion
        // on the new link would retire an upload from the old one.
        let (addr, _seen) = fake_session(1);
        let mut app = test_app(addr, Some(Endpoint::direct(addr).unwrap()));
        app.uploads.push((1, "a.txt".into()));
        app.upload_queue
            .push_back((PathBuf::from("/tmp/b.txt"), "b.txt".into()));
        app.clipboard_batch = Some(ClipBatch::new(
            PathBuf::from("/staging"),
            &entries(&["/a/one.txt"]),
        ));
        app.last_clipboard = Some("stale".into());

        app.on_link_lost("connection closed".into());
        assert!(app.uploads.is_empty());
        assert!(app.upload_queue.is_empty());
        assert!(app.clipboard_batch.is_none());
        assert_eq!(app.last_clipboard, None);

        // And a drop onto a dimmed window queues nothing rather than failing
        // one file at a time.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("notes.txt");
        std::fs::write(&f, b"x").unwrap();
        app.on_dropped_file(&f);
        assert!(app.upload_queue.is_empty() && app.uploads.is_empty());
    }

    #[test]
    fn a_reconnection_takes_over_and_unsticks_the_keyboard() {
        // The whole of stage one, end to end: the link drops, the window
        // stays, a worker reconnects through the endpoint, and the session is
        // told to let go of what the old link left held. Without that last
        // part a Ctrl that was down when the network went is down for ever.
        let (addr, seen) = fake_session(2);
        let mut app = test_app(addr, Some(Endpoint::direct(addr).unwrap()));
        app.pressed_keys.push(keysym::CONTROL_L);
        app.remote_buttons = 1;
        app.on_link_lost("connection closed".into());
        assert!(matches!(app.link, Link::Reconnecting { .. }));
        // The mouse moved while the window was dark, which sends nothing --
        // so the session's pointer is somewhere else entirely until the
        // reconnection says where ours is.
        app.on_pointer_moved(PhysicalPosition::new(11.0, 7.0));
        assert_eq!(app.pointer, Some((11, 7)));

        // Drive the machine the way `about_to_wait` does, with a clock that
        // runs faster than the backoff.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut now = Instant::now();
        while !app.link_up() {
            assert!(
                Instant::now() < deadline,
                "never reconnected: {:?}",
                app.link
            );
            now += Duration::from_millis(500);
            app.poll_reconnect(now);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.attempts, 0, "the budget is restored by success");
        assert!(app.pressed_keys.is_empty());
        assert_eq!(app.remote_buttons, 0);
        assert!(
            app.full_redraw,
            "the new client's first frame is a whole one"
        );
        assert!(app.session.endpoint.is_some(), "the endpoint came back");

        // And the session was actually told, on the new link.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got = seen.lock().unwrap().clone();
            let released_key = got.iter().any(|m| {
                matches!(
                    m,
                    Message::KeyEvent {
                        keysym: k,
                        down: false
                    } if *k == keysym::CONTROL_L
                )
            });
            let released_button = got.iter().any(|m| {
                matches!(
                    m,
                    Message::PointerButton {
                        button: button::LEFT,
                        down: false
                    }
                )
            });
            // And where the pointer ended up, after the releases rather than
            // before: warping first would drag what the old link was holding.
            let placed = got
                .iter()
                .position(|m| matches!(m, Message::PointerMotion { x: 11, y: 7 }));
            if released_key && released_button {
                if let Some(at) = placed {
                    assert!(
                        got[..at]
                            .iter()
                            .any(|m| matches!(m, Message::PointerButton { down: false, .. })),
                        "the pointer was warped before the drag was let go: {got:?}"
                    );
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the reconnected session was never told to let go: {got:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_link_that_will_not_come_back_stops_trying_and_waits_to_be_asked() {
        // The budget: an automatic retry is only polite while there is reason
        // to think it will work. After that the window says what happened and
        // waits, rather than dialling a dead host until the battery runs out.
        let (addr, _seen) = fake_session(1);
        let mut app = test_app(addr, Some(Endpoint::direct(addr).unwrap()));
        app.session.endpoint = Some(Endpoint::direct(refusing_addr()).unwrap());
        app.on_link_lost("connection closed".into());

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut now = Instant::now();
        while !matches!(app.link, Link::Lost { .. }) {
            assert!(Instant::now() < deadline, "still trying: {:?}", app.link);
            now += Duration::from_secs(60);
            app.poll_reconnect(now);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(app.attempts, RETRY_BUDGET);
        assert!(app.attempt.is_none());
        let notice = app.link_notice(now).expect("a notice");
        assert_eq!(notice.headline, "Connection lost");
        assert!(notice.hint.contains("Ctrl+Alt+R"), "{}", notice.hint);
        // Asking by hand puts the budget back: the user may have just plugged
        // the network in.
        app.reconnect_now();
        assert_eq!(app.attempts, 1);
        assert!(matches!(app.link, Link::Reconnecting { .. }));
    }

    #[test]
    fn an_attempt_that_is_refused_stops_attempting() {
        // A reconnection can fail for a reason of its own, and a rejection is
        // the one that matters: repeating it achieves nothing and writes
        // another refusal into the server's log every time.
        let (addr, _seen) = fake_session(1);
        let mut app = test_app(addr, Some(Endpoint::direct(addr).unwrap()));
        app.on_link_lost("connection closed".into());
        assert!(matches!(app.link, Link::Reconnecting { .. }));
        app.attempt_failed(
            crate::connection::rejection_reason(2, "user not allowed"),
            Instant::now(),
        );
        assert!(
            matches!(app.link, Link::Gone { .. }),
            "{:?} would have been retried",
            app.link
        );
        assert!(app.link.reason().contains("not allowed"));
    }

    #[test]
    fn a_refusal_is_still_a_refusal_by_the_time_the_window_sees_it() {
        // The classifier reads the rejection back out of a string, and the
        // string it is handed is whatever the worker's error formats to --
        // not the one `rejection_reason` returned. One `.context()` added to
        // the connect chain in `start_attempt` would prefix it, `rejection_code`
        // would stop recognising it, and every policy refusal would quietly
        // become eight of them in the server's log. So this drives the real
        // worker against a server that really refuses, rather than calling
        // `attempt_failed` with a string composed by the test.
        let addr = fake_session_then_rejecting(3, "this user may not open a session");
        let mut app = test_app(addr, Some(Endpoint::direct(addr).unwrap()));
        app.on_link_lost("connection closed".into());
        assert!(matches!(app.link, Link::Reconnecting { .. }));

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut now = Instant::now();
        while matches!(app.link, Link::Reconnecting { .. }) {
            assert!(Instant::now() < deadline, "still trying: {:?}", app.link);
            now += Duration::from_secs(5);
            app.poll_reconnect(now);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(app.link, Link::Gone { .. }),
            "a refusal was retried: {:?}",
            app.link
        );
        assert_eq!(app.attempts, 1, "one attempt, not the whole budget");
        assert!(
            app.link.reason().contains("may not open a session"),
            "{}",
            app.link.reason()
        );
    }

    #[test]
    fn the_reconnect_accelerator_belongs_to_the_session_while_the_link_is_up() {
        // Ctrl+Alt+R is a shortcut inside plenty of desktop applications.
        // Swallowing it for the whole session to serve a command that does
        // nothing for all but a few seconds of it would be a bad trade, so it
        // is ours only while there is nothing to send it to.
        use winit::keyboard::SmolStr;
        let ctrl_alt = ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(
            accelerator_for(
                ctrl_alt,
                &Key::Character(SmolStr::new("r")),
                PhysicalKey::Code(KeyCode::KeyR)
            ),
            Some(Accelerator::Reconnect)
        );
        let (addr, _seen) = fake_session(1);
        let mut app = test_app(addr, Some(Endpoint::direct(addr).unwrap()));
        app.modifiers = ctrl_alt;
        let key = Key::Character(SmolStr::new("r"));
        let code = PhysicalKey::Code(KeyCode::KeyR);
        assert_eq!(app.accelerator(&key, code), None);
        app.on_link_lost("connection closed".into());
        assert_eq!(app.accelerator(&key, code), Some(Accelerator::Reconnect));
        // The other four are the window's whatever the link is doing.
        assert_eq!(
            app.accelerator(
                &Key::Named(NamedKey::Enter),
                PhysicalKey::Code(KeyCode::Enter)
            ),
            Some(Accelerator::Fullscreen)
        );
    }

    #[test]
    fn two_devices_must_not_ping_pong_a_session() {
        // The one classification that has to be right. The server replaces an
        // attached client on purpose; retrying that turns two machines into a
        // loop where each evicts the other for ever, which is worse than the
        // bug the reconnection exists to fix.
        assert_eq!(
            classify("Another client connected to this session."),
            Fate::Fatal
        );
        // Authentication and policy: the answer will be the same every time,
        // and every attempt is another refusal in the server's log.
        for code in [1u16, 2, 3, 4] {
            let text = crate::connection::rejection_reason(code, "no");
            assert_eq!(classify(&text), Fate::Fatal);
            // And still, once a `Result` chain has wrapped it in context on
            // the way up here, which is the form the reconnect worker's
            // failures actually arrive in.
            assert_eq!(classify(&format!("reconnecting: {text}")), Fate::Fatal);
        }
        assert_eq!(classify("protocol error: bad tile"), Fate::Fatal);
        assert_eq!(classify("The desktop session has ended."), Fate::Fatal);
        // And the ordinary ways a link dies, all of which recover.
        for retryable in [
            "connection closed",
            "reader stopped",
            "no response from the session for 30s",
            "The server is shutting down.",
            "Connection reset by peer (os error 54)",
            "",
        ] {
            assert_eq!(classify(retryable), Fate::Retry, "{retryable}");
        }
    }

    #[test]
    fn the_backoff_grows_and_then_stops_growing() {
        let mut last = Duration::ZERO;
        for spent in 0..RETRY_BUDGET {
            let d = retry_delay(spent);
            assert!(d >= last, "attempt {spent} waits less than the one before");
            assert!(d <= *RETRY_BACKOFF.last().unwrap());
            last = d;
        }
        // Past the table it repeats rather than running off.
        assert_eq!(retry_delay(u32::MAX), *RETRY_BACKOFF.last().unwrap());
        // The whole budget is spent in a bounded, human amount of time.
        let total: Duration = (0..RETRY_BUDGET).map(retry_delay).sum();
        assert!(total <= Duration::from_secs(180), "{total:?}");
    }

    // ------------------------------------------------------ the notice

    #[test]
    fn the_text_metric_matches_the_font_that_paints_it() {
        // `chars_fitting` is the bar's arithmetic written out a second time,
        // so it is checked against pixels the font actually painted rather
        // than against itself.
        for s in 1..=3u32 {
            for n in 1..=6u32 {
                let (w, h) = (6 * s * n, 16 * s);
                let mut buf = vec![0u32; (w * h) as usize];
                let text = "M".repeat(n as usize);
                draw_text_line(&mut buf, w, h, s, (0, 0), &text, 0x00FF_FFFF);
                let right = (0..w)
                    .rev()
                    .find(|&x| (0..h).any(|y| buf[(y * w + x) as usize] != 0))
                    .expect("the font drew nothing");
                // The advance the metric assumes: six columns per glyph, with
                // no gap after the last one.
                assert!(right < 6 * s * n - s, "{n} glyphs at scale {s} ran over");
                assert_eq!(chars_fitting(w, s), n as usize);
                assert_eq!(chars_fitting(6 * s * n - s, s), n as usize);
            }
        }
    }

    #[test]
    fn a_shortened_reason_says_that_it_was_shortened() {
        // A silently clipped reason is a different reason, and reads as if it
        // were the whole of what happened.
        let long = "no route to host after the network changed";
        let cut = fit_text(long, 6 * 10, 1);
        assert!(cut.ends_with(".."), "{cut}");
        assert!(cut.chars().count() <= 10, "{cut}");
        // What fits is left exactly alone.
        assert_eq!(fit_text("short", 6 * 20, 1), "short");
        // And a space too small for anything readable draws nothing rather
        // than two dots on their own.
        assert_eq!(fit_text(long, 6, 1), "");
    }

    #[test]
    fn the_notice_stays_in_its_own_strip() {
        // It is drawn over the last frame, and the bar owns the top edge: a
        // notice that wandered would either cover the remote screen or fight
        // the bar for the same pixels.
        let (w, h) = (300u32, 200u32);
        let mut buf = vec![0u32; (w * h) as usize];
        let n = Notice {
            headline: "Connection lost".into(),
            detail: "connection closed".into(),
            hint: "Ctrl+Alt+R",
            colour: overlay::colour::WARN,
        };
        let panel = draw_notice(&mut buf, w, h, 2, &n);
        assert_eq!(panel.bottom(), h, "the strip sits on the bottom edge");
        assert!(panel.height <= h);
        for y in 0..h {
            for x in 0..w {
                if buf[(y * w + x) as usize] != 0 {
                    assert!(
                        panel.contains(&Rect::new(x, y, 1, 1)),
                        "({x},{y}) is outside {panel}"
                    );
                }
            }
        }
        // A window shorter than the strip is covered rather than overrun.
        let mut small = vec![0u32; (w * 10) as usize];
        let panel = draw_notice(&mut small, w, 10, 2, &n);
        assert_eq!(panel, Rect::new(0, 0, w, 10));
    }

    #[test]
    fn dimming_leaves_the_last_frame_recognisable() {
        // The point of keeping the frame at all: what is on screen is what the
        // session still has, and it is the user's evidence that nothing was
        // lost. Dimmed to nothing would say the opposite.
        let mut buf = vec![0x00FF_FFFFu32; 4];
        blend_rect(
            &mut buf,
            2,
            2,
            Rect::new(0, 0, 2, 2),
            0x0000_0000,
            DIM_ALPHA,
        );
        let v = buf[0] & 0xff;
        assert!(v > 0x30 && v < 0xC0, "white went to {v:02x}");
        assert!(buf.iter().all(|&p| p == buf[0]));
        // Alpha zero is a no-op, and the whole thing clips to the buffer.
        let mut buf = vec![0x0012_3456u32; 4];
        blend_rect(&mut buf, 2, 2, Rect::new(0, 0, 2, 2), 0, 0);
        blend_rect(&mut buf, 2, 2, Rect::new(5, 5, 9, 9), 0, 255);
        assert!(buf.iter().all(|&p| p == 0x0012_3456));
    }

    #[test]
    fn cursor_blends_and_clips() {
        let mut dst = vec![0u32; 4 * 4];
        let cur = CursorImage {
            width: 2,
            height: 2,
            hot_x: 1,
            hot_y: 1,
            argb: vec![0xFF00FF00, 0x80800000, 0x00000000, 0xFF0000FF],
        };
        draw_cursor(&mut dst, 4, 4, &cur, 0, 0);
        // Hotspot at (0,0) => image origin at (-1,-1); only bottom-right pixel visible.
        assert_eq!(dst[0], 0x0000FF);
        draw_cursor(&mut dst, 4, 4, &cur, 2, 2);
        assert_eq!(dst[4 + 1], 0x00FF00);
        assert_eq!(dst[4 + 2], 0x800000);
        assert_eq!(dst[8 + 2], 0x0000FF);
        assert_eq!(dst[8 + 1], 0);
        let hidden = CursorImage {
            width: 0,
            height: 0,
            hot_x: 0,
            hot_y: 0,
            argb: vec![],
        };
        draw_cursor(&mut dst, 4, 4, &hidden, 1, 1);
    }
}
