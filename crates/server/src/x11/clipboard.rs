//! X11 clipboard (`CLIPBOARD` selection) bridging for text and images.
//!
//! # How a copy inside the session reaches the client
//!
//! XFIXES tells us the moment another client takes ownership of the
//! selection. We then ask that owner for `TARGETS` to learn which formats it
//! can produce, and report those formats upwards as
//! [`ClipboardEvent::Formats`]. Text is small, so it is fetched immediately;
//! an image is only fetched when the far end asks for it with
//! [`Clipboard::request_format`].
//!
//! # How a copy on the client reaches the session
//!
//! [`Clipboard::set_text`] and [`Clipboard::set_image`] take ownership of the
//! selection and answer `SelectionRequest` events from session applications.
//! Both hold the content in memory, because an X11 selection owner has to be
//! able to answer a conversion request promptly; deferring a reply until a
//! blob arrived over the network would block the pasting application.
//!
//! Incoming `INCR` transfers are handled, so a session application offering
//! content larger than the X server's maximum request size is read in full
//! rather than truncated.
//!
//! Outgoing `INCR` is *not* implemented: when the client puts something on the
//! clipboard that will not fit in one `ChangeProperty`, that format is declined
//! (`property: NONE`) and left out of `TARGETS`, so the pasting application
//! falls back to a format that fits instead of receiving nothing. Until this
//! gap is closed, a very large image copied on the client cannot be pasted into
//! a session application -- which is a limitation, where propagating the
//! resulting protocol error used to end the whole session.

use std::sync::Arc;

use anyhow::{Context, Result};
use lynxrdp_proto::message::clipboard_format;
// RequestConnection carries `maximum_request_bytes`. It is a supertrait of
// Connection, but Rust still wants the defining trait in scope to call it.
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::{xfixes, Event};
use x11rb::wrapper::ConnectionExt as _;

use super::XDisplay;

/// Largest clipboard text accepted in either direction (4 MiB).
pub const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;

/// Largest clipboard image accepted in either direction (64 MiB). Images are
/// held in memory on both sides, so this bounds that cost.
pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;

struct Atoms {
    clipboard: xproto::Atom,
    utf8_string: xproto::Atom,
    text: xproto::Atom,
    targets: xproto::Atom,
    incr: xproto::Atom,
    timestamp: xproto::Atom,
    png: xproto::Atom,
    uri_list: xproto::Atom,
    gnome_copied: xproto::Atom,
    prop: xproto::Atom,
}

/// A conversion request in flight.
struct Fetch {
    /// Target atom we asked for.
    target: xproto::Atom,
    /// Which [`clipboard_format`] this will produce (0 for `TARGETS`).
    format: u32,
    /// Whether the owner switched to an `INCR` transfer.
    incr: bool,
    /// Accumulated `INCR` chunks.
    buf: Vec<u8>,
    /// Whether a plain `STRING` fallback has already been tried.
    tried_string: bool,
}

/// What the session's clipboard did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardEvent {
    /// New content was copied inside the session, offering these formats
    /// (a mask of [`clipboard_format`]).
    Formats(u32),
    /// Text copied inside the session.
    Text(String),
    /// A PNG image produced in answer to [`Clipboard::request_format`].
    Image(Vec<u8>),
    /// Files copied inside the session, as local paths.
    Files(Vec<std::path::PathBuf>),
    /// A requested format turned out to be unavailable after all.
    Unavailable(u32),
}

/// Clipboard bridge state.
pub struct Clipboard {
    display: Arc<XDisplay>,
    window: xproto::Window,
    atoms: Atoms,
    /// Text we are currently offering to the session, if any.
    owned_text: Option<String>,
    /// PNG we are currently offering to the session, if any.
    owned_png: Option<Vec<u8>>,
    /// Staged files we are currently offering to the session, if any.
    owned_files: Option<Vec<std::path::PathBuf>>,
    fetch: Option<Fetch>,
    /// Formats the current session-side owner advertised.
    available: u32,
    /// Last text reported upwards, to suppress echoes.
    last_text: Option<String>,
    /// Timestamp of the selection we are reading from.
    owner_time: xproto::Timestamp,
}

