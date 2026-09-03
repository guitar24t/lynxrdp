//! Putting files on the *local* clipboard, so a file copied inside the
//! session can be pasted into the local file manager.
//!
//! The opposite direction — local files into the session — is covered by
//! dropping them onto the window, which needs no clipboard support at all.
//!
//! `arboard` has no file-list API, so this is per-platform. Every backend
//! offers the same thing: a list of local paths, already downloaded, that the
//! platform's file manager understands. The three differ in how long the
//! offer has to be kept alive:
//!
//! * X11 treats the clipboard as a protocol rather than storage, so the owner
//!   must stay running and answer conversion requests. That needs a
//!   background thread with its own X connection.
//! * Windows and macOS copy the data when it is set, so the call can put the
//!   list down and return.

use std::path::PathBuf;

use anyhow::Result;

/// Whether this build can put files on the local clipboard.
pub const SUPPORTED: bool = cfg!(any(unix, windows));

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

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    use anyhow::{bail, Context, Result};
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};

    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            bail!("no files to put on the clipboard");
        }
        // Finder pastes file URLs, so that is what goes on the pasteboard.
        let mut urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> =
            Vec::with_capacity(paths.len());
        for path in paths {
            let absolute = std::path::absolute(path)
                .with_context(|| format!("resolving {}", path.display()))?;
            let Some(text) = absolute.to_str() else {
                bail!(
                    "{} is not valid UTF-8 and cannot become a file URL",
                    path.display()
                );
            };
            let url = NSURL::fileURLWithPath(&NSString::from_str(text));
            urls.push(ProtocolObject::from_retained(url));
        }
        let pasteboard = NSPasteboard::generalPasteboard();
        // clearContents must come first: it takes ownership of the pasteboard
        // and invalidates whatever the previous owner left there.
        pasteboard.clearContents();
        if !pasteboard.writeObjects(&NSArray::from_retained_slice(&urls)) {
            bail!("the pasteboard refused the file list");
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;

    use anyhow::{bail, Context, Result};
    use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HANDLE, HGLOBAL};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_HDROP;

    /// Size of the `DROPFILES` header that precedes the names in a CF_HDROP
    /// block: `DWORD pFiles; POINT pt; BOOL fNC; BOOL fWide;`.
    const DROPFILES_SIZE: u32 = 20;

    /// `DROPEFFECT_COPY`. Without it Explorer may treat the paste as a move
    /// and delete the staged file it just read.
    const DROPEFFECT_COPY: u32 = 1;

    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            bail!("no files to put on the clipboard");
        }
        let blob = hdrop_blob(paths)?;
        // SAFETY: every handle below is either handed to the clipboard, which
        // then owns it, or freed on the failure path before returning.
        unsafe {
            open_clipboard()?;
            let _close = CloseGuard;
            if EmptyClipboard() == 0 {
                bail!("EmptyClipboard failed (error {})", GetLastError());
            }
            let files = global_from(&blob)?;
            if SetClipboardData(CF_HDROP as u32, files as HANDLE).is_null() {
                let err = GetLastError();
                GlobalFree(files);
                bail!("SetClipboardData(CF_HDROP) failed (error {err})");
            }
            // Best effort: a missing drop effect only means Explorer picks its
            // own default, which is not worth failing the whole paste over.
            let effect_format = RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr());
            if effect_format != 0 {
                if let Ok(effect) = global_from(&DROPEFFECT_COPY.to_ne_bytes()) {
                    if SetClipboardData(effect_format, effect as HANDLE).is_null() {
                        GlobalFree(effect);
                    }
                }
            }
        }
        Ok(())
    }

    /// Closes the clipboard however the caller leaves the block.
    struct CloseGuard;

    impl Drop for CloseGuard {
        fn drop(&mut self) {
            // SAFETY: only constructed after a successful OpenClipboard.
            unsafe { CloseClipboard() };
        }
    }

    /// Build the CF_HDROP payload: a `DROPFILES` header followed by the
    /// UTF-16 names, each NUL terminated, with a second NUL closing the list.
    pub(super) fn hdrop_blob(paths: &[PathBuf]) -> Result<Vec<u8>> {
        let mut names: Vec<u16> = Vec::new();
        for path in paths {
            // Explorer resolves the names relative to nothing at all, so they
            // have to be absolute.
            let absolute = std::path::absolute(path)
                .with_context(|| format!("resolving {}", path.display()))?;
            let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
            if wide.contains(&0) {
                bail!(
                    "{} contains a NUL and cannot go on the clipboard",
                    path.display()
                );
            }
            names.extend_from_slice(&wide);
            names.push(0);
        }
        names.push(0);

        let mut blob = Vec::with_capacity(DROPFILES_SIZE as usize + names.len() * 2);
        blob.extend_from_slice(&DROPFILES_SIZE.to_ne_bytes()); // pFiles: names start here
        blob.extend_from_slice(&0i32.to_ne_bytes()); // pt.x
        blob.extend_from_slice(&0i32.to_ne_bytes()); // pt.y
        blob.extend_from_slice(&0i32.to_ne_bytes()); // fNC: point is client area
        blob.extend_from_slice(&1i32.to_ne_bytes()); // fWide: names are UTF-16
        for unit in names {
            blob.extend_from_slice(&unit.to_ne_bytes());
        }
        Ok(blob)
    }

    /// Copy `bytes` into a moveable global block, as the clipboard requires.
    ///
    /// # Safety
    /// The returned handle is owned by the caller until the clipboard takes it.
    unsafe fn global_from(bytes: &[u8]) -> Result<HGLOBAL> {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if handle.is_null() {
            bail!("GlobalAlloc of {} bytes failed", bytes.len());
        }
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            let err = GetLastError();
            GlobalFree(handle);
            bail!("GlobalLock failed (error {err})");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        GlobalUnlock(handle);
        Ok(handle)
    }

    /// Take the clipboard, retrying briefly: another process holding it is
    /// normal and momentary, not an error worth reporting to the user.
    unsafe fn open_clipboard() -> Result<()> {
        let mut err = 0;
        for attempt in 0..10 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                return Ok(());
            }
            err = GetLastError();
            std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1)));
        }
        bail!("could not open the clipboard; another program is holding it (error {err})")
    }

    /// A NUL terminated UTF-16 string for the `...W` entry points.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::path::PathBuf;

    use anyhow::{bail, Result};

    pub fn write_files(_paths: &[PathBuf]) -> Result<()> {
        // The files are still downloaded, and the caller reports where they
        // landed, so nothing is lost on a platform without a backend.
        bail!("putting files on the clipboard is not supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_matches_the_platform() {
        // Where there is no backend, write_files must fail cleanly rather
        // than silently pretending to have worked.
        if !SUPPORTED {
            assert!(write_files(&[PathBuf::from("/tmp/x")]).is_err());
        }
    }

    // Nothing to offer is a caller bug: taking the clipboard to publish an
    // empty list would just wipe whatever the user had on it.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn an_empty_list_is_refused() {
        assert!(write_files(&[]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn hdrop_blob_has_a_header_and_a_double_nul_terminated_list() {
        let blob = super::imp::hdrop_blob(&[PathBuf::from(r"C:\tmp\a.txt")]).unwrap();
        // pFiles: the names start straight after the 20 byte DROPFILES header.
        assert_eq!(u32::from_ne_bytes(blob[..4].try_into().unwrap()), 20);
        // fWide: the names are UTF-16, not bytes.
        assert_eq!(i32::from_ne_bytes(blob[16..20].try_into().unwrap()), 1);

        let units: Vec<u16> = blob[20..]
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes(c.try_into().unwrap()))
            .collect();
        let name: String = String::from_utf16(&units[..units.len() - 2]).unwrap();
        assert_eq!(name, r"C:\tmp\a.txt");
        // One NUL ends the name, a second ends the list.
        assert_eq!(&units[units.len() - 2..], &[0, 0]);
    }

    #[cfg(windows)]
    #[test]
    fn hdrop_blob_separates_several_names() {
        let blob = super::imp::hdrop_blob(&[
            PathBuf::from(r"C:\a.txt"),
            PathBuf::from(r"C:\b with space.txt"),
        ])
        .unwrap();
        let units: Vec<u16> = blob[20..]
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes(c.try_into().unwrap()))
            .collect();
        let names: Vec<String> = units[..units.len() - 1]
            .split(|&u| u == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf16(s).unwrap())
            .collect();
        assert_eq!(names, vec![r"C:\a.txt", r"C:\b with space.txt"]);
    }
}
