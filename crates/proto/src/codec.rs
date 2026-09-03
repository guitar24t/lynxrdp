//! Tile based screen codec.
//!
//! The screen is divided into a grid of [`TILE_SIZE`](crate::TILE_SIZE)
//! square tiles. When a region of the screen changes the server compares
//! the new pixels against the previously transmitted frame tile by tile.
//! Each tile that differs is trimmed to the bounding box of the changed
//! pixels and encoded with the cheapest suitable [`TileEncoding`].
//!
//! # Why lossless
//!
//! The codec is exact: the client's framebuffer is always bit-identical to
//! the server's. That is deliberate. This protocol is aimed at desktop and
//! development work — text, terminals, editors and UI — where the chroma
//! subsampling of a video codec would blur glyph edges and colour-fringe
//! antialiased text, and where a keyframe cadence would spend bandwidth on
//! an idle screen. Exactness also lets the tests assert
//! `decode(encode(x)) == x` for every input.
//!
//! # Representations
//!
//! A tile's pixels are described in one of two *families*:
//!
//! * **RGB** – packed 24-bit `R G B`, three bytes per pixel.
//! * **Palette** – the distinct colours of the tile, followed by an index
//!   per pixel at 1, 2, 4 or 8 bits. Screen content is overwhelmingly
//!   flat-coloured, so a tile of text is typically two colours: one bit per
//!   pixel instead of twenty-four.
//!
//! Whichever is smaller is then optionally compressed with LZ4 or Zstd,
//! and the smallest of the three results wins. A tile of a single colour
//! short-circuits all of this and costs three bytes.
//!
//! # Copies
//!
//! Scrolling a terminal or editor moves a large region without changing
//! its pixels. Re-encoding it would be wasteful, so before diffing, the
//! encoder looks for a vertical translation between the previous and
//! current frame ([`detect_scroll`]) and emits a [`CopyRect`] instead: a
//! dozen bytes that tell the client to move pixels it already has. The
//! candidate shift is found with row hashes and then **verified pixel by
//! pixel**, so a hash collision can never corrupt the screen.

use std::collections::HashMap;

use crate::image::{Framebuffer, Rect};
use crate::wire::DecodeError;
use crate::TILE_SIZE;

/// Compression level used for Zstd tiles. Kept low (1–3) so that encoding
/// stays cheap enough for interactive frame rates.
pub const ZSTD_LEVEL: i32 = 3;

/// Largest palette that will be built for a tile.
pub const MAX_PALETTE: usize = 256;

/// Payloads smaller than this are stored uncompressed; the framing overhead
/// of a compressor outweighs any gain.
const COMPRESS_MIN: usize = 48;

/// Minimum number of rows a detected scroll must cover to be worth sending
/// as a [`CopyRect`] rather than as ordinary tiles.
pub const MIN_SCROLL_ROWS: u32 = 16;

/// Minimum damaged area (in pixels) before scroll detection runs at all.
/// Small edits such as typing never benefit from it, and this keeps the
/// row-hashing cost off the common path.
pub const SCROLL_MIN_AREA: u64 = 128 * 128;

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
    /// Zstd frame of the `Raw` payload.
    Zstd = 3,
    /// Palette payload (see [`encode_palette`]).
    Palette = 4,
    /// LZ4 block of the palette payload.
    PaletteLz4 = 5,
    /// Zstd frame of the palette payload.
    PaletteZstd = 6,
}

impl TileEncoding {
    /// Convert from the wire tag.
    pub fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(TileEncoding::Solid),
            1 => Ok(TileEncoding::Raw),
            2 => Ok(TileEncoding::Lz4),
            3 => Ok(TileEncoding::Zstd),
            4 => Ok(TileEncoding::Palette),
            5 => Ok(TileEncoding::PaletteLz4),
            6 => Ok(TileEncoding::PaletteZstd),
            other => Err(DecodeError::InvalidTag(u32::from(other))),
        }
    }

    /// Whether the payload (after decompression) is a palette payload.
    pub fn is_palette(self) -> bool {
        matches!(
            self,
            TileEncoding::Palette | TileEncoding::PaletteLz4 | TileEncoding::PaletteZstd
        )
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

/// A region of the previous frame that reappears, translated, in this one.
///
/// The client copies `dest.width` x `dest.height` pixels from
/// (`src_x`, `src_y`) of the framebuffer it already holds to `dest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyRect {
    /// Left edge of the source region in the previous frame.
    pub src_x: u32,
    /// Top edge of the source region in the previous frame.
    pub src_y: u32,
    /// Where the pixels belong in the new frame.
    pub dest: Rect,
}

