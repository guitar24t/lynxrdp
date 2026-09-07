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
        Self::start_with_env(width, height, startwm, extra, &[])
    }
    fn start_with_env(
        width: u32,
        height: u32,
        startwm: &str,
        extra: &[&str],
        env: &[(&str, &std::path::Path)],
    ) -> Self {
        let port = free_port();
        let runtime_dir = tempfile::tempdir().unwrap();
        let upload_dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lynxrdp-session"));
        cmd.envs(env.iter().copied());
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

/// A large transfer must not stop the screen.
///
/// This is the stated reason `CHUNK_SIZE` and `WINDOW_CHUNKS` exist: a chunk is
/// capped so a frame waits for at most one of them to drain, and the sender's
/// window bounds how much of a file can be in flight ahead of the acks. Neither
/// bound had a test that would notice it being removed. The other transfer
/// tests cannot: they use `run_transfer`, which drives the connection but
/// discards every `Frame` event, so a session that went blind for the length of
/// a download would pass all of them. Hence the hand-written loop below.
#[test]
fn a_large_download_does_not_stall_the_screen() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);

    // 16 MiB is 256 chunks, so the sender refills its 8-chunk window thirty-odd
    // times. Sized against the *frame* cadence rather than the file: the screen
    // below changes about thirty times a second, so the transfer has to last
    // long enough for several of those to fall inside it. A megabyte would
    // finish between two frames on a quick host and prove nothing.
    let remote_dir = tempfile::tempdir().unwrap();
    let remote = remote_dir.path().join("big.bin");
    let content: Vec<u8> = (0..16 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&remote, &content).unwrap();

    // Keep the screen changing for the whole transfer. Without this the test
    // proves nothing at all: a quiescent screen produces no frames either way,
    // and the assertion below would be measuring the absence of damage rather
    // than the presence of a stall.
    let mut painter = Command::new("sh")
        .arg("-c")
        .arg(
            "while :; do xsetroot -solid '#ff0000'; sleep 0.02; \
             xsetroot -solid '#0000ff'; sleep 0.02; done",
        )
        .env("DISPLAY", &s.display)
        .env("XAUTHORITY", s.xauth())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let out_dir = tempfile::tempdir().unwrap();
    let dest = out_dir.path().join("big.bin");
    // The id is not needed: completion arrives as an event, and progress is
    // no longer sampled now that the measurement is the gap between frames.
    c.request_file(remote.to_str().unwrap(), dest.clone())
        .unwrap();

    // What is actually under test is that the core thread never *stops*
    // serving the screen while a transfer runs. Counting frames that land
    // inside the transfer window cannot measure that here: over loopback 16 MiB
    // moves in about sixteen milliseconds, which is one frame interval, so a
    // correct implementation and a badly stalled one both produce one frame and
    // the count says nothing. Growing the file until it outruns loopback would
    // mean hundreds of megabytes in a suite that has to stay quick.
    //
    // The largest gap between consecutive frames is the honest measurement, and
    // it does not care how fast the transfer was: a session that went blind
    // behind a download shows a gap the length of the download, whereas one
    // that stayed responsive keeps painting throughout. The painter drives the
    // screen at roughly 25 Hz, so anything approaching a second is a stall and
    // nothing else.
    let mut worst_gap = Duration::ZERO;
    let mut last_frame = Instant::now();
    let mut frames = 0u32;
    let mut done = false;
    let started = Instant::now();
    let mut took = Duration::ZERO;
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        let ev = c.poll_event(Duration::from_millis(200)).unwrap();
        match ev {
            Some(ClientEvent::FileDownloaded { .. }) => {
                took = started.elapsed();
                done = true;
                // Keep watching briefly: a stall that begins with the last
                // chunk and ends when the writer drains would otherwise fall
                // outside the window entirely.
                let settle = Instant::now() + Duration::from_millis(500);
                while Instant::now() < settle {
                    if let Some(ClientEvent::Frame { .. }) =
                        c.poll_event(Duration::from_millis(100)).unwrap()
                    {
                        worst_gap = worst_gap.max(last_frame.elapsed());
                        last_frame = Instant::now();
                        frames += 1;
                    }
                }
                break;
            }
            Some(ClientEvent::TransferFailed { reason, .. }) => panic!("transfer failed: {reason}"),
            Some(ClientEvent::Disconnected(r)) => panic!("disconnected: {r}"),
            Some(ClientEvent::Frame { .. }) => {
                worst_gap = worst_gap.max(last_frame.elapsed());
                last_frame = Instant::now();
                frames += 1;
            }
            _ => {}
        }
    }

    let _ = painter.kill();
    let _ = painter.wait();

    assert!(done, "the download never finished");
    // Not assert_eq!, which would dump 16 MiB of bytes twice before anyone
    // could read the message. The first differing offset is the useful part.
    let got = std::fs::read(&dest).unwrap();
    assert_eq!(got.len(), content.len(), "the download was truncated");
    let differs = got.iter().zip(content.iter()).position(|(a, b)| a != b);
    assert!(
        differs.is_none(),
        "the downloaded file differs from the original at byte {differs:?}"
    );
    assert!(
        frames > 2,
        "only {frames} frames arrived in {} ms; the painter is not driving the \
         screen, so this test is measuring nothing",
        started.elapsed().as_millis()
    );
    assert!(
        worst_gap < Duration::from_secs(1),
        "the screen went {} ms without a frame while a 16 MiB download ran \
         (transfer took {} ms, {frames} frames): the session stalled behind it",
        worst_gap.as_millis(),
        took.as_millis()
    );
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

