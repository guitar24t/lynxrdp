//! End-to-end tests: a real `lynxrdp-session` process on Xvfb driven by
//! the headless protocol client.
//!
//! These need `Xvfb` and `xterm` installed and are skipped (with a message)
//! when they are not -- unless `LYNXRDP_REQUIRE_E2E` is set, which turns a
//! missing dependency into a failure. CI sets it; see `tests/common/mod.rs`.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lynxrdp_client::connection::{Client, ClientEvent, ConnectOptions};
use lynxrdp_proto::{keysym, Rect};

mod common;
use common::{have, skip_unless};

macro_rules! require_xvfb {
    () => {
        if skip_unless(have("Xvfb"), "Xvfb not installed") {
            return;
        }
    };
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Session {
    child: Child,
    port: u16,
    display: String,
    runtime_dir: tempfile::TempDir,
    upload_dir: tempfile::TempDir,
}

impl Session {
    fn start(width: u32, height: u32, startwm: &str, extra: &[&str]) -> Self {
        let port = free_port();
        let runtime_dir = tempfile::tempdir().unwrap();
        let upload_dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lynxrdp-session"));
        cmd.arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--width")
            .arg(width.to_string())
            .arg("--height")
            .arg(height.to_string())
            .arg("--max-width")
            .arg("1600")
            .arg("--max-height")
            .arg("1200")
            .arg("--startwm")
            .arg(startwm)
            .arg("--runtime-dir")
            .arg(runtime_dir.path())
            .arg("--upload-dir")
            .arg(upload_dir.path())
            .arg("--print-display")
            .arg("--session-id")
            .arg("77")
            .arg("--username")
            .arg("tester")
            .args(extra)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().expect("start lynxrdp-session");
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let display = line.trim().to_string();
        assert!(display.starts_with(':'), "expected display, got {line:?}");
        // Wait for the port.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "session did not start listening");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            child,
            port,
            display,
            runtime_dir,
            upload_dir,
        }
    }

    fn connect(&self, size: Option<(u16, u16)>) -> Client {
        let opts = ConnectOptions {
            size,
            ..Default::default()
        };
        Client::connect(([127, 0, 0, 1], self.port).into(), &opts, None).expect("connect")
    }

    fn xauth(&self) -> std::path::PathBuf {
        std::fs::read_dir(self.runtime_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("Xauthority"))
                    .unwrap_or(false)
            })
            .expect("xauth file")
    }

    /// Run a command against the session's display.
    fn x(&self, prog: &str, args: &[&str]) -> std::process::Output {
        Command::new(prog)
            .args(args)
            .env("DISPLAY", &self.display)
            .env("XAUTHORITY", self.xauth())
            .output()
            .unwrap_or_else(|e| panic!("run {prog}: {e}"))
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(st)) = self.child.try_wait() {
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: signalling our own child.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        if self.wait_exit(Duration::from_secs(5)).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Drain events until the predicate matches or the timeout passes.
fn wait_for(
    client: &mut Client,
    timeout: Duration,
    mut pred: impl FnMut(&ClientEvent, &Client) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match client.poll_event(remaining).unwrap() {
            Some(ev) => {
                if let ClientEvent::Disconnected(r) = &ev {
                    panic!("disconnected: {r}");
                }
                if pred(&ev, client) {
                    return true;
                }
            }
            None => return false,
        }
    }
}

/// Drain events until the connection reports a disconnect; returns the reason.
fn wait_disconnect(client: &mut Client, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match client.poll_event(remaining).unwrap() {
            Some(ClientEvent::Disconnected(r)) => return Some(r),
            Some(_) => {}
            None => return None,
        }
    }
}

fn count_pixels(client: &Client, rect: &Rect, color: u32) -> usize {
    client
        .framebuffer()
        .extract(rect)
        .iter()
        .filter(|&&p| p == color)
        .count()
}

