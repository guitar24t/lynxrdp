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
//!
//! # One conversion at a time, but never a dropped one
//!
//! Only one conversion may be outstanding: an X selection transfer is a
//! conversation with another client conducted through one property on one
//! window, and running two of them at once would interleave their `INCR`
//! chunks. Requests that arrive meanwhile are therefore queued rather than
//! dropped. Dropping them was not an edge case -- a client that has just been
//! offered PNG and FILES asks for both in the same burst, and the second
//! request used to be answered with nothing at all, so that format never
//! arrived and the paste stayed pending forever.
//!
//! Every conversion also carries a deadline. `ConvertSelection` has no timeout
//! of its own, and a selection owner that never replies would otherwise block
//! every later clipboard read until some other application happened to take
//! the selection.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// How long a conversion may go unanswered before it is abandoned.
///
/// This bounds *silence*, not the length of a transfer: an `INCR` transfer
/// refreshes the deadline on every chunk, so a 64 MiB image arriving in
/// property-sized pieces is legitimately slow and stays welcome, while an
/// owner that has stopped answering is cut loose. Ten seconds is far longer
/// than any healthy toolkit takes to serialise a selection and short enough
/// that a user who pressed Ctrl+V is still connecting cause and effect.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// When to give up. Refreshed by every sign of life from the owner.
    deadline: Instant,
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
    /// Formats asked for while another conversion was outstanding.
    ///
    /// Bounded by construction: only the three known [`clipboard_format`]
    /// values can be enqueued and each appears at most once, so this holds
    /// three entries at the very most however hard a client pushes.
    queue: VecDeque<u32>,
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
            queue: VecDeque::new(),
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
    ///
    /// A request made while another conversion is outstanding is queued, not
    /// refused. This is the ordinary case rather than a corner one: an offer of
    /// PNG and FILES draws two requests from the client back to back, both
    /// arrive in the same drain of its message channel, and the second used to
    /// be discarded with a debug line and no reply at all.
    pub fn request_format(&mut self, format: u32) -> Result<()> {
        if self.available & format == 0 {
            return Ok(());
        }
        let Some(target) = self.target_for(format) else {
            return Ok(());
        };
        // Already coming, or already waiting: asking twice would convert the
        // same selection twice for one paste. This is tested before the
        // in-flight check rather than inside it, because the queue can briefly
        // outlive the fetch it was waiting behind -- `start_next` runs only
        // from the event tail, so an X error on the way out of `dispatch`
        // leaves entries in place with nothing in flight. Starting one of them
        // here as if it were new would convert it twice and offer the client
        // two copies of the same paste.
        if self.fetch.as_ref().is_some_and(|f| f.format == format) || self.queue.contains(&format) {
            return Ok(());
        }
        if self.fetch.is_some() || !self.queue.is_empty() {
            log::debug!("clipboard: queueing a request for format {format:#x}");
            self.queue.push_back(format);
            return Ok(());
        }
        self.request(target, format, self.owner_time)
    }

    /// The X target atom that produces `format`, if we can ask for it at all.
    fn target_for(&self, format: u32) -> Option<xproto::Atom> {
        match format {
            f if f == clipboard_format::PNG => Some(self.atoms.png),
            f if f == clipboard_format::FILES => Some(self.atoms.uri_list),
            f if f == clipboard_format::TEXT => Some(self.atoms.utf8_string),
            _ => None,
        }
    }

    /// Feed an X event, producing any clipboard activity it implies.
    ///
    /// The tail of this function is the *only* place a queued conversion is
    /// started. Draining at each site that clears `fetch` would be wrong twice
    /// over: `on_selection_notify` deliberately re-arms it with a plain
    /// `STRING` fallback when a UTF-8 conversion is refused, and `on_targets`
    /// deliberately re-arms it to pull text eagerly. Both survive because
    /// `start_next` refuses to act while any fetch exists, so the queue can
    /// only ever fill a genuine gap.
    pub fn handle_event(&mut self, ev: &Event) -> Result<Vec<ClipboardEvent>> {
        let mut events = self.dispatch(ev)?;
        // Expire after dispatching, not before: a reply that arrives in the
        // same wake-up as the deadline came from a live owner, not a wedged
        // one, and abandoning it would throw away a paste that did work.
        events.extend(self.expire_fetch());
        events.extend(self.start_next()?);
        Ok(events)
    }

    /// Advance the clipboard without an X event.
    ///
    /// [`Clipboard::handle_event`] can only notice a wedged selection owner
    /// when some other X event happens to arrive, and a desktop with nothing
    /// moving on it produces none -- which is exactly the state a session is in
    /// while its user waits on a paste. Calling this from the session's
    /// housekeeping tick is what bounds the wait in that case; it is otherwise
    /// cheap and does nothing.
    pub fn tick(&mut self) -> Result<Vec<ClipboardEvent>> {
        let mut events = self.expire_fetch();
        events.extend(self.start_next()?);
        Ok(events)
    }

    /// Abandon a conversion whose deadline has passed.
    ///
    /// The queue is left alone: whatever is waiting behind the dead request is
    /// addressed to the same owner, but it is a different target and may well
    /// be one the owner can answer, so `start_next` gets to try it.
    fn expire_fetch(&mut self) -> Vec<ClipboardEvent> {
        let Some(fetch) = self.fetch.as_ref() else {
            return Vec::new();
        };
        if Instant::now() < fetch.deadline {
            return Vec::new();
        }
        let (format, target) = (fetch.format, fetch.target);
        log::warn!(
            "clipboard: the selection owner did not answer a conversion to atom {target} \
             within {}s; abandoning it",
            FETCH_TIMEOUT.as_secs()
        );
        self.fetch = None;
        if target == self.atoms.targets {
            // Nobody was promised anything: TARGETS is asked for on our own
            // initiative, before the far end has been told a thing.
            Vec::new()
        } else {
            vec![ClipboardEvent::Unavailable(format)]
        }
    }

    /// Start the next queued conversion, if nothing is in flight.
    fn start_next(&mut self) -> Result<Vec<ClipboardEvent>> {
        let mut events = Vec::new();
        while self.fetch.is_none() {
            let Some(format) = self.queue.pop_front() else {
                break;
            };
            // Belt and braces: the queue is emptied on an owner change, so a
            // format the current owner does not offer should be impossible.
            match self.target_for(format) {
                Some(target) if self.available & format != 0 => {
                    self.request(target, format, self.owner_time)?;
                }
                _ => events.push(ClipboardEvent::Unavailable(format)),
            }
        }
        Ok(events)
    }

    /// Drop the outstanding conversion and everything queued behind it,
    /// telling the far end that none of it is coming.
    ///
    /// Silence is the one answer we must not give: the client shows a paste as
    /// pending until it hears something, so an abandoned conversion has to come
    /// back as `Unavailable` even though nothing went wrong on the wire.
    fn cancel_all(&mut self) -> Vec<ClipboardEvent> {
        let mut events = Vec::new();
        if let Some(fetch) = self.fetch.take() {
            if fetch.target != self.atoms.targets {
                events.push(ClipboardEvent::Unavailable(fetch.format));
            }
        }
        events.extend(self.queue.drain(..).map(ClipboardEvent::Unavailable));
        events
    }

    fn dispatch(&mut self, ev: &Event) -> Result<Vec<ClipboardEvent>> {
        match ev {
            Event::XfixesSelectionNotify(e) if e.selection == self.atoms.clipboard => {
                if e.owner == self.window {
                    return Ok(Vec::new());
                }
                self.owned_text = None;
                self.owned_png = None;
                self.available = 0;
                // Anything in flight or queued was addressed to the previous
                // owner and its format list. That owner has gone and will never
                // answer, and the formats were indexes into a list that no
                // longer applies, so none of it may be carried across.
                let events = self.cancel_all();
                if e.owner == x11rb::NONE {
                    return Ok(events);
                }
                self.owner_time = e.timestamp;
                // Learn what the new owner can produce before fetching.
                self.request(self.atoms.targets, 0, e.timestamp)?;
                Ok(events)
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
            deadline: Instant::now() + FETCH_TIMEOUT,
        });
        Ok(())
    }

    fn on_selection_notify(&mut self, e: &SelectionNotifyEvent) -> Result<Vec<ClipboardEvent>> {
        let Some(fetch) = self.fetch.as_mut() else {
            return Ok(Vec::new());
        };
        if e.target != fetch.target {
            // An answer to a conversion we already gave up on. Since the
            // deadline exists, a slow owner can reply after we have moved on
            // to the next queued format, and every conversion lands in the
            // same property -- so the echoed target (ICCCM 2.2 requires both
            // the owner and the server to copy it from the request) is the
            // only thing that tells them apart. Accepting one would hand the
            // live fetch someone else's bytes and report, say, a text
            // selection to the client as a PNG. Leaving the property untouched
            // is safe: the owner we are actually waiting on writes it before
            // sending its own notify.
            log::debug!(
                "clipboard: ignoring a late SelectionNotify for atom {} while converting atom {}",
                e.target,
                fetch.target
            );
            return Ok(Vec::new());
        }
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
            // A transfer that is only now beginning gets the full deadline for
            // its first chunk rather than whatever the handshake left of it.
            fetch.deadline = Instant::now() + FETCH_TIMEOUT;
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
        // Every chunk is a sign of life, so the deadline bounds the gap
        // between chunks rather than the length of the whole transfer.
        fetch.deadline = Instant::now() + FETCH_TIMEOUT;
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