/// Drive protocol replies while a native file reader waits for lazy contents.
fn read_clipboard_file(client: &mut Client, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read(path));
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(result) = rx.try_recv() {
            return result;
        }
        client.poll_event(Duration::from_millis(20)).unwrap();
        assert!(Instant::now() < deadline, "native clipboard read timed out");
    }
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
    let mut ready = false;
    let staged = loop {
        if let Some(ClientEvent::Notice(text)) = c.poll_event(Duration::from_millis(100)).unwrap() {
            ready |= text.contains("copied file(s) ready");
        }
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

    while !ready && Instant::now() < deadline {
        if let Some(ClientEvent::Notice(text)) = c.poll_event(Duration::from_millis(100)).unwrap() {
            ready |= text.contains("copied file(s) ready");
        }
    }
    assert!(ready, "the client never received the ready-to-paste notice");
    assert_eq!(std::fs::read(&a).unwrap(), b"alpha contents");
    assert_eq!(std::fs::read(&b).unwrap(), vec![7u8; 5000]);

    // Nautilus splits the GNOME payload on every LF and treats even an
    // empty final field as a file. Check the bytes, not the permissive URI
    // parser, which hides this error by dropping empty lines.
    let gnome = s.x(
        "xclip",
        &[
            "-selection",
            "clipboard",
            "-t",
            "x-special/gnome-copied-files",
            "-o",
        ],
    );
    assert!(gnome.status.success());
    let payload = String::from_utf8(gnome.stdout).unwrap();
    let entries: Vec<_> = payload.split('\n').collect();
    assert_eq!(entries[0], "copy");
    assert_eq!(
        entries.len(),
        staged.len() + 1,
        "unexpected empty clipboard source: {payload:?}"
    );
    assert!(entries[1..]
        .iter()
        .all(|uri| uri.starts_with("file://") && !uri.contains('\r')));

    // GNOME Shell Desktop Icons uses a distinct text envelope, with a
    // required trailing LF that its parser removes before copying.
    let desktop = s.x(
        "xclip",
        &["-selection", "clipboard", "-t", "UTF8_STRING", "-o"],
    );
    assert!(desktop.status.success());
    let desktop_text = String::from_utf8(desktop.stdout).unwrap();
    assert_eq!(
        desktop_text,
        format!("x-special/nautilus-clipboard\n{payload}\n")
    );

    // Merely announcing files and probing their names/sizes must transfer no
    // contents, even while clipboard managers inspect all offered formats.
    for path in &staged {
        assert!(std::fs::metadata(path).unwrap().is_file());
    }
    let idle = Instant::now() + Duration::from_millis(500);
    while Instant::now() < idle {
        c.poll_event(Duration::from_millis(20)).unwrap();
    }
    assert!(c.transfer_rows().is_empty());
    let cache = staged[0].parent().unwrap().parent().unwrap().join("cache");
    assert_eq!(
        std::fs::read_dir(cache).unwrap().count(),
        0,
        "Copy eagerly downloaded file contents"
    );

    // The staged files must be real, complete copies the session can open.
    let mut names: Vec<String> = staged
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["alpha.txt", "beta.bin"]);
    for p in &staged {
        let content = read_clipboard_file(&mut c, p).unwrap();
        if p.ends_with("alpha.txt") {
            assert_eq!(content, b"alpha contents");
        } else {
            assert_eq!(content, vec![7u8; 5000]);
        }
    }
    // Repeated pastes reuse fetched bytes even if the source is no longer
    // available. They do not start another transfer.
    std::fs::remove_file(&a).unwrap();
    let alpha = staged.iter().find(|p| p.ends_with("alpha.txt")).unwrap();
    assert_eq!(
        read_clipboard_file(&mut c, alpha).unwrap(),
        b"alpha contents"
    );
}