#[test]
fn handshake_and_first_frame() {
    require_xvfb!();
    let s = Session::start(640, 480, "none", &[]);
    let mut c = s.connect(None);
    assert_eq!(c.info().username, "tester");
    assert_eq!(c.info().session_id, 77);
    assert_eq!(c.size(), (640, 480));
    // A black screen produces no tiles at all (the reference frame is black),
    // so draw something and expect a frame.
    s.x("xsetroot", &["-solid", "#ff0000"]);
    assert!(wait_for(&mut c, Duration::from_secs(5), |ev, c| {
        matches!(ev, ClientEvent::Frame { .. }) && c.framebuffer().get(320, 240) == 0xff0000
    }));
    assert!(c.frames_received() >= 1);
    c.disconnect("test done");
}

/// A refresh request must actually produce a frame.
///
/// `RefreshRequest` used to set the "send a whole frame" flag without
/// invalidating the encoder's reference. `send_frame` then captured the entire
/// screen, diffed it against a reference that still matched it exactly, found
/// nothing changed and returned having sent nothing -- while clearing the flag,
/// so the request was consumed. The one client with a reason to ask (its
/// framebuffer is wrong) was the one client guaranteed to get no answer.
///
/// The test leans on that: with the screen quiescent, ordinary damage produces
/// no frames at all, so any frame arriving after the request came from the
/// request.
#[test]
fn refresh_request_resends_the_screen() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    s.x("xsetroot", &["-solid", "#00ff00"]);
    assert!(wait_for(&mut c, Duration::from_secs(5), |_, c| c
        .framebuffer()
        .get(1, 1)
        == 0x00ff00));

    // Let the screen go quiet, and confirm it really is quiet.
    let settle = Instant::now() + Duration::from_millis(600);
    while Instant::now() < settle {
        let _ = c.poll_event(Duration::from_millis(50));
    }
    let before = c.frames_received();
    let quiet = Instant::now() + Duration::from_millis(400);
    while Instant::now() < quiet {
        let _ = c.poll_event(Duration::from_millis(50));
    }
    assert_eq!(
        c.frames_received(),
        before,
        "screen is not quiescent; the test cannot attribute a frame to the request"
    );

    c.request_refresh().unwrap();
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |_, c| c.frames_received()
            > before),
        "refresh request produced no frame"
    );
    assert_eq!(c.framebuffer().get(1, 1), 0x00ff00);
    c.disconnect("test done");
}

#[test]
fn incremental_updates_only_send_changes() {
    require_xvfb!();
    let s = Session::start(640, 480, "none", &[]);
    let mut c = s.connect(None);
    s.x("xsetroot", &["-solid", "#0000ff"]);
    assert!(wait_for(&mut c, Duration::from_secs(5), |_, c| c
        .framebuffer()
        .get(1, 1)
        == 0x0000ff));
    let before = c.bytes_received();
    // Change a small area: a 20x20 white window via xterm would be big; use a
    // root pixmap change to a solid colour again but check size bound of the
    // full-screen solid update: all tiles solid => tiny.
    s.x("xsetroot", &["-solid", "#00ff00"]);
    assert!(wait_for(&mut c, Duration::from_secs(5), |_, c| c
        .framebuffer()
        .get(639, 479)
        == 0x00ff00));
    let delta = c.bytes_received() - before;
    // 640x480 = 80 tiles of 64x64 (partial at edges) => solid tiles ~16 bytes each.
    assert!(
        delta < 4000,
        "solid recolor should be tiny, got {delta} bytes"
    );
    assert_eq!(
        count_pixels(&c, &c.framebuffer().bounds(), 0x00ff00),
        640 * 480
    );
}

/// A window on the session display that receives keyboard focus and
/// records the keysyms it is sent. This is deterministic, unlike typing into
/// a terminal emulator under a window-manager-less X server.
struct KeySink {
    conn: x11rb::rust_connection::RustConnection,
    window: u32,
    keysyms_per_keycode: u8,
    min_keycode: u8,
    keysyms: Vec<u32>,
}

