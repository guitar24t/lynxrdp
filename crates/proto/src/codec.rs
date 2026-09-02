//! Tile based screen codec.
//!
//! The screen is divided into a grid of [`TILE_SIZE`](crate::TILE_SIZE)
//! square tiles. When a region of the screen changes the server compares
//! the new pixels against the previously transmitted frame tile by tile.
//! Each tile that differs is trimmed to the bounding box of the changed
//! pixels and encoded with the cheapest suitable [`TileEncoding`]:
//!
//! * `Solid` – every pixel has the same colour: 3 bytes.
//! * `Lz4` – LZ4 block compressed packed 24-bit RGB.
//! * `Raw` – packed 24-bit RGB, used when compression would not help.
//!
//! Decoding is the inverse and is exercised by property tests to guarantee
//! that `decode(encode(x)) == x` for every input.

use crate::image::{Framebuffer, Rect};
use crate::wire::DecodeError;
use crate::TILE_SIZE;

/// How the pixel data of a [`TileUpdate`] is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TileEncoding {
    /// Single colour, payload is 3 bytes `R G B`.
    Solid = 0,
    /// Packed 24-bit RGB, row major, no padding.
    Raw = 1,
    /// LZ4 block (with `lz4_flex` size prefix) of the `Raw` payload.
    Lz4 = 2,
}

impl TileEncoding {
    /// Convert from the wire tag.
    pub fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(TileEncoding::Solid),
            1 => Ok(TileEncoding::Raw),
            2 => Ok(TileEncoding::Lz4),
            other => Err(DecodeError::InvalidTag(u32::from(other))),
        }
    }
}

/// One encoded rectangle of changed pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileUpdate {
    /// Destination rectangle on the remote screen.
    pub rect: Rect,
    /// Encoding of `data`.
    pub encoding: TileEncoding,
    /// Encoded pixel data.
    pub data: Vec<u8>,
}

/// Errors produced while decoding a tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// Decompression failed or produced the wrong amount of data.
    Corrupt(&'static str),
    /// Tile rectangle lies outside the framebuffer.
    OutOfBounds(Rect),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Corrupt(what) => write!(f, "corrupt tile data: {what}"),
            CodecError::OutOfBounds(r) => write!(f, "tile {r} outside framebuffer"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Pack `0x00RRGGBB` pixels into tightly packed RGB bytes.
pub fn pack_rgb(pixels: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 3);
    for &p in pixels {
        out.push((p >> 16) as u8);
        out.push((p >> 8) as u8);
        out.push(p as u8);
    }
    out
}

/// Unpack tightly packed RGB bytes into `0x00RRGGBB` pixels.
pub fn unpack_rgb(bytes: &[u8]) -> Result<Vec<u32>, CodecError> {
    if bytes.len() % 3 != 0 {
        return Err(CodecError::Corrupt("rgb length not a multiple of 3"));
    }
    Ok(bytes
        .chunks_exact(3)
        .map(|c| (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]))
        .collect())
}

/// Encode a rectangle of pixels choosing the smallest representation.
pub fn encode_pixels(rect: Rect, pixels: &[u32]) -> TileUpdate {
    debug_assert_eq!(pixels.len(), rect.area() as usize);
    let first = pixels[0] & 0x00FF_FFFF;
    if pixels.iter().all(|&p| (p & 0x00FF_FFFF) == first) {
        return TileUpdate {
            rect,
            encoding: TileEncoding::Solid,
            data: vec![(first >> 16) as u8, (first >> 8) as u8, first as u8],
        };
    }
    let raw = pack_rgb(pixels);
    // Very small tiles are not worth compressing.
    if raw.len() >= 48 {
        let compressed = lz4_flex::block::compress_prepend_size(&raw);
        if compressed.len() < raw.len() {
            return TileUpdate { rect, encoding: TileEncoding::Lz4, data: compressed };
        }
    }
    TileUpdate { rect, encoding: TileEncoding::Raw, data: raw }
}

