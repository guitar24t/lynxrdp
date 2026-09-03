//! Screen capture with MIT-SHM (falling back to plain `GetImage`) and
//! change tracking with the DAMAGE extension.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use lynxrdp_proto::{Framebuffer, Rect};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat};
use x11rb::protocol::{damage, xfixes};

use super::XDisplay;

/// A System V shared memory segment attached both locally and to the server.
struct ShmSegment {
    display: Arc<XDisplay>,
    seg: u32,
    addr: *mut u8,
    size: usize,
}

// SAFETY: the mapping is only accessed through &self/&mut self methods and
// the X server side is identified by the `seg` id; nothing thread-local.
unsafe impl Send for ShmSegment {}

impl ShmSegment {
    fn new(display: Arc<XDisplay>, size: usize) -> Result<Self> {
        // SAFETY: plain libc calls with checked return values.
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if id < 0 {
                return Err(std::io::Error::last_os_error()).context("shmget");
            }
            let addr = libc::shmat(id, std::ptr::null(), 0);
            if addr == usize::MAX as *mut libc::c_void {
                let e = std::io::Error::last_os_error();
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                return Err(e).context("shmat");
            }
            let seg = display.generate_id()?;
            let attach = x11rb::protocol::shm::attach(display.conn(), seg, id as u32, false)
                .map_err(anyhow::Error::from)
                .and_then(|c| c.check().map_err(anyhow::Error::from));
            // Mark for removal now: the segment survives until both sides detach.
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            if let Err(e) = attach {
                libc::shmdt(addr);
                bail!("shm attach failed: {e}");
            }
            Ok(Self {
                display,
                seg,
                addr: addr as *mut u8,
                size,
            })
        }
    }

    fn bytes(&self, len: usize) -> &[u8] {
        assert!(len <= self.size);
        // SAFETY: the mapping is valid for `size` bytes for our lifetime.
        unsafe { std::slice::from_raw_parts(self.addr, len) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        if let Ok(c) = x11rb::protocol::shm::detach(self.display.conn(), self.seg) {
            let _ = c.check();
        }
        // SAFETY: addr came from shmat and is detached exactly once.
        unsafe {
            libc::shmdt(self.addr as *const libc::c_void);
        }
    }
}

/// Captures rectangles of the root window into a [`Framebuffer`].
pub struct ScreenCapture {
    display: Arc<XDisplay>,
    shm: Option<ShmSegment>,
    /// Largest capture that fits the SHM segment, in pixels.
    shm_pixels: usize,
}

impl ScreenCapture {
    /// Prepare capture for screens up to `max_width` x `max_height`.
    pub fn new(display: Arc<XDisplay>, max_width: u32, max_height: u32) -> Result<Self> {
        let shm_pixels = (max_width as usize) * (max_height as usize);
        let shm = if display.ext.shm {
            match ShmSegment::new(display.clone(), shm_pixels * 4) {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!("MIT-SHM unavailable ({e:#}); falling back to GetImage");
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            display,
            shm,
            shm_pixels,
        })
    }

    /// Whether shared memory capture is active.
    pub fn uses_shm(&self) -> bool {
        self.shm.is_some()
    }

    /// Capture `rect` (clipped to `fb`) from the root window into `fb`.
    pub fn capture_into(&mut self, fb: &mut Framebuffer, rect: &Rect) -> Result<()> {
        let rect = rect.intersect(&fb.bounds());
        if rect.is_empty() {
            return Ok(());
        }
        // Split very tall captures so they fit the SHM segment / request limits.
        let max_rows = (self.shm_pixels / rect.width as usize).clamp(1, 4096) as u32;
        let mut y = rect.y;
        while y < rect.bottom() {
            let h = (rect.bottom() - y).min(max_rows);
            let part = Rect::new(rect.x, y, rect.width, h);
            self.capture_part(fb, &part)?;
            y += h;
        }
        Ok(())
    }

    fn capture_part(&mut self, fb: &mut Framebuffer, rect: &Rect) -> Result<()> {
        let conn = self.display.conn();
        let root = self.display.root();
        let n = rect.area() as usize;
        let msb = self.display.msb_first();
        if let Some(shm) = &self.shm {
            let reply = x11rb::protocol::shm::get_image(
                conn,
                root,
                rect.x as i16,
                rect.y as i16,
                rect.width as u16,
                rect.height as u16,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                shm.seg,
                0,
            )?
            .reply()
            .context("shm GetImage")?;
            if reply.size as usize != n * 4 {
                bail!("unexpected shm image size {} for {}", reply.size, rect);
            }
            let bytes = shm.bytes(n * 4);
            copy_pixels(fb, rect, bytes, msb);
            Ok(())
        } else {
            let reply = conn
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    root,
                    rect.x as i16,
                    rect.y as i16,
                    rect.width as u16,
                    rect.height as u16,
                    !0,
                )?
                .reply()
                .context("GetImage")?;
            if reply.data.len() < n * 4 {
                bail!("short GetImage reply for {}", rect);
            }
            copy_pixels(fb, rect, &reply.data[..n * 4], msb);
            Ok(())
        }
    }
}

/// Convert server pixel bytes (32 bpp ZPixmap) to `0x00RRGGBB` and store them.
fn copy_pixels(fb: &mut Framebuffer, rect: &Rect, bytes: &[u8], msb_first: bool) {
    let w = rect.width as usize;
    for (row_idx, y) in (rect.y..rect.bottom()).enumerate() {
        let src = &bytes[row_idx * w * 4..(row_idx + 1) * w * 4];
        let dst = fb.row_mut(y, rect.x, rect.width);
        if msb_first {
            for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
                *d = u32::from_be_bytes([0, s[1], s[2], s[3]]);
            }
        } else {
            for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
                *d = u32::from_le_bytes([s[0], s[1], s[2], 0]);
            }
        }
    }
}