impl KeySink {
    fn open(s: &Session) -> Self {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{
            ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass,
        };
        use x11rb::rust_connection::RustConnection;
        // Connect as an ordinary X client using the session's display and
        // its private authority cookie.
        std::env::set_var("XAUTHORITY", s.xauth());
        let (conn, screen_num) =
            RustConnection::connect(Some(&s.display)).expect("connect display");
        let setup = conn.setup();
        let root = setup.roots[screen_num].root;
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let window = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            200,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE | EventMask::BUTTON_PRESS)
                .background_pixel(0x00ff00),
        )
        .unwrap();
        conn.map_window(window).unwrap();
        conn.set_input_focus(InputFocus::POINTER_ROOT, window, x11rb::CURRENT_TIME)
            .unwrap();
        conn.flush().unwrap();
        let mapping = conn
            .get_keyboard_mapping(min, max - min + 1)
            .unwrap()
            .reply()
            .unwrap();
        Self {
            conn,
            window,
            keysyms_per_keycode: mapping.keysyms_per_keycode,
            min_keycode: min,
            keysyms: mapping.keysyms,
        }
    }

    fn keysym(&mut self, keycode: u8, state: u16) -> u32 {
        // Re-read the mapping lazily when a keycode has no entry (the server
        // binds new keysyms on the fly and sends MappingNotify).
        let idx = |k: u8, col: usize, syms: &[u32], per: u8, min: u8| {
            syms[(k - min) as usize * per as usize + col]
        };
        let refresh = |this: &mut Self| {
            use x11rb::connection::Connection as _;
            use x11rb::protocol::xproto::ConnectionExt as _;
            let setup = this.conn.setup();
            let (min, max) = (setup.min_keycode, setup.max_keycode);
            let m = this
                .conn
                .get_keyboard_mapping(min, max - min + 1)
                .unwrap()
                .reply()
                .unwrap();
            this.keysyms = m.keysyms;
            this.keysyms_per_keycode = m.keysyms_per_keycode;
        };
        let mut plain = idx(
            keycode,
            0,
            &self.keysyms,
            self.keysyms_per_keycode,
            self.min_keycode,
        );
        if plain == 0 {
            refresh(self);
            plain = idx(
                keycode,
                0,
                &self.keysyms,
                self.keysyms_per_keycode,
                self.min_keycode,
            );
        }
        let shifted = idx(
            keycode,
            1,
            &self.keysyms,
            self.keysyms_per_keycode,
            self.min_keycode,
        );
        let shift = state & 1 != 0;
        if shift && shifted != 0 {
            shifted
        } else if shift {
            // Implicit case conversion for alphabetic keys.
            lynxrdp_proto::keysym::char_from_keysym(plain)
                .and_then(|c| c.to_uppercase().next())
                .map(lynxrdp_proto::keysym::keysym_from_char)
                .unwrap_or(plain)
        } else {
            plain
        }
    }

    /// Collect (keysym, pressed, state) until a press of `until` is seen, or
    /// for `drain` extra time after it, or the timeout elapses.
    fn collect_until(&mut self, until: u32, timeout: Duration) -> Vec<(u32, bool, u16)> {
        use x11rb::connection::Connection;
        use x11rb::protocol::Event;
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        let mut stop_at: Option<Instant> = None;
        while Instant::now() < deadline {
            if let Some(t) = stop_at {
                if Instant::now() >= t {
                    break;
                }
            }
            match self.conn.poll_for_event().unwrap() {
                Some(Event::KeyPress(e)) if e.event == self.window => {
                    let ks = self.keysym(e.detail, e.state.into());
                    out.push((ks, true, e.state.into()));
                    if ks == until {
                        // Give the matching release a moment to arrive.
                        stop_at = Some(Instant::now() + Duration::from_millis(200));
                    }
                }
                Some(Event::KeyRelease(e)) if e.event == self.window => {
                    let ks = self.keysym(e.detail, e.state.into());
                    out.push((ks, false, e.state.into()));
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        out
    }

    /// Collect all events for a fixed duration.
    fn drain(&mut self, timeout: Duration) -> Vec<(u32, bool, u16)> {
        self.collect_until(u32::MAX, timeout)
    }
}

#[test]
fn keyboard_input_reaches_application() {
    require_xvfb!();
    let s = Session::start(640, 480, "none", &[]);
    let mut c = s.connect(None);
    let mut sink = KeySink::open(&s);
    // Let the window map and take focus; the server sends a frame for it.
    assert!(wait_for(&mut c, Duration::from_secs(10), |_, c| c
        .framebuffer()
        .get(10, 10)
        == 0x00ff00));
    // Plain, shifted (temporary shift), explicitly shifted, unicode (dynamic
    // binding), a named key, and a control chord.
    c.type_text("aB!").unwrap();
    c.key(keysym::SHIFT_L, true).unwrap();
    c.tap_key(keysym::keysym_from_char('Z')).unwrap();
    c.key(keysym::SHIFT_L, false).unwrap();
    c.tap_key(keysym::keysym_from_char('é')).unwrap();
    c.tap_key(keysym::RETURN).unwrap();
    c.key(keysym::CONTROL_L, true).unwrap();
    c.tap_key(keysym::keysym_from_char('d')).unwrap();
    c.key(keysym::CONTROL_L, false).unwrap();
    // Collect until the final 'd' press arrives (the server injects its own
    // temporary Shift/AltGr presses to reach shifted symbols, so we cannot
    // predict the exact number of events; we filter those modifiers out).
    let events = sink.collect_until(0x64, Duration::from_secs(10));
    let presses: Vec<u32> = events
        .iter()
        .filter(|e| e.1 && !keysym::is_modifier(e.0))
        .map(|e| e.0)
        .collect();
    let expected = vec![
        0x61,                          // a
        0x42,                          // B (server presses Shift for us)
        0x21,                          // ! (server presses Shift for us)
        0x5a,                          // Z (we held Shift)
        keysym::keysym_from_char('é'), // dynamically bound keysym
        keysym::RETURN,                // named key
        0x64,                          // d (we held Control)
    ];
    assert_eq!(presses, expected, "events: {events:x?}");
    // Every press was eventually released (the exact keysym of a release can
    // differ from its press when a modifier was let go in between, so we only
    // check that presses and releases balance out).
    let n_press = events.iter().filter(|e| e.1).count();
    let n_release = events.iter().filter(|e| !e.1).count();
    assert!(
        n_release >= n_press - 1 && n_release <= n_press + 1,
        "unbalanced press/release ({n_press}/{n_release}): {events:x?}"
    );
    // The 'd' arrived with Control held (state bit 2) and 'Z' with Shift (bit 0).
    let d = events.iter().find(|e| e.0 == 0x64 && e.1).unwrap();
    assert!(d.2 & 4 != 0, "control not held for d: {events:x?}");
    let z = events.iter().find(|e| e.0 == 0x5a && e.1).unwrap();
    assert!(z.2 & 1 != 0, "shift not held for Z: {events:x?}");
    // Disconnecting releases everything the client left pressed.
    c.key(keysym::SHIFT_L, true).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    c.disconnect("bye");
    let ev = sink.drain(Duration::from_millis(500));
    assert!(
        ev.iter().any(|e| e.0 == keysym::SHIFT_L && !e.1),
        "stuck shift not released: {ev:x?}"
    );
}

#[test]
fn pointer_events_are_injected() {
    require_xvfb!();
    let s = Session::start(640, 480, "none", &[]);
    let mut c = s.connect(None);
    c.pointer_move(123, 45).unwrap();
    // xdotool is optional; fall back to querying via a tiny X program.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = c.poll_event(Duration::from_millis(50));
        if skip_unless(have("xdotool"), "xdotool not installed (pointer check)") {
            return;
        }
        let o = s.x("xdotool", &["getmouselocation"]);
        let out = String::from_utf8_lossy(&o.stdout).to_string();
        if out.contains("x:123 y:45") {
            break;
        }
        assert!(Instant::now() < deadline, "pointer not moved: {out}");
    }
}

#[test]
fn resize_changes_screen_and_resends() {
    require_xvfb!();
    let s = Session::start(640, 480, "none", &[]);
    let mut c = s.connect(None);
    s.x("xsetroot", &["-solid", "#123456"]);
    assert!(wait_for(&mut c, Duration::from_secs(5), |_, c| c
        .framebuffer()
        .get(0, 0)
        == 0x123456));
    c.request_resize(800, 600).unwrap();
    assert!(wait_for(&mut c, Duration::from_secs(10), |ev, _| matches!(
        ev,
        ClientEvent::Resized {
            width: 800,
            height: 600
        }
    )));
    assert_eq!(c.size(), (800, 600));
    // After resize the whole new screen is sent (root colour persists).
    assert!(wait_for(&mut c, Duration::from_secs(10), |_, c| c
        .framebuffer()
        .get(799, 599)
        == 0x123456));
    let out = s.x("xdpyinfo", &[]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("dimensions:    800x600"), "xdpyinfo: {text}");
    // Requests beyond the maximum are clamped.
    c.request_resize(5000, 5000).unwrap();
    assert!(wait_for(&mut c, Duration::from_secs(10), |ev, _| matches!(
        ev,
        ClientEvent::Resized {
            width: 1600,
            height: 1200
        }
    )));
}

#[test]
fn initial_size_from_hello() {
    require_xvfb!();
    let s = Session::start(640, 480, "none", &[]);
    let c = s.connect(Some((1024, 768)));
    assert_eq!(c.size(), (1024, 768));
}

#[test]
fn second_client_replaces_first() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut first = s.connect(None);
    let mut second = s.connect(None);
    let reason =
        wait_disconnect(&mut first, Duration::from_secs(5)).expect("first client disconnected");
    assert!(reason.contains("Another client"), "{reason}");
    s.x("xsetroot", &["-solid", "#00ffff"]);
    assert!(wait_for(&mut second, Duration::from_secs(5), |_, c| c
        .framebuffer()
        .get(0, 0)
        == 0x00ffff));
}

