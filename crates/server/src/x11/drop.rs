//! Native XDND copy delivery. The receiving file manager chooses the folder;
//! the server never guesses a destination from a window title or process cwd.
use super::XDisplay;
use anyhow::{bail, Result};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{self, AtomEnum, ConnectionExt as _, EventMask, PropMode},
        Event,
    },
    wrapper::ConnectionExt as _,
};

#[derive(Clone, Copy)]
pub struct Target {
    window: u32,
    proxy: u32,
    version: u32,
    pub desktop: bool,
    x: u16,
    y: u16,
}
struct Active {
    id: u64,
    target: Target,
    original_target: Target,
    empty_probe: Option<super::empty_drop::Probe>,
    empty_result: Option<crossbeam_channel::Receiver<Option<(u16, u16)>>>,
    files: String,
    time: u32,
    dropped: bool,
    rejected_at: Option<Instant>,
    retry_at: Option<Instant>,
    deadline: Instant,
}
pub struct DropSource {
    display: Arc<XDisplay>,
    window: u32,
    aware: u32,
    proxy: u32,
    enter: u32,
    position: u32,
    status: u32,
    drop: u32,
    leave: u32,
    finished: u32,
    selection: u32,
    uri: u32,
    targets: u32,
    copy: u32,
    clock: u32,
    active: Option<Active>,
}

impl DropSource {
    pub fn new(display: Arc<XDisplay>) -> Result<Self> {
        let c = display.conn();
        let window = c.generate_id()?;
        c.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            display.root(),
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            0,
            &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
        let s = Self {
            window,
            aware: display.atom("XdndAware")?,
            proxy: display.atom("XdndProxy")?,
            enter: display.atom("XdndEnter")?,
            position: display.atom("XdndPosition")?,
            status: display.atom("XdndStatus")?,
            drop: display.atom("XdndDrop")?,
            leave: display.atom("XdndLeave")?,
            finished: display.atom("XdndFinished")?,
            selection: display.atom("XdndSelection")?,
            uri: display.atom("text/uri-list")?,
            targets: display.atom("TARGETS")?,
            copy: display.atom("XdndActionCopy")?,
            clock: display.atom("LYNXRDP_DROP_TIME")?,
            display,
            active: None,
        };
        s.display.conn().change_property32(
            PropMode::REPLACE,
            window,
            s.aware,
            AtomEnum::ATOM,
            &[5],
        )?;
        s.display.conn().change_property32(
            PropMode::REPLACE,
            window,
            s.display.atom("XdndActionList")?,
            AtomEnum::ATOM,
            &[s.copy],
        )?;
        Ok(s)
    }

    fn property(&self, window: u32, atom: u32) -> Result<Option<u32>> {
        let reply = self
            .display
            .conn()
            .get_property(false, window, atom, AtomEnum::ANY, 0, 1)?
            .reply()?;
        Ok(reply.value32().and_then(|mut v| v.next()))
    }

    /// Snapshot the actual drop-aware window under the released pointer.
    pub fn target(&self, x: u16, y: u16) -> Result<Target> {
        let mut window = self.display.root();
        let mut target = None;
        for _ in 0..64 {
            let proxy = self
                .property(window, self.proxy)?
                .filter(|p| self.property(*p, self.proxy).ok().flatten() == Some(*p))
                .unwrap_or(window);
            if let Some(version) = self.property(proxy, self.aware)? {
                if version >= 3 {
                    target = Some(Target {
                        window,
                        proxy,
                        version: version.min(5),
                        desktop: false,
                        x,
                        y,
                    });
                }
            }
            let child = self
                .display
                .conn()
                .translate_coordinates(self.display.root(), window, x as i16, y as i16)?
                .reply()?
                .child;
            if child == 0 || child == window {
                break;
            }
            window = child;
        }
        if target.is_none() && self.is_bare_desktop(window, x, y)? {
            target = Some(Target {
                window,
                proxy: window,
                version: 0,
                desktop: true,
                x,
                y,
            });
        }
        target.ok_or_else(|| {
            anyhow::anyhow!("Drop onto a file manager folder or desktop that accepts files.")
        })
    }

    fn text_property(&self, window: u32, name: &str) -> Result<Vec<u8>> {
        Ok(self
            .display
            .conn()
            .get_property(
                false,
                window,
                self.display.atom(name)?,
                AtomEnum::ANY,
                0,
                256,
            )?
            .reply()?
            .value)
    }