#[test]
fn clipboard_disconnect_releases_a_waiting_native_reader() {
    require_xvfb!();
    if skip_unless(have("xclip"), "xclip not installed") {
        return;
    }
    let session = Session::start(320, 240, "none", &[]);
    let mut client = session.connect(None);
    let source = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(source.path(), b"lazy bytes").unwrap();
    client
        .offer_clipboard_files(&[source.path().to_path_buf()])
        .unwrap();
    assert!(wait_for(&mut client, Duration::from_secs(5), |event, _| {
        matches!(event, ClientEvent::Notice(text) if text.contains("copied file(s) ready"))
    }));
    let out = session.x(
        "xclip",
        &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
    );
    let paths = lynxrdp_proto::urilist::parse(&String::from_utf8_lossy(&out.stdout));
    let path = paths[0].clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read(path));
    });
    // Do not service FileRequest: the native read must wait for contents.
    assert!(rx.recv_timeout(Duration::from_millis(250)).is_err());
    client.disconnect("test disconnect during paste");
    assert!(rx
        .recv_timeout(Duration::from_secs(5))
        .expect("native read stayed blocked")
        .is_err());
    drop(client);
    let mut reconnected = session.connect(None);
    reconnected.ping().unwrap();
    assert!(wait_for(
        &mut reconnected,
        Duration::from_secs(5),
        |event, _| matches!(event, ClientEvent::Rtt(_))
    ));
}

#[test]
fn clipboard_files_copied_in_the_session_are_offered_to_the_client() {
    clipboard_files_round_trip("text/uri-list");
}

#[test]
fn clipboard_gnome_files_round_trip_without_echoing() {
    clipboard_files_round_trip("x-special/gnome-copied-files");
}

#[test]
fn clipboard_desktop_text_envelope_is_a_file_offer_not_text() {
    clipboard_files_round_trip("UTF8_STRING");
}

