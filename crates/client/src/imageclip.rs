//! PNG encoding and decoding for clipboard images.
//!
//! The wire format for clipboard images is PNG, because it is lossless
//! (matching the rest of this protocol), compact, and the format X11
//! applications actually offer as `image/png`. The local clipboard APIs work
//! in raw RGBA instead, so this module converts between the two.

use anyhow::{bail, Context, Result};

/// Largest image accepted in either direction, in pixels. Bounds the memory a
/// malformed or hostile PNG can make us allocate.
pub const MAX_PIXELS: usize = 64 * 1024 * 1024;

/// An image as the clipboard APIs want it: 8-bit RGBA, row major.
#[derive(Clone, PartialEq, Eq)]
pub struct Rgba {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// `width * height * 4` bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for Rgba {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rgba({}x{})", self.width, self.height)
    }
}

impl Rgba {
    /// Build from raw RGBA, checking the length matches the dimensions.
    pub fn new(width: usize, height: usize, bytes: Vec<u8>) -> Result<Self> {
        let expected = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .context("image dimensions overflow")?;
        if bytes.len() != expected {
            bail!(
                "image is {} bytes, expected {expected} for {width}x{height}",
                bytes.len()
            );
        }
        Ok(Self {
            width,
            height,
            bytes,
        })
    }
}

/// Encode RGBA pixels as PNG.
pub fn encode_png(image: &Rgba) -> Result<Vec<u8>> {
    if image.width == 0 || image.height == 0 {
        bail!("refusing to encode an empty image");
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("writing PNG header")?;
        writer
            .write_image_data(&image.bytes)
            .context("writing PNG data")?;
    }
    Ok(out)
}

/// Decode a PNG into RGBA, expanding greyscale and RGB sources.
pub fn decode_png(bytes: &[u8]) -> Result<Rgba> {
    let mut decoder = png::Decoder::new(bytes);
    // Bring 1/2/4/16-bit and palette images to plain 8-bit channels so the
    // match below only has to deal with a handful of cases.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().context("reading PNG header")?;
    let info = reader.info();
    let (width, height) = (info.width as usize, info.height as usize);
    let pixels = width
        .checked_mul(height)
        .context("PNG dimensions overflow")?;
    if pixels == 0 {
        bail!("PNG has zero pixels");
    }
    if pixels > MAX_PIXELS {
        bail!("PNG is {width}x{height}, larger than this client will decode");
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).context("decoding PNG")?;
    let data = &buf[..frame.buffer_size()];
    let rgba = match frame.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => {
            let mut v = Vec::with_capacity(pixels * 4);
            for px in data.chunks_exact(3) {
                v.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            v
        }
        png::ColorType::Grayscale => {
            let mut v = Vec::with_capacity(pixels * 4);
            for g in data {
                v.extend_from_slice(&[*g, *g, *g, 0xFF]);
            }
            v
        }
        png::ColorType::GrayscaleAlpha => {
            let mut v = Vec::with_capacity(pixels * 4);
            for px in data.chunks_exact(2) {
                v.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            v
        }
        other => bail!("unsupported PNG colour type {other:?}"),
    };
    Rgba::new(width, height, rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> Rgba {
        let mut bytes = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                bytes.extend_from_slice(&[
                    (x % 256) as u8,
                    (y % 256) as u8,
                    ((x + y) % 256) as u8,
                    0xFF,
                ]);
            }
        }
        Rgba::new(w, h, bytes).unwrap()
    }

    #[test]
    fn png_roundtrip_is_lossless() {
        for (w, h) in [(1, 1), (7, 3), (64, 64), (200, 137)] {
            let img = gradient(w, h);
            let png = encode_png(&img).unwrap();
            assert_eq!(&png[1..4], b"PNG");
            let back = decode_png(&png).unwrap();
            assert_eq!(back, img, "{w}x{h}");
        }
    }

    #[test]
    fn alpha_survives_the_roundtrip() {
        let bytes = vec![10, 20, 30, 0, 40, 50, 60, 128, 70, 80, 90, 255, 1, 2, 3, 77];
        let img = Rgba::new(2, 2, bytes).unwrap();
        let back = decode_png(&encode_png(&img).unwrap()).unwrap();
        assert_eq!(back, img);
    }

    #[test]
    fn rgb_and_grayscale_sources_expand_to_rgba() {
        // An RGB PNG (no alpha channel) must come back fully opaque.
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        }
        let img = decode_png(&png_bytes).unwrap();
        assert_eq!(img.bytes, vec![1, 2, 3, 255, 4, 5, 6, 255]);

        let mut gray = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut gray, 2, 1);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[9, 200]).unwrap();
        }
        let img = decode_png(&gray).unwrap();
        assert_eq!(img.bytes, vec![9, 9, 9, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn malformed_input_is_rejected_cleanly() {
        assert!(decode_png(b"").is_err());
        assert!(decode_png(b"not a png at all").is_err());
        // A truncated but well-signed PNG must error, not panic.
        let img = gradient(16, 16);
        let png = encode_png(&img).unwrap();
        assert!(decode_png(&png[..png.len() / 2]).is_err());
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        assert!(Rgba::new(2, 2, vec![0; 8]).is_err());
        assert!(Rgba::new(2, 2, vec![0; 16]).is_ok());
        assert!(encode_png(&Rgba {
            width: 0,
            height: 5,
            bytes: Vec::new()
        })
        .is_err());
    }
}
