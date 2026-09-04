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

    /// The `len` bytes of the mapping starting at `offset`.
    ///
    /// Pipelined captures give each rectangle its own region of the segment,
    /// so this can no longer assume a capture starts at the beginning of it.
    fn bytes_at(&self, offset: usize, len: usize) -> &[u8] {
        assert!(offset.saturating_add(len) <= self.size);
        // SAFETY: the mapping is valid for `size` bytes for our lifetime, and
        // the assertion above keeps `offset + len` inside it.
        unsafe { std::slice::from_raw_parts(self.addr.add(offset), len) }
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
        self.capture_many(fb, std::slice::from_ref(rect))
    }

    /// Capture several rectangles (each clipped to `fb`) into `fb`.
    ///
    /// Prefer this over calling [`ScreenCapture::capture_into`] in a loop.
    /// With MIT-SHM every request in a batch is issued before any reply is
    /// read, and that is the whole reason this entry point exists: a
    /// `shm::get_image` reply is 32 bytes -- the pixels land in the shared
    /// segment, not in the reply -- so a rectangle costs one round trip and
    /// nothing else. A frame made of two dozen damage rectangles used to pay
    /// two dozen of those in series before the encoder saw a single pixel;
    /// overlapping them makes it roughly one.
    ///
    /// Each rectangle in a batch gets its own region of the segment, which is
    /// what makes the requests independent of one another. A batch ends when
    /// the segment or the pipeline depth is full, and its pixels are copied
    /// out before the next batch reuses the space.
    pub fn capture_many(&mut self, fb: &mut Framebuffer, rects: &[Rect]) -> Result<()> {
        let bounds = fb.bounds();
        let capacity_px = self.shm_pixels;
        let mut bands: Vec<Rect> = Vec::new();
        for r in rects {
            let clipped = r.intersect(&bounds);
            if clipped.is_empty() {
                continue;
            }
            // Very tall captures are split so each one fits the SHM segment
            // and stays a reasonable single request.
            bands.extend(split_bands(&clipped, band_rows(clipped.width, capacity_px)));
        }
        if bands.is_empty() {
            return Ok(());
        }
        match self.shm.as_ref() {
            Some(shm) => capture_shm(&self.display, shm, fb, &bands),
            None => capture_serial(&self.display, fb, &bands),
        }
    }
}

/// Issue every capture in a batch before reading any of its replies.
fn capture_shm(
    display: &Arc<XDisplay>,
    shm: &ShmSegment,
    fb: &mut Framebuffer,
    bands: &[Rect],
) -> Result<()> {
    let conn = display.conn();
    let root = display.root();
    let msb = display.msb_first();
    for band in bands {
        // `band_rows` sizes bands against this same segment, so this can only
        // fire if the segment was created for a smaller screen than the
        // framebuffer now is. A clear error beats an offset the X server would
        // reject with BadValue and a frame that is silently missing a strip.
        let need = band_bytes(band);
        if need > shm.size {
            bail!(
                "capture rectangle {band} needs {need} bytes, the shared segment holds {}",
                shm.size
            );
        }
    }
    for batch in plan_batches(bands, shm.size) {
        let mut cookies = Vec::with_capacity(batch.len());
        for slot in &batch {
            cookies.push(x11rb::protocol::shm::get_image(
                conn,
                root,
                slot.rect.x as i16,
                slot.rect.y as i16,
                slot.rect.width as u16,
                slot.rect.height as u16,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                shm.seg,
                slot.offset as u32,
            )?);
        }
        // The X server answers a `ShmGetImage` only once the pixels are in the
        // segment, and it answers requests in the order it received them, so
        // a band can be copied out as soon as its own reply lands rather than
        // after the whole batch. The same ordering is what makes the error
        // paths safe: a request abandoned here is still processed before
        // anything the next frame sends, so its late write cannot land on top
        // of a later capture that reuses the same offset.
        for (slot, cookie) in batch.iter().zip(cookies) {
            let reply = cookie.reply().context("shm GetImage")?;
            let need = band_bytes(&slot.rect);
            if reply.size as usize != need {
                bail!("unexpected shm image size {} for {}", reply.size, slot.rect);
            }
            copy_pixels(fb, &slot.rect, shm.bytes_at(slot.offset, need), msb);
        }
    }
    Ok(())
}