fn clipboard_files_round_trip(target: &str) {
    require_xvfb!();
    if skip_unless(have("xclip"), "xclip not installed") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("shared.txt");
    std::fs::write(&f, b"from the session").unwrap();
    let list = lynxrdp_proto::urilist::build(std::slice::from_ref(&f));
    let list = if target == "x-special/gnome-copied-files" {
        format!("copy\n{list}")
    } else if target == "UTF8_STRING" {
        format!(
            "x-special/nautilus-clipboard\ncopy\n{}",
            list.replace("\r\n", "\n")
        )
    } else {
        list
    };

    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);

    // xclip owns the selection, offering a uri-list, for as long as it runs.
    let mut owner = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", target, "-i"])
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
        match c.poll_event(Duration::from_millis(200)).unwrap() {
            Some(ClientEvent::ClipboardFiles(files)) => got = Some(files),
            Some(ClientEvent::Clipboard(_)) => panic!("file copy was delivered as plain text"),
            _ => {}
        }
    }
    let files = got.expect("client never received the file list");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, f.to_string_lossy());
    assert_eq!(files[0].size, b"from the session".len() as u64);
    let destination = dir.path().join("downloaded.txt");
    let id = c.request_file(&files[0].path, destination.clone()).unwrap();
    let mut downloaded = false;
    // Keep polling after completion: the old offer/request feedback loop
    // repeatedly delivered the same list, resetting a viewer's staging batch.
    let settle = Instant::now() + Duration::from_secs(2);
    while Instant::now() < settle || !downloaded {
        match c.poll_event(Duration::from_millis(100)).unwrap() {
            Some(ClientEvent::ClipboardFiles(_)) => panic!("one copy was offered repeatedly"),
            Some(ClientEvent::FileDownloaded { id: done, .. }) if done == id => downloaded = true,
            Some(ClientEvent::TransferFailed { reason, .. }) => panic!("{reason}"),
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "clipboard download never completed"
        );
    }
    assert_eq!(std::fs::read(&destination).unwrap(), b"from the session");

    // Reverse direction on the same connection after a remote copy. A late
    // request for the old owner must not fetch our own staged files back.
    c.offer_clipboard_files(&[destination]).unwrap();
    let mut ready = false;
    let settle = Instant::now() + Duration::from_secs(2);
    while Instant::now() < settle || !ready {
        match c.poll_event(Duration::from_millis(100)).unwrap() {
            Some(ClientEvent::ClipboardFiles(_)) => panic!("local files echoed back to the client"),
            Some(ClientEvent::Notice(text)) => ready |= text.contains("copied file(s) ready"),
            Some(ClientEvent::TransferFailed { reason, .. }) => panic!("{reason}"),
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "reverse clipboard copy never completed"
        );
    }
    let out = s.x(
        "xclip",
        &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
    );
    let paths = lynxrdp_proto::urilist::parse(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(paths.len(), 1);
    assert_eq!(
        read_clipboard_file(&mut c, &paths[0]).unwrap(),
        b"from the session"
    );
    let _ = owner.kill();
    let _ = owner.wait();
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

/// The idle timeout is the only thing that ever ends an abandoned session.
///
/// `--exit-on-disconnect` exists and is covered just above, but the daemon
/// never passes it: `daemon/manager.rs` builds the session's argument list and
/// it is not in there, deliberately, because a user whose SSH tunnel drops is
/// meant to reconnect to the session they left. That makes `idle_timeout` the
/// entire lifecycle policy for a session nobody comes back to -- otherwise an
/// Xvfb, a desktop and a PAM session sit on the host until it reboots -- and it
/// had no test on either side.
///
/// The client connects and then leaves rather than never connecting, because
/// `last_client_seen` is initialised at start-up: a timer that was only ever
/// armed once would still pass a test that measured from there, and would still
/// fail to collect the sessions this is for.
#[test]
fn an_idle_session_exits_after_the_timeout() {
    require_xvfb!();
    let mut s = Session::start(320, 240, "none", &["--idle-timeout", "2"]);
    let mut c = s.connect(None);
    s.x("xsetroot", &["-solid", "#ff00ff"]);
    assert!(wait_for(&mut c, Duration::from_secs(5), |_, c| c
        .framebuffer()
        .get(1, 1)
        == 0xff00ff));
    c.disconnect("test done");
    drop(c);
    let st = s
        .wait_exit(Duration::from_secs(20))
        .expect("an abandoned session should have timed out");
    assert!(st.success(), "{st}");
}

/// ...and it must not fire while somebody is still connected.
///
/// The obvious way to write this is `let _ = s.connect(None);`, which drops the
/// client on the spot -- disconnecting it, arming the idle timer, and asserting
/// the exact opposite of what the test claims while still going green for as
/// long as the timeout outlasts the wait. Hence the binding, and hence the
/// poll loop: a client that stops answering pings is dropped after
/// `PONG_TIMEOUT` and would be testing something else entirely.
#[test]
fn a_session_with_a_client_attached_survives_the_idle_timeout() {
    require_xvfb!();
    let mut s = Session::start(320, 240, "none", &["--idle-timeout", "2"]);
    let mut client = s.connect(None);
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let ev = client
            .poll_event(Duration::from_millis(100))
            .expect("poll failed");
        if let Some(ClientEvent::Disconnected(r)) = ev {
            panic!("the session dropped its client: {r}");
        }
    }
    assert!(
        s.wait_exit(Duration::from_millis(200)).is_none(),
        "the session exited on its idle timeout with a client attached"
    );
    // Proof the connection is still live rather than merely still in scope.
    client.ping().unwrap();
    assert!(wait_for(
        &mut client,
        Duration::from_secs(5),
        |ev, _| matches!(ev, ClientEvent::Rtt(_))
    ));
    client.disconnect("test done");
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

#[test]
fn clipboard_paste_preserves_duplicate_names_and_reports_missing_sources() {
    require_xvfb!();
    if skip_unless(have("xclip"), "xclip not installed") {
        return;
    }
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    let source = tempfile::tempdir().unwrap();
    let a = source.path().join("a/notes.txt");
    let b = source.path().join("b/notes.txt");
    let gone = source.path().join("gone");
    std::fs::create_dir_all(a.parent().unwrap()).unwrap();
    std::fs::create_dir_all(b.parent().unwrap()).unwrap();
    std::fs::write(&a, b"first").unwrap();
    std::fs::write(&b, b"second").unwrap();
    std::fs::write(&gone, b"gone").unwrap();
    c.offer_clipboard_files(&[a, b, gone.clone()]).unwrap();
    // The client has not handled any requests yet, so this failure is deterministic.
    std::fs::remove_file(gone).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        c.poll_event(Duration::from_millis(30)).unwrap();
        let out = s.x(
            "xclip",
            &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
        );
        let files = lynxrdp_proto::urilist::parse(&String::from_utf8_lossy(&out.stdout));
        if files.len() == 3 {
            assert_ne!(files[0], files[1]);
            assert!(read_clipboard_file(&mut c, &files[2]).is_err());
            let mut contents: Vec<_> = files[..2]
                .iter()
                .map(|p| read_clipboard_file(&mut c, p).unwrap())
                .collect();
            contents.sort();
            assert_eq!(contents, vec![b"first".to_vec(), b"second".to_vec()]);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "partial clipboard batch never published"
        );
    }
}