/// Tracks damaged regions of the root window.
pub struct DamageTracker {
    display: Arc<XDisplay>,
    damage: damage::Damage,
    region: xfixes::Region,
    dirty: bool,
}

impl DamageTracker {
    /// Start tracking damage on the root window.
    pub fn new(display: Arc<XDisplay>) -> Result<Self> {
        if !display.ext.damage || !display.ext.xfixes {
            bail!("DAMAGE and XFIXES extensions are required");
        }
        let conn = display.conn();
        let dmg = display.generate_id()?;
        damage::create(conn, dmg, display.root(), damage::ReportLevel::NON_EMPTY)?
            .check()
            .context("damage create")?;
        let region = display.generate_id()?;
        xfixes::create_region(conn, region, &[])?
            .check()
            .context("create region")?;
        conn.flush()?;
        Ok(Self {
            display,
            damage: dmg,
            region,
            dirty: true,
        })
    }

    /// Note that a `DamageNotify` event arrived.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether damage has been reported since the last [`DamageTracker::take`].
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Fetch and clear the accumulated damage as a list of rectangles.
    pub fn take(&mut self) -> Result<Vec<Rect>> {
        self.dirty = false;
        let conn = self.display.conn();
        damage::subtract(conn, self.damage, x11rb::NONE, self.region)?;
        let reply = xfixes::fetch_region(conn, self.region)?
            .reply()
            .context("fetch region")?;
        Ok(reply
            .rectangles
            .iter()
            .filter(|r| r.width > 0 && r.height > 0)
            .map(|r| {
                Rect::new(
                    r.x.max(0) as u32,
                    r.y.max(0) as u32,
                    u32::from(r.width),
                    u32::from(r.height),
                )
            })
            .collect())
    }

    /// The damage object's id (to match events).
    pub fn id(&self) -> damage::Damage {
        self.damage
    }
}

impl Drop for DamageTracker {
    fn drop(&mut self) {
        let conn = self.display.conn();
        let _ = damage::destroy(conn, self.damage);
        let _ = xfixes::destroy_region(conn, self.region);
        let _ = conn.flush();
    }
}

/// Merge overlapping/adjacent rectangles to reduce capture requests. The
/// result is a list of tile-aligned rectangles that cover the input.
pub fn coalesce(rects: &[Rect], tile: u32, bounds: &Rect) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    for r in rects {
        let mut cur = r.align_to_tiles(tile, bounds);
        if cur.is_empty() {
            continue;
        }
        // Absorb any existing rectangle this one touches; repeat until stable.
        loop {
            let mut merged = false;
            let mut i = 0;
            while i < out.len() {
                let o = out[i];
                if touches(&o, &cur) {
                    cur = cur.union(&o);
                    out.swap_remove(i);
                    merged = true;
                } else {
                    i += 1;
                }
            }
            if !merged {
                break;
            }
        }
        out.push(cur);
    }
    out
}

fn touches(a: &Rect, b: &Rect) -> bool {
    a.x <= b.right() && b.x <= a.right() && a.y <= b.bottom() && b.y <= a.bottom()
}

/// Sum of areas, used to decide between many small captures and one big one.
pub fn total_area(rects: &[Rect]) -> u64 {
    rects.iter().map(Rect::area).sum()
}

#[allow(dead_code)]
fn _assert_types(_: &xproto::Rectangle) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_pixels_handles_byte_orders() {
        let mut fb = Framebuffer::new(2, 1);
        let bytes = [0x33, 0x22, 0x11, 0xFF, 0x66, 0x55, 0x44, 0x00];
        copy_pixels(&mut fb, &Rect::new(0, 0, 2, 1), &bytes, false);
        assert_eq!(fb.pixels(), &[0x112233, 0x445566]);
        let bytes = [0xFF, 0x11, 0x22, 0x33, 0x00, 0x44, 0x55, 0x66];
        copy_pixels(&mut fb, &Rect::new(0, 0, 2, 1), &bytes, true);
        assert_eq!(fb.pixels(), &[0x112233, 0x445566]);
    }

    #[test]
    fn coalesce_merges_touching_rects() {
        let bounds = Rect::new(0, 0, 1000, 1000);
        let r = coalesce(
            &[Rect::new(0, 0, 10, 10), Rect::new(70, 0, 10, 10)],
            64,
            &bounds,
        );
        assert_eq!(r, vec![Rect::new(0, 0, 128, 64)]);
        let r = coalesce(
            &[Rect::new(0, 0, 10, 10), Rect::new(500, 500, 10, 10)],
            64,
            &bounds,
        );
        assert_eq!(r.len(), 2);
        let r = coalesce(
            &[
                Rect::new(0, 0, 10, 10),
                Rect::new(500, 500, 10, 10),
                Rect::new(0, 0, 600, 600),
            ],
            64,
            &bounds,
        );
        assert_eq!(r, vec![Rect::new(0, 0, 640, 640)]);
        assert!(coalesce(&[Rect::new(2000, 2000, 5, 5)], 64, &bounds).is_empty());
        assert_eq!(
            total_area(&[Rect::new(0, 0, 2, 2), Rect::new(0, 0, 3, 1)]),
            7
        );
    }
}
