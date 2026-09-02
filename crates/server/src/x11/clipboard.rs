//! X11 clipboard (`CLIPBOARD` selection) bridging.
//!
//! Text copied inside the session is detected through XFIXES selection
//! owner notifications, fetched with `ConvertSelection` (INCR aware) and
//! reported to the caller. Text arriving from the remote client is offered
//! to the session by taking ownership of the selection and answering
//! `SelectionRequest`s for the common text targets.
//!
//! This is a state machine driven from the session core thread with the X
//! events it receives.

use std::sync::Arc;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::{xfixes, Event};
use x11rb::wrapper::ConnectionExt as _;

use super::XDisplay;

/// Largest clipboard text accepted in either direction (4 MiB).
pub const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;

struct Atoms {
    clipboard: xproto::Atom,
    utf8_string: xproto::Atom,
    text: xproto::Atom,
    targets: xproto::Atom,
    incr: xproto::Atom,
    timestamp: xproto::Atom,
    prop: xproto::Atom,
}

struct Fetch {
    target: xproto::Atom,
    incr: bool,
    buf: Vec<u8>,
    tried_string: bool,
}

/// Clipboard bridge state.
pub struct Clipboard {
    display: Arc<XDisplay>,
    window: xproto::Window,
    atoms: Atoms,
    owned_text: Option<String>,
    fetch: Option<Fetch>,
    last_reported: Option<String>,
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
            fetch: None,
            last_reported: None,
        })
    }

    /// Offer `text` to applications in the session.
    pub fn set_text(&mut self, text: String) -> Result<()> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            log::warn!("ignoring clipboard text of {} bytes", text.len());
            return Ok(());
        }
        let conn = self.display.conn();
        conn.set_selection_owner(self.window, self.atoms.clipboard, x11rb::CURRENT_TIME)?
            .check()?;
        let owner = conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        if owner != self.window {
            log::warn!("could not acquire CLIPBOARD ownership");
            return Ok(());
        }
        self.last_reported = Some(text.clone());
        self.owned_text = Some(text);
        conn.flush()?;
        Ok(())
    }

    /// Whether we currently own the selection.
    pub fn owns_selection(&self) -> bool {
        self.owned_text.is_some()
    }

    /// Feed an X event. Returns text newly copied inside the session, if any.
    pub fn handle_event(&mut self, ev: &Event) -> Result<Option<String>> {
        match ev {
            Event::XfixesSelectionNotify(e) if e.selection == self.atoms.clipboard => {
                if e.owner == self.window {
                    return Ok(None);
                }
                if e.owner == x11rb::NONE {
                    // Selection cleared; nothing to fetch.
                    return Ok(None);
                }
                if self.fetch.is_some() {
                    // A fetch is in flight; the new owner's content will be
                    // picked up once this completes (we re-request then).
                    return Ok(None);
                }
                self.owned_text = None;
                self.request(self.atoms.utf8_string, e.timestamp)?;
                Ok(None)
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
                Ok(None)
            }
            Event::SelectionClear(e)
                if e.owner == self.window && e.selection == self.atoms.clipboard =>
            {
                self.owned_text = None;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn request(&mut self, target: xproto::Atom, time: xproto::Timestamp) -> Result<()> {
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
            incr: false,
            buf: Vec::new(),
            tried_string: target == AtomEnum::STRING.into(),
        });
        Ok(())
    }

    fn on_selection_notify(&mut self, e: &SelectionNotifyEvent) -> Result<Option<String>> {
        let Some(fetch) = self.fetch.as_mut() else {
            return Ok(None);
        };
        if e.property == x11rb::NONE {
            // Conversion refused. Fall back to STRING once.
            let tried_string = fetch.tried_string;
            self.fetch = None;
            if !tried_string {
                self.request(AtomEnum::STRING.into(), e.time)?;
            }
            return Ok(None);
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
            // INCR transfer: deleting the property (done above) starts it.
            fetch.incr = true;
            fetch.buf.clear();
            return Ok(None);
        }
        let _ = fetch.target;
        self.fetch = None;
        self.finish(reply.value)
    }

    fn on_incr_chunk(&mut self) -> Result<Option<String>> {
        let Some(fetch) = self.fetch.as_mut() else {
            return Ok(None);
        };
        if !fetch.incr {
            return Ok(None);
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
            let data = std::mem::take(&mut fetch.buf);
            self.fetch = None;
            return self.finish(data);
        }
        if fetch.buf.len() + reply.value.len() > MAX_CLIPBOARD_BYTES {
            log::warn!("clipboard INCR transfer exceeds limit; dropping");
            self.fetch = None;
            return Ok(None);
        }
        fetch.buf.extend_from_slice(&reply.value);
        Ok(None)
    }

    fn finish(&mut self, data: Vec<u8>) -> Result<Option<String>> {
        if data.len() > MAX_CLIPBOARD_BYTES {
            log::warn!("clipboard text too large ({} bytes)", data.len());
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        if text.is_empty() || self.last_reported.as_deref() == Some(text.as_str()) {
            return Ok(None);
        }
        self.last_reported = Some(text.clone());
        Ok(Some(text))
    }

    fn on_selection_request(&mut self, e: &SelectionRequestEvent) -> Result<()> {
        let conn = self.display.conn();
        let mut property = e.property;
        if property == x11rb::NONE {
            property = e.target;
        }
        let served = match &self.owned_text {
            Some(text) if e.selection == self.atoms.clipboard => {
                if e.target == self.atoms.targets {
                    let list = [
                        self.atoms.targets,
                        self.atoms.timestamp,
                        self.atoms.utf8_string,
                        self.atoms.text,
                        AtomEnum::STRING.into(),
                    ];
                    conn.change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )?;
                    true
                } else if e.target == self.atoms.timestamp {
                    conn.change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        AtomEnum::INTEGER,
                        &[0u32],
                    )?;
                    true
                } else if e.target == self.atoms.utf8_string || e.target == self.atoms.text {
                    conn.change_property8(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        self.atoms.utf8_string,
                        text.as_bytes(),
                    )?;
                    true
                } else if e.target == AtomEnum::STRING.into() {
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
                    true
                } else {
                    false
                }
            }
            _ => false,
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
        conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
        conn.flush()?;
        Ok(())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        let conn = self.display.conn();
        let _ = conn.destroy_window(self.window);
        let _ = conn.flush();
    }
}