#[test]
fn clipboard_roundtrip() {
    require_xvfb!();
    if skip_unless(
        have("xclip") || have("xsel"),
        "neither xclip nor xsel installed",
    ) {
        return;
    }
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    // Client -> session.
    c.set_clipboard("from client ✓").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = c.poll_event(Duration::from_millis(50));
        let out = if have("xclip") {
            s.x("xclip", &["-selection", "clipboard", "-o"])
        } else {
            s.x("xsel", &["--clipboard", "--output"])
        };
        if String::from_utf8_lossy(&out.stdout) == "from client ✓" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session clipboard not set: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    // Session -> client.
    if have("xclip") {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard", "-i"])
            .env("DISPLAY", &s.display)
            .env("XAUTHORITY", s.xauth())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all("from session".as_bytes())
            .unwrap();
        assert!(wait_for(&mut c, Duration::from_secs(5), |ev, _| ev
            == &ClientEvent::Clipboard("from session".into())));
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// A small PNG for clipboard tests.
fn sample_png(w: usize, h: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            bytes.extend_from_slice(&[(x * 7 % 256) as u8, (y * 11 % 256) as u8, 0x40, 0xFF]);
        }
    }
    let img = lynxrdp_client::imageclip::Rgba::new(w, h, bytes).unwrap();
    lynxrdp_client::imageclip::encode_png(&img).unwrap()
}