    fn is_bare_desktop(&self, window: u32, x: u16, y: u16) -> Result<bool> {
        let root = self.display.root();
        let desktop_type = self.display.atom("_NET_WM_WINDOW_TYPE_DESKTOP")?;
        let desktop = window == root
            || self.property(window, self.display.atom("_NET_WM_WINDOW_TYPE")?)?
                == Some(desktop_type);
        let guard = self.text_property(window, "WM_NAME")? == b"mutter guard window";
        if !desktop && !guard {
            return Ok(false);
        }
        if guard {
            let Some(wm) = self.property(root, self.display.atom("_NET_SUPPORTING_WM_CHECK")?)?
            else {
                return Ok(false);
            };
            if self.text_property(wm, "_NET_WM_NAME")? != b"GNOME Shell" {
                return Ok(false);
            }
        }
        // Exclude panels and docks from the desktop fallback.
        let workarea = self
            .display
            .conn()
            .get_property(
                false,
                root,
                self.display.atom("_NET_WORKAREA")?,
                AtomEnum::CARDINAL,
                0,
                4096,
            )?
            .reply()?;
        if let Some(values) = workarea.value32() {
            let values: Vec<_> = values.collect();
            let current = self
                .property(root, self.display.atom("_NET_CURRENT_DESKTOP")?)?
                .unwrap_or(0) as usize;
            if let Some(area) = values.get(current * 4..current * 4 + 4) {
                return Ok(u32::from(x) >= area[0]
                    && u32::from(y) >= area[1]
                    && u32::from(x) < area[0].saturating_add(area[2])
                    && u32::from(y) < area[1].saturating_add(area[3]));
            }
        }
        Ok(desktop && !guard)
    }

    pub fn validate(&self, target: Target) -> Result<()> {
        let current = self.target(target.x, target.y)?;
        if current.window != target.window || current.desktop != target.desktop {
            bail!("The drop target changed. Drop the files again.");
        }
        Ok(())
    }

    pub fn busy(&self) -> bool {
        self.active.is_some()
    }

    pub fn start(&mut self, id: u64, target: Target, files: &[PathBuf]) -> Result<()> {
        if self.busy() {
            bail!("another drop is being delivered");
        }
        // Refuse a closed/moved-over target rather than delivering elsewhere.
        let current = self.target(target.x, target.y)?;
        if current.window != target.window {
            bail!("The drop target changed. Drop the files again.");
        }
        let files = lynxrdp_proto::urilist::build(files);
        if files.len() > 128 * 1024 {
            bail!("Too many file names for this drop. Drop fewer files at once.");
        }
        let empty_probe = self.empty_probe(target).ok().flatten();
        self.active = Some(Active {
            id,
            target,
            original_target: target,
            empty_probe,
            empty_result: None,
            files,
            time: 0,
            dropped: false,
            rejected_at: None,
            retry_at: None,
            deadline: Instant::now() + Duration::from_secs(15),
        });
        // PropertyNotify supplies an X-server timestamp without blocking UI.
        self.display.conn().change_property32(
            PropMode::REPLACE,
            self.window,
            self.clock,
            AtomEnum::CARDINAL,
            &[1],
        )?;
        self.display.conn().flush()?;
        Ok(())
    }

    fn empty_probe(&self, target: Target) -> Result<Option<super::empty_drop::Probe>> {
        let class = self.text_property(target.window, "WM_CLASS")?;
        if !class
            .split(|b| *b == 0)
            .any(|name| name.eq_ignore_ascii_case(b"org.gnome.nautilus"))
        {
            return Ok(None);
        }
        let Some(pid) = self.property(target.window, self.display.atom("_NET_WM_PID")?)? else {
            return Ok(None);
        };
        let geometry = self.display.conn().get_geometry(target.window)?.reply()?;
        let origin = self
            .display
            .conn()
            .translate_coordinates(target.window, self.display.root(), 0, 0)?
            .reply()?;
        Ok(Some(super::empty_drop::Probe {
            pid,
            x: target.x,
            y: target.y,
            window: (origin.dst_x, origin.dst_y, geometry.width, geometry.height),
        }))
    }

    fn send(&self, target: Target, kind: u32, data: [u32; 5]) -> Result<()> {
        self.display.conn().send_event(
            false,
            target.proxy,
            EventMask::NO_EVENT,
            xproto::ClientMessageEvent::new(32, target.window, kind, data),
        )?;
        self.display.conn().flush()?;
        Ok(())
    }

    fn finish(&mut self, ok: bool, reason: &str) -> Option<(u64, bool, String)> {
        let active = self.active.take()?;
        let _ = self.send(active.target, self.leave, [self.window, 0, 0, 0, 0]);
        Some((active.id, ok, reason.into()))
    }

    pub fn fail(&mut self, reason: &str) -> Option<(u64, bool, String)> {
        self.finish(false, reason)
    }

    pub fn cancel(&mut self) {
        self.finish(false, "Drop cancelled");
    }