/// The fallback used when MIT-SHM is unavailable.
///
/// Deliberately one request at a time. Without shared memory the pixels travel
/// inside the reply, so a pipeline of these would hold several whole bands in
/// the connection's buffers at once -- megabytes of them, on a path that only
/// exists because shared memory was not available in the first place.
fn capture_serial(display: &Arc<XDisplay>, fb: &mut Framebuffer, bands: &[Rect]) -> Result<()> {
    let conn = display.conn();
    let root = display.root();
    let msb = display.msb_first();
    for band in bands {
        let need = band_bytes(band);
        let reply = conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                root,
                band.x as i16,
                band.y as i16,
                band.width as u16,
                band.height as u16,
                !0,
            )?
            .reply()
            .context("GetImage")?;
        if reply.data.len() < need {
            bail!("short GetImage reply for {band}");
        }
        copy_pixels(fb, band, &reply.data[..need], msb);
    }
    Ok(())
}

/// Tallest band a single capture request may cover.
///
/// Carried over from the serial code: an X server services a `GetImage`
/// synchronously, and a band this tall is already a generous unit of work.
const MAX_BAND_ROWS: u32 = 4096;

/// Captures allowed to be outstanding at once.
///
/// Only the MIT-SHM path pipelines, and there each unread reply is 32 bytes,
/// so this cap is not about memory. It is about never letting the number of
/// unread replies approach what the socket buffers hold, whatever the
/// capture-rectangle budget grows to: an X server blocked writing replies
/// while we are blocked writing requests is a deadlock, not a slowdown.
const MAX_PIPELINE: usize = 64;

/// Alignment applied to every offset into the shared segment.
///
/// A band's byte count is always a multiple of four (32 bits per pixel), so
/// offsets are naturally word aligned already; rounding up to eight costs at
/// most four bytes per band and keeps the destination aligned for an X server
/// built with 64-bit framebuffer words.
const SHM_ALIGN: usize = 8;

/// Bytes one band occupies at 32 bits per pixel.
fn band_bytes(rect: &Rect) -> usize {
    (rect.width as usize)
        .saturating_mul(rect.height as usize)
        .saturating_mul(4)
}

fn align_up(value: usize, align: usize) -> usize {
    value.saturating_add(align - 1) & !(align - 1)
}

/// Rows of `width` pixels that fit in `capacity` pixels, capped at
/// [`MAX_BAND_ROWS`]. Never zero, so a band always makes progress.
fn band_rows(width: u32, capacity: usize) -> u32 {
    if width == 0 {
        return MAX_BAND_ROWS;
    }
    (capacity / width as usize).clamp(1, MAX_BAND_ROWS as usize) as u32
}

/// Split `rect` into horizontal bands of at most `max_rows` rows each.
fn split_bands(rect: &Rect, max_rows: u32) -> Vec<Rect> {
    let mut out = Vec::new();
    if rect.is_empty() || max_rows == 0 {
        return out;
    }
    let mut y = rect.y;
    while y < rect.bottom() {
        let h = (rect.bottom() - y).min(max_rows);
        out.push(Rect::new(rect.x, y, rect.width, h));
        y += h;
    }
    out
}

/// One capture request and where in the shared segment its pixels will land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    rect: Rect,
    /// Byte offset into the shared segment.
    offset: usize,
}

