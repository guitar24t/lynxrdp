//! Pointer shape tracking with XFIXES.

use std::sync::Arc;

use anyhow::{Context, Result};
use lynxrdp_proto::message::CursorImage;
use x11rb::protocol::xfixes;

use super::XDisplay;

/// Tracks the current cursor image.
pub struct CursorTracker {
    display: Arc<XDisplay>,
    last_serial: u32,
}

impl CursorTracker {
    /// Subscribe to cursor change notifications.
    pub fn new(display: Arc<XDisplay>) -> Result<Self> {
        anyhow::ensure!(display.ext.xfixes, "XFIXES required for cursor tracking");
        xfixes::select_cursor_input(
            display.conn(),
            display.root(),
            xfixes::CursorNotifyMask::DISPLAY_CURSOR,
        )?
        .check()
        .context("select cursor input")?;
        Ok(Self {
            display,
            last_serial: 0,
        })
    }

    /// Fetch the current cursor image. Returns `None` if it is unchanged
    /// since the last fetch (by serial) unless `force` is set.
    pub fn fetch(&mut self, force: bool) -> Result<Option<CursorImage>> {
        let reply = xfixes::get_cursor_image(self.display.conn())?
            .reply()
            .context("get cursor image")?;
        if !force && reply.cursor_serial == self.last_serial {
            return Ok(None);
        }
        self.last_serial = reply.cursor_serial;
        let (w, h) = (reply.width, reply.height);
        let n = usize::from(w) * usize::from(h);
        if w > lynxrdp_proto::message::MAX_CURSOR_DIM
            || h > lynxrdp_proto::message::MAX_CURSOR_DIM
            || reply.cursor_image.len() < n
        {
            log::warn!("ignoring oversized cursor {w}x{h}");
            return Ok(Some(CursorImage {
                width: 0,
                height: 0,
                hot_x: 0,
                hot_y: 0,
                argb: Vec::new(),
            }));
        }
        Ok(Some(CursorImage {
            width: w,
            height: h,
            hot_x: reply.xhot,
            hot_y: reply.yhot,
            argb: reply.cursor_image[..n].to_vec(),
        }))
    }
}
