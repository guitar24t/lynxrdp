//! Observe X11 file selections without blocking the viewer on clipboard owners.
use anyhow::Result;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        xfixes::{self, ConnectionExt as _},
        xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, WindowClass},
        Event,
    },
    rust_connection::RustConnection,
};
#[derive(Default)]
struct Snapshot {
    revision: u64,
    available: bool,
    ready: bool,
    files: Option<Vec<PathBuf>>,
    error: Option<String>,
}
fn state() -> &'static Arc<Mutex<Snapshot>> {
    static STATE: OnceLock<Arc<Mutex<Snapshot>>> = OnceLock::new();
    STATE.get_or_init(|| {
        let state = Arc::new(Mutex::new(Snapshot {
            revision: 1,
            available: true,
            ..Default::default()
        }));
        let worker = state.clone();
        let result = std::thread::Builder::new()
            .name("file-clipboard-watch".into())
            .spawn(move || {
                if let Err(e) = watch(&worker) {
                    let mut s = worker.lock().unwrap();
                    s.revision += 1;
                    s.available = false;
                    s.ready = true;
                    s.error = Some(e.to_string());
                }
            });
        if let Err(e) = result {
            let mut s = state.lock().unwrap();
            s.ready = true;
            s.available = false;
            s.error = Some(e.to_string());
        }
        state
    })
}
pub fn file_counter() -> Option<u64> {
    let s = state().lock().unwrap();
    s.available.then_some(s.revision)
}
pub fn read_files() -> Result<Option<Vec<PathBuf>>> {
    let s = state().lock().unwrap();
    if !s.ready {
        return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into());
    }
    if let Some(e) = &s.error {
        anyhow::bail!("{e}");
    }
    Ok(s.files.clone())
}
fn atom(conn: &RustConnection, name: &str) -> Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}
fn watch(state: &Mutex<Snapshot>) -> Result<()> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::default(),
    )?;
    conn.xfixes_query_version(5, 0)?.reply()?;
    let clipboard = atom(&conn, "CLIPBOARD")?;
    let uri = atom(&conn, "text/uri-list")?;
    let prop = atom(&conn, "LYNXRDP_FILES")?;
    conn.xfixes_select_selection_input(
        window,
        clipboard,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )?;
    let request = || -> Result<()> {
        conn.convert_selection(window, clipboard, uri, prop, x11rb::CURRENT_TIME)?;
        conn.flush()?;
        Ok(())
    };
    request()?;
    let mut deadline = Some(Instant::now() + Duration::from_secs(2));
    loop {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::XfixesSelectionNotify(_) => {
                    {
                        let mut s = state.lock().unwrap();
                        s.revision = s.revision.wrapping_add(1);
                        s.ready = false;
                        s.error = None;
                    }
                    request()?;
                    deadline = Some(Instant::now() + Duration::from_secs(2));
                }
                Event::SelectionNotify(e)
                    if e.requestor == window && e.selection == clipboard && e.target == uri =>
                {
                    let mut s = state.lock().unwrap();
                    s.files = None;
                    s.error = None;
                    if e.property != 0 {
                        let value = conn
                            .get_property(true, window, prop, AtomEnum::ANY, 0, 256 * 1024)?
                            .reply()?;
                        if value.bytes_after != 0 || value.type_ != uri || value.format != 8 {
                            s.error =
                                Some("Unsupported or oversized file clipboard selection".into());
                        } else {
                            match std::str::from_utf8(&value.value) {
                                Ok(text) => {
                                    let paths = lynxrdp_proto::urilist::parse(text);
                                    if !paths.is_empty() {
                                        s.files = Some(paths);
                                    }
                                }
                                Err(e) => s.error = Some(e.to_string()),
                            }
                        }
                    }
                    s.ready = true;
                    deadline = None;
                }
                _ => {}
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            let mut s = state.lock().unwrap();
            s.ready = true;
            s.files = None;
            s.error = Some("Clipboard owner did not respond".into());
            deadline = None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
