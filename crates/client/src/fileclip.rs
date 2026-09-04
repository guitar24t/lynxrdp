//! Putting files on the *local* clipboard, so a file copied inside the
//! session can be pasted into the local file manager.
//!
//! The opposite direction — local files into the session — is covered by
//! dropping them onto the window, which needs no clipboard support at all.
//!
//! `arboard` has no file-list API, so this is per-platform. Every backend
//! offers the same thing: a list of local paths, already downloaded, that the
//! platform's file manager understands. They also refuse the same things — an
//! empty list, and paths a pasting application would resolve differently from
//! us. Those two rules live in [`write_files`] rather than in each backend:
//! three copies of a rule are three chances to drift, and they had already
//! drifted once.
//!
//! What is genuinely per-platform is how long the offer has to be kept alive:
//!
//! * X11 treats the clipboard as a protocol rather than storage, so the owner
//!   must stay running and answer conversion requests. That needs a
//!   background thread with its own X connection. The connection is opened
//!   and the selection taken *before* the call returns, so a client with no
//!   reachable X server — Wayland without XWayland — is told so, instead of
//!   being told the files are on a clipboard that does not exist.
//! * Windows and macOS copy the data when it is set, so the call can put the
//!   list down and return.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// Whether this build can put files on the local clipboard.
pub const SUPPORTED: bool = cfg!(any(unix, windows));

/// Put `paths` on the local clipboard as a file list.
///
/// The paths must already exist: a file manager pasting them will read them
/// straight off disk.
pub fn write_files(paths: &[PathBuf]) -> Result<()> {
    // Publishing nothing is a caller bug, and acting on it is destructive:
    // every backend has to take the clipboard before it can offer anything,
    // so an empty list would wipe whatever the user had on it and offer
    // nothing back.
    if paths.is_empty() {
        bail!("no files to put on the clipboard");
    }
    let paths = absolute_paths(paths)?;
    imp::write_files(&paths)
}

/// Resolve every path against *our* working directory before it is published.
///
/// A clipboard file list carries no base directory, so the pasting
/// application resolves a relative name against its own working directory and
/// gets a file that is missing or, worse, a different one. `std::path::absolute`
/// is purely lexical — it never touches the disk — which is what we want here:
/// a staged file is offered by the name it was staged under, with no symlink
/// or `..` rewriting the caller did not ask for.
fn absolute_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|p| std::path::absolute(p).with_context(|| format!("resolving {}", p.display())))
        .collect()
}

/// Size of the `DROPFILES` header that precedes the names in a CF_HDROP
/// block: `DWORD pFiles; POINT pt; BOOL fNC; BOOL fWide;`.
#[cfg(any(windows, test))]
const DROPFILES_SIZE: u32 = 20;

