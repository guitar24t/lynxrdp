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

use std::num::NonZeroU32;
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

use crate::connection::{Client, ClientEvent};
use crate::keymap;
use crate::overlay::{self, Overlay};

/// How long to wait after the last resize before asking the server.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(200);
/// Clipboard poll interval while focused.
const CLIPBOARD_POLL: Duration = Duration::from_millis(700);
/// RTT probe interval.
const PING_INTERVAL: Duration = Duration::from_secs(3);
/// How long the link may stay silent before the bar says so. Two probe
/// intervals: one missed pong is a slow link, two is a link that has stopped.
const STALL_AFTER: Duration = Duration::from_secs(2 * PING_INTERVAL.as_secs());
/// How often the window repaints while the bar is up, so the round-trip and
/// upload figures on it are not stale.
const OVERLAY_TICK: Duration = Duration::from_millis(100);

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
    /// Uploads started by dropping files on the window: (transfer id, name).
    uploads: Vec<(u64, String)>,
    /// Clipboard files downloaded so far in the current batch.
    clipboard_files: Vec<std::path::PathBuf>,
    /// How many clipboard files are still downloading.
    clipboard_pending: usize,
    last_clipboard_poll: Instant,
    last_ping: Instant,
    rtt: Option<Duration>,
    dirty: Option<Rect>,
    /// Where the local cursor was drawn last frame, so the pixels it is
    /// leaving are presented along with the ones it is entering.
    last_cursor: Option<Rect>,
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
            uploads: Vec::new(),
            clipboard_files: Vec::new(),
            clipboard_pending: 0,
            last_clipboard_poll: now,
            last_ping: now,
            rtt: None,
            dirty: None,
            last_cursor: None,
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
        if !self.uploads.is_empty() {
            let (done, total) = self.uploads.iter().fold((0u64, 0u64), |(d, t), (id, _)| {
                let (a, b) = self.client.transfer_progress(*id).unwrap_or((0, 0));
                (d + a, t + b)
            });
            let pct = (done * 100).checked_div(total).unwrap_or(0);
            t.push_str(&format!(
                " - uploading {} file(s) {pct}%",
                self.uploads.len()
            ));
        }
        t
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
    /// The whole window is still blitted every frame -- the framebuffer copy
    /// is cheap and, more to the point, it means the buffer is complete
    /// whichever of `softbuffer`'s buffers we were handed, so damage can be
    /// declared without any buffer-age reasoning. What the damage list saves
    /// is the upload: without it the compositor takes a full-window texture
    /// for a two-line terminal cursor.
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
        blit(&mut buf, size.width, size.height, self.client.framebuffer());
        // After the blit and before the cursor: the bar sits over the remote
        // screen and under the pointer, and it is drawn into this buffer
        // rather than into the framebuffer the next frame will diff against.
        let (bar, bar_was) = self
            .overlay
            .draw(&mut buf, size.width, size.height, scale, &status);
        if cursor_now.is_some() {
            if let (Some(cur), Some((px, py))) = (&self.cursor, self.pointer) {
                draw_cursor(&mut buf, size.width, size.height, cur, px, py);
            }
        }
        let parts = [
            self.dirty.unwrap_or_default(),
            self.last_cursor.unwrap_or_default(),
            cursor_now.unwrap_or_default(),
            bar.unwrap_or_default(),
            // What the bar covered last frame but does not now: the blit has
            // already restored those pixels, but nothing would present them.
            bar_was.unwrap_or_default(),
        ];
        let damage: Vec<softbuffer::Rect> = damage_regions(win, self.full_redraw, &parts)
            .into_iter()
            .filter_map(surface_rect)
            .collect();
        buf.present_with_damage(&damage)
            .map_err(|e| anyhow::anyhow!("present: {e}"))?;
        self.last_cursor = cursor_now;
        self.dirty = None;
        self.full_redraw = false;
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
        if !self.uploads.is_empty() {
            let (done, total) = self.uploads.iter().fold((0u64, 0u64), |(d, t), (id, _)| {
                let (a, b) = self.client.transfer_progress(*id).unwrap_or((0, 0));
                (d + a, t + b)
            });
            st.uploads = Some((
                self.uploads.len(),
                (done * 100).checked_div(total).unwrap_or(0),
            ));
        }
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
                    ClientEvent::FileDownloaded { path, name } => {
                        log::info!("downloaded {name} to {}", path.display());
                        self.on_clipboard_file_ready(path);
                    }
                    ClientEvent::FileUploaded { name } => {
                        log::info!("uploaded {name}");
                        let done = self
                            .uploads
                            .iter()
                            .find(|(_, n)| *n == name)
                            .map(|(id, _)| *id);
                        if let Some(id) = done {
                            self.finish_upload(id);
                        }
                    }
                    ClientEvent::TransferFailed { id, reason } => {
                        log::warn!("transfer {id} failed: {reason}");
                        self.finish_upload(id);
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
            self.poll_clipboard();
            self.poll_clipboard_image();
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
        for (local, dest) in files {
            match self.client.send_file(&local, &dest) {
                Ok(id) => {
                    log::info!("uploading {} as {dest}", local.display());
                    self.uploads.push((id, dest));
                }
                Err(e) => log::warn!("uploading {} failed: {e:#}", local.display()),
            }
        }
        self.update_title();
    }

    /// The session copied files: download them, then offer them locally.
    fn on_remote_files(&mut self, files: Vec<lynxrdp_proto::FileEntry>) {
        if files.is_empty() {
            return;
        }
        let dir = match clipboard_staging_dir() {
            Ok(d) => d,
            Err(e) => {
                log::warn!("cannot prepare the clipboard staging directory: {e:#}");
                return;
            }
        };
        self.clipboard_files.clear();
        self.clipboard_pending = 0;
        for f in &files {
            let name = f.path.rsplit(['/', '\\']).next().unwrap_or("file");
            let Some(safe) = lynxrdp_proto::transfer::safe_relative_path(name) else {
                continue;
            };
            let dest = dir.join(&safe);
            match self.client.request_file(&f.path, dest) {
                Ok(_) => self.clipboard_pending += 1,
                Err(e) => log::warn!("cannot fetch {}: {e:#}", f.path),
            }
        }
        log::info!(
            "fetching {} file(s) copied in the session",
            self.clipboard_pending
        );
    }

    /// One staged clipboard file arrived; publish once the batch is complete.
    fn on_clipboard_file_ready(&mut self, path: std::path::PathBuf) {
        if self.clipboard_pending == 0 {
            return;
        }
        self.clipboard_files.push(path);
        self.clipboard_pending -= 1;
        if self.clipboard_pending > 0 {
            return;
        }
        let files = std::mem::take(&mut self.clipboard_files);
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

    /// Forget an upload that finished or failed.
    fn finish_upload(&mut self, id: u64) {
        self.uploads.retain(|(uid, _)| *uid != id);
        self.update_title();
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
    fn poll_clipboard_image(&mut self) {
        if self.client.info().features & features::CLIPBOARD_IMAGE == 0 {
            return;
        }
        let Some(cb) = self.clipboard.as_mut() else {
            return;
        };
        let img = match cb.get_image() {
            Ok(i) => i,
            // No image on the clipboard is the common case, not an error.
            Err(arboard::Error::ContentNotAvailable) => return,
            Err(e) => {
                log::debug!("reading the clipboard image failed: {e}");
                return;
            }
        };
        let digest = image_digest(&img.bytes);
        if self.last_image == Some(digest) {
            return;
        }
        self.last_image = Some(digest);
        let rgba = match crate::imageclip::Rgba::new(img.width, img.height, img.bytes.into_owned())
        {
            Ok(r) => r,
            Err(e) => {
                log::debug!("clipboard image was not usable: {e:#}");
                return;
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
pub fn clipboard_staging_dir() -> anyhow::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("lynxrdp-clipboard-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Largest number of files one drop may upload, so dropping a huge tree
/// does not queue thousands of transfers by accident.
pub const MAX_DROPPED_FILES: usize = 512;

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