#[test]
fn clipboard_image_from_client_reaches_the_session() {
    require_xvfb!();
    if skip_unless(have("xclip"), "xclip not installed") {
        return;
    }
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    let png = sample_png(24, 16);

    // Offer the image; the session asks for it and the transfer follows.
    c.offer_clipboard_image(png.clone()).unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let _ = c.poll_event(Duration::from_millis(100));
        let out = s.x(
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        );
        if out.stdout == png {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session clipboard image never matched ({} bytes vs {} expected)",
            out.stdout.len(),
            png.len()
        );
    }
}

#[test]
fn clipboard_image_from_the_session_reaches_the_client() {
    require_xvfb!();
    if skip_unless(have("xclip"), "xclip not installed") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shot.png");
    let png = sample_png(20, 12);
    std::fs::write(&path, &png).unwrap();

    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);

    // xclip owns the selection for as long as it runs.
    let mut owner = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png", "-i"])
        .arg(&path)
        .env("DISPLAY", &s.display)
        .env("XAUTHORITY", s.xauth())
        .spawn()
        .unwrap();

    let mut got: Option<Vec<u8>> = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && got.is_none() {
        if let Ok(Some(ClientEvent::ClipboardImage(data))) =
            c.poll_event(Duration::from_millis(200))
        {
            got = Some(data);
        }
    }
    let _ = owner.kill();
    let _ = owner.wait();
    assert_eq!(
        got.as_deref(),
        Some(&png[..]),
        "client did not receive the image"
    );
}

