//! The session core: one thread that owns the X display objects, the
//! current client, and all frame pacing decisions.
//!
//! ## Latency model
//!
//! * Damage events only set a flag; pixels are fetched lazily right before
//!   a frame is sent, so a frame always carries the newest content.
//! * At most `max_in_flight` frames are unacknowledged. When the client is
//!   slow, changes accumulate and are sent as one frame once an ack arrives,
//!   so the client never falls behind the screen by more than one frame's
//!   worth of transmission time.
//! * Frames are rate limited to `max_fps`.
//! * Input is applied the moment it arrives, ahead of any frame work.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{after, never, select, Receiver, Sender};
use lynxrdp_proto::codec::Encoder;
use lynxrdp_proto::frame::frame_message;
use lynxrdp_proto::message::{features, reject, CursorImage};
use lynxrdp_proto::{Framebuffer, Message, Rect, PROTOCOL_VERSION, TILE_SIZE};
use x11rb::protocol::Event;

use super::listener::spawn_client_reader;
use super::socket::ClientSocket;
use super::{CoreEvent, NewClient, SessionOptions};
use crate::x11::capture::{coalesce, DamageTracker, ScreenCapture};
use crate::x11::clipboard::Clipboard;
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

/// Supported feature bits.
const SUPPORTED_FEATURES: u32 = features::LOCAL_CURSOR | features::CLIPBOARD | features::RESIZE;

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
    in_flight: u32,
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
    /// Last clipboard text sent to the client (to suppress echoes).
    last_clipboard_sent: Option<String>,
    bytes_sent: u64,
    frames_sent: u64,
}

impl Client {
    fn send(&mut self, msg: &Message) -> bool {
        let mut buf = Vec::new();
        frame_message(msg, &mut buf);
        self.bytes_sent += buf.len() as u64;
        match self.writer_tx.try_send(buf) {
            Ok(()) => true,
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                log::warn!(
                    "client {} is not draining its socket; dropping",
                    self.description
                );
                false
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
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
    /// Clipboard text received from the client, to avoid echoing it back.
    last_clipboard_received: Option<String>,
    /// Queue of X events that arrived while we were busy (drained in order).
    pending_x: VecDeque<Event>,
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
        let (w, h) = display.refresh_size()?;
        let capture = ScreenCapture::new(
            display.clone(),
            opts.max_width.max(w),
            opts.max_height.max(h),
        )?;
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
        log::info!(
            "session core ready: {w}x{h}, shm={}, cursor={}, clipboard={}, max_fps={}, in_flight={}",
            capture.uses_shm(),
            cursor.is_some(),
            clipboard.is_some(),
            opts.max_fps,
            opts.max_in_flight
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
            last_clipboard_received: None,
            pending_x: VecDeque::new(),
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
            match outcome {
                Ok(Some(exit)) => return exit,
                Ok(None) => {}
                Err(e) => {
                    log::error!("session core error: {e:#}");
                    return Exit::XError(format!("{e:#}"));
                }
            }
            // Drain anything else that is already queued before doing frame
            // work, so a burst of input is applied in one go.
            while let Ok(ev) = self.events_rx.try_recv() {
                match self.handle(ev) {
                    Ok(Some(exit)) => return exit,
                    Ok(None) => {}
                    Err(e) => {
                        log::error!("session core error: {e:#}");
                        return Exit::XError(format!("{e:#}"));
                    }
                }
            }
            if let Err(e) = self.pump() {
                log::error!("frame pump error: {e:#}");
                return Exit::XError(format!("{e:#}"));
            }
        }
    }

    fn next_frame_delay(&self) -> Option<Duration> {
        let c = self.client.as_ref()?;
        if !c.ready || c.in_flight >= self.opts.max_in_flight {
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
                    if self.opts.exit_on_disconnect {
                        return Ok(Some(Exit::ClientDisconnected));
                    }
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
        }
    }

    fn handle_x_event(&mut self, ev: Event) -> Result<()> {
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
            Event::Error(e) => {
                log::debug!("X error: {e:?}");
            }
            _ => {}
        }
        if let Some(cb) = self.clipboard.as_mut() {
            if let Some(text) = cb.handle_event(&ev)? {
                self.on_session_clipboard(text);
            }
        }
        // Keep the queue type in use for future ordering needs.
        self.pending_x.clear();
        Ok(())
    }