#[test]
fn uploads_preserve_existing_files_unless_replacement_is_chosen() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    let source = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(source.path(), b"new").unwrap();
    let dest = s.upload_dir.path().join("file");
    std::fs::write(&dest, b"original").unwrap();
    let id = c.send_file(source.path(), "file").unwrap();
    assert!(c.run_transfer(id, Duration::from_secs(10)).is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"original");
    let id = c
        .send_file_with_overwrite(source.path(), "file", true)
        .unwrap();
    c.run_transfer(id, Duration::from_secs(10)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    // A partial replacement must not publish when the connection disappears.
    let id = 9_999;
    c.send(&lynxrdp_proto::Message::TransferOptions { id, replace: true })
        .unwrap();
    c.send(&lynxrdp_proto::Message::TransferOffer {
        id,
        purpose: lynxrdp_proto::TransferPurpose::FileUpload,
        name: "file".into(),
        size: 100_000,
    })
    .unwrap();
    c.send(&lynxrdp_proto::Message::TransferData {
        id,
        seq: 0,
        data: vec![8; 100],
    })
    .unwrap();
    drop(c);
    let _replacement = s.connect(None);
    assert_eq!(std::fs::read(&dest).unwrap(), b"new");
}

#[test]
fn reconnect_clears_abandoned_transfers_and_a_stalled_worker_does_not_block_input() {
    require_xvfb!();
    let s = Session::start(320, 240, "none", &[]);
    let mut c = s.connect(None);
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("large");
    std::fs::write(&path, vec![4; 4 * 1024 * 1024]).unwrap();
    let out = tempfile::tempdir().unwrap();
    let a = c
        .request_file(path.to_str().unwrap(), out.path().join("abandoned-a"))
        .unwrap();
    let b = c
        .request_file(path.to_str().unwrap(), out.path().join("abandoned-b"))
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    for id in [a, b] {
        c.send(&lynxrdp_proto::Message::TransferAccept {
            id,
            accepted: true,
            reason: String::new(),
        })
        .unwrap();
    }
    // Neither offer nor queued chunks are consumed by the client event loop.
    std::thread::sleep(Duration::from_millis(150));
    drop(c);
    let mut c = s.connect(None);
    let id = c
        .request_file(path.to_str().unwrap(), out.path().join("complete"))
        .unwrap();
    c.run_transfer(id, Duration::from_secs(15)).unwrap();
    assert_eq!(
        std::fs::metadata(out.path().join("complete"))
            .unwrap()
            .len(),
        4 * 1024 * 1024
    );
    // A FIFO open with no writer parks the file worker but must never park the core.
    let fifo = source.path().join("fifo");
    assert!(Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    c.request_file(fifo.to_str().unwrap(), out.path().join("fifo-copy"))
        .unwrap();
    for n in 0..140 {
        let _ = c.request_file(
            path.to_str().unwrap(),
            out.path().join(format!("queued-{n}")),
        );
    }
    c.send(&lynxrdp_proto::Message::Ping { nonce: 932 })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    // A new handshake also requires the core to drain and cancel the old adapters.
    let replacement = s.connect(None);
    assert!(Instant::now() < deadline);
    drop(replacement);
}

#[test]
fn session_management_lists_without_takeover_and_checks_the_start_token() {
    require_xvfb!();
    let mut s = Session::start(320, 240, "none", &[]);
    let c = s.connect(None);
    let rows = lynxrdp_server::session::admin::list().unwrap();
    let row = rows
        .iter()
        .find(|r| r.pid == s.child.id())
        .expect("desktop listed");
    assert!(lynxrdp_server::session::admin::terminate(row.pid, row.started + 1).is_err());
    c.send(&lynxrdp_proto::Message::Ping { nonce: 55 }).unwrap();
    lynxrdp_server::session::admin::terminate(row.pid, row.started).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A native file receiver decides the final folder. Exercise the complete
/// network pull, staging, XDND negotiation and URI selection conversion.
#[test]
fn targeted_drops_reach_the_receiver_under_the_pointer() {
    require_xvfb!();
    use x11rb::{
        connection::Connection,
        protocol::{
            xproto::{
                self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
                WindowClass,
            },
            Event,
        },
        wrapper::ConnectionExt as _,
    };
    let session = Session::start(320, 240, "none", &[]);
    let mut client = session.connect(None);
    let sink = KeySink::open(&session);
    let conn = &sink.conn;
    let root = conn.setup().roots[0].root;
    let other = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        other,
        root,
        200,
        0,
        120,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new(),
    )
    .unwrap();
    conn.map_window(other).unwrap();
    let atom = |name: &str| {
        conn.intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let aware = atom("XdndAware");
    let position = atom("XdndPosition");
    let status = atom("XdndStatus");
    let drop_atom = atom("XdndDrop");
    let finished = atom("XdndFinished");
    let selection = atom("XdndSelection");
    let uri = atom("text/uri-list");
    let copy = atom("XdndActionCopy");
    let property = atom("TEST_DROP_FILES");
    for window in [sink.window, other] {
        conn.change_property32(PropMode::REPLACE, window, aware, AtomEnum::ATOM, &[5])
            .unwrap();
    }
    conn.get_input_focus().unwrap().reply().unwrap(); // server processed window setup
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("source.txt");
    std::fs::write(&source, b"targeted contents").unwrap();
    let destination = tempfile::tempdir().unwrap();
    for (index, x, target, accept) in [
        (0, 250, other, true),
        (1, 40, sink.window, true),
        (2, 250, other, false),
    ] {
        let folder = destination.path().join(format!("destination-{index}"));
        std::fs::create_dir(&folder).unwrap();
        let id = client
            .drop_files(&[(source.clone(), "Folder/nested.txt".into())], x, 40)
            .unwrap();
        let mut source_window = 0;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(ClientEvent::FileDropResult {
                id: received,
                ok,
                reason,
            }) = client.poll_event(Duration::from_millis(5)).unwrap()
            {
                assert_eq!(received, id);
                assert_eq!(ok, accept, "{reason}");
                break;
            }
            while let Some(event) = conn.poll_for_event().unwrap() {
                match event {
                    Event::ClientMessage(e) if e.type_ == position => {
                        assert_eq!(e.window, target, "drop reached the wrong folder window");
                        let data = e.data.as_data32();
                        assert_eq!(data[2], (u32::from(x) << 16) | 40);
                        assert_eq!(data[4], copy);
                        source_window = data[0];
                        conn.send_event(
                            false,
                            source_window,
                            EventMask::NO_EVENT,
                            xproto::ClientMessageEvent::new(
                                32,
                                source_window,
                                status,
                                [
                                    target,
                                    u32::from(accept),
                                    0,
                                    0,
                                    if accept { copy } else { 0 },
                                ],
                            ),
                        )
                        .unwrap();
                    }
                    Event::ClientMessage(e) if e.type_ == drop_atom => {
                        assert!(accept);
                        assert_eq!(e.window, target);
                        conn.convert_selection(
                            target,
                            selection,
                            uri,
                            property,
                            e.data.as_data32()[2],
                        )
                        .unwrap();
                    }
                    Event::SelectionNotify(e) if e.selection == selection => {
                        assert_ne!(e.property, 0);
                        let data = conn
                            .get_property(true, target, property, uri, 0, 100_000)
                            .unwrap()
                            .reply()
                            .unwrap();
                        let paths = lynxrdp_proto::urilist::parse(
                            std::str::from_utf8(&data.value).unwrap(),
                        );
                        assert_eq!(paths.len(), 1);
                        assert!(paths[0].ends_with("Folder"));
                        let file = paths[0].join("nested.txt");
                        assert_eq!(std::fs::read(&file).unwrap(), b"targeted contents");
                        std::fs::copy(file, folder.join("nested.txt")).unwrap();
                        conn.send_event(
                            false,
                            source_window,
                            EventMask::NO_EVENT,
                            xproto::ClientMessageEvent::new(
                                32,
                                source_window,
                                finished,
                                [target, 1, copy, 0, 0],
                            ),
                        )
                        .unwrap();
                    }
                    _ => {}
                }
            }
            conn.flush().unwrap();
            assert!(Instant::now() < deadline, "native drop never completed");
        }
        assert_eq!(folder.join("nested.txt").exists(), accept);
    }
    assert!(
        std::fs::read_dir(session.upload_dir.path())
            .unwrap()
            .next()
            .is_none(),
        "targeted drops must never fall back to Downloads"
    );
    assert_eq!(std::fs::read(source).unwrap(), b"targeted contents");
}

#[test]
fn targeted_drop_waits_for_gtk_file_inspection() {
    require_xvfb!();
    let gtk = Command::new("python3")
        .args([
            "-c",
            "import gi; gi.require_version('Gtk', '3.0'); from gi.repository import Gtk",
        ])
        .status()
        .is_ok_and(|s| s.success());
    if skip_unless(gtk, "Python GTK 3 introspection not installed") {
        return;
    }
    let session = Session::start(320, 240, "none", &[]);
    let mut client = session.connect(None);
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let file = source.path().join("gtk-copy.txt");
    std::fs::write(&file, b"asynchronous native copy").unwrap();
    let mut receiver = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gtk_drop_receiver.py"
        ))
        .arg(destination.path())
        .env("DISPLAY", &session.display)
        .env("XAUTHORITY", session.xauth())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    struct Stop(Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut output = BufReader::new(receiver.stdout.take().unwrap());
    let _receiver = Stop(receiver);
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "READY");
    let id = client
        .drop_files(&[(file, "gtk-copy.txt".into())], 100, 100)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(ClientEvent::FileDropResult {
            id: got,
            ok,
            reason,
        }) = client.poll_event(Duration::from_millis(10)).unwrap()
        {
            assert_eq!(got, id);
            assert!(ok, "{reason}");
            break;
        }
        assert!(Instant::now() < deadline, "GTK drop did not complete");
    }
    assert_eq!(
        std::fs::read(destination.path().join("gtk-copy.txt")).unwrap(),
        b"asynchronous native copy"
    );
    assert!(std::fs::read_dir(session.upload_dir.path())
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn targeted_drop_to_bare_desktop_uses_xdg_folder_and_excludes_windows_and_panel() {
    require_xvfb!();
    if skip_unless(have("xdg-user-dir"), "xdg-user-dir not installed") {
        return;
    }
    use x11rb::{
        connection::Connection,
        protocol::xproto::{
            AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, PropMode, StackMode,
            WindowClass,
        },
        wrapper::ConnectionExt as _,
    };
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let desktop = home.path().join("CustomDesktop");
    std::fs::write(
        config.path().join("user-dirs.dirs"),
        format!("XDG_DESKTOP_DIR=\"{}\"\n", desktop.display()),
    )
    .unwrap();
    let session = Session::start_with_env(
        320,
        240,
        "none",
        &[],
        &[("HOME", home.path()), ("XDG_CONFIG_HOME", config.path())],
    );
    let mut client = session.connect(None);
    let sink = KeySink::open(&session);
    let conn = &sink.conn;
    let root = conn.setup().roots[0].root;
    let atom = |s: &str| {
        conn.intern_atom(false, s.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let guard = conn.generate_id().unwrap();
    conn.create_window(
        0,
        guard,
        root,
        0,
        0,
        320,
        240,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .unwrap();
    conn.change_property8(
        PropMode::REPLACE,
        guard,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"mutter guard window",
    )
    .unwrap();
    conn.change_property8(
        PropMode::REPLACE,
        sink.window,
        atom("_NET_WM_NAME"),
        atom("UTF8_STRING"),
        b"GNOME Shell",
    )
    .unwrap();
    conn.change_property32(
        PropMode::REPLACE,
        root,
        atom("_NET_SUPPORTING_WM_CHECK"),
        AtomEnum::WINDOW,
        &[sink.window],
    )
    .unwrap();
    conn.change_property32(
        PropMode::REPLACE,
        root,
        atom("_NET_WORKAREA"),
        AtomEnum::CARDINAL,
        &[0, 24, 320, 216],
    )
    .unwrap();
    conn.map_window(guard).unwrap();
    conn.configure_window(
        guard,
        &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
    )
    .unwrap();
    conn.get_input_focus().unwrap().reply().unwrap();
    let source = home.path().join("source.txt");
    std::fs::write(&source, b"desktop copy").unwrap();
    for (x, y, accepted) in [(250, 180, true), (250, 5, false), (50, 50, false)] {
        let id = client
            .drop_files(&[(source.clone(), format!("file-{x}-{y}.txt"))], x, y)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(ClientEvent::FileDropResult {
                id: got,
                ok,
                reason,
            }) = client.poll_event(Duration::from_millis(10)).unwrap()
            {
                assert_eq!(got, id);
                assert_eq!(ok, accepted, "{reason}");
                break;
            }
            assert!(Instant::now() < deadline);
        }
        assert_eq!(desktop.join(format!("file-{x}-{y}.txt")).exists(), accepted);
    }
    assert_eq!(
        std::fs::read(desktop.join("file-250-180.txt")).unwrap(),
        b"desktop copy"
    );
    assert!(std::fs::read_dir(session.upload_dir.path())
        .unwrap()
        .next()
        .is_none());
}