/// Decode a tile into `0x00RRGGBB` pixels.
pub fn decode_pixels(tile: &TileUpdate) -> Result<Vec<u32>, CodecError> {
    let n = tile.rect.area() as usize;
    match tile.encoding {
        TileEncoding::Solid => {
            if tile.data.len() != 3 {
                return Err(CodecError::Corrupt("solid payload must be 3 bytes"));
            }
            let c = (u32::from(tile.data[0]) << 16)
                | (u32::from(tile.data[1]) << 8)
                | u32::from(tile.data[2]);
            Ok(vec![c; n])
        }
        TileEncoding::Raw => {
            if tile.data.len() != n * 3 {
                return Err(CodecError::Corrupt("raw payload size mismatch"));
            }
            unpack_rgb(&tile.data)
        }
        TileEncoding::Lz4 => {
            let raw = lz4_flex::block::decompress_size_prepended(&tile.data)
                .map_err(|_| CodecError::Corrupt("lz4 decompression failed"))?;
            if raw.len() != n * 3 {
                return Err(CodecError::Corrupt("lz4 payload size mismatch"));
            }
            unpack_rgb(&raw)
        }
    }
}

/// Apply a tile update to a framebuffer.
pub fn apply_tile(fb: &mut Framebuffer, tile: &TileUpdate) -> Result<(), CodecError> {
    if !fb.bounds().contains(&tile.rect) || tile.rect.is_empty() {
        return Err(CodecError::OutOfBounds(tile.rect));
    }
    let pixels = decode_pixels(tile)?;
    fb.blit_pixels(&tile.rect, &pixels);
    Ok(())
}

