//! Protocol client without any user interface.

use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::Receiver;
use lynxrdp_proto::codec::Decoder;
use lynxrdp_proto::frame::{frame_message, read_message, FrameError};
use lynxrdp_proto::message::{features, CursorImage};
use lynxrdp_proto::{Framebuffer, Message, Rect, PROTOCOL_VERSION};

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
            features: features::LOCAL_CURSOR | features::CLIPBOARD | features::RESIZE,
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
    /// A notice for the user.
    Notice(String),
    /// Round trip time measured from a ping/pong.
    Rtt(Duration),
    /// The connection ended.
    Disconnected(String),
}

/// Client-side connection state.
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
        let (info, width, height) = match read_message(&mut reader)
            .map_err(|e| anyhow!("handshake failed: {e}"))?
        {
            Message::ServerHello {
                version,
                server_name,
                features,
                session_id,
                username,
                width,
                height,
            } => {
                if version != PROTOCOL_VERSION {
                    bail!("server speaks protocol version {version}, this client speaks {PROTOCOL_VERSION}");
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
            Message::Rejected { code, reason } => {
                bail!("server rejected the connection (code {code}): {reason}")
            }
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
                            if let Message::ScreenUpdate { tiles, .. } = &msg {
                                let n: usize = tiles.iter().map(|t| t.data.len() + 13).sum();
                                *counter.lock().unwrap() += n as u64 + 16;
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

    /// Ask the server to resend the whole screen.
    pub fn request_refresh(&self) -> Result<()> {
        self.send(&Message::RefreshRequest)
    }

    /// Process the next server message, applying frames to the framebuffer
    /// and answering pings. Returns `None` on timeout.
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<ClientEvent>> {
        if let Some(reason) = &self.closed {
            return Ok(Some(ClientEvent::Disconnected(reason.clone())));
        }
        loop {
            let msg = match self.events.recv_timeout(timeout) {
                Ok(m) => m,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => return Ok(None),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.closed = Some("reader stopped".into());
                    return Ok(Some(ClientEvent::Disconnected("reader stopped".into())));
                }
            };
            if let Some(ev) = self.handle(msg)? {
                return Ok(Some(ev));
            }
        }
    }

    /// Non-blocking variant of [`Client::poll_event`].
    pub fn try_event(&mut self) -> Result<Option<ClientEvent>> {
        self.poll_event(Duration::ZERO)
    }

    fn handle(&mut self, msg: Message) -> Result<Option<ClientEvent>> {
        Ok(match msg {
            Message::ScreenUpdate { frame_id, tiles } => {
                let dirty = self
                    .decoder
                    .apply(&tiles)
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
                let r = format!("rejected (code {code}): {reason}");
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
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.reader.take() {
            let _ = t.join();
        }
        self.closed
            .get_or_insert_with(|| "closed by client".to_string());
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
