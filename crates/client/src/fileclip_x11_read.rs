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

/// How many request properties are cycled through.
///
/// Every request names a property of its own, because that is the only thing
/// an answer carries that says which request it answers: the time is
/// `CURRENT_TIME`, and requestor, selection and target are the same every
/// time. Without it the first answer to arrive after an owner change -- as
/// likely the old owner's late one as the new owner's -- was published as the
/// new clipboard. A ring this size would only be confused by an owner that
/// took eight further copies' worth of time to answer.
const PROPERTY_RING: usize = 8;

#[derive(Default)]
struct Snapshot {
    revision: u64,
    available: bool,
    ready: bool,
    files: Option<Vec<PathBuf>>,
    error: Option<String>,
}

impl Snapshot {
    /// A new request has gone out; the viewer must not act on the old answer.
    fn requested(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.ready = false;
        self.error = None;
    }

    /// Publish an answer to the latest request.
    ///
    /// One request can be answered twice. A refusal names no property, so a
    /// refusal from the previous owner that arrives first is taken for the
    /// current owner's, and the real answer follows it. That answer has to
    /// reach the viewer, which reads again only when the revision moves -- so
    /// a second answer that says something different moves it.
    fn answer(&mut self, outcome: Result<Option<Vec<PathBuf>>, String>) {
        let (files, error) = match outcome {
            Ok(files) => (files, None),
            Err(e) => (None, Some(e)),
        };
        if self.ready && (self.files != files || self.error != error) {
            self.revision = self.revision.wrapping_add(1);
        }
        self.files = files;
        self.error = error;
        self.ready = true;
    }
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
/// Take the answer an owner left in `prop`, deleting it on the way: the
/// owner's file list, `None` where it holds none, or what was wrong with it.
/// The outer error is the X connection's, which ends the watcher.
fn read_answer(
    conn: &RustConnection,
    window: u32,
    prop: u32,
    uri: u32,
) -> Result<Result<Option<Vec<PathBuf>>, String>> {
    let value = conn
        .get_property(true, window, prop, AtomEnum::ANY, 0, 256 * 1024)?
        .reply()?;
    if value.bytes_after != 0 || value.type_ != uri || value.format != 8 {
        return Ok(Err(
            "Unsupported or oversized file clipboard selection".into()
        ));
    }
    Ok(match std::str::from_utf8(&value.value) {
        Ok(text) => {
            let paths = lynxrdp_proto::urilist::parse(text);
            Ok((!paths.is_empty()).then_some(paths))
        }
        Err(e) => Err(e.to_string()),
    })
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
    let props = (0..PROPERTY_RING)
        .map(|i| atom(&conn, &format!("LYNXRDP_FILES_{i}")))
        .collect::<Result<Vec<_>>>()?;
    conn.xfixes_select_selection_input(
        window,
        clipboard,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )?;
    let mut requests = 0usize;
    let mut request = || -> Result<u32> {
        let prop = props[requests % PROPERTY_RING];
        requests += 1;
        conn.convert_selection(window, clipboard, uri, prop, x11rb::CURRENT_TIME)?;
        conn.flush()?;
        Ok(prop)
    };
    let mut latest = request()?;
    let mut deadline = Some(Instant::now() + Duration::from_secs(2));
    loop {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::XfixesSelectionNotify(_) => {
                    state.lock().unwrap().requested();
                    latest = request()?;
                    deadline = Some(Instant::now() + Duration::from_secs(2));
                }
                Event::SelectionNotify(e)
                    if e.requestor == window && e.selection == clipboard && e.target == uri =>
                {
                    if e.property == 0 {
                        // A refusal names no property, so nothing says which
                        // request it declines. Once the latest one has its
                        // answer it can only be an old owner's; before that
                        // it is taken as the current owner's, which a real
                        // answer arriving later corrects.
                        let mut s = state.lock().unwrap();
                        if !s.ready {
                            s.answer(Ok(None));
                            deadline = None;
                        }
                    } else if e.property == latest {
                        let outcome = read_answer(&conn, window, e.property, uri)?;
                        state.lock().unwrap().answer(outcome);
                        deadline = None;
                    } else {
                        // An earlier request's answer, from an owner that
                        // has since been replaced: it describes a clipboard
                        // that no longer exists.
                        conn.delete_property(window, e.property)?;
                        conn.flush()?;
                    }
                }
                _ => {}
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            state
                .lock()
                .unwrap()
                .answer(Err("Clipboard owner did not respond".into()));
            deadline = None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_corrected_answer_moves_the_revision_and_a_repeated_one_does_not() {
        let mut s = Snapshot {
            revision: 1,
            available: true,
            ..Default::default()
        };
        s.requested();
        assert_eq!(s.revision, 2);
        assert!(!s.ready);
        // The old owner's refusal, taken for the new owner's.
        s.answer(Ok(None));
        assert!(s.ready);
        assert_eq!(s.revision, 2);
        assert_eq!(s.files, None);
        // The new owner's real answer: the viewer has already read revision
        // 2 as "no files" and has to be made to read again.
        let files = vec![PathBuf::from("/tmp/a")];
        s.answer(Ok(Some(files.clone())));
        assert_eq!(s.revision, 3);
        assert_eq!(s.files, Some(files));
        // Saying the same thing again is not news.
        s.answer(Ok(s.files.clone()));
        assert_eq!(s.revision, 3);
        // An error is a change too.
        s.answer(Err("bad".into()));
        assert_eq!(s.revision, 4);
        assert_eq!(s.error.as_deref(), Some("bad"));
    }
}
