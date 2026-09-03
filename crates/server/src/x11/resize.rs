//! Screen resizing with RANDR.
//!
//! Xvfb (and Xorg with the dummy driver) allow any size up to the virtual
//! screen size they were started with. We create a mode for the requested
//! size, attach it to the first output and switch the CRTC to it.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use x11rb::protocol::randr::{self, ConnectionExt as _};

use super::XDisplay;

/// Resize the root window to `width` x `height`.
pub fn resize_screen(display: &Arc<XDisplay>, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        bail!("invalid size {width}x{height}");
    }
    anyhow::ensure!(display.ext.randr, "RANDR extension required for resizing");
    let conn = display.conn();
    let root = display.root();
    let (cur_w, cur_h) = display.refresh_size()?;
    if (cur_w, cur_h) == (width, height) {
        return Ok(());
    }
    let res = conn
        .randr_get_screen_resources_current(root)?
        .reply()
        .context("screen resources")?;
    let output = *res
        .outputs
        .first()
        .ok_or_else(|| anyhow!("no RANDR outputs"))?;
    let crtc = *res.crtcs.first().ok_or_else(|| anyhow!("no RANDR crtcs"))?;

    // Find or create a mode of the requested size.
    let name = format!("lynx-{width}x{height}");
    let existing = res
        .modes
        .iter()
        .find(|m| u32::from(m.width) == width && u32::from(m.height) == height)
        .map(|m| m.id);
    let mode = match existing {
        Some(id) => id,
        None => {
            let info = randr::ModeInfo {
                id: 0,
                width: width as u16,
                height: height as u16,
                dot_clock: 0,
                hsync_start: 0,
                hsync_end: 0,
                htotal: 0,
                hskew: 0,
                vsync_start: 0,
                vsync_end: 0,
                vtotal: 0,
                name_len: name.len() as u16,
                mode_flags: randr::ModeFlag::default(),
            };
            let id = conn
                .randr_create_mode(root, info, name.as_bytes())?
                .reply()
                .context("create mode")?
                .mode;
            conn.randr_add_output_mode(output, id)?
                .check()
                .context("add output mode")?;
            id
        }
    };
    let (mm_w, mm_h) = mm_for(width, height, 96);
    let growing = width > cur_w || height > cur_h;
    if growing {
        conn.randr_set_screen_size(root, width as u16, height as u16, mm_w, mm_h)?
            .check()
            .context("set screen size")?;
    }
    let r = conn
        .randr_set_crtc_config(
            crtc,
            x11rb::CURRENT_TIME,
            x11rb::CURRENT_TIME,
            0,
            0,
            mode,
            randr::Rotation::ROTATE0,
            &[output],
        )?
        .reply()
        .context("set crtc config")?;
    if r.status != randr::SetConfig::SUCCESS {
        bail!("RANDR SetCrtcConfig failed: {:?}", r.status);
    }
    if !growing {
        // Shrinking: the CRTC must fit before the screen can shrink.
        conn.randr_set_screen_size(root, width as u16, height as u16, mm_w, mm_h)?
            .check()
            .context("set screen size")?;
    }
    // Remove modes we created earlier that are no longer in use, to keep the
    // mode list from growing without bound during interactive resizing.
    for m in &res.modes {
        if m.id != mode && res.names_for_mode(m).starts_with(b"lynx-") {
            let _ = conn.randr_delete_output_mode(output, m.id);
            let _ = conn.randr_destroy_mode(m.id);
        }
    }
    display.sync()?;
    let got = display.refresh_size()?;
    if got != (width, height) {
        bail!("resize to {width}x{height} produced {}x{}", got.0, got.1);
    }
    Ok(())
}

/// Physical size in millimetres for a pixel size at the given DPI.
pub fn mm_for(width: u32, height: u32, dpi: u32) -> (u32, u32) {
    let f = |px: u32| ((f64::from(px) * 25.4) / f64::from(dpi)).round().max(1.0) as u32;
    (f(width), f(height))
}

trait ModeNames {
    fn names_for_mode(&self, mode: &randr::ModeInfo) -> &[u8];
}

impl ModeNames for randr::GetScreenResourcesCurrentReply {
    fn names_for_mode(&self, mode: &randr::ModeInfo) -> &[u8] {
        // Names are concatenated in mode order.
        let mut offset = 0usize;
        for m in &self.modes {
            let len = usize::from(m.name_len);
            if m.id == mode.id {
                return self.names.get(offset..offset + len).unwrap_or(&[]);
            }
            offset += len;
        }
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millimetres_at_96_dpi() {
        assert_eq!(mm_for(1920, 1080, 96), (508, 286));
        assert_eq!(mm_for(1, 1, 96), (1, 1));
    }
}
