//! Putting files on the *local* clipboard, so a file copied inside the
//! session can be pasted into the local file manager.
//!
//! The opposite direction — local files into the session — is covered by
//! dropping them onto the window, which needs no clipboard support at all.
//!
//! `arboard` has no file-list API, so this is per-platform. Every backend
//! offers the same thing: a list of local paths, already downloaded, that the
//! platform's file manager understands.
//!
//! On X11 the clipboard is not storage but a protocol: the owner answers
//! conversion requests for as long as it holds the selection. That means a
//! background thread with its own X connection, which keeps serving the list
//! until something else takes the selection or the client exits.

use std::path::PathBuf;

use anyhow::Result;

/// Whether this build can put files on the local clipboard.
pub const SUPPORTED: bool = cfg!(all(unix, not(target_os = "macos")));

/// Put `paths` on the local clipboard as a file list.
///
/// The paths must already exist: a file manager pasting them will read them
/// straight off disk.
pub fn write_files(paths: &[PathBuf]) -> Result<()> {
    imp::write_files(paths)
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use anyhow::{Context, Result};
    use lynxrdp_proto::urilist;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
        SelectionNotifyEvent, SelectionRequestEvent, WindowClass, SELECTION_NOTIFY_EVENT,
    };
    use x11rb::protocol::Event;
    use x11rb::wrapper::ConnectionExt as _;

    /// The owner thread, started on first use and reused afterwards.
    static OWNER: std::sync::OnceLock<mpsc::Sender<Vec<PathBuf>>> = std::sync::OnceLock::new();

    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        let tx = OWNER.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
            std::thread::Builder::new()
                .name("lynxrdp-fileclip".into())
                .spawn(move || {
                    if let Err(e) = run(rx) {
                        log::warn!("the file clipboard owner stopped: {e:#}");
                    }
                })
                .expect("spawn file clipboard thread");
            tx
        });
        tx.send(paths.to_vec())
            .context("the file clipboard thread has stopped")?;
        Ok(())
    }

    struct Atoms {
        clipboard: xproto::Atom,
        targets: xproto::Atom,
        timestamp: xproto::Atom,
        uri_list: xproto::Atom,
        gnome_copied: xproto::Atom,
    }

    /// Own the CLIPBOARD selection and answer `text/uri-list` conversions.
    fn run(rx: mpsc::Receiver<Vec<PathBuf>>) -> Result<()> {
        let (conn, screen_num) =
            x11rb::connect(None).context("connecting to the local X server")?;
        let screen = &conn.setup().roots[screen_num];
        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()
        .context("creating the file clipboard window")?;
        let atom = |name: &str| -> Result<xproto::Atom> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let atoms = Atoms {
            clipboard: atom("CLIPBOARD")?,
            targets: atom("TARGETS")?,
            timestamp: atom("TIMESTAMP")?,
            uri_list: atom("text/uri-list")?,
            gnome_copied: atom("x-special/gnome-copied-files")?,
        };
        let mut files: Vec<PathBuf> = Vec::new();

        loop {
            // Take any new list before serving requests for the old one.
            match rx.try_recv() {
                Ok(new) => {
                    files = new;
                    conn.set_selection_owner(window, atoms.clipboard, x11rb::CURRENT_TIME)?
                        .check()?;
                    conn.flush()?;
                    log::debug!("file clipboard now offers {} file(s)", files.len());
                }
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match conn.poll_for_event()? {
                Some(Event::SelectionRequest(e)) if e.owner == window => {
                    serve(&conn, &atoms, &files, &e)?;
                }
                Some(Event::SelectionClear(_)) => {
                    // Something else took the clipboard; stop offering.
                    files.clear();
                }
                Some(_) => {}
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }

    fn serve(
        conn: &impl Connection,
        atoms: &Atoms,
        files: &[PathBuf],
        e: &SelectionRequestEvent,
    ) -> Result<()> {
        let mut property = e.property;
        if property == x11rb::NONE {
            property = e.target;
        }
        let mut served = false;
        if !files.is_empty() {
            if e.target == atoms.targets {
                let list = [
                    atoms.targets,
                    atoms.timestamp,
                    atoms.uri_list,
                    atoms.gnome_copied,
                ];
                conn.change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::ATOM,
                    &list,
                )?;
                served = true;
            } else if e.target == atoms.timestamp {
                conn.change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[0u32],
                )?;
                served = true;
            } else if e.target == atoms.uri_list || e.target == atoms.gnome_copied {
                let list = urilist::build(files);
                let payload = if e.target == atoms.gnome_copied {
                    format!("copy\n{}", list.replace("\r\n", "\n"))
                } else {
                    list
                };
                conn.change_property8(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    e.target,
                    payload.as_bytes(),
                )?;
                served = true;
            }
        }
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if served { property } else { x11rb::NONE },
        };
        conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
        conn.flush()?;
        let _ = std::io::stdout().flush();
        Ok(())
    }
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
mod imp {
    use std::path::PathBuf;

    use anyhow::{bail, Result};

    pub fn write_files(_paths: &[PathBuf]) -> Result<()> {
        // Windows (CF_HDROP) and macOS (NSPasteboard) file lists are not
        // implemented yet. The files are still downloaded, and the caller
        // reports where they landed, so nothing is lost.
        bail!("putting files on the clipboard is not supported on this platform yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_matches_the_platform() {
        // On Linux this is implemented; elsewhere write_files must fail
        // cleanly rather than silently pretending to work.
        if !SUPPORTED {
            assert!(write_files(&[PathBuf::from("/tmp/x")]).is_err());
        }
    }
}
