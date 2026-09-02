//! Generation of an `Xauthority` file with a random MIT-MAGIC-COOKIE-1.
//!
//! Every session's X server is started with `-auth <file>` so that other
//! local users cannot connect to the display, which they otherwise could
//! because Xvfb allows all local connections when no authority file is
//! configured.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// `FamilyLocal` in `Xauth.h`.
const FAMILY_LOCAL: u16 = 256;
/// `FamilyWild` in `Xauth.h`.
const FAMILY_WILD: u16 = 0xFFFF;

/// Generate 16 cryptographically random cookie bytes.
pub fn random_cookie() -> io::Result<[u8; 16]> {
    let mut cookie = [0u8; 16];
    let mut filled = 0usize;
    while filled < cookie.len() {
        // SAFETY: buffer pointer and length describe `cookie[filled..]`.
        let n = unsafe {
            libc::getrandom(
                cookie[filled..].as_mut_ptr() as *mut libc::c_void,
                cookie.len() - filled,
                0,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        filled += n as usize;
    }
    Ok(cookie)
}

/// Encode one Xauthority entry.
pub fn encode_entry(family: u16, address: &[u8], number: &str, name: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&family.to_be_bytes());
    for field in [address, number.as_bytes(), name.as_bytes(), data] {
        out.extend_from_slice(&(field.len() as u16).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// Encode the content of an authority file valid for display `display_num`
/// on this host (both `FamilyLocal` with the hostname and a wildcard entry,
/// so that the file works regardless of how the hostname resolves).
pub fn encode_file(display_num: u32, cookie: &[u8; 16]) -> Vec<u8> {
    let host = hostname().unwrap_or_else(|| "localhost".to_string());
    let num = display_num.to_string();
    let mut out = encode_entry(FAMILY_LOCAL, host.as_bytes(), &num, "MIT-MAGIC-COOKIE-1", cookie);
    out.extend(encode_entry(FAMILY_WILD, &[], &num, "MIT-MAGIC-COOKIE-1", cookie));
    out
}

/// Write an authority file readable only by the owner.
pub fn write_file(path: &Path, display_num: u32, cookie: &[u8; 16]) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(&encode_file(display_num, cookie))?;
    f.sync_all()
}

/// Hex encoding of the cookie, as accepted by `xauth add`.
pub fn cookie_hex(cookie: &[u8; 16]) -> String {
    cookie.iter().map(|b| format!("{b:02x}")).collect()
}

/// The system hostname.
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: buffer is valid for its length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_layout() {
        let e = encode_entry(256, b"host", "7", "MIT-MAGIC-COOKIE-1", &[0xAA; 16]);
        assert_eq!(&e[..2], &[1, 0]);
        assert_eq!(&e[2..4], &[0, 4]);
        assert_eq!(&e[4..8], b"host");
        assert_eq!(&e[8..10], &[0, 1]);
        assert_eq!(&e[10..11], b"7");
        assert_eq!(&e[11..13], &[0, 18]);
        assert_eq!(&e[13..31], b"MIT-MAGIC-COOKIE-1");
        assert_eq!(&e[31..33], &[0, 16]);
        assert_eq!(&e[33..], &[0xAA; 16]);
    }

    #[test]
    fn cookies_are_random_and_hex_encodes() {
        let a = random_cookie().unwrap();
        let b = random_cookie().unwrap();
        assert_ne!(a, b);
        assert_eq!(cookie_hex(&[0x0f; 16]).len(), 32);
        assert!(cookie_hex(&[0x0f; 16]).starts_with("0f0f"));
    }

    #[test]
    fn writes_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Xauthority");
        let cookie = [7u8; 16];
        write_file(&path, 42, &cookie).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let content = std::fs::read(&path).unwrap();
        assert_eq!(content, encode_file(42, &cookie));
        assert!(content.windows(2).any(|w| w == b"42"));
    }
}