impl Clipboard {
    /// Create the helper window and subscribe to selection owner changes.
    pub fn new(display: Arc<XDisplay>) -> Result<Self> {
        anyhow::ensure!(display.ext.xfixes, "XFIXES required for clipboard");
        let conn = display.conn();
        let window = display.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            display.root(),
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
        .context("create clipboard window")?;
        let atoms = Atoms {
            clipboard: display.atom("CLIPBOARD")?,
            utf8_string: display.atom("UTF8_STRING")?,
            text: display.atom("TEXT")?,
            targets: display.atom("TARGETS")?,
            incr: display.atom("INCR")?,
            timestamp: display.atom("TIMESTAMP")?,
            png: display.atom("image/png")?,
            uri_list: display.atom("text/uri-list")?,
            // Nautilus and friends also look for this one when pasting.
            gnome_copied: display.atom("x-special/gnome-copied-files")?,
            prop: display.atom("LYNXRDP_CLIP")?,
        };
        xfixes::select_selection_input(
            conn,
            window,
            atoms.clipboard,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?
        .check()
        .context("select selection input")?;
        conn.flush()?;
        Ok(Self {
            display,
            window,
            atoms,
            owned_text: None,
            owned_png: None,
            owned_files: None,
            fetch: None,
            available: 0,
            last_text: None,
            owner_time: x11rb::CURRENT_TIME,
        })
    }

    /// Offer `text` to applications in the session.
    pub fn set_text(&mut self, text: String) -> Result<()> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            log::warn!("ignoring clipboard text of {} bytes", text.len());
            return Ok(());
        }
        self.last_text = Some(text.clone());
        self.owned_text = Some(text);
        // Replacing the clipboard drops anything else we were offering.
        self.owned_png = None;
        self.owned_files = None;
        self.acquire()
    }

    /// Offer a PNG image to applications in the session.
    pub fn set_image(&mut self, png: Vec<u8>) -> Result<()> {
        if png.len() > MAX_CLIPBOARD_IMAGE_BYTES {
            log::warn!("ignoring clipboard image of {} bytes", png.len());
            return Ok(());
        }
        self.owned_png = Some(png);
        self.owned_text = None;
        self.owned_files = None;
        self.acquire()
    }

    /// Offer staged files to applications in the session.
    ///
    /// The paths must already exist locally: an X11 selection owner has to be
    /// able to answer a paste immediately, so the files are staged before
    /// they are offered rather than fetched on demand.
    pub fn set_files(&mut self, paths: Vec<std::path::PathBuf>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.owned_files = Some(paths);
        self.owned_text = None;
        self.owned_png = None;
        self.acquire()
    }

    fn acquire(&mut self) -> Result<()> {
        let conn = self.display.conn();
        conn.set_selection_owner(self.window, self.atoms.clipboard, x11rb::CURRENT_TIME)?
            .check()?;
        let owner = conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        if owner != self.window {
            log::warn!("could not acquire CLIPBOARD ownership");
            self.owned_text = None;
            self.owned_png = None;
            self.owned_files = None;
            return Ok(());
        }
        conn.flush()?;
        Ok(())
    }

    /// Whether we currently own the selection.
    pub fn owns_selection(&self) -> bool {
        self.owned_text.is_some() || self.owned_png.is_some() || self.owned_files.is_some()
    }

    /// Formats the session currently offers.
    pub fn available_formats(&self) -> u32 {
        self.available
    }

    /// Ask the session's clipboard owner for one format. The result arrives
    /// from [`Clipboard::handle_event`] as [`ClipboardEvent::Image`] (or
    /// [`ClipboardEvent::Unavailable`]).
    pub fn request_format(&mut self, format: u32) -> Result<()> {
        if self.available & format == 0 {
            return Ok(());
        }
        if self.fetch.is_some() {
            // One conversion at a time; the caller can ask again later.
            log::debug!("clipboard fetch already in flight, ignoring request");
            return Ok(());
        }
        let target = match format {
            f if f == clipboard_format::PNG => self.atoms.png,
            f if f == clipboard_format::FILES => self.atoms.uri_list,
            f if f == clipboard_format::TEXT => self.atoms.utf8_string,
            _ => return Ok(()),
        };
        self.request(target, format, self.owner_time)
    }

    /// Feed an X event, producing any clipboard activity it implies.
    pub fn handle_event(&mut self, ev: &Event) -> Result<Vec<ClipboardEvent>> {
        match ev {
            Event::XfixesSelectionNotify(e) if e.selection == self.atoms.clipboard => {
                if e.owner == self.window {
                    return Ok(Vec::new());
                }
                self.owned_text = None;
                self.owned_png = None;
                self.available = 0;
                if e.owner == x11rb::NONE {
                    return Ok(Vec::new());
                }
                self.owner_time = e.timestamp;
                // Learn what the new owner can produce before fetching.
                self.fetch = None;
                self.request(self.atoms.targets, 0, e.timestamp)?;
                Ok(Vec::new())
            }
            Event::SelectionNotify(e) if e.requestor == self.window => self.on_selection_notify(e),
            Event::PropertyNotify(e)
                if e.window == self.window
                    && e.atom == self.atoms.prop
                    && e.state == Property::NEW_VALUE =>
            {
                self.on_incr_chunk()
            }
            Event::SelectionRequest(e) if e.owner == self.window => {
                self.on_selection_request(e)?;
                Ok(Vec::new())
            }
            Event::SelectionClear(e)
                if e.owner == self.window && e.selection == self.atoms.clipboard =>
            {
                self.owned_text = None;
                self.owned_png = None;
                self.owned_files = None;
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    fn request(
        &mut self,
        target: xproto::Atom,
        format: u32,
        time: xproto::Timestamp,
    ) -> Result<()> {
        let conn = self.display.conn();
        conn.delete_property(self.window, self.atoms.prop)?;
        conn.convert_selection(
            self.window,
            self.atoms.clipboard,
            target,
            self.atoms.prop,
            time,
        )?;
        conn.flush()?;
        self.fetch = Some(Fetch {
            target,
            format,
            incr: false,
            buf: Vec::new(),
            tried_string: target == AtomEnum::STRING.into(),
        });
        Ok(())
    }

    fn on_selection_notify(&mut self, e: &SelectionNotifyEvent) -> Result<Vec<ClipboardEvent>> {
        let Some(fetch) = self.fetch.as_mut() else {
            return Ok(Vec::new());
        };
        if e.property == x11rb::NONE {
            // The owner refused this conversion.
            let (format, tried_string, target) = (fetch.format, fetch.tried_string, fetch.target);
            self.fetch = None;
            if format == clipboard_format::TEXT && !tried_string {
                self.request(AtomEnum::STRING.into(), format, e.time)?;
                return Ok(Vec::new());
            }
            if target == self.atoms.targets {
                return Ok(Vec::new());
            }
            return Ok(vec![ClipboardEvent::Unavailable(format)]);
        }
        let conn = self.display.conn();
        let reply = conn
            .get_property(
                true,
                self.window,
                self.atoms.prop,
                AtomEnum::ANY,
                0,
                u32::MAX / 4,
            )?
            .reply()
            .context("get clipboard property")?;
        conn.flush()?;
        if reply.type_ == self.atoms.incr {
            // Deleting the property (done above) starts the INCR transfer.
            fetch.incr = true;
            fetch.buf.clear();
            return Ok(Vec::new());
        }
        if fetch.target == self.atoms.targets {
            let targets: Vec<u32> = reply.value32().map(|v| v.collect()).unwrap_or_default();
            self.fetch = None;
            return self.on_targets(&targets, e.time);
        }
        let format = fetch.format;
        self.fetch = None;
        Ok(self.finish(format, reply.value))
    }

    /// Decide what the session's clipboard offers, and start fetching text.
    fn on_targets(
        &mut self,
        targets: &[u32],
        time: xproto::Timestamp,
    ) -> Result<Vec<ClipboardEvent>> {
        let has = |a: xproto::Atom| targets.contains(&a);
        let mut formats = 0u32;
        if has(self.atoms.utf8_string) || has(self.atoms.text) || has(AtomEnum::STRING.into()) {
            formats |= clipboard_format::TEXT;
        }
        if has(self.atoms.png) {
            formats |= clipboard_format::PNG;
        }
        if has(self.atoms.uri_list) || has(self.atoms.gnome_copied) {
            formats |= clipboard_format::FILES;
        }
        self.available = formats;
        let mut events = vec![ClipboardEvent::Formats(formats)];
        // Text is small enough to be worth having before the user pastes.
        if formats & clipboard_format::TEXT != 0 {
            self.request(self.atoms.utf8_string, clipboard_format::TEXT, time)?;
        } else if formats == 0 {
            events.clear();
            events.push(ClipboardEvent::Formats(0));
        }
        Ok(events)
    }

    fn on_incr_chunk(&mut self) -> Result<Vec<ClipboardEvent>> {
        let Some(fetch) = self.fetch.as_mut() else {
            return Ok(Vec::new());
        };
        if !fetch.incr {
            return Ok(Vec::new());
        }
        let conn = self.display.conn();
        let reply = conn
            .get_property(
                true,
                self.window,
                self.atoms.prop,
                AtomEnum::ANY,
                0,
                u32::MAX / 4,
            )?
            .reply()
            .context("get INCR chunk")?;
        conn.flush()?;
        if reply.value.is_empty() {
            let format = fetch.format;
            let is_targets = fetch.target == self.atoms.targets;
            let data = std::mem::take(&mut fetch.buf);
            self.fetch = None;
            if is_targets {
                let targets: Vec<u32> = data
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                return self.on_targets(&targets, x11rb::CURRENT_TIME);
            }
            return Ok(self.finish(format, data));
        }
        let limit = if fetch.format == clipboard_format::PNG {
            MAX_CLIPBOARD_IMAGE_BYTES
        } else {
            MAX_CLIPBOARD_BYTES
        };
        if fetch.buf.len() + reply.value.len() > limit {
            log::warn!("clipboard INCR transfer exceeds limit; dropping");
            let format = fetch.format;
            self.fetch = None;
            return Ok(vec![ClipboardEvent::Unavailable(format)]);
        }
        fetch.buf.extend_from_slice(&reply.value);
        Ok(Vec::new())
    }

    fn finish(&mut self, format: u32, data: Vec<u8>) -> Vec<ClipboardEvent> {
        if format == clipboard_format::FILES {
            let list = String::from_utf8_lossy(&data);
            let paths = lynxrdp_proto::urilist::parse(&list);
            if paths.is_empty() {
                return vec![ClipboardEvent::Unavailable(format)];
            }
            return vec![ClipboardEvent::Files(paths)];
        }
        if format == clipboard_format::PNG {
            if data.is_empty() || data.len() > MAX_CLIPBOARD_IMAGE_BYTES {
                return vec![ClipboardEvent::Unavailable(format)];
            }
            return vec![ClipboardEvent::Image(data)];
        }
        if data.len() > MAX_CLIPBOARD_BYTES {
            log::warn!("clipboard text too large ({} bytes)", data.len());
            return Vec::new();
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        if text.is_empty() || self.last_text.as_deref() == Some(text.as_str()) {
            return Vec::new();
        }
        self.last_text = Some(text.clone());
        vec![ClipboardEvent::Text(text)]
    }

    fn on_selection_request(&mut self, e: &SelectionRequestEvent) -> Result<()> {
        let mut property = e.property;
        if property == x11rb::NONE {
            property = e.target;
        }
        let served = self.serve(e, property)?;
        let conn = self.display.conn();
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
        Ok(())
    }

    /// The largest payload one `ChangeProperty` can carry on this connection.
    ///
    /// The X maximum request length is a property of the connection (and of
    /// whether BIG-REQUESTS was negotiated), not a constant: it is around
    /// 256 KiB without the extension and 16 MiB with it. A `change_property8`
    /// over that limit is a protocol error, and a protocol error here used to
    /// end the session.
    ///
    /// The real fix for an oversized selection is outgoing INCR, which this
    /// module does not implement in that direction. Until it does, an
    /// unservable format is better declined than promised: `serve` returns
    /// `Ok(false)`, which `on_selection_request` turns into the
    /// protocol-correct `property: NONE`, and it is left out of `TARGETS` so
    /// the pasting application falls back to a format that does fit instead of
    /// choosing one that cannot work.
    fn max_property_bytes(&self) -> usize {
        // The ChangeProperty header is 24 bytes; leave a little more than that
        // so a rounding difference cannot turn into a protocol error.
        self.display
            .conn()
            .maximum_request_bytes()
            .saturating_sub(64)
    }

    /// The `text/uri-list` payload for the files currently owned, if any.
    fn uri_list_payload(&self, gnome: bool) -> Option<String> {
        let files = self.owned_files.as_ref()?;
        let list = lynxrdp_proto::urilist::build(files);
        // GNOME's variant is the same list prefixed with the operation.
        Some(if gnome {
            format!("copy\n{}", list.replace("\r\n", "\n"))
        } else {
            list
        })
    }

    /// Answer one conversion request; returns whether it was satisfied.
    fn serve(&mut self, e: &SelectionRequestEvent, property: xproto::Atom) -> Result<bool> {
        if e.selection != self.atoms.clipboard {
            return Ok(false);
        }
        let limit = self.max_property_bytes();
        let conn = self.display.conn();
        if e.target == self.atoms.targets {
            let mut list = vec![self.atoms.targets, self.atoms.timestamp];
            if self.owned_text.as_ref().is_some_and(|t| t.len() <= limit) {
                list.extend([
                    self.atoms.utf8_string,
                    self.atoms.text,
                    AtomEnum::STRING.into(),
                ]);
            }
            if self.owned_png.as_ref().is_some_and(|p| p.len() <= limit) {
                list.push(self.atoms.png);
            }
            if self
                .uri_list_payload(false)
                .is_some_and(|l| l.len() <= limit)
            {
                list.push(self.atoms.uri_list);
                list.push(self.atoms.gnome_copied);
            }
            conn.change_property32(
                PropMode::REPLACE,
                e.requestor,
                property,
                AtomEnum::ATOM,
                &list,
            )?;
            return Ok(true);
        }
        if e.target == self.atoms.timestamp {
            conn.change_property32(
                PropMode::REPLACE,
                e.requestor,
                property,
                AtomEnum::INTEGER,
                &[0u32],
            )?;
            return Ok(true);
        }
        if e.target == self.atoms.uri_list || e.target == self.atoms.gnome_copied {
            let Some(payload) = self.uri_list_payload(e.target == self.atoms.gnome_copied) else {
                return Ok(false);
            };
            if payload.len() > limit {
                log::warn!(
                    "clipboard: file list of {} bytes exceeds the {limit}-byte \
                     property limit; declining the conversion",
                    payload.len()
                );
                return Ok(false);
            }
            conn.change_property8(
                PropMode::REPLACE,
                e.requestor,
                property,
                e.target,
                payload.as_bytes(),
            )?;
            return Ok(true);
        }
        if e.target == self.atoms.png {
            let Some(png) = self.owned_png.as_ref() else {
                return Ok(false);
            };
            if png.len() > limit {
                // The common way to reach this: copy a large screenshot on the
                // client, then press Ctrl+V in the session.
                log::warn!(
                    "clipboard: PNG of {} bytes exceeds the {limit}-byte property \
                     limit; declining the conversion",
                    png.len()
                );
                return Ok(false);
            }
            conn.change_property8(
                PropMode::REPLACE,
                e.requestor,
                property,
                self.atoms.png,
                png,
            )?;
            return Ok(true);
        }
        let Some(text) = self.owned_text.as_ref() else {
            return Ok(false);
        };
        if text.len() > limit {
            log::warn!(
                "clipboard: text of {} bytes exceeds the {limit}-byte property \
                 limit; declining the conversion",
                text.len()
            );
            return Ok(false);
        }
        if e.target == self.atoms.utf8_string || e.target == self.atoms.text {
            conn.change_property8(
                PropMode::REPLACE,
                e.requestor,
                property,
                self.atoms.utf8_string,
                text.as_bytes(),
            )?;
            return Ok(true);
        }
        if e.target == AtomEnum::STRING.into() {
            let latin1: Vec<u8> = text
                .chars()
                .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
                .collect();
            conn.change_property8(
                PropMode::REPLACE,
                e.requestor,
                property,
                AtomEnum::STRING,
                &latin1,
            )?;
            return Ok(true);
        }
        Ok(false)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        let conn = self.display.conn();
        let _ = conn.destroy_window(self.window);
        let _ = conn.flush();
    }
}