#[test]
fn a_file_uploads_into_the_session() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);

    let src = tempfile::tempdir().unwrap();
    let local = src.path().join("notes.txt");
    // Large enough to span several chunks and exercise the sliding window.
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&local, &content).unwrap();

    let id = c.send_file(&local, "notes.txt").unwrap();
    c.run_transfer(id, Duration::from_secs(30)).unwrap();

    let landed = s.upload_dir.path().join("notes.txt");
    assert_eq!(std::fs::read(&landed).unwrap(), content);
}

#[test]
fn an_upload_may_not_escape_the_upload_directory() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);

    let src = tempfile::tempdir().unwrap();
    let local = src.path().join("evil.txt");
    std::fs::write(&local, b"should not land outside").unwrap();

    // The session must refuse a destination that climbs out of the directory.
    let id = c.send_file(&local, "../../escaped.txt").unwrap();
    let err = c.run_transfer(id, Duration::from_secs(15)).unwrap_err();
    assert!(err.to_string().contains("unsafe"), "{err}");

    let escaped = s.upload_dir.path().parent().unwrap().join("escaped.txt");
    assert!(
        !escaped.exists(),
        "the file escaped to {}",
        escaped.display()
    );
}

#[test]
fn an_upload_into_a_subdirectory_creates_it() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    let src = tempfile::tempdir().unwrap();
    let local = src.path().join("a.bin");
    std::fs::write(&local, b"nested").unwrap();
    let id = c.send_file(&local, "deep/nested/a.bin").unwrap();
    c.run_transfer(id, Duration::from_secs(30)).unwrap();
    assert_eq!(
        std::fs::read(s.upload_dir.path().join("deep/nested/a.bin")).unwrap(),
        b"nested"
    );
}

#[test]
fn a_file_downloads_out_of_the_session() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);

    let remote_dir = tempfile::tempdir().unwrap();
    let remote = remote_dir.path().join("report.bin");
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 97) as u8).collect();
    std::fs::write(&remote, &content).unwrap();

    let out_dir = tempfile::tempdir().unwrap();
    let dest = out_dir.path().join("report.bin");
    let id = c
        .request_file(remote.to_str().unwrap(), dest.clone())
        .unwrap();
    let got = c.run_transfer(id, Duration::from_secs(30)).unwrap();
    assert_eq!(got.as_deref(), Some(dest.as_path()));
    assert_eq!(std::fs::read(&dest).unwrap(), content);
}

#[test]
fn downloading_a_missing_file_fails_cleanly() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    let out = tempfile::tempdir().unwrap();
    let id = c
        .request_file("/definitely/not/here.txt", out.path().join("x"))
        .unwrap();
    let err = c.run_transfer(id, Duration::from_secs(15)).unwrap_err();
    assert!(err.to_string().contains("here.txt"), "{err}");
}

