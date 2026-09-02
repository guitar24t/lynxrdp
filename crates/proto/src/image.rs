//! Framebuffer and rectangle types.
//!
//! Pixels are stored as `u32` in `0x00RRGGBB` layout (the top byte is
//! ignored). This matches an X11 depth-24 ZPixmap on little-endian hosts
//! (`B G R X` bytes) and is what the client presents to the window system
//! without conversion.

use std::fmt;

/// An axis aligned rectangle in pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    /// Left edge (inclusive).
    pub x: u32,
    /// Top edge (inclusive).
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// Construct a rectangle.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// True if the rectangle covers no pixels.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Right edge (exclusive).
    pub fn right(&self) -> u32 {
        self.x + self.width
    }

    /// Bottom edge (exclusive).
    pub fn bottom(&self) -> u32 {
        self.y + self.height
    }

    /// Number of pixels covered.
    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Intersection with another rectangle, or an empty rectangle at the origin.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = self.right().min(other.right());
        let y2 = self.bottom().min(other.bottom());
        if x2 > x1 && y2 > y1 {
            Rect::new(x1, y1, x2 - x1, y2 - y1)
        } else {
            Rect::default()
        }
    }

    /// Smallest rectangle containing both.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x1 = self.x.min(other.x);
        let y1 = self.y.min(other.y);
        let x2 = self.right().max(other.right());
        let y2 = self.bottom().max(other.bottom());
        Rect::new(x1, y1, x2 - x1, y2 - y1)
    }

    /// Whether the rectangles share any pixel.
    pub fn intersects(&self, other: &Rect) -> bool {
        !self.intersect(other).is_empty()
    }

    /// Whether `other` lies entirely inside `self`.
    pub fn contains(&self, other: &Rect) -> bool {
        other.is_empty()
            || (other.x >= self.x
                && other.y >= self.y
                && other.right() <= self.right()
                && other.bottom() <= self.bottom())
    }

    /// Expand to the enclosing grid of `tile`-sized cells, clipped to `bounds`.
    pub fn align_to_tiles(&self, tile: u32, bounds: &Rect) -> Rect {
        if self.is_empty() {
            return Rect::default();
        }
        let x1 = self.x / tile * tile;
        let y1 = self.y / tile * tile;
        let x2 = self.right().div_ceil(tile) * tile;
        let y2 = self.bottom().div_ceil(tile) * tile;
        Rect::new(x1, y1, x2 - x1, y2 - y1).intersect(bounds)
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}+{}+{}", self.width, self.height, self.x, self.y)
    }
}

/// A 32-bit-per-pixel framebuffer in `0x00RRGGBB` layout.
#[derive(Clone, PartialEq, Eq)]
pub struct Framebuffer {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl fmt::Debug for Framebuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Framebuffer({}x{})", self.width, self.height)
    }
}