    pub fn poll(&mut self) -> Option<(u64, bool, String)> {
        let candidate = self
            .active
            .as_mut()
            .filter(|a| !a.dropped)
            .and_then(|active| {
                let result = active.empty_result.as_ref()?.try_recv();
                match result {
                    Ok(point) => {
                        active.empty_result = None;
                        point.map(|point| (active.original_target, point))
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        active.empty_result = None;
                        None
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => None,
                }
            });
        if let Some((original, (x, y))) = candidate {
            let target = self.validate(original).and_then(|()| self.target(x, y));
            if let Ok(target) = target {
                if target.window == original.window && !target.desktop {
                    if let Some(active) = &mut self.active {
                        log::debug!(
                            "retrying drop {} in the same empty Nautilus view at {x},{y}",
                            active.id
                        );
                        active.target = target;
                        active.rejected_at = None;
                        active.retry_at = Some(Instant::now());
                    }
                }
            }
        }
        if let Some(active) = self.active.as_ref().filter(|a| !a.dropped) {
            let now = Instant::now();
            if active
                .rejected_at
                .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(3))
            {
                return self.finish(
                    false,
                    "That location does not accept copied files. Drop onto a folder or desktop.",
                );
            }
            if active.retry_at.is_some_and(|at| now >= at) {
                let target = active.target;
                let time = active.time;
                self.active.as_mut().unwrap().retry_at = None;
                // A file manager may need a new motion after asynchronously
                // reading URI data. Reoffer the same location, never another.
                let result = self.target(target.x, target.y).and_then(|current| {
                    if current.window != target.window {
                        bail!("The drop target changed. Drop the files again.");
                    }
                    self.send(
                        target,
                        self.position,
                        [
                            self.window,
                            0,
                            (u32::from(target.x) << 16) | u32::from(target.y),
                            time,
                            self.copy,
                        ],
                    )
                });
                if let Err(e) = result {
                    return self.finish(false, &e.to_string());
                }
            }
        }
        if self
            .active
            .as_ref()
            .is_some_and(|a| Instant::now() >= a.deadline)
        {
            return self.finish(false, "The target did not finish accepting the drop. Check the destination before retrying.");
        }
        None
    }

    pub fn event(&mut self, event: &Event) -> Result<Option<(u64, bool, String)>> {
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        match event {
            Event::PropertyNotify(e)
                if e.window == self.window && e.atom == self.clock && active.time == 0 =>
            {
                active.time = e.time;
                let (target, time) = (active.target, active.time);
                self.display
                    .conn()
                    .set_selection_owner(self.window, self.selection, time)?;
                self.send(
                    target,
                    self.enter,
                    [self.window, target.version << 24, self.uri, 0, 0],
                )?;
                self.send(
                    target,
                    self.position,
                    [
                        self.window,
                        0,
                        (u32::from(target.x) << 16) | u32::from(target.y),
                        time,
                        self.copy,
                    ],
                )?;
            }
            Event::ClientMessage(e) if e.window == self.window && e.format == 32 => {
                let data = e.data.as_data32();
                if data[0] != active.target.window && data[0] != active.target.proxy {
                    return Ok(None);
                }
                if e.type_ == self.status && !active.dropped {
                    if data[1] & 1 == 0 || data[4] != self.copy {
                        // GTK/Nautilus can report no action until its async
                        // selection conversion and file metadata are ready.
                        if let Some(probe) = active.empty_probe.take() {
                            active.empty_result = Some(probe.start());
                        }
                        active.rejected_at.get_or_insert_with(Instant::now);
                        active.retry_at = Some(Instant::now() + Duration::from_millis(100));
                        return Ok(None);
                    }
                    active.dropped = true;
                    active.deadline = Instant::now() + Duration::from_secs(300);
                    let (target, time) = (active.target, active.time);
                    self.send(target, self.drop, [self.window, 0, time, 0, 0])?;
                } else if e.type_ == self.finished && active.dropped {
                    let ok = active.target.version < 5 || data[1] & 1 != 0;
                    return Ok(self.finish(
                        ok,
                        if ok {
                            "Files delivered to the selected location."
                        } else {
                            "The target rejected the file drop."
                        },
                    ));
                }
            }
            Event::SelectionRequest(e)
                if e.owner == self.window && e.selection == self.selection =>
            {
                let c = self.display.conn();
                let property = if e.property == 0 {
                    e.target
                } else {
                    e.property
                };
                let accepted = if e.target == self.uri {
                    c.change_property8(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        self.uri,
                        active.files.as_bytes(),
                    )?;
                    true
                } else if e.target == self.targets {
                    c.change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        AtomEnum::ATOM,
                        &[self.targets, self.uri],
                    )?;
                    true
                } else {
                    false
                };
                c.send_event(
                    false,
                    e.requestor,
                    EventMask::NO_EVENT,
                    xproto::SelectionNotifyEvent {
                        response_type: xproto::SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: e.time,
                        requestor: e.requestor,
                        selection: e.selection,
                        target: e.target,
                        property: if accepted { property } else { 0 },
                    },
                )?;
                c.flush()?;
            }
            _ => {}
        }
        Ok(None)
    }
}

impl Drop for DropSource {
    fn drop(&mut self) {
        let _ = self.display.conn().destroy_window(self.window);
    }
}