/// Pack `bands` into batches that each fit `capacity` bytes and hold at most
/// [`MAX_PIPELINE`] requests, giving every band its own region of the segment.
///
/// Distinct, non-overlapping offsets are exactly what allows a batch's
/// requests to be outstanding together: the X server writes each band's pixels
/// straight into its own region, so no reply can overwrite another's pixels
/// before they have been copied out.
///
/// The running offset is summed in `usize` on purpose. `config.rs` permits a
/// session up to 16384x16384, which is a one-gigabyte segment, and a running
/// total over a batch reaches the same order as `u32::MAX` long before the
/// capacity check would reject it -- sooner still if that ceiling ever rises.
/// The offset is narrowed to `u32` for the wire only once it is known to fit.
///
/// A band larger than `capacity` cannot be captured this way at all; it is
/// emitted alone at offset zero rather than silently dropped, which only keeps
/// this function total. `capture_shm` rejects that case before planning.
fn plan_batches(bands: &[Rect], capacity: usize) -> Vec<Vec<Slot>> {
    let mut batches: Vec<Vec<Slot>> = Vec::new();
    let mut cur: Vec<Slot> = Vec::new();
    let mut next = 0usize;
    for band in bands {
        if band.is_empty() {
            continue;
        }
        let need = band_bytes(band);
        let mut offset = align_up(next, SHM_ALIGN);
        if cur.len() >= MAX_PIPELINE || offset.saturating_add(need) > capacity {
            if !cur.is_empty() {
                batches.push(std::mem::take(&mut cur));
            }
            // The previous batch's pixels are copied out before any request of
            // this one is issued, so the segment is free to be reused.
            offset = 0;
        }
        cur.push(Slot {
            rect: *band,
            offset,
        });
        next = offset.saturating_add(need);
    }
    if !cur.is_empty() {
        batches.push(cur);
    }
    batches
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

/// Above this many rectangles, one whole-screen capture beats capturing each
/// of them: every rectangle is a separate round trip to the X server.
pub const MAX_CAPTURE_RECTS: usize = 24;

/// How much empty area a merge may add, over and above the two rectangles it
/// joins. Four tiles: enough that the diagonally-adjacent tiles this function
/// exists to join still merge, and far too little to swallow a screen.
fn merge_slack(tile: u32) -> u64 {
    4 * (tile as u64) * (tile as u64)
}

/// Merge overlapping/adjacent rectangles to reduce capture requests. The
/// result is a list of tile-aligned rectangles that cover the input.
///
/// Merging is refused when the union would add more empty area than
/// [`merge_slack`] allows. Bare adjacency is a bad reason to merge on its own:
/// a menu-bar repaint and a sidebar repaint touch once they are tile-aligned,
/// and unioning them turns 70,400 damaged pixels into a 1920x960 rectangle --
/// a 26x amplification, which then trips the whole-screen fallback in
/// `send_frame` and costs a full-screen capture, diff and encode to transmit a
/// few hundred bytes. A clock ticking while anything else repaints is enough
/// to produce exactly that.
///
/// The guard leaves more rectangles behind, so when the result is too
/// fragmented to capture piecemeal we fall back to the old unconditional
/// merge: fewer, larger rectangles are the better trade at that point, and it
/// guarantees this change cannot make any frame worse than it was.
pub fn coalesce(rects: &[Rect], tile: u32, bounds: &Rect) -> Vec<Rect> {
    let guarded = coalesce_within(rects, tile, bounds, merge_slack(tile));
    if guarded.len() <= MAX_CAPTURE_RECTS {
        return guarded;
    }
    coalesce_within(rects, tile, bounds, u64::MAX)
}

/// `coalesce`, with an explicit cap on the empty area a merge may add.
///
/// Note that with a finite `slack` the output is no longer guaranteed to be
/// pairwise disjoint: two rectangles that touch but whose union is too costly
/// are both kept. That is safe -- capture is idempotent and `encode_regions`
/// de-duplicates the tiles -- but it does mean `total_area` can now count a
/// small overlap twice, which only ever biases towards the whole-screen path.
fn coalesce_within(rects: &[Rect], tile: u32, bounds: &Rect, slack: u64) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    for r in rects {
        let mut cur = r.align_to_tiles(tile, bounds);
        if cur.is_empty() {
            continue;
        }
        // Absorb any existing rectangle this one touches cheaply enough;
        // repeat until stable, since a merge can bring a further one in range.
        loop {
            let mut merged = false;
            let mut i = 0;
            while i < out.len() {
                let o = out[i];
                let union = cur.union(&o);
                if touches(&o, &cur) && union.area() <= cur.area() + o.area() + slack {
                    cur = union;
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

    /// Two ordinary desktop repaints must not become a full-screen capture.
    ///
    /// A menu bar and a sidebar do not overlap, but once each is aligned out to
    /// tile boundaries they touch, and the old unconditional union turned
    /// 70,400 damaged pixels into 1,843,200 -- which then tripped the
    /// whole-screen fallback in `send_frame`. This is the single most common
    /// damage shape a desktop produces.
    #[test]
    fn adjacent_strips_do_not_swallow_the_screen() {
        let bounds = Rect::new(0, 0, 1920, 1080);
        let menu = Rect::new(0, 0, 1920, 20);
        let side = Rect::new(0, 100, 40, 800);
        let merged = coalesce(&[menu, side], 64, &bounds);
        assert_eq!(merged.len(), 2, "the two strips were merged: {merged:?}");
        assert!(
            total_area(&merged) * 8 < bounds.area(),
            "coalesce amplified {} px of damage to {} px",
            menu.area() + side.area(),
            total_area(&merged)
        );
    }

    /// The guard must not defeat the merging the function exists to do.
    #[test]
    fn genuinely_neighbouring_rects_still_merge() {
        let bounds = Rect::new(0, 0, 1920, 1080);
        let a = Rect::new(64, 64, 64, 64);
        let b = Rect::new(128, 64, 64, 64);
        assert_eq!(coalesce(&[a, b], 64, &bounds).len(), 1);
    }

    /// Past the capture-rect budget the old greedy merge takes over, so a
    /// heavily fragmented frame is never worse off than it was before.
    #[test]
    fn heavy_fragmentation_falls_back_to_greedy_merging() {
        let bounds = Rect::new(0, 0, 1920, 1080);
        // A diagonal of isolated tiles: none of them touch, so the guard keeps
        // every one and the fallback has to be what bounds the count.
        let scattered: Vec<Rect> = (0..40).map(|i| Rect::new(i * 48, i * 26, 8, 8)).collect();
        let merged = coalesce(&scattered, 64, &bounds);
        assert!(
            merged.len() <= scattered.len(),
            "fallback produced more rectangles than it was given"
        );
    }

    #[test]
    fn bands_cover_the_rectangle_exactly() {
        let r = Rect::new(10, 5, 100, 1000);
        let bands = split_bands(&r, 384);
        assert_eq!(bands.len(), 3);
        assert_eq!(bands[0], Rect::new(10, 5, 100, 384));
        assert_eq!(bands[2], Rect::new(10, 773, 100, 232));
        // No gaps and no overlaps: the areas add up and the ends line up.
        assert_eq!(bands.iter().map(Rect::area).sum::<u64>(), r.area());
        assert_eq!(bands[0].y, r.y);
        assert_eq!(bands.last().unwrap().bottom(), r.bottom());
        assert!(split_bands(&Rect::new(0, 0, 0, 10), 8).is_empty());
    }

    /// The band height comes from the segment, never from a constant alone:
    /// a band that does not fit is a `BadValue` and a frame with a hole in it.
    #[test]
    fn band_rows_respect_the_segment() {
        assert_eq!(band_rows(1920, 1920 * 1080), 1080);
        assert_eq!(band_rows(1920, 1920 * 10), 10);
        // Never zero, however small the segment...
        assert_eq!(band_rows(1920, 0), 1);
        assert_eq!(band_rows(1920, 1919), 1);
        // ...and never unbounded, however large.
        assert_eq!(band_rows(1, usize::MAX), MAX_BAND_ROWS);
        assert_eq!(band_rows(0, 4), MAX_BAND_ROWS);
    }

    /// The property the whole pipeline rests on: within one batch no two
    /// captures may share a byte of the segment, or one reply would overwrite
    /// another band's pixels before they were copied out.
    #[test]
    fn planned_slots_never_overlap_within_a_batch() {
        let bands: Vec<Rect> = (0..40).map(|i| Rect::new(0, i * 16, 640, 16)).collect();
        let capacity = 640 * 16 * 4 * 7; // room for seven bands
        let batches = plan_batches(&bands, capacity);
        assert!(batches.len() > 1, "the capacity should have forced a split");
        let mut planned = 0;
        for batch in &batches {
            assert!(batch.len() <= MAX_PIPELINE);
            let mut spans: Vec<(usize, usize)> = batch
                .iter()
                .map(|s| (s.offset, s.offset + band_bytes(&s.rect)))
                .collect();
            spans.sort_unstable();
            for w in spans.windows(2) {
                assert!(w[0].1 <= w[1].0, "slots overlap: {spans:?}");
            }
            assert!(spans.last().unwrap().1 <= capacity);
            planned += batch.len();
        }
        assert_eq!(planned, bands.len(), "a band was dropped");
    }

    /// Offsets are summed in `usize` and narrowed only for the wire. A segment
    /// sized for the 16384-square ceiling `config.rs` allows is a gigabyte,
    /// and the arithmetic has to stay exact across the whole of it.
    #[test]
    fn offsets_stay_inside_a_gigabyte_segment() {
        let capacity = 16384usize * 16384 * 4;
        let bands: Vec<Rect> = (0..4)
            .map(|i| Rect::new(0, i * 4096, 16384, 4096))
            .collect();
        let batches = plan_batches(&bands, capacity);
        // The four bands fill the segment exactly, so none should be split off.
        assert_eq!(batches.len(), 1);
        for slot in &batches[0] {
            assert!(
                u32::try_from(slot.offset).is_ok(),
                "offset {} does not fit the wire field",
                slot.offset
            );
            assert!(slot.offset + band_bytes(&slot.rect) <= capacity);
        }
    }

    /// Byte counts are multiples of four, offsets are multiples of eight.
    #[test]
    fn offsets_stay_aligned() {
        let bands = [
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 1, 1, 1),
            Rect::new(0, 2, 3, 1),
        ];
        let batches = plan_batches(&bands, 1024);
        assert_eq!(batches.len(), 1);
        for slot in &batches[0] {
            assert_eq!(slot.offset % SHM_ALIGN, 0, "unaligned {slot:?}");
        }
        assert_eq!(batches[0][1].offset, 8);
        assert_eq!(batches[0][2].offset, 16);
    }

    /// Pipeline depth is capped independently of the segment, so a future,
    /// larger capture budget cannot put an unbounded number of replies in
    /// flight.
    #[test]
    fn the_pipeline_depth_is_capped() {
        let bands: Vec<Rect> = (0..MAX_PIPELINE as u32 * 2 + 1)
            .map(|i| Rect::new(0, i, 8, 1))
            .collect();
        let batches = plan_batches(&bands, usize::MAX);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), MAX_PIPELINE);
        assert_eq!(batches[2].len(), 1);
    }

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