impl CopyRect {
    /// The source rectangle.
    pub fn src(&self) -> Rect {
        Rect::new(self.src_x, self.src_y, self.dest.width, self.dest.height)
    }
}

/// Everything the server sends for one frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameUpdate {
    /// Regions moved from elsewhere in the previous frame. Applied first.
    pub copies: Vec<CopyRect>,
    /// Changed tiles, applied after the copies.
    pub tiles: Vec<TileUpdate>,
}

impl FrameUpdate {
    /// Whether this frame carries nothing at all.
    pub fn is_empty(&self) -> bool {
        self.copies.is_empty() && self.tiles.is_empty()
    }

    /// Total encoded payload size, for logging and benchmarks.
    pub fn payload_bytes(&self) -> usize {
        self.tiles.iter().map(|t| t.data.len()).sum::<usize>() + self.copies.len() * 12
    }
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

/// Bits needed per index for a palette of `n` colours.
fn bits_for(n: usize) -> u8 {
    match n {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

/// Pack indices at `bpi` bits each, most significant bits first.
fn pack_indices(indices: &[u16], bpi: u8) -> Vec<u8> {
    let per_byte = 8 / usize::from(bpi);
    let mut out = vec![0u8; indices.len().div_ceil(per_byte)];
    let mask = ((1u16 << bpi) - 1) as u8;
    for (i, &idx) in indices.iter().enumerate() {
        let shift = 8 - usize::from(bpi) * (i % per_byte + 1);
        out[i / per_byte] |= ((idx as u8) & mask) << shift;
    }
    out
}

/// Inverse of [`pack_indices`].
fn unpack_indices(data: &[u8], count: usize, bpi: u8) -> Result<Vec<u16>, CodecError> {
    let per_byte = 8 / usize::from(bpi);
    if data.len() < count.div_ceil(per_byte) {
        return Err(CodecError::Corrupt("palette index stream too short"));
    }
    let mask = ((1u16 << bpi) - 1) as u8;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let shift = 8 - usize::from(bpi) * (i % per_byte + 1);
        out.push(u16::from((data[i / per_byte] >> shift) & mask));
    }
    Ok(out)
}

/// Build the palette payload for `pixels`, or `None` if the tile has more
/// than [`MAX_PALETTE`] distinct colours.
///
/// Layout: `u16` colour count, then that many `R G B` triples, then one
/// index per pixel packed at 1, 2, 4 or 8 bits.
pub fn encode_palette(pixels: &[u32]) -> Option<Vec<u8>> {
    let mut lookup: HashMap<u32, u16> = HashMap::new();
    let mut colors: Vec<u32> = Vec::new();
    let mut indices: Vec<u16> = Vec::with_capacity(pixels.len());
    for &p in pixels {
        let c = p & 0x00FF_FFFF;
        let next = colors.len() as u16;
        let idx = *lookup.entry(c).or_insert_with(|| {
            colors.push(c);
            next
        });
        if colors.len() > MAX_PALETTE {
            return None;
        }
        indices.push(idx);
    }
    let bpi = bits_for(colors.len());
    let mut out = Vec::with_capacity(2 + colors.len() * 3 + pixels.len());
    out.extend_from_slice(&(colors.len() as u16).to_le_bytes());
    for c in &colors {
        out.push((c >> 16) as u8);
        out.push((c >> 8) as u8);
        out.push(*c as u8);
    }
    out.extend_from_slice(&pack_indices(&indices, bpi));
    Some(out)
}

/// Decode a palette payload into `count` pixels.
pub fn decode_palette(data: &[u8], count: usize) -> Result<Vec<u32>, CodecError> {
    if data.len() < 2 {
        return Err(CodecError::Corrupt("palette payload too short"));
    }
    let n = usize::from(u16::from_le_bytes([data[0], data[1]]));
    if n == 0 || n > MAX_PALETTE {
        return Err(CodecError::Corrupt("bad palette colour count"));
    }
    let table_end = 2 + n * 3;
    if data.len() < table_end {
        return Err(CodecError::Corrupt("palette table truncated"));
    }
    let colors: Vec<u32> = data[2..table_end]
        .chunks_exact(3)
        .map(|c| (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]))
        .collect();
    let indices = unpack_indices(&data[table_end..], count, bits_for(n))?;
    let mut out = Vec::with_capacity(count);
    for i in indices {
        let c = colors
            .get(usize::from(i))
            .ok_or(CodecError::Corrupt("palette index out of range"))?;
        out.push(*c);
    }
    Ok(out)
}

/// Upper bound on the decompressed size of a payload, used to cap
/// allocation when decompressing untrusted data.
fn max_payload_len(encoding: TileEncoding, pixel_count: usize) -> usize {
    if encoding.is_palette() {
        // count + full table + one byte of indices per pixel (8bpi worst case)
        2 + MAX_PALETTE * 3 + pixel_count
    } else {
        pixel_count * 3
    }
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

    // Choose the cheaper representation before spending any time compressing.
    let rgb = pack_rgb(pixels);
    let (base, palette) = match encode_palette(pixels) {
        Some(p) if p.len() < rgb.len() => (p, true),
        _ => (rgb, false),
    };

    let base_len = base.len();
    if base_len < COMPRESS_MIN {
        let encoding = if palette {
            TileEncoding::Palette
        } else {
            TileEncoding::Raw
        };
        return TileUpdate {
            rect,
            encoding,
            data: base,
        };
    }

    let lz4 = lz4_flex::block::compress_prepend_size(&base);
    let zstd = zstd::bulk::compress(&base, ZSTD_LEVEL).unwrap_or_default();
    let zstd_ok = !zstd.is_empty();

    if zstd_ok && zstd.len() <= lz4.len() && zstd.len() < base_len {
        let encoding = if palette {
            TileEncoding::PaletteZstd
        } else {
            TileEncoding::Zstd
        };
        TileUpdate {
            rect,
            encoding,
            data: zstd,
        }
    } else if lz4.len() < base_len {
        let encoding = if palette {
            TileEncoding::PaletteLz4
        } else {
            TileEncoding::Lz4
        };
        TileUpdate {
            rect,
            encoding,
            data: lz4,
        }
    } else {
        let encoding = if palette {
            TileEncoding::Palette
        } else {
            TileEncoding::Raw
        };
        TileUpdate {
            rect,
            encoding,
            data: base,
        }
    }
}

/// Decode a tile into `0x00RRGGBB` pixels.
pub fn decode_pixels(tile: &TileUpdate) -> Result<Vec<u32>, CodecError> {
    let n = tile.rect.area() as usize;
    if tile.encoding == TileEncoding::Solid {
        if tile.data.len() != 3 {
            return Err(CodecError::Corrupt("solid payload must be 3 bytes"));
        }
        let c = (u32::from(tile.data[0]) << 16)
            | (u32::from(tile.data[1]) << 8)
            | u32::from(tile.data[2]);
        return Ok(vec![c; n]);
    }

    let cap = max_payload_len(tile.encoding, n);
    let payload = match tile.encoding {
        TileEncoding::Raw | TileEncoding::Palette => std::borrow::Cow::Borrowed(&tile.data[..]),
        TileEncoding::Lz4 | TileEncoding::PaletteLz4 => {
            let raw = lz4_flex::block::decompress_size_prepended(&tile.data)
                .map_err(|_| CodecError::Corrupt("lz4 decompression failed"))?;
            if raw.len() > cap {
                return Err(CodecError::Corrupt("lz4 payload too large"));
            }
            std::borrow::Cow::Owned(raw)
        }
        TileEncoding::Zstd | TileEncoding::PaletteZstd => {
            let raw = zstd::bulk::decompress(&tile.data, cap)
                .map_err(|_| CodecError::Corrupt("zstd decompression failed"))?;
            std::borrow::Cow::Owned(raw)
        }
        TileEncoding::Solid => unreachable!("handled above"),
    };

    if tile.encoding.is_palette() {
        decode_palette(&payload, n)
    } else {
        if payload.len() != n * 3 {
            return Err(CodecError::Corrupt("rgb payload size mismatch"));
        }
        unpack_rgb(&payload)
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

/// Apply copy rectangles to a framebuffer.
///
/// Every source region is read before any destination is written, so
/// overlapping copies (the usual case when scrolling) behave as if they
/// all read the frame as it was before this call. Returns the union of the
/// destination rectangles.
pub fn apply_copies(fb: &mut Framebuffer, copies: &[CopyRect]) -> Result<Rect, CodecError> {
    let bounds = fb.bounds();
    let mut sources = Vec::with_capacity(copies.len());
    for c in copies {
        if c.dest.is_empty() || !bounds.contains(&c.dest) || !bounds.contains(&c.src()) {
            return Err(CodecError::OutOfBounds(c.dest));
        }
        sources.push(fb.extract(&c.src()));
    }
    let mut dirty = Rect::default();
    for (c, pixels) in copies.iter().zip(sources) {
        fb.blit_pixels(&c.dest, &pixels);
        dirty = dirty.union(&c.dest);
    }
    Ok(dirty)
}

/// FNV-1a over one row of pixels, used to find candidate scroll offsets.
fn row_hash(row: &[u32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &p in row {
        h ^= u64::from(p & 0x00FF_FFFF);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Longest run of rows consistent with shift `dy`, judged by row hashes.
/// Indices are relative to the region; the source row must also lie inside
/// it, which keeps the resulting copy in bounds by construction.
fn longest_hash_run(prev_h: &[u64], cur_h: &[u64], dy: i64) -> Option<(usize, usize)> {
    let rows = cur_h.len();
    let mut best: Option<(usize, usize)> = None;
    let mut start: Option<usize> = None;
    let close = |start: &mut Option<usize>, end: usize, best: &mut Option<(usize, usize)>| {
        if let Some(s) = start.take() {
            if best.map(|(a, b)| b - a).unwrap_or(0) < end - s {
                *best = Some((s, end));
            }
        }
    };
    for (i, &h) in cur_h.iter().enumerate() {
        let p = i as i64 - dy;
        let matches = p >= 0 && (p as usize) < rows && prev_h[p as usize] == h;
        if matches {
            start.get_or_insert(i);
        } else {
            close(&mut start, i, &mut best);
        }
    }
    close(&mut start, rows, &mut best);
    best
}

/// Look for a vertical translation of `region` between `prev` and `cur`.
///
/// Row hashes propose candidate shifts and rank them, then the best
/// candidate is confirmed by comparing the actual pixels, so the returned
/// copy is always exact — a hash collision can shorten a copy but can never
/// corrupt the screen. Only runs of at least `min_rows` rows are reported.
pub fn detect_scroll(
    prev: &Framebuffer,
    cur: &Framebuffer,
    region: &Rect,
    min_rows: u32,
) -> Option<CopyRect> {
    if prev.width() != cur.width() || prev.height() != cur.height() {
        return None;
    }
    let r = region.intersect(&cur.bounds());
    if r.width == 0 || r.height < min_rows.saturating_mul(2) {
        return None;
    }

    let rows = r.height as usize;
    let mut prev_h = Vec::with_capacity(rows);
    let mut cur_h = Vec::with_capacity(rows);
    for y in r.y..r.bottom() {
        prev_h.push(row_hash(prev.row(y, r.x, r.width)));
        cur_h.push(row_hash(cur.row(y, r.x, r.width)));
    }

    // Candidate source rows per hash. Identical rows are common (blank
    // lines, flat backgrounds), so the list is capped to bound the work;
    // that biases the vote, which is why several candidates are ranked
    // below rather than trusting the top one.
    const MAX_CANDIDATES_PER_HASH: usize = 16;
    let mut by_hash: HashMap<u64, Vec<u32>> = HashMap::new();
    for (i, &h) in prev_h.iter().enumerate() {
        let e = by_hash.entry(h).or_default();
        if e.len() < MAX_CANDIDATES_PER_HASH {
            e.push(i as u32);
        }
    }

    // Tally shifts, considering only rows that actually changed: an
    // unchanged row is equally consistent with "no scroll".
    let mut tally: HashMap<i64, u32> = HashMap::new();
    for (i, &h) in cur_h.iter().enumerate() {
        if prev_h[i] == h {
            continue;
        }
        if let Some(candidates) = by_hash.get(&h) {
            for &p in candidates {
                let dy = i as i64 - i64::from(p);
                if dy != 0 {
                    *tally.entry(dy).or_insert(0) += 1;
                }
            }
        }
    }
    if tally.is_empty() {
        return None;
    }

    // Rank the most-voted shifts by the length of the row run they explain.
    const MAX_SHIFTS_CONSIDERED: usize = 8;
    let mut shifts: Vec<(i64, u32)> = tally.into_iter().collect();
    shifts.sort_unstable_by_key(|&(dy, votes)| (std::cmp::Reverse(votes), dy.abs()));
    let (dy, span) = shifts
        .into_iter()
        .take(MAX_SHIFTS_CONSIDERED)
        .filter_map(|(dy, _)| longest_hash_run(&prev_h, &cur_h, dy).map(|run| (dy, run)))
        .max_by_key(|(_, (a, b))| b - a)?;
    if (span.1 - span.0) < min_rows as usize {
        return None;
    }

    // Confirm the run pixel by pixel, shrinking it to the verified prefix.
    let y0 = r.y + span.0 as u32;
    let mut end = y0;
    for y in y0..r.y + span.1 as u32 {
        let sy = (i64::from(y) - dy) as u32;
        if cur.row(y, r.x, r.width) != prev.row(sy, r.x, r.width) {
            break;
        }
        end = y + 1;
    }
    if end - y0 < min_rows {
        return None;
    }
    Some(CopyRect {
        src_x: r.x,
        src_y: (i64::from(y0) - dy) as u32,
        dest: Rect::new(r.x, y0, r.width, end - y0),
    })
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
        Some(Rect::new(
            min_x,
            min_y,
            max_x - min_x + 1,
            max_y - min_y + 1,
        ))
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
        Self {
            prev: Framebuffer::new(width, height),
            tile: TILE_SIZE,
        }
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

    /// Encode one frame: look for a scroll first, then diff what remains.
    ///
    /// Set `allow_copy` to `false` when the reference frame is not a real
    /// previous frame (for instance right after [`Encoder::invalidate`]),
    /// so no time is spent hunting for a translation that cannot exist.
    pub fn encode_frame(
        &mut self,
        current: &Framebuffer,
        regions: &[Rect],
        allow_copy: bool,
    ) -> FrameUpdate {
        let mut copies = Vec::new();
        if allow_copy {
            let bbox = regions
                .iter()
                .fold(Rect::default(), |acc, r| acc.union(r))
                .intersect(&self.prev.bounds());
            if bbox.area() >= SCROLL_MIN_AREA {
                if let Some(copy) = detect_scroll(&self.prev, current, &bbox, MIN_SCROLL_ROWS) {
                    // Keep the reference in step so the tile pass only sees
                    // what the copy did not already account for.
                    if apply_copies(&mut self.prev, &[copy]).is_ok() {
                        copies.push(copy);
                    }
                }
            }
        }
        let tiles = self.encode_regions(current, regions);
        FrameUpdate { copies, tiles }
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
        Self {
            fb: Framebuffer::new(width, height),
        }
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

    /// Apply a whole frame: copies first, then tiles.
    pub fn apply_frame(
        &mut self,
        copies: &[CopyRect],
        tiles: &[TileUpdate],
    ) -> Result<Rect, CodecError> {
        let moved = apply_copies(&mut self.fb, copies)?;
        let drawn = self.apply(tiles)?;
        Ok(moved.union(&drawn))
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

    /// A framebuffer that looks like text: a light background with dark
    /// glyph-ish runs, i.e. very few distinct colours per tile.
    fn texty(w: u32, h: u32, offset: u32) -> Framebuffer {
        let mut fb = Framebuffer::new(w, h);
        fb.fill(&fb.bounds(), 0xFFFFFF);
        for row in 0..h / 16 {
            let y = row * 16 + 4;
            for x in 0..w {
                if (x / 3 + row * 7 + offset) % 5 < 2 && y + 8 < h {
                    for dy in 0..8 {
                        fb.set(x, y + dy, 0x202020);
                    }
                }
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
    fn two_colour_tile_uses_palette_and_is_tiny() {
        // A 64x64 tile of two colours: 1 bit per pixel before compression.
        let mut fb = Framebuffer::new(64, 64);
        fb.fill(&fb.bounds(), 0xFFFFFF);
        for y in 0..64 {
            for x in 0..64 {
                if (x / 2 + y / 3) % 3 == 0 {
                    fb.set(x, y, 0x101010);
                }
            }
        }
        let px = fb.extract(&fb.bounds());
        let t = encode_pixels(fb.bounds(), &px);
        assert!(
            t.encoding.is_palette(),
            "expected a palette encoding, got {:?}",
            t.encoding
        );
        assert_eq!(decode_pixels(&t).unwrap(), px);
        // Raw would be 12288 bytes; the palette form must be far smaller.
        assert!(
            t.data.len() < 700,
            "palette tile was {} bytes",
            t.data.len()
        );
    }

    #[test]
    fn palette_beats_raw_on_text_like_content() {
        let fb = texty(256, 256, 0);
        let px = fb.extract(&fb.bounds());
        let t = encode_pixels(fb.bounds(), &px);
        assert_eq!(decode_pixels(&t).unwrap(), px);
        let raw = px.len() * 3;
        assert!(
            t.data.len() * 20 < raw,
            "expected >20x saving, got {} vs {raw}",
            t.data.len()
        );
    }

    #[test]
    fn palette_supports_every_index_width() {
        for n in [2usize, 3, 5, 16, 17, 256] {
            let pixels: Vec<u32> = (0..1024)
                .map(|i| (((i % n) as u32) * 0x010203) & 0xFF_FFFF)
                .collect();
            let payload = encode_palette(&pixels).expect("palette fits");
            assert_eq!(decode_palette(&payload, pixels.len()).unwrap(), pixels);
            let t = encode_pixels(Rect::new(0, 0, 32, 32), &pixels);
            assert_eq!(decode_pixels(&t).unwrap(), pixels, "n = {n}");
        }
    }

    #[test]
    fn palette_declines_when_too_many_colours() {
        let pixels: Vec<u32> = (0..1024).map(|i| (i * 7919) & 0xFFFFFF).collect();
        assert!(encode_palette(&pixels).is_none());
        // The tile still round-trips through an RGB encoding.
        let t = encode_pixels(Rect::new(0, 0, 32, 32), &pixels);
        assert!(!t.encoding.is_palette());
        assert_eq!(decode_pixels(&t).unwrap(), pixels);
    }

    #[test]
    fn compressible_tile_is_compressed() {
        let mut fb = Framebuffer::new(64, 64);
        fb.fill(&Rect::new(0, 0, 64, 32), 0x102030);
        fb.fill(&Rect::new(0, 32, 64, 32), 0x405060);
        let px = fb.extract(&fb.bounds());
        let t = encode_pixels(fb.bounds(), &px);
        assert!(
            matches!(
                t.encoding,
                TileEncoding::Lz4
                    | TileEncoding::Zstd
                    | TileEncoding::Palette
                    | TileEncoding::PaletteLz4
                    | TileEncoding::PaletteZstd
            ),
            "got {:?}",
            t.encoding
        );
        assert!(t.data.len() < 200);
        assert_eq!(decode_pixels(&t).unwrap(), px);
    }

    #[test]
    fn every_encoding_roundtrips_when_constructed_directly() {
        let pixels: Vec<u32> = (0..256).map(|i| (i * 37) & 0xFFFFFF).collect();
        let rect = Rect::new(0, 0, 16, 16);
        let rgb = pack_rgb(&pixels);
        let pal = encode_palette(&pixels).unwrap();
        let cases = vec![
            (TileEncoding::Raw, rgb.clone()),
            (
                TileEncoding::Lz4,
                lz4_flex::block::compress_prepend_size(&rgb),
            ),
            (
                TileEncoding::Zstd,
                zstd::bulk::compress(&rgb, ZSTD_LEVEL).unwrap(),
            ),
            (TileEncoding::Palette, pal.clone()),
            (
                TileEncoding::PaletteLz4,
                lz4_flex::block::compress_prepend_size(&pal),
            ),
            (
                TileEncoding::PaletteZstd,
                zstd::bulk::compress(&pal, ZSTD_LEVEL).unwrap(),
            ),
        ];
        for (encoding, data) in cases {
            let t = TileUpdate {
                rect,
                encoding,
                data,
            };
            assert_eq!(decode_pixels(&t).unwrap(), pixels, "encoding {encoding:?}");
        }
    }

    #[test]
    fn encoding_tags_roundtrip() {
        for e in [
            TileEncoding::Solid,
            TileEncoding::Raw,
            TileEncoding::Lz4,
            TileEncoding::Zstd,
            TileEncoding::Palette,
            TileEncoding::PaletteLz4,
            TileEncoding::PaletteZstd,
        ] {
            assert_eq!(TileEncoding::from_u8(e as u8).unwrap(), e);
        }
        assert!(TileEncoding::from_u8(7).is_err());
        assert!(TileEncoding::from_u8(255).is_err());
    }

    #[test]
    fn corrupt_data_is_rejected() {
        let bad = |encoding, data| TileUpdate {
            rect: Rect::new(0, 0, 2, 2),
            encoding,
            data,
        };
        assert!(decode_pixels(&bad(TileEncoding::Raw, vec![1])).is_err());
        assert!(decode_pixels(&bad(TileEncoding::Lz4, vec![1, 2, 3])).is_err());
        assert!(decode_pixels(&bad(TileEncoding::Zstd, vec![1, 2, 3])).is_err());
        assert!(decode_pixels(&bad(TileEncoding::Solid, vec![1])).is_err());
        assert!(decode_pixels(&bad(TileEncoding::Palette, vec![1])).is_err());
        // Palette claiming zero colours, and one whose table is truncated.
        assert!(decode_pixels(&bad(TileEncoding::Palette, vec![0, 0])).is_err());
        assert!(decode_pixels(&bad(TileEncoding::Palette, vec![2, 0, 1, 2, 3])).is_err());
        let mut fb = Framebuffer::new(4, 4);
        let t = bad(TileEncoding::Solid, vec![1, 2, 3]);
        let t = TileUpdate {
            rect: Rect::new(3, 3, 2, 2),
            ..t
        };
        assert!(matches!(
            apply_tile(&mut fb, &t),
            Err(CodecError::OutOfBounds(_))
        ));
    }

    #[test]
    fn palette_index_out_of_range_is_rejected() {
        // One colour declared, but 4bpi indices referencing colour 5.
        let mut data = vec![1, 0, 0xAA, 0xBB, 0xCC];
        data.extend_from_slice(&[0x55, 0x55]);
        let t = TileUpdate {
            rect: Rect::new(0, 0, 2, 2),
            encoding: TileEncoding::Palette,
            data,
        };
        assert_eq!(
            decode_pixels(&t),
            Err(CodecError::Corrupt("palette index out of range"))
        );
    }

    #[test]
    fn zstd_bomb_is_capped() {
        // A payload that decompresses far beyond the tile size must be refused
        // rather than allocated.
        let huge = vec![0u8; 4 * 1024 * 1024];
        let bomb = zstd::bulk::compress(&huge, 1).unwrap();
        let t = TileUpdate {
            rect: Rect::new(0, 0, 2, 2),
            encoding: TileEncoding::Zstd,
            data: bomb,
        };
        assert!(decode_pixels(&t).is_err());
    }

    #[test]
    fn detects_a_clean_scroll_and_makes_it_nearly_free() {
        let before = texty(320, 320, 0);
        // Scroll up by 32 pixels: every row moves, the bottom strip is new.
        let mut after = Framebuffer::new(320, 320);
        for y in 0..320 - 32 {
            after
                .row_mut(y, 0, 320)
                .copy_from_slice(before.row(y + 32, 0, 320));
        }
        after.fill(&Rect::new(0, 288, 320, 32), 0xFFFFFF);

        let copy = detect_scroll(&before, &after, &before.bounds(), MIN_SCROLL_ROWS)
            .expect("scroll detected");
        assert_eq!(copy.dest.y, 0);
        assert_eq!(copy.src_y, 32);
        assert!(
            copy.dest.height >= 288 - 32,
            "run was only {} rows",
            copy.dest.height
        );

        // A full frame carrying the copy costs far less than re-encoding.
        let mut enc = Encoder::new(320, 320);
        enc.encode_frame(&before, &[before.bounds()], false);
        let with_copy = enc.encode_frame(&after, &[after.bounds()], true);
        assert_eq!(with_copy.copies.len(), 1);

        let mut plain = Encoder::new(320, 320);
        plain.encode_frame(&before, &[before.bounds()], false);
        let without = plain.encode_frame(&after, &[after.bounds()], false);
        assert!(without.copies.is_empty());
        assert!(
            with_copy.payload_bytes() * 4 < without.payload_bytes(),
            "copy {} bytes vs plain {} bytes",
            with_copy.payload_bytes(),
            without.payload_bytes()
        );
    }

    #[test]
    fn scroll_result_is_pixel_exact_end_to_end() {
        let before = texty(256, 256, 0);
        let mut after = Framebuffer::new(256, 256);
        for y in 0..256 - 48 {
            after
                .row_mut(y, 0, 256)
                .copy_from_slice(before.row(y + 48, 0, 256));
        }
        after.fill(&Rect::new(0, 208, 256, 48), 0xEEEEEE);

        let mut enc = Encoder::new(256, 256);
        let mut dec = Decoder::new(256, 256);
        let f1 = enc.encode_frame(&before, &[before.bounds()], false);
        dec.apply_frame(&f1.copies, &f1.tiles).unwrap();
        assert_eq!(dec.framebuffer(), &before);

        let f2 = enc.encode_frame(&after, &[after.bounds()], true);
        assert_eq!(f2.copies.len(), 1);
        dec.apply_frame(&f2.copies, &f2.tiles).unwrap();
        assert_eq!(dec.framebuffer(), &after);
        assert_eq!(enc.reference(), &after);
    }

    #[test]
    fn no_scroll_reported_for_unrelated_frames() {
        let a = checker(200, 200, 1);
        let b = checker(200, 200, 2);
        assert!(detect_scroll(&a, &b, &a.bounds(), MIN_SCROLL_ROWS).is_none());
        // Identical frames have no non-zero shift either.
        assert!(detect_scroll(&a, &a, &a.bounds(), MIN_SCROLL_ROWS).is_none());
        // Too small a region to bother.
        let small = Rect::new(0, 0, 200, 8);
        assert!(detect_scroll(&a, &b, &small, MIN_SCROLL_ROWS).is_none());
    }

    #[test]
    fn copies_are_bounds_checked() {
        let mut fb = Framebuffer::new(32, 32);
        let outside = CopyRect {
            src_x: 0,
            src_y: 0,
            dest: Rect::new(24, 24, 16, 16),
        };
        assert!(matches!(
            apply_copies(&mut fb, &[outside]),
            Err(CodecError::OutOfBounds(_))
        ));
        let bad_src = CopyRect {
            src_x: 30,
            src_y: 30,
            dest: Rect::new(0, 0, 8, 8),
        };
        assert!(matches!(
            apply_copies(&mut fb, &[bad_src]),
            Err(CodecError::OutOfBounds(_))
        ));
        let empty = CopyRect {
            src_x: 0,
            src_y: 0,
            dest: Rect::new(0, 0, 0, 4),
        };
        assert!(apply_copies(&mut fb, &[empty]).is_err());
    }

    #[test]
    fn overlapping_copies_read_the_pre_copy_frame() {
        // Shifting a gradient down by one row must not smear the first row
        // over the whole region, which is what an in-place loop would do.
        let mut fb = Framebuffer::new(4, 4);
        for y in 0..4 {
            fb.fill(&Rect::new(0, y, 4, 1), y * 0x010101);
        }
        let copy = CopyRect {
            src_x: 0,
            src_y: 0,
            dest: Rect::new(0, 1, 4, 3),
        };
        apply_copies(&mut fb, &[copy]).unwrap();
        assert_eq!(fb.get(0, 1), 0x000000);
        assert_eq!(fb.get(0, 2), 0x010101);
        assert_eq!(fb.get(0, 3), 0x020202);
    }

    #[test]
    fn encoder_only_sends_changed_tiles() {
        let mut enc = Encoder::new(200, 150).with_tile_size(64);
        let mut dec = Decoder::new(200, 150);
        let mut screen = Framebuffer::new(200, 150);
        assert!(enc.encode_region(&screen, &screen.bounds()).is_empty());
        screen.fill(&Rect::new(70, 70, 10, 10), 0xAABBCC);
        let ups = enc.encode_region(&screen, &screen.bounds());
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].rect, Rect::new(70, 70, 10, 10));
        assert_eq!(ups[0].encoding, TileEncoding::Solid);
        dec.apply(&ups).unwrap();
        assert_eq!(dec.framebuffer(), &screen);
        assert!(enc.encode_region(&screen, &screen.bounds()).is_empty());
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
        assert!(enc
            .encode_region(&screen, &Rect::new(100, 100, 10, 10))
            .is_empty());
        assert!(enc.encode_region(&screen, &Rect::default()).is_empty());
        assert!(enc.encode_frame(&screen, &[], true).is_empty());
    }

    #[test]
    fn changed_bbox_is_tight() {
        let a = Framebuffer::new(10, 10);
        let mut b = Framebuffer::new(10, 10);
        assert_eq!(changed_bbox(&a, &b, &a.bounds()), None);
        b.set(3, 4, 1);
        b.set(7, 8, 1);
        assert_eq!(
            changed_bbox(&a, &b, &a.bounds()),
            Some(Rect::new(3, 4, 5, 5))
        );
        assert_eq!(
            changed_bbox(&a, &b, &Rect::new(0, 0, 5, 5)),
            Some(Rect::new(3, 4, 1, 1))
        );
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

        /// Few distinct colours is the case palette encoding exists for, so
        /// exercise it heavily.
        #[test]
        fn low_colour_roundtrip(
            w in 1u32..32, h in 1u32..32,
            palette in proptest::collection::vec(any::<u32>(), 1..17usize),
            picks in proptest::collection::vec(any::<u8>(), 1..1024usize),
        ) {
            let n = (w * h) as usize;
            let colors: Vec<u32> = palette.iter().map(|c| c & 0xFFFFFF).collect();
            let px: Vec<u32> = picks
                .iter()
                .cycle()
                .take(n)
                .map(|p| colors[usize::from(*p) % colors.len()])
                .collect();
            let t = encode_pixels(Rect::new(0, 0, w, h), &px);
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

        /// Whatever the encoder decides — copy, palette, compression or a
        /// plain tile — the client must end up with the identical screen.
        #[test]
        fn frames_with_scrolling_stay_exact(
            shifts in proptest::collection::vec(-40i32..40, 1..8),
        ) {
            let mut enc = Encoder::new(128, 128).with_tile_size(32);
            let mut dec = Decoder::new(128, 128);
            let mut screen = texty(128, 128, 0);
            let first = enc.encode_frame(&screen, &[screen.bounds()], false);
            dec.apply_frame(&first.copies, &first.tiles).unwrap();
            prop_assert_eq!(dec.framebuffer(), &screen);

            for (i, dy) in shifts.into_iter().enumerate() {
                let mut next = Framebuffer::new(128, 128);
                next.fill(&next.bounds(), 0xFFFFFF);
                for y in 0..128i32 {
                    let sy = y + dy;
                    if (0..128).contains(&sy) {
                        let row = screen.row(sy as u32, 0, 128).to_vec();
                        next.row_mut(y as u32, 0, 128).copy_from_slice(&row);
                    }
                }
                // A little fresh content so frames are not pure translations.
                next.fill(&Rect::new(0, (i as u32 * 8) % 120, 128, 6), 0x3366AA);
                let f = enc.encode_frame(&next, &[next.bounds()], true);
                dec.apply_frame(&f.copies, &f.tiles).unwrap();
                prop_assert_eq!(dec.framebuffer(), &next);
                prop_assert_eq!(enc.reference(), &next);
                screen = next;
            }
        }
    }
}