/// Find the bounding box of pixels that differ between `a` and `b` within `rect`.
/// Returns `None` if nothing differs.
pub fn changed_bbox(a: &Framebuffer, b: &Framebuffer, rect: &Rect) -> Option<Rect> {
    let r = rect.intersect(&a.bounds());
    if r.is_empty() {
        return None;
    }
    let mut min_x = u32::MAX;
    let mut max_x = 0u32;
    let mut min_y = u32::MAX;
    let mut max_y = 0u32;
    for y in r.y..r.bottom() {
        let ra = a.row(y, r.x, r.width);
        let rb = b.row(y, r.x, r.width);
        if ra == rb {
            continue;
        }
        // First and last differing pixel in this row.
        let first = ra.iter().zip(rb).position(|(p, q)| p != q).unwrap_or(0) as u32;
        let last = ra.iter().zip(rb).rposition(|(p, q)| p != q).unwrap_or(0) as u32;
        min_x = min_x.min(r.x + first);
        max_x = max_x.max(r.x + last);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    if min_x == u32::MAX {
        None
    } else {
        Some(Rect::new(min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
    }
}

/// Encoder state: keeps the last transmitted frame to diff against.
#[derive(Debug)]
pub struct Encoder {
    prev: Framebuffer,
    tile: u32,
}

impl Encoder {
    /// Create an encoder for a screen of the given size. The reference frame
    /// starts black, so the first call to [`Encoder::encode_region`] emits
    /// tiles for every non-black pixel.
    pub fn new(width: u32, height: u32) -> Self {
        Self { prev: Framebuffer::new(width, height), tile: TILE_SIZE }
    }

    /// Use a custom tile size (mostly for tests).
    pub fn with_tile_size(mut self, tile: u32) -> Self {
        assert!(tile > 0);
        self.tile = tile;
        self
    }

    /// Current screen size the encoder is configured for.
    pub fn size(&self) -> (u32, u32) {
        (self.prev.width(), self.prev.height())
    }

    /// Reset for a new screen size. The reference frame becomes black so the
    /// next encode of the full bounds resends everything.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.prev = Framebuffer::new(width, height);
    }

    /// Force the next encode of `rect` to resend every tile regardless of
    /// whether it changed (used after a client reconnects).
    pub fn invalidate(&mut self, rect: &Rect) {
        // Fill with a value that can never appear on screen (alpha bits set)
        // so any real pixel compares unequal.
        let r = rect.intersect(&self.prev.bounds());
        for y in r.y..r.bottom() {
            self.prev.row_mut(y, r.x, r.width).fill(0xFFFF_FFFF);
        }
    }

    /// The reference framebuffer (what the client is believed to display).
    pub fn reference(&self) -> &Framebuffer {
        &self.prev
    }

    /// Diff `current` against the reference within `region` and return
    /// updates for every changed tile. The reference is updated in place.
    /// `current` must have the same dimensions as the encoder.
    pub fn encode_region(&mut self, current: &Framebuffer, region: &Rect) -> Vec<TileUpdate> {
        assert_eq!(current.width(), self.prev.width());
        assert_eq!(current.height(), self.prev.height());
        let bounds = self.prev.bounds();
        let region = region.intersect(&bounds);
        if region.is_empty() {
            return Vec::new();
        }
        let mut updates = Vec::new();
        let ts = self.tile;
        let mut ty = region.y / ts * ts;
        while ty < region.bottom() {
            let mut tx = region.x / ts * ts;
            while tx < region.right() {
                let tile = Rect::new(tx, ty, ts, ts).intersect(&region);
                if let Some(bbox) = changed_bbox(current, &self.prev, &tile) {
                    let pixels = current.extract(&bbox);
                    updates.push(encode_pixels(bbox, &pixels));
                    self.prev.blit_pixels(&bbox, &pixels);
                }
                tx += ts;
            }
            ty += ts;
        }
        updates
    }

    /// Encode a list of regions, de-duplicating overlapping tiles by
    /// processing them sequentially against the updated reference.
    pub fn encode_regions(&mut self, current: &Framebuffer, regions: &[Rect]) -> Vec<TileUpdate> {
        let mut out = Vec::new();
        for r in regions {
            out.extend(self.encode_region(current, r));
        }
        out
    }
}

/// Decoder state: the client's copy of the remote screen.
#[derive(Debug)]
pub struct Decoder {
    fb: Framebuffer,
}

impl Decoder {
    /// Create a decoder with a black screen of the given size.
    pub fn new(width: u32, height: u32) -> Self {
        Self { fb: Framebuffer::new(width, height) }
    }

    /// Resize the local copy (content outside the new size is dropped).
    pub fn resize(&mut self, width: u32, height: u32) {
        self.fb.resize(width, height);
    }

    /// Apply all tiles; returns the union rectangle that changed.
    pub fn apply(&mut self, tiles: &[TileUpdate]) -> Result<Rect, CodecError> {
        let mut dirty = Rect::default();
        for t in tiles {
            apply_tile(&mut self.fb, t)?;
            dirty = dirty.union(&t.rect);
        }
        Ok(dirty)
    }

    /// The decoded screen.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.fb
    }

    /// Mutable access to the decoded screen.
    pub fn framebuffer_mut(&mut self) -> &mut Framebuffer {
        &mut self.fb
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn checker(w: u32, h: u32, seed: u32) -> Framebuffer {
        let mut fb = Framebuffer::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = (x.wrapping_mul(31) ^ y.wrapping_mul(17) ^ seed) & 0xFFFFFF;
                fb.set(x, y, v);
            }
        }
        fb
    }

    #[test]
    fn solid_tile_roundtrip() {
        let rect = Rect::new(3, 4, 5, 6);
        let px = vec![0xFF123456u32; 30];
        let t = encode_pixels(rect, &px);
        assert_eq!(t.encoding, TileEncoding::Solid);
        assert_eq!(t.data.len(), 3);
        assert_eq!(decode_pixels(&t).unwrap(), vec![0x123456; 30]);
    }

    #[test]
    fn noisy_tile_roundtrip() {
        let fb = checker(64, 64, 0x55);
        let rect = fb.bounds();
        let px = fb.extract(&rect);
        let t = encode_pixels(rect, &px);
        assert_eq!(decode_pixels(&t).unwrap(), px);
    }

    #[test]
    fn compressible_tile_uses_lz4() {
        let mut fb = Framebuffer::new(64, 64);
        fb.fill(&Rect::new(0, 0, 64, 32), 0x102030);
        fb.fill(&Rect::new(0, 32, 64, 32), 0x405060);
        let px = fb.extract(&fb.bounds());
        let t = encode_pixels(fb.bounds(), &px);
        assert_eq!(t.encoding, TileEncoding::Lz4);
        assert!(t.data.len() < 200);
        assert_eq!(decode_pixels(&t).unwrap(), px);
    }

    #[test]
    fn corrupt_data_is_rejected() {
        let t = TileUpdate { rect: Rect::new(0, 0, 2, 2), encoding: TileEncoding::Raw, data: vec![1] };
        assert!(decode_pixels(&t).is_err());
        let t = TileUpdate { rect: Rect::new(0, 0, 2, 2), encoding: TileEncoding::Lz4, data: vec![1, 2, 3] };
        assert!(decode_pixels(&t).is_err());
        let t = TileUpdate { rect: Rect::new(0, 0, 2, 2), encoding: TileEncoding::Solid, data: vec![1] };
        assert!(decode_pixels(&t).is_err());
        let mut fb = Framebuffer::new(4, 4);
        let t = TileUpdate { rect: Rect::new(3, 3, 2, 2), encoding: TileEncoding::Solid, data: vec![1, 2, 3] };
        assert!(matches!(apply_tile(&mut fb, &t), Err(CodecError::OutOfBounds(_))));
    }

    #[test]
    fn encoder_only_sends_changed_tiles() {
        let mut enc = Encoder::new(200, 150).with_tile_size(64);
        let mut dec = Decoder::new(200, 150);
        let mut screen = Framebuffer::new(200, 150);
        // First frame: all black == reference, nothing to send.
        assert!(enc.encode_region(&screen, &screen.bounds()).is_empty());
        // Draw something in the middle.
        screen.fill(&Rect::new(70, 70, 10, 10), 0xAABBCC);
        let ups = enc.encode_region(&screen, &screen.bounds());
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].rect, Rect::new(70, 70, 10, 10));
        assert_eq!(ups[0].encoding, TileEncoding::Solid);
        dec.apply(&ups).unwrap();
        assert_eq!(dec.framebuffer(), &screen);
        // Nothing changed: nothing sent.
        assert!(enc.encode_region(&screen, &screen.bounds()).is_empty());
        // Change spanning tile boundary produces two tiles.
        screen.fill(&Rect::new(60, 10, 10, 2), 0x010203);
        let ups = enc.encode_region(&screen, &Rect::new(60, 10, 10, 2));
        assert_eq!(ups.len(), 2);
        dec.apply(&ups).unwrap();
        assert_eq!(dec.framebuffer(), &screen);
    }

    #[test]
    fn invalidate_forces_resend() {
        let mut enc = Encoder::new(64, 64);
        let screen = Framebuffer::new(64, 64);
        assert!(enc.encode_region(&screen, &screen.bounds()).is_empty());
        enc.invalidate(&screen.bounds());
        let ups = enc.encode_region(&screen, &screen.bounds());
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].rect, screen.bounds());
    }

    #[test]
    fn region_outside_bounds_is_ignored() {
        let mut enc = Encoder::new(64, 64);
        let screen = checker(64, 64, 1);
        assert!(enc.encode_region(&screen, &Rect::new(100, 100, 10, 10)).is_empty());
        assert!(enc.encode_region(&screen, &Rect::default()).is_empty());
    }

    #[test]
    fn changed_bbox_is_tight() {
        let a = Framebuffer::new(10, 10);
        let mut b = Framebuffer::new(10, 10);
        assert_eq!(changed_bbox(&a, &b, &a.bounds()), None);
        b.set(3, 4, 1);
        b.set(7, 8, 1);
        assert_eq!(changed_bbox(&a, &b, &a.bounds()), Some(Rect::new(3, 4, 5, 5)));
        assert_eq!(changed_bbox(&a, &b, &Rect::new(0, 0, 5, 5)), Some(Rect::new(3, 4, 1, 1)));
    }

    proptest! {
        #[test]
        fn encode_decode_roundtrip(
            w in 1u32..40, h in 1u32..40,
            pixels in proptest::collection::vec(any::<u32>(), 1..1600usize),
        ) {
            let n = (w * h) as usize;
            let px: Vec<u32> = pixels.iter().cycle().take(n).map(|p| p & 0xFFFFFF).collect();
            let rect = Rect::new(0, 0, w, h);
            let t = encode_pixels(rect, &px);
            prop_assert_eq!(decode_pixels(&t).unwrap(), px);
        }

        #[test]
        fn incremental_encoding_reconstructs_screen(
            ops in proptest::collection::vec((0u32..90, 0u32..70, 1u32..40, 1u32..40, any::<u32>()), 1..20)
        ) {
            let mut enc = Encoder::new(90, 70).with_tile_size(16);
            let mut dec = Decoder::new(90, 70);
            let mut screen = Framebuffer::new(90, 70);
            for (x, y, w, h, c) in ops {
                let r = Rect::new(x, y, w, h);
                screen.fill(&r, c);
                let ups = enc.encode_region(&screen, &r);
                dec.apply(&ups).unwrap();
                prop_assert_eq!(dec.framebuffer(), &screen);
                prop_assert_eq!(enc.reference(), &screen);
            }
        }
    }
}
