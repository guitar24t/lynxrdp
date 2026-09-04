//! The graphical client: a winit window painted with softbuffer.
//!
//! The window shows the remote framebuffer 1:1. When the window is resized
//! the remote screen is asked to follow (debounced), so there is never any
//! scaling and text stays sharp. The pointer is drawn locally from the
//! cursor images the server sends, which makes it feel instantaneous even
//! on slow links.
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
use crate::connection::{Client, ClientEvent};
use crate::keymap;
use crate::overlay::{self, Overlay};

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
    /// When the connection was made; the fallback for "last known alive".
    started: Instant,
    /// When the last pong arrived.
    last_pong: Option<Instant>,
    /// Whether the link was answering at the last check, so a change can show
    /// the bar without documentation.
    link_ok: bool,
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
    pub fn new(client: Client, opts: AppOptions) -> Self {
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
            started: now,
            last_pong: None,
            link_ok: true,
            last_overlay_frame: now,
            frames: 0,
            last_title_update: now,
        }
    }

    /// Run the event loop until the window closes or the connection ends.
    ///
    /// `waker` is the slot returned by [`make_waker`]; it is filled with the
    /// event loop proxy so the network reader thread can wake the loop.
    pub fn run(client: Client, opts: AppOptions, waker: WakerSlot) -> Result<Option<String>> {
        let event_loop = EventLoop::<Wake>::with_user_event()
            .build()
            .context("creating event loop")?;
        *waker.lock().unwrap() = Some(event_loop.create_proxy());
        let mut app = App::new(client, opts);
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
        let mut attrs = Window::default_attributes()
            .with_title(self.title())
            .with_inner_size(PhysicalSize::new(w, h))
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
        let win = Rect::new(0, 0, size.width, size.height);
        let scale = self.overlay_scale();
        let status = self.overlay_status(Instant::now());
        // While the pointer is on the bar it is the OS cursor the user is
        // steering, not the session's, so ours is neither drawn nor moved.
        let cursor_now = match (
            self.uses_local_cursor() && !self.pointer_on_bar,
            &self.cursor,
        ) {
            (true, Some(cur)) => self.pointer.map(|(px, py)| cursor_bounds(cur, px, py)),
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
            .then(|| Rect::new(0, 0, size.width, overlay::bar_height(scale)));
        // A resize invalidates every buffer the surface holds, and
        // softbuffer's Wayland backend reallocates the mapping without
        // resetting `age`, so this check has to be ours rather than the
        // backend's.
        if self.presented_size != Some((size.width, size.height)) {
            self.full_redraw = true;
        }
        // Snapshot before `gfx` is borrowed; it is at most three short lists.
        let log: Vec<Vec<Rect>> = self.present_log.iter().cloned().collect();
        let asked_full = self.full_redraw;

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
            blit_rect(
                &mut buf,
                size.width,
                size.height,
                self.client.framebuffer(),
                *r,
            );
        }
        // After the blit and before the cursor: the bar sits over the remote
        // screen and under the pointer, and it is drawn into this buffer
        // rather than into the framebuffer the next frame will diff against.
        let (bar, bar_was) = self
            .overlay
            .draw(&mut buf, size.width, size.height, scale, &status);
        debug_assert_eq!(
            bar, bar_now,
            "the predicted bar rectangle must match the painted one, or the \
             blit did not cover the pixels the scrim was blended onto"
        );
        debug_assert_eq!(bar_was, self.last_bar, "the bar's own history drifted");
        if cursor_now.is_some() {
            if let (Some(cur), Some((px, py))) = (&self.cursor, self.pointer) {
                draw_cursor(&mut buf, size.width, size.height, cur, px, py);
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

    /// How long the link has been silent, when that is long enough to say so.
    ///
    /// Before the first pong there is nothing to measure from, so the
    /// connection's own start stands in: the handshake completing is itself
    /// proof the link was alive, and calling a three-second-old session
    /// "stalled" because our first probe has not come back yet would be a lie.
    ///
    /// `Client` keeps a better signal -- `last_message_at`, which any message
    /// refreshes, not only a pong -- but it is private and `connection.rs` is
    /// not this change's to edit. If it grows an accessor, prefer it here: it
    /// notices a wedged session without depending on our own probe.
    fn stalled_for(&self, now: Instant) -> Option<Duration> {
        let quiet = now.saturating_duration_since(self.last_pong.unwrap_or(self.started));
        (quiet > STALL_AFTER).then_some(quiet)
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

    fn drain_network(&mut self, event_loop: &ActiveEventLoop) {
        if self.exit_reason.is_some() {
            return;
        }
        loop {
            match self.client.try_event() {
                Ok(Some(ev)) => match ev {
                    ClientEvent::Frame { dirty, .. } => {
                        self.frames += 1;
                        self.dirty = Some(self.dirty.map(|d| d.union(&dirty)).unwrap_or(dirty));
                        self.request_redraw();
                    }
                    ClientEvent::Resized { width, height } => {
                        log::info!("remote screen is now {width}x{height}");
                        self.full_redraw = true;
                        if let Some(g) = &self.gfx {
                            if !self.fullscreen {
                                let cur = g.window.inner_size();
                                if (cur.width, cur.height) != (width, height) {
                                    let _ = g
                                        .window
                                        .request_inner_size(PhysicalSize::new(width, height));
                                }
                            }
                            g.window.set_title(&self.title());
                        }
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
                    ClientEvent::Rtt(rtt) => {
                        self.rtt = Some(rtt);
                        self.last_pong = Some(Instant::now());
                    }
                    ClientEvent::Disconnected(reason) => {
                        log::info!("disconnected: {reason}");
                        self.exit_reason = Some(reason);
                        event_loop.exit();
                        return;
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    log::error!("protocol error: {e:#}");
                    self.exit_reason = Some(format!("protocol error: {e:#}"));
                    event_loop.exit();
                    return;
                }
            }
        }
    }

    fn housekeeping(&mut self) {
        let now = Instant::now();
        if let (Some(t), Some((w, h))) = (self.last_resize_event, self.pending_size) {
            if now.duration_since(t) >= RESIZE_DEBOUNCE {
                self.last_resize_event = None;
                self.pending_size = None;
                if (w, h) != self.client.size() && w >= 64 && h >= 64 {
                    log::debug!("requesting remote resize to {w}x{h}");
                    let _ = self.client.request_resize(
                        w.min(u16::MAX as u32) as u16,
                        h.min(u16::MAX as u32) as u16,
                    );
                }
            }
        }
        if now.duration_since(self.last_ping) >= PING_INTERVAL {
            self.last_ping = now;
            let _ = self.client.ping();
        }
        if self.focused && now.duration_since(self.last_clipboard_poll) >= CLIPBOARD_POLL {
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
                .set_cursor_visible(!self.uses_local_cursor() || self.cursor.is_none());
        }
    }

    /// Do what a bar button or its accelerator asks.
    fn run_overlay_action(&mut self, action: overlay::Action, event_loop: &ActiveEventLoop) {
        match action {
            overlay::Action::Fullscreen => self.toggle_fullscreen(),
            overlay::Action::SecureAttention => self.send_secure_attention(),
            overlay::Action::Disconnect => {
                // The same path CloseRequested takes, so there is one exit.
                self.exit_reason = Some("disconnected by the user".into());
                event_loop.exit();
            }
        }
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
            let _ = self.client.key(ks, true);
        }
        let _ = self.client.key(keysym::DELETE, true);
        let _ = self.client.key(keysym::DELETE, false);
        for &ks in synth.iter().rev() {
            self.pressed_keys.retain(|&k| k != ks);
            let _ = self.client.key(ks, false);
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
        while self.uploads.len() < MAX_CONCURRENT_UPLOADS {
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
        if files.is_empty() {
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
        if self.client.info().features & features::CLIPBOARD_IMAGE == 0 {
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
                        let _ = self.client.set_clipboard(&text);
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
        if self.overlay.visible() {
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
    fn accelerator(&self, event: &KeyEvent) -> Option<Accelerator> {
        accelerator_for(self.modifiers, &event.logical_key, event.physical_key)
    }

    fn on_key(&mut self, event: KeyEvent, event_loop: &ActiveEventLoop) {
        let Some(ks) = keymap::keysym_for(&event.logical_key, event.location) else {
            return;
        };
        if event.state == ElementState::Pressed {
            if let Some(acc) = self.accelerator(&event) {
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
        let _ = self.client.key(ks, down);
    }

    fn release_all_keys(&mut self) {
        for ks in std::mem::take(&mut self.pressed_keys) {
            let _ = self.client.key(ks, false);
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
        // it crosses the top edge, and the bar does not come up under it.
        let claimed = !self.remote_drag()
            && self.overlay.track(
                pos.x.max(0.0) as u32,
                pos.y.max(0.0) as u32,
                self.overlay_scale(),
            );
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
        let (w, h) = self.client.size();
        let x = pos.x.max(0.0).min(f64::from(w.saturating_sub(1))) as u32;
        let y = pos.y.max(0.0).min(f64::from(h.saturating_sub(1))) as u32;
        if resync || self.pointer != Some((x, y)) {
            self.pointer = Some((x, y));
            let _ = self.client.pointer_move(x as u16, y as u16);
            if self.uses_local_cursor() {
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
        Key::Character(c) if c.eq_ignore_ascii_case("q") => return Some(Accelerator::Disconnect),
        // A letter the layout produced that is not one of ours is the
        // layout's final word; the session gets it.
        Key::Character(c) if c.chars().all(|c| c.is_ascii_alphanumeric()) => return None,
        _ => {}
    }
    match physical {
        PhysicalKey::Code(KeyCode::KeyB) => Some(Accelerator::Pin),
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
    let x = i64::from(px) - i64::from(cur.hot_x);
    let y = i64::from(py) - i64::from(cur.hot_y);
    let (x0, y0) = (x.max(0), y.max(0));
    Rect::new(
        x0 as u32,
        y0 as u32,
        (x + i64::from(cur.width) - x0).max(0) as u32,
        (y + i64::from(cur.height) - y0).max(0) as u32,
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
    let s2 = slot.clone();
    let f: Box<dyn Fn() + Send> = Box::new(move || {
        if let Some(p) = s2.lock().unwrap().as_ref() {
            let _ = p.send_event(Wake);
        }
    });
    (f, slot)
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

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: Wake) {
        self.drain_network(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::DroppedFile(path) => self.on_dropped_file(&path),
            WindowEvent::CloseRequested => {
                self.exit_reason = Some("window closed".into());
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                // A redraw nobody asked for is an expose. Our own state says
                // nothing changed, so the damage list would name at most the
                // cursor -- and `present_with_damage` copies only what it is
                // given, leaving the rest of the window showing whatever
                // uncovered it. Repaint the lot instead.
                if !std::mem::take(&mut self.redraw_asked) {
                    self.full_redraw = true;
                }
                self.drain_network(event_loop);
                if let Err(e) = self.redraw() {
                    log::error!("redraw failed: {e:#}");
                }
            }
            WindowEvent::Resized(size) => {
                self.full_redraw = true;
                if self.opts.dynamic_resize && self.client.info().features & features::RESIZE != 0 {
                    self.pending_size = Some((size.width, size.height));
                    self.last_resize_event = Some(Instant::now());
                }
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
                    self.release_all_keys();
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
            WindowEvent::CursorEntered { .. } => {
                if let Some(g) = &self.gfx {
                    g.window
                        .set_cursor_visible(!self.uses_local_cursor() || self.cursor.is_none());
                }
            }
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
                let Some((btn, bit)) = button_codes(b) else {
                    return;
                };
                if down {
                    self.remote_buttons |= bit;
                } else {
                    self.remote_buttons &= !bit;
                }
                let _ = self.client.pointer_button(btn, down);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.pointer_on_bar {
                    return;
                }
                let (dx, dy) = scroll_units(delta);
                let (sx, sy) = self.scroll.feed(dx, dy);
                if sx != 0 || sy != 0 {
                    let _ = self.client.send(&Message::Scroll { dx: sx, dy: sy });
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_network(event_loop);
        self.housekeeping();
        if self.dirty.is_some() || self.full_redraw {
            self.request_redraw();
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            Instant::now() + self.next_wake(),
        ));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.release_all_keys();
        // Release Shift/Control keysym remnants explicitly for safety.
        let _ = self.client.key(keysym::SHIFT_L, false);
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

/// Alpha-blend a premultiplied ARGB cursor onto the buffer.
pub fn draw_cursor(dst: &mut [u32], dst_w: u32, dst_h: u32, cur: &CursorImage, px: u32, py: u32) {
    if cur.width == 0 || cur.height == 0 {
        return;
    }
    let ox = px as i64 - i64::from(cur.hot_x);
    let oy = py as i64 - i64::from(cur.hot_y);
    for cy in 0..i64::from(cur.height) {
        let y = oy + cy;
        if y < 0 || y >= i64::from(dst_h) {
            continue;
        }
        for cx in 0..i64::from(cur.width) {
            let x = ox + cx;
            if x < 0 || x >= i64::from(dst_w) {
                continue;
            }
            let s = cur.argb[(cy as usize) * usize::from(cur.width) + cx as usize];
            let a = s >> 24;
            if a == 0 {
                continue;
            }
            let idx = (y as usize) * (dst_w as usize) + x as usize;
            if a == 255 {
                dst[idx] = s & 0x00FF_FFFF;
                continue;
            }
            let d = dst[idx];
            let inv = 255 - a;
            let blend = |sc: u32, dc: u32| -> u32 { (sc + (dc * inv + 127) / 255).min(255) };
            let r = blend((s >> 16) & 0xff, (d >> 16) & 0xff);
            let g = blend((s >> 8) & 0xff, (d >> 8) & 0xff);
            let b = blend(s & 0xff, d & 0xff);
            dst[idx] = (r << 16) | (g << 8) | b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
