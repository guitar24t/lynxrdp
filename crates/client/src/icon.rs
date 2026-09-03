//! The application icon, decoded from the copy compiled into the binary.
//!
//! One PNG serves both windows. Keeping it in the binary rather than reading
//! it from disk means the icon is there however the client was installed --
//! from a package, from a tarball, or run straight out of `target/`.

/// The 256x256 icon, the same file the packages install.
const PNG: &[u8] = include_bytes!("../../../assets/lynxrdp-256.png");

/// A decoded icon: RGBA8, row-major, `width * height * 4` bytes.
pub struct Icon {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Decode the icon, or `None` if it cannot be read.
///
/// A window with no icon is a cosmetic loss and nothing more, so every
/// failure here is silent: refusing to open the launcher because a decorative
/// image did not decode would be far worse than the missing image.
pub fn load() -> Option<Icon> {
    let decoder = png::Decoder::new(PNG);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());

    // The committed icon is RGBA, but decode defensively: a re-export from a
    // different tool could drop the alpha channel, and silently handing a
    // three-byte-per-pixel buffer to a window API would render as noise.
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };
    if rgba.len() != (info.width as usize) * (info.height as usize) * 4 {
        return None;
    }
    Some(Icon {
        rgba,
        width: info.width,
        height: info.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_icon_decodes() {
        let icon = load().expect("the icon compiled into the binary should decode");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

    #[test]
    fn the_icon_is_not_blank() {
        // A file that decoded but is fully transparent would pass every
        // structural check above and still show nothing.
        let icon = load().unwrap();
        assert!(
            icon.rgba.chunks_exact(4).any(|p| p[3] > 0),
            "every pixel is transparent"
        );
    }
}