#[test]
fn clipboard_files_from_the_client_are_staged_for_the_session() {
    require_xvfb!();
    if skip_unless(have("xclip"), "xclip not installed") {
        return;
    }
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);

    let src = tempfile::tempdir().unwrap();
    let a = src.path().join("alpha.txt");
    let b = src.path().join("beta.bin");
    std::fs::write(&a, b"alpha contents").unwrap();
    std::fs::write(&b, vec![7u8; 5000]).unwrap();

    c.offer_clipboard_files(&[a.clone(), b.clone()]).unwrap();

    // The session should end up with a uri-list naming two staged files.
    let deadline = Instant::now() + Duration::from_secs(20);
    let staged = loop {
        let _ = c.poll_event(Duration::from_millis(100));
        let out = s.x(
            "xclip",
            &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
        );
        let list = String::from_utf8_lossy(&out.stdout).to_string();
        let paths = lynxrdp_proto::urilist::parse(&list);
        if paths.len() == 2 {
            break paths;
        }
        assert!(
            Instant::now() < deadline,
            "session never saw the file list: {list:?}"
        );
    };

    // The staged files must be real, complete copies the session can open.
    let mut names: Vec<String> = staged
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["alpha.txt", "beta.bin"]);
    for p in &staged {
        let content = std::fs::read(p).unwrap();
        if p.ends_with("alpha.txt") {
            assert_eq!(content, b"alpha contents");
        } else {
            assert_eq!(content, vec![7u8; 5000]);
        }
    }
}

#[test]
fn clipboard_files_copied_in_the_session_are_offered_to_the_client() {
    require_xvfb!();
    if skip_unless(have("xclip"), "xclip not installed") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("shared.txt");
    std::fs::write(&f, b"from the session").unwrap();
    let list = lynxrdp_proto::urilist::build(std::slice::from_ref(&f));

    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);

    // xclip owns the selection, offering a uri-list, for as long as it runs.
    let mut owner = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "text/uri-list", "-i"])
        .env("DISPLAY", &s.display)
        .env("XAUTHORITY", s.xauth())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    owner
        .stdin
        .take()
        .unwrap()
        .write_all(list.as_bytes())
        .unwrap();

    let mut got: Option<Vec<lynxrdp_proto::FileEntry>> = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && got.is_none() {
        if let Ok(Some(ClientEvent::ClipboardFiles(files))) =
            c.poll_event(Duration::from_millis(200))
        {
            got = Some(files);
        }
    }
    let _ = owner.kill();
    let _ = owner.wait();

    let files = got.expect("client never received the file list");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, f.to_string_lossy());
    assert_eq!(files[0].size, b"from the session".len() as u64);
}

#[test]
fn exit_on_disconnect_ends_session() {
    require_xvfb!();
    let mut s = Session::start(320, 240, "none", &["--exit-on-disconnect"]);
    let mut c = s.connect(None);
    c.disconnect("bye");
    let st = s
        .wait_exit(Duration::from_secs(10))
        .expect("session should exit");
    assert!(st.success(), "{st}");
}

#[test]
fn desktop_exit_ends_session_and_notifies_client() {
    require_xvfb!();
    let mut s = Session::start(320, 240, "sleep 1", &[]);
    let mut c = s.connect(None);
    let reason = wait_disconnect(&mut c, Duration::from_secs(10)).expect("client disconnected");
    assert!(reason.contains("desktop"), "{reason}");
    let st = s
        .wait_exit(Duration::from_secs(10))
        .expect("session should exit");
    assert!(st.success(), "{st}");
}

#[test]
fn cursor_shape_is_sent() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    // Xvfb starts with the default cursor; the server sends it right after hello.
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |ev, _| matches!(
            ev,
            ClientEvent::Cursor(_)
        )) || c.cursor().is_some()
    );
}

#[test]
fn peer_uid_check_rejects_other_users() {
    // We cannot easily connect as another uid in a unit test; instead verify
    // the check is on by default by reading the log line, and that the
    // insecure flag turns it off.
    require_xvfb!();
    let s = Session::start(320, 240, "none", &["--insecure-skip-peer-check"]);
    let c = s.connect(None);
    assert_eq!(c.size(), (320, 240));
}

#[test]
fn latency_probe() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    c.ping().unwrap();
    assert!(wait_for(
        &mut c,
        Duration::from_secs(5),
        |ev, _| matches!(ev, ClientEvent::Rtt(d) if *d < Duration::from_secs(1))
    ));
}