    fn on_session_clipboard(&mut self, text: String) {
        if self.last_clipboard_received.as_deref() == Some(text.as_str()) {
            return;
        }
        if let Some(c) = self.client.as_mut() {
            if c.ready && c.features & features::CLIPBOARD != 0 {
                log::debug!("clipboard -> client ({} bytes)", text.len());
                c.last_clipboard_sent = Some(text.clone());
                c.send(&Message::ClipboardText { text });
            }
        }
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
            in_flight: 0,
            next_frame_id: 1,
            last_frame_at: None,
            full_refresh: false,
            last_ping_at: now,
            last_pong_at: now,
            ping_nonce: 0,
            last_client_pointer: self.input.last_pointer(),
            last_sent_pointer: None,
            last_clipboard_sent: None,
            bytes_sent: 0,
            frames_sent: 0,
        });
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
        // Close the queue so the writer flushes what it has and exits.
        drop(c.writer_tx);
        if let Some(w) = c.writer.take() {
            let _ = w.join();
        }
        c.socket.shutdown();
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
        self.last_client_seen = Instant::now();
        Ok(())
    }

    fn reject(&mut self, code: u16, reason: &str) -> Result<()> {
        log::warn!("rejecting client: {reason}");
        if let Some(c) = self.client.as_mut() {
            c.send(&Message::Rejected {
                code,
                reason: reason.to_string(),
            });
        }
        self.drop_client(None)
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
                } => {
                    self.on_hello(
                        version,
                        client_name,
                        want,
                        u32::from(width),
                        u32::from(height),
                    )?;
                    Ok(None)
                }
                other => {
                    self.reject(
                        reject::VERSION,
                        &format!("expected ClientHello, got {:?}", other.kind()),
                    )?;
                    Ok(None)
                }
            };
        }
        match msg {
            Message::KeyEvent { keysym, down } => {
                if let Err(e) = self.input.key(keysym, down) {
                    log::warn!("key injection failed: {e:#}");
                }
            }
            Message::PointerMotion { x, y } => {
                let (w, h) = self.display.size();
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
                if let Some(c) = self.client.as_mut() {
                    if c.in_flight > 0 {
                        c.in_flight -= 1;
                    }
                    log::trace!("ack frame {frame_id}, in flight {}", c.in_flight);
                }
            }
            Message::ResizeRequest { width, height } => {
                self.on_resize_request(u32::from(width), u32::from(height))?;
            }
            Message::ClipboardText { text } => {
                if let Some(c) = self.client.as_ref() {
                    if c.features & features::CLIPBOARD == 0 {
                        return Ok(None);
                    }
                    if c.last_clipboard_sent.as_deref() == Some(text.as_str()) {
                        return Ok(None);
                    }
                }
                self.last_clipboard_received = Some(text.clone());
                if let Some(cb) = self.clipboard.as_mut() {
                    log::debug!("clipboard <- client ({} bytes)", text.len());
                    cb.set_text(text)?;
                }
            }
            Message::Pong { nonce } => {
                if let Some(c) = self.client.as_mut() {
                    if nonce == c.ping_nonce {
                        c.last_pong_at = Instant::now();
                    }
                }
            }
            Message::RefreshRequest => {
                if let Some(c) = self.client.as_mut() {
                    c.full_refresh = true;
                }
            }
            Message::Disconnect { reason } => {
                log::info!("client requested disconnect: {reason}");
                self.drop_client(None)?;
                if self.opts.exit_on_disconnect {
                    return Ok(Some(Exit::ClientDisconnected));
                }
            }
            Message::ClientHello { .. } => {
                self.reject(reject::VERSION, "duplicate ClientHello")?;
            }
            other => {
                self.reject(
                    reject::VERSION,
                    &format!("unexpected message {:?} from client", other.kind()),
                )?;
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
    ) -> Result<()> {
        if version != PROTOCOL_VERSION {
            return self.reject(
                reject::VERSION,
                &format!(
                    "protocol version {version} not supported (server speaks {PROTOCOL_VERSION})"
                ),
            );
        }
        let features = want & SUPPORTED_FEATURES;
        // Apply the requested size (or the default) before the first frame.
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
        let (w, h) = self.display.size();
        let Some(c) = self.client.as_mut() else {
            return Ok(());
        };
        c.ready = true;
        c.features = features;
        c.full_refresh = true;
        log::info!(
            "client {} ({client_name}) accepted: features 0x{features:x}, screen {w}x{h}",
            c.description
        );
        c.send(&Message::ServerHello {
            version: PROTOCOL_VERSION,
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
        self.encoder.invalidate(&Rect::new(0, 0, w, h));
        Ok(())
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

    /// Resize the X screen and reset frame state accordingly.
    fn apply_resize(&mut self, width: u32, height: u32) -> Result<()> {
        let width = width.clamp(64, self.opts.max_width);
        let height = height.clamp(64, self.opts.max_height);
        resize_screen(&self.display, width, height)?;
        let (w, h) = self.display.size();
        self.screen = Framebuffer::new(w, h);
        self.encoder.resize(w, h);
        self.encoder.invalidate(&Rect::new(0, 0, w, h));
        self.damage.mark_dirty();
        if let Some(c) = self.client.as_mut() {
            c.full_refresh = true;
            if c.ready {
                c.send(&Message::ScreenResized {
                    width: w as u16,
                    height: h as u16,
                });
            }
        }
        log::info!("screen resized to {w}x{h}");
        Ok(())
    }

    /// Send a frame if one is due, and sync the pointer position.
    fn pump(&mut self) -> Result<()> {
        let Some(c) = self.client.as_ref() else {
            return Ok(());
        };
        if !c.ready || c.in_flight >= self.opts.max_in_flight {
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
        let (w, h) = self.display.size();
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
            if merged.len() > 24 || crate::x11::capture::total_area(&merged) * 2 > bounds.area() {
                merged = vec![bounds];
            }
            merged
        };
        if self.screen.width() != w || self.screen.height() != h {
            self.screen = Framebuffer::new(w, h);
        }
        for r in &rects {
            self.capture.capture_into(&mut self.screen, r)?;
        }
        let tiles = self.encoder.encode_regions(&self.screen, &rects);
        let Some(c) = self.client.as_mut() else {
            return Ok(());
        };
        c.full_refresh = false;
        if tiles.is_empty() {
            return Ok(());
        }
        let frame_id = c.next_frame_id;
        c.next_frame_id += 1;
        let bytes: usize = tiles.iter().map(|t| t.data.len()).sum();
        log::trace!(
            "frame {frame_id}: {} rects -> {} tiles, {} bytes, {:.2} ms",
            rects.len(),
            tiles.len(),
            bytes,
            started.elapsed().as_secs_f64() * 1000.0
        );
        if !c.send(&Message::ScreenUpdate { frame_id, tiles }) {
            self.drop_client(None)?;
            return Ok(());
        }
        c.in_flight += 1;
        c.frames_sent += 1;
        c.last_frame_at = Some(Instant::now());
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
            if self.opts.exit_on_disconnect {
                return Ok(Some(Exit::ClientDisconnected));
            }
        }
        if self.client.is_some() {
            self.sync_pointer()?;
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
