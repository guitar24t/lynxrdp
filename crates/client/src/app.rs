//! The graphical client: a winit window painted with softbuffer.
//!
//! The window shows the remote framebuffer 1:1. When the window is resized
//! the remote screen is asked to follow (debounced), so there is never any
//! scaling and text stays sharp. The pointer is drawn locally from the
//! cursor images the server sends, which makes it feel instantaneous even
//! on slow links.

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
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::connection::{Client, ClientEvent};
use crate::keymap;

/// How long to wait after the last resize before asking the server.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(200);
/// Clipboard poll interval while focused.
const CLIPBOARD_POLL: Duration = Duration::from_millis(700);
/// RTT probe interval.
const PING_INTERVAL: Duration = Duration::from_secs(3);

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
    last_clipboard_poll: Instant,
    last_ping: Instant,
    rtt: Option<Duration>,
    dirty: Option<Rect>,
    full_redraw: bool,
    exit_reason: Option<String>,
    fullscreen: bool,
    pressed_keys: Vec<u32>,
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
            last_clipboard_poll: Instant::now(),
            last_ping: Instant::now(),
            rtt: None,
            dirty: None,
            full_redraw: true,
            exit_reason: None,
            pressed_keys: Vec::new(),
            frames: 0,
            last_title_update: Instant::now(),
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

    fn redraw(&mut self) -> Result<()> {
        let local_cursor = self.uses_local_cursor();
        let Some(gfx) = self.gfx.as_mut() else {
            return Ok(());
        };
        let size = gfx.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };
        gfx.surface
            .resize(w, h)
            .map_err(|e| anyhow::anyhow!("surface resize: {e}"))?;
        let mut buf = gfx
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("surface buffer: {e}"))?;
        let fb = self.client.framebuffer();
        blit(&mut buf, size.width, size.height, fb);
        if local_cursor {
            if let (Some(cur), Some((px, py))) = (&self.cursor, self.pointer) {
                draw_cursor(&mut buf, size.width, size.height, cur, px, py);
            }
        }
        buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        self.dirty = None;
        self.full_redraw = false;
        Ok(())
    }

    fn request_redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
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
                    ClientEvent::Notice(text) => log::info!("server: {text}"),
                    ClientEvent::Rtt(rtt) => {
                        self.rtt = Some(rtt);
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
        }
        if now.duration_since(self.last_title_update) >= Duration::from_secs(1) {
            self.last_title_update = now;
            if let Some(g) = &self.gfx {
                g.window.set_title(&self.title());
            }
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
        d
    }

    fn on_key(&mut self, event: KeyEvent) {
        // Fullscreen toggle: Ctrl+Alt+Enter (never forwarded).
        if event.state == ElementState::Pressed
            && matches!(event.logical_key, Key::Named(NamedKey::Enter))
            && self.modifiers.control_key()
            && self.modifiers.alt_key()
        {
            self.toggle_fullscreen();
            return;
        }
        let Some(ks) = keymap::keysym_for(&event.logical_key, event.location) else {
            return;
        };
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
        let (w, h) = self.client.size();
        let x = pos.x.max(0.0).min(f64::from(w.saturating_sub(1))) as u32;
        let y = pos.y.max(0.0).min(f64::from(h.saturating_sub(1))) as u32;
        if self.pointer != Some((x, y)) {
            self.pointer = Some((x, y));
            let _ = self.client.pointer_move(x as u16, y as u16);
            if self.uses_local_cursor() {
                self.request_redraw();
            }
        }
    }
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
            WindowEvent::CloseRequested => {
                self.exit_reason = Some("window closed".into());
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
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
                if f {
                    self.poll_clipboard();
                } else {
                    self.release_all_keys();
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => self.on_key(event),
            WindowEvent::CursorMoved { position, .. } => self.on_pointer_moved(position),
            WindowEvent::CursorLeft { .. } => {
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
                let btn = match b {
                    MouseButton::Left => button::LEFT,
                    MouseButton::Middle => button::MIDDLE,
                    MouseButton::Right => button::RIGHT,
                    MouseButton::Back => button::BACK,
                    MouseButton::Forward => button::FORWARD,
                    MouseButton::Other(_) => return,
                };
                let _ = self
                    .client
                    .pointer_button(btn, state == ElementState::Pressed);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    // Pixel deltas (touchpads): ~40 px per detent.
                    MouseScrollDelta::PixelDelta(p) => ((p.x / 40.0) as f32, (p.y / 40.0) as f32),
                };
                let sx = dx.round() as i16;
                let sy = (-dy).round() as i16;
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