impl Framebuffer {
    /// Create a black framebuffer.
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, pixels: vec![0; (width as usize) * (height as usize)] }
    }

    /// Create a framebuffer from existing pixels. Panics if the length is wrong.
    pub fn from_pixels(width: u32, height: u32, pixels: Vec<u32>) -> Self {
        assert_eq!(pixels.len(), (width as usize) * (height as usize));
        Self { width, height, pixels }
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The full-framebuffer rectangle.
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// All pixels, row major.
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// Mutable access to all pixels.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }

    /// Pixel at (x, y). Panics if out of bounds.
    pub fn get(&self, x: u32, y: u32) -> u32 {
        self.pixels[(y as usize) * (self.width as usize) + x as usize]
    }

    /// Set the pixel at (x, y). Panics if out of bounds.
    pub fn set(&mut self, x: u32, y: u32, v: u32) {
        let w = self.width as usize;
        self.pixels[(y as usize) * w + x as usize] = v;
    }

    /// Slice of one row segment.
    pub fn row(&self, y: u32, x: u32, len: u32) -> &[u32] {
        let start = (y as usize) * (self.width as usize) + x as usize;
        &self.pixels[start..start + len as usize]
    }

    /// Mutable slice of one row segment.
    pub fn row_mut(&mut self, y: u32, x: u32, len: u32) -> &mut [u32] {
        let start = (y as usize) * (self.width as usize) + x as usize;
        &mut self.pixels[start..start + len as usize]
    }

    /// Fill a rectangle with a colour (clipped to bounds).
    pub fn fill(&mut self, rect: &Rect, color: u32) {
        let r = rect.intersect(&self.bounds());
        for y in r.y..r.bottom() {
            self.row_mut(y, r.x, r.width).fill(color & 0x00FF_FFFF);
        }
    }

    /// Copy the pixels of `rect` from `src` (same coordinates) into `self`.
    /// Both buffers must have identical dimensions.
    pub fn copy_rect_from(&mut self, src: &Framebuffer, rect: &Rect) {
        assert_eq!(self.width, src.width);
        assert_eq!(self.height, src.height);
        let r = rect.intersect(&self.bounds());
        for y in r.y..r.bottom() {
            let s = src.row(y, r.x, r.width);
            self.row_mut(y, r.x, r.width).copy_from_slice(s);
        }
    }

    /// Resize to new dimensions, preserving the overlapping top-left region.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height {
            return;
        }
        let mut new = Framebuffer::new(width, height);
        let w = width.min(self.width);
        let h = height.min(self.height);
        for y in 0..h {
            new.row_mut(y, 0, w).copy_from_slice(self.row(y, 0, w));
        }
        *self = new;
    }

    /// Write `rect` of this framebuffer from tightly packed `0x00RRGGBB` pixels.
    pub fn blit_pixels(&mut self, rect: &Rect, pixels: &[u32]) {
        assert_eq!(pixels.len(), rect.area() as usize);
        let bounds = self.bounds();
        assert!(bounds.contains(rect), "blit outside framebuffer");
        for (i, y) in (rect.y..rect.bottom()).enumerate() {
            let src = &pixels[i * rect.width as usize..(i + 1) * rect.width as usize];
            self.row_mut(y, rect.x, rect.width).copy_from_slice(src);
        }
    }

    /// Extract `rect` as tightly packed pixels.
    pub fn extract(&self, rect: &Rect) -> Vec<u32> {
        let r = rect.intersect(&self.bounds());
        let mut out = Vec::with_capacity(r.area() as usize);
        for y in r.y..r.bottom() {
            out.extend_from_slice(self.row(y, r.x, r.width));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_ops() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Rect::new(5, 5, 5, 5));
        assert_eq!(a.union(&b), Rect::new(0, 0, 15, 15));
        assert!(a.intersects(&b));
        assert!(!a.intersects(&Rect::new(10, 10, 1, 1)));
        assert!(a.contains(&Rect::new(1, 1, 2, 2)));
        assert!(!a.contains(&b));
        assert!(a.contains(&Rect::default()));
        assert_eq!(Rect::default().union(&a), a);
    }

    #[test]
    fn tile_alignment() {
        let bounds = Rect::new(0, 0, 100, 100);
        let r = Rect::new(10, 70, 5, 5);
        assert_eq!(r.align_to_tiles(64, &bounds), Rect::new(0, 64, 64, 36));
        assert_eq!(Rect::default().align_to_tiles(64, &bounds), Rect::default());
        assert_eq!(Rect::new(63, 63, 2, 2).align_to_tiles(64, &bounds), Rect::new(0, 0, 100, 100));
    }

    #[test]
    fn framebuffer_fill_and_extract() {
        let mut fb = Framebuffer::new(8, 4);
        fb.fill(&Rect::new(2, 1, 3, 2), 0xFF112233);
        assert_eq!(fb.get(2, 1), 0x112233);
        assert_eq!(fb.get(4, 2), 0x112233);
        assert_eq!(fb.get(5, 2), 0);
        assert_eq!(fb.get(1, 1), 0);
        let px = fb.extract(&Rect::new(2, 1, 3, 2));
        assert_eq!(px, vec![0x112233; 6]);
        let mut fb2 = Framebuffer::new(8, 4);
        fb2.blit_pixels(&Rect::new(2, 1, 3, 2), &px);
        assert_eq!(fb, fb2);
    }

    #[test]
    fn framebuffer_resize_preserves_content() {
        let mut fb = Framebuffer::new(4, 4);
        fb.fill(&fb.bounds(), 0xABCDEF);
        fb.resize(6, 2);
        assert_eq!(fb.width(), 6);
        assert_eq!(fb.height(), 2);
        assert_eq!(fb.get(3, 1), 0xABCDEF);
        assert_eq!(fb.get(5, 1), 0);
    }
}