/// Lay out a CF_HDROP payload: a `DROPFILES` header followed by `names`, each
/// NUL terminated, with a second NUL closing the list.
///
/// This is the shape Explorer parses, and it is the one thing here that no
/// test on a real machine can check for us — a headless runner has no
/// Explorer to paste into. So the layout is kept out of the Windows module
/// and fed pre-encoded UTF-16 names, which makes it a pure function that
/// every platform's test run exercises, including a developer's Linux or Mac.
#[cfg(any(windows, test))]
fn dropfiles_blob(names: &[Vec<u16>]) -> Vec<u8> {
    let units: usize = names.iter().map(|n| n.len() + 1).sum::<usize>() + 1;
    let mut blob = Vec::with_capacity(DROPFILES_SIZE as usize + units * 2);
    blob.extend_from_slice(&DROPFILES_SIZE.to_ne_bytes()); // pFiles: names start here
    blob.extend_from_slice(&0i32.to_ne_bytes()); // pt.x
    blob.extend_from_slice(&0i32.to_ne_bytes()); // pt.y
    blob.extend_from_slice(&0i32.to_ne_bytes()); // fNC: point is client area
    blob.extend_from_slice(&1i32.to_ne_bytes()); // fWide: names are UTF-16
    for name in names {
        for unit in name {
            blob.extend_from_slice(&unit.to_ne_bytes());
        }
        blob.extend_from_slice(&0u16.to_ne_bytes());
    }
    blob.extend_from_slice(&0u16.to_ne_bytes());
    blob
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use anyhow::{bail, Context, Result};
    use lynxrdp_proto::urilist;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
        SelectionNotifyEvent, SelectionRequestEvent, WindowClass, SELECTION_NOTIFY_EVENT,
    };
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    /// The selection owner, started on first use and reused afterwards.
    ///
    /// It is a `Mutex` rather than a `OnceLock` because starting it can fail —
    /// there may be no X server — and because a failure is not necessarily
    /// permanent: if the connection dies, the next copy builds a new owner
    /// instead of reporting a dead one forever.
    static OWNER: Mutex<Option<Arc<Owner>>> = Mutex::new(None);

    /// Publish `paths` and take the CLIPBOARD selection.
    ///
    /// Unlike the other backends this leaves a thread behind, but the parts
    /// that can fail — connecting, creating the window, winning the selection
    /// — all happen here, so the caller's error path means what it says.
    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        owner()?.publish(paths)
    }

    /// The running owner, started if there is none and restarted if the one
    /// we have has lost its connection.
    fn owner() -> Result<Arc<Owner>> {
        let mut slot = OWNER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = slot.as_ref() {
            if existing.alive.load(Ordering::Acquire) {
                return Ok(Arc::clone(existing));
            }
            // The serving thread stopped, so the connection is gone and with
            // it the window and any ownership. Drop it here so the last
            // reference goes with it and the socket is closed.
            *slot = None;
        }
        let owner = Owner::start()?;
        *slot = Some(Arc::clone(&owner));
        Ok(owner)
    }

    struct Atoms {
        clipboard: xproto::Atom,
        targets: xproto::Atom,
        timestamp: xproto::Atom,
        uri_list: xproto::Atom,
        gnome_copied: xproto::Atom,
    }

    /// An X connection that owns the CLIPBOARD selection, shared between the
    /// thread that publishes a list and the thread that answers conversions.
    ///
    /// x11rb explicitly supports this split — one thread parked in
    /// `wait_for_event` while others make requests — and it is what lets the
    /// publishing side report a real result rather than posting to a channel
    /// and hoping.
    struct Owner {
        conn: RustConnection,
        window: xproto::Window,
        atoms: Atoms,
        /// What we are currently offering. Empty means "nothing to convert",
        /// which is also how a lost selection is recorded.
        files: Mutex<Vec<PathBuf>>,
        /// Cleared when the serving thread stops, which is the only signal
        /// that the connection underneath is no longer usable.
        alive: AtomicBool,
    }

    impl Owner {
        fn start() -> Result<Arc<Self>> {
            let (conn, screen_num) =
                x11rb::connect(None).context("connecting to the local X server")?;
            let root = conn.setup().roots[screen_num].root;
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
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()
            .context("creating the file clipboard window")?;
            let atoms = Atoms {
                clipboard: atom(&conn, "CLIPBOARD")?,
                targets: atom(&conn, "TARGETS")?,
                timestamp: atom(&conn, "TIMESTAMP")?,
                uri_list: atom(&conn, "text/uri-list")?,
                // Nautilus and friends look for this one when pasting.
                gnome_copied: atom(&conn, "x-special/gnome-copied-files")?,
            };
            let owner = Arc::new(Owner {
                conn,
                window,
                atoms,
                files: Mutex::new(Vec::new()),
                alive: AtomicBool::new(true),
            });
            let serving = Arc::clone(&owner);
            std::thread::Builder::new()
                .name("lynxrdp-fileclip".into())
                .spawn(move || {
                    let stopped = serving.serve_forever();
                    // Mark it dead before logging: a copy racing this must
                    // start a new owner rather than publish into a socket
                    // that is already gone.
                    serving.alive.store(false, Ordering::Release);
                    if let Err(e) = stopped {
                        log::warn!("the file clipboard owner stopped: {e:#}");
                    }
                })
                .context("starting the file clipboard owner thread")?;
            Ok(owner)
        }

        /// The offered list. A panic while serving cannot leave it in a state
        /// that is worse than the list it holds, so poisoning is recovered
        /// from rather than propagated into the UI thread.
        fn files(&self) -> MutexGuard<'_, Vec<PathBuf>> {
            self.files.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Offer `paths`, taking the CLIPBOARD selection to do it, and fail
        /// if the selection did not come to us.
        ///
        /// The list stays locked across both halves, because they have to
        /// look like one step to the serving thread: that thread can still
        /// be carrying the SelectionClear from the moment another program
        /// took the selection away, and applying it in the gap between the
        /// new list and the acquisition leaves us owning CLIPBOARD with
        /// nothing to offer — every paste refused, nothing logged, and a
        /// `write_files` that returned `Ok`. The order within the lock
        /// matters too: the list goes down before the selection is taken,
        /// so a requestor that asks the instant ownership changes hands
        /// cannot be answered with the previous copy's files.
        fn publish(&self, paths: &[PathBuf]) -> Result<()> {
            let mut files = self.files();
            *files = paths.to_vec();
            // ICCCM asks for the timestamp of the event that prompted the
            // copy. We have none — the copy happened on the far end of a
            // network — and obtaining a fresh server timestamp means routing
            // a PropertyNotify past the serving thread that is draining the
            // event queue. What CURRENT_TIME costs is the ability to date
            // this acquisition against a SelectionClear; asking the server
            // who holds the selection — here, and again on that event —
            // covers the same ground without the extra plumbing.
            self.conn
                .set_selection_owner(self.window, self.atoms.clipboard, x11rb::CURRENT_TIME)?
                .check()
                .context("taking the CLIPBOARD selection")?;
            if !self.owns_clipboard()? {
                files.clear();
                bail!("another program holds the clipboard");
            }
            self.conn.flush()?;
            log::debug!("file clipboard now offers {} file(s)", files.len());
            Ok(())
        }

        /// Whether the X server currently has us down as the CLIPBOARD
        /// owner. Asked rather than remembered: ownership is the server's to
        /// decide, and both callers are dealing with the moment it changes.
        fn owns_clipboard(&self) -> Result<bool> {
            let owner = self
                .conn
                .get_selection_owner(self.atoms.clipboard)?
                .reply()
                .context("asking who owns the CLIPBOARD selection")?
                .owner;
            Ok(owner == self.window)
        }

        /// Answer conversion requests until the connection goes away.
        fn serve_forever(&self) -> Result<()> {
            loop {
                // Blocking, not polling. A selection owner has nothing to do
                // between requests, and this thread lives as long as the
                // process does: a poll loop here would keep a core warm for
                // the whole session on the off chance of a paste.
                match self.conn.wait_for_event().context("reading X events")? {
                    Event::SelectionRequest(e) if e.owner == self.window => self.serve(&e)?,
                    Event::SelectionClear(e) if e.selection == self.atoms.clipboard => {
                        // Something else took the clipboard, so there is
                        // nothing left to offer — unless a copy has taken it
                        // back since, which is what happens when the user
                        // copies again straight after another program
                        // grabbed the selection. The event's timestamp
                        // cannot settle that order for us (see `publish`),
                        // so ask the server who holds the selection now:
                        // one round trip, on an event that only arrives
                        // when some other program takes the clipboard. The
                        // list is locked before the question so a publish in
                        // flight finishes first, which is what makes the
                        // answer decisive rather than another race.
                        let mut files = self.files();
                        if self.owns_clipboard()? {
                            log::debug!("a stale SelectionClear left the file list alone");
                        } else {
                            files.clear();
                        }
                    }
                    // Requests are sent unchecked, so an error — a requestor
                    // that vanished mid-conversion, say — arrives here rather
                    // than at the call site. It is not fatal to the owner.
                    Event::Error(e) => log::debug!("file clipboard X error: {e:?}"),
                    _ => {}
                }
            }
        }

        fn serve(&self, e: &SelectionRequestEvent) -> Result<()> {
            // A requestor predating ICCCM sends NONE for the property,
            // meaning "put it where the target atom says".
            let property = if e.property == x11rb::NONE {
                e.target
            } else {
                e.property
            };
            let served = match self.convert(e.target, e.requestor, property) {
                Ok(served) => served,
                Err(err) => {
                    // Every request gets an answer, including one we could
                    // not fulfil: a requestor left without a SelectionNotify
                    // blocks until its own timeout, which in a file manager
                    // is a frozen window.
                    log::warn!("file clipboard conversion failed: {err:#}");
                    false
                }
            };
            let notify = SelectionNotifyEvent {
                response_type: SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: e.time,
                requestor: e.requestor,
                selection: e.selection,
                target: e.target,
                property: if served { property } else { x11rb::NONE },
            };
            self.conn
                .send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
            self.conn.flush()?;
            Ok(())
        }

        /// Write the conversion onto the requestor's property, reporting
        /// whether `target` was one we can produce at all.
        fn convert(
            &self,
            target: xproto::Atom,
            requestor: xproto::Window,
            property: xproto::Atom,
        ) -> Result<bool> {
            let files = self.files();
            if files.is_empty() {
                return Ok(false);
            }
            if target == self.atoms.targets {
                let list = [
                    self.atoms.targets,
                    self.atoms.timestamp,
                    self.atoms.uri_list,
                    self.atoms.gnome_copied,
                ];
                self.conn.change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::ATOM,
                    &list,
                )?;
                return Ok(true);
            }
            if target == self.atoms.timestamp {
                // The time we acquired with, which is CURRENT_TIME — see
                // `acquire`.
                self.conn.change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[x11rb::CURRENT_TIME],
                )?;
                return Ok(true);
            }
            if target == self.atoms.uri_list || target == self.atoms.gnome_copied {
                let list = urilist::build(&files);
                let payload = if target == self.atoms.gnome_copied {
                    // The GNOME format puts the operation on the first line
                    // and separates with LF, not the CRLF of RFC 2483.
                    format!("copy\n{}", list.replace("\r\n", "\n"))
                } else {
                    list
                };
                // A list too large for one request errors out here rather
                // than being truncated; ICCCM's answer is an INCR transfer,
                // and until something needs one, refusing a list of tens of
                // thousands of staged files is the honest stand-in.
                self.conn.change_property8(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    target,
                    payload.as_bytes(),
                )?;
                return Ok(true);
            }
            Ok(false)
        }
    }

    fn atom(conn: &RustConnection, name: &str) -> Result<xproto::Atom> {
        Ok(conn
            .intern_atom(false, name.as_bytes())?
            .reply()
            .with_context(|| format!("interning {name}"))?
            .atom)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    use anyhow::{bail, Result};
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};

    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        // Finder pastes file URLs, so that is what goes on the pasteboard.
        let mut urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> =
            Vec::with_capacity(paths.len());
        for path in paths {
            let Some(text) = path.to_str() else {
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

    use anyhow::{bail, Result};
    use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HANDLE, HGLOBAL};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_HDROP;

    /// `DROPEFFECT_COPY`. Without it Explorer may treat the paste as a move
    /// and delete the staged file it just read.
    const DROPEFFECT_COPY: u32 = 1;

    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        let blob = hdrop_blob(paths)?;
        // The format name is bound to a local rather than being built inside
        // the call: a pointer into a temporary is legal only because the
        // temporary outlives the enclosing statement, which is exactly the
        // kind of thing an edit here would quietly break.
        let effect_name = wide("Preferred DropEffect");
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
            let effect_format = RegisterClipboardFormatW(effect_name.as_ptr());
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
            // SAFETY: only constructed after a successful OpenClipboard, and
            // on the same thread, which is what CloseClipboard requires.
            unsafe { CloseClipboard() };
        }
    }

    /// Encode `paths` for [`super::dropfiles_blob`], which lays out the block
    /// itself. The names must already be absolute — Explorer resolves them
    /// against nothing at all — which [`super::write_files`] guarantees.
    pub(super) fn hdrop_blob(paths: &[PathBuf]) -> Result<Vec<u8>> {
        let mut names: Vec<Vec<u16>> = Vec::with_capacity(paths.len());
        for path in paths {
            let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            // The block separates names with NULs, so an embedded one would
            // not corrupt the list so much as silently split a path in two.
            if wide.contains(&0) {
                bail!(
                    "{} contains a NUL and cannot go on the clipboard",
                    path.display()
                );
            }
            names.push(wide);
        }
        Ok(super::dropfiles_blob(&names))
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
        const ATTEMPTS: u32 = 10;
        let mut err = 0;
        for attempt in 0..ATTEMPTS {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                return Ok(());
            }
            err = GetLastError();
            // No sleep after the last try: the answer is already decided and
            // the user is waiting on the paste failing, not on us backing off.
            if attempt + 1 < ATTEMPTS {
                std::thread::sleep(std::time::Duration::from_millis(
                    10 * u64::from(attempt + 1),
                ));
            }
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
    // empty list would just wipe whatever the user had on it. This is checked
    // before any backend runs, so it holds on every platform — X11 used to
    // accept it and wipe the clipboard.
    #[test]
    fn an_empty_list_is_refused() {
        assert!(write_files(&[]).is_err());
    }

    // The caller's fallback — telling the user where the files were staged —
    // only runs if we admit that the clipboard did not take them, which the
    // X11 backend used to hide behind a channel send. A machine with a
    // display would really take the clipboard, so only the headless case is
    // asserted; that is the case CI and a Wayland-without-XWayland client are
    // both in.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn an_unreachable_x_server_is_reported() {
        if std::env::var_os("DISPLAY").is_some() {
            return;
        }
        let err = write_files(&[PathBuf::from("/tmp/lynxrdp-not-copied")]).unwrap_err();
        assert!(format!("{err:#}").contains("X server"), "{err:#}");
    }

    #[test]
    fn relative_paths_are_resolved_before_publishing() {
        let cwd = std::env::current_dir().unwrap();
        let out =
            absolute_paths(&[PathBuf::from("staged"), PathBuf::from("staged/a.txt")]).unwrap();
        assert_eq!(out, vec![cwd.join("staged"), cwd.join("staged/a.txt")]);
    }

    #[test]
    fn absolute_paths_are_published_as_they_are() {
        let already = if cfg!(windows) {
            PathBuf::from(r"C:\tmp\a.txt")
        } else {
            PathBuf::from("/tmp/a.txt")
        };
        let out = absolute_paths(std::slice::from_ref(&already)).unwrap();
        assert_eq!(out, vec![already]);
    }

    /// The names as CF_HDROP carries them: UTF-16, no terminator (the block
    /// builder adds those).
    fn wide_name(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// Read a CF_HDROP block back into the names it offers.
    fn names_in(blob: &[u8]) -> Vec<String> {
        let units: Vec<u16> = blob[DROPFILES_SIZE as usize..]
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes(c.try_into().unwrap()))
            .collect();
        // The list ends with a NUL of its own, so the final split is empty.
        units[..units.len() - 1]
            .split(|&u| u == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf16(s).unwrap())
            .collect()
    }

    #[test]
    fn a_dropfiles_block_has_a_header_and_a_double_nul_terminated_list() {
        let blob = dropfiles_blob(&[wide_name(r"C:\tmp\a.txt")]);
        // pFiles: the names start straight after the 20 byte DROPFILES header.
        assert_eq!(u32::from_ne_bytes(blob[..4].try_into().unwrap()), 20);
        // fWide: the names are UTF-16, not bytes.
        assert_eq!(i32::from_ne_bytes(blob[16..20].try_into().unwrap()), 1);
        assert_eq!(names_in(&blob), vec![r"C:\tmp\a.txt"]);
        // One NUL ends the name, a second ends the list.
        assert_eq!(&blob[blob.len() - 4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_dropfiles_block_separates_several_names() {
        let blob = dropfiles_blob(&[
            wide_name(r"C:\a.txt"),
            wide_name(r"C:\b with space.txt"),
            wide_name(r"C:\héllo.txt"),
        ]);
        assert_eq!(
            names_in(&blob),
            vec![r"C:\a.txt", r"C:\b with space.txt", r"C:\héllo.txt"]
        );
    }

    #[test]
    fn a_dropfiles_block_is_exactly_as_long_as_its_contents() {
        // Explorer walks the block by the NULs, so trailing slack would be
        // read as another, empty name.
        let blob = dropfiles_blob(&[wide_name("ab"), wide_name("c")]);
        assert_eq!(blob.len(), DROPFILES_SIZE as usize + (3 + 2 + 1) * 2);
    }

    #[cfg(windows)]
    #[test]
    fn hdrop_blob_encodes_the_names_it_is_given() {
        let blob = super::imp::hdrop_blob(&[PathBuf::from(r"C:\tmp\a.txt")]).unwrap();
        assert_eq!(blob, dropfiles_blob(&[wide_name(r"C:\tmp\a.txt")]));
    }

    #[cfg(windows)]
    #[test]
    fn a_name_containing_a_nul_is_refused() {
        // Passing it through would end the name early and offer a file the
        // user never copied.
        assert!(super::imp::hdrop_blob(&[PathBuf::from("C:\\tmp\\a\0b.txt")]).is_err());
    }
}
