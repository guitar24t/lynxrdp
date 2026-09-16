//! `text/uri-list`, the format file managers use for clipboard file copies.
//!
//! Both ends of a Linux connection speak it: the session's clipboard hands us
//! one when the user copies files in a file manager, and we hand one back
//! when the user copies files on the client. It is defined by RFC 2483 and
//! carries percent-encoded URIs, one per line, with `#` comment lines.

use std::path::{Path, PathBuf};

/// Largest number of URIs accepted from a clipboard, so a pathological list
/// cannot make us allocate without bound.
pub const MAX_URIS: usize = 4096;

/// Percent-decode a URI path component.
///
/// Invalid escapes are kept literally rather than rejected: file managers do
/// emit odd bytes, and a filename is better recovered imperfectly than lost.
pub fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // Two hex digits exactly. `u8::from_str_radix` also takes a
            // leading sign, which turned `%+4` into byte 4 instead of leaving
            // it alone.
            let digit = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (digit(bytes[i + 1]), digit(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Percent-encode everything outside the unreserved set, keeping `/`.
pub fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse a `text/uri-list`, returning the local paths of its `file://` URIs.
///
/// URIs with any other scheme, and those naming a different host, are skipped:
/// this side can only act on files it can actually open.
pub fn parse(list: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in list.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("file://") else {
            continue;
        };
        // file://host/path — an empty or "localhost" host means this machine.
        let path_part = match rest.find('/') {
            Some(0) => rest,
            Some(idx) => {
                let host = &rest[..idx];
                if !host.eq_ignore_ascii_case("localhost") {
                    continue;
                }
                &rest[idx..]
            }
            None => continue,
        };
        let decoded = percent_decode(path_part);
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStrExt;
            PathBuf::from(std::ffi::OsStr::from_bytes(&decoded))
        };
        #[cfg(not(unix))]
        let path = PathBuf::from(String::from_utf8_lossy(&decoded).into_owned());
        out.push(path);
        if out.len() >= MAX_URIS {
            break;
        }
    }
    out
}

/// GNOME Desktop Icons places file copies on the text clipboard using this
/// envelope instead of advertising text/uri-list. Recognize only the complete
/// native format; ordinary text and malformed/partial lists remain text.
/// A cut is intentionally treated as a copy: remote pastes never delete sources.
pub fn parse_gnome_text(text: &str) -> Option<Vec<PathBuf>> {
    let mut lines = text.lines();
    if lines.next()? != "x-special/nautilus-clipboard" {
        return None;
    }
    if !matches!(lines.next()?, "copy" | "cut") {
        return None;
    }
    let mut paths = Vec::new();
    for line in lines {
        // Ignore the envelope's final newline, not malformed empty entries.
        if paths.len() >= MAX_URIS || line.is_empty() || line.contains('\0') {
            return None;
        }
        let mut parsed = parse(line);
        if parsed.len() != 1 {
            return None;
        }
        paths.push(parsed.remove(0));
    }
    (!paths.is_empty()).then_some(paths)
}

/// Build a `text/uri-list` from local paths.
///
/// Lines are CRLF terminated, which is what RFC 2483 specifies and what file
/// managers expect.
pub fn build(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        #[cfg(unix)]
        let encoded = {
            use std::os::unix::ffi::OsStrExt;
            percent_encode(p.as_os_str().as_bytes())
        };
        #[cfg(not(unix))]
        let encoded = percent_encode(p.to_string_lossy().as_bytes());
        out.push_str("file://");
        out.push_str(&encoded);
        out.push_str("\r\n");
    }
    out
}

/// The name a path should keep when it moves to the other machine.
///
/// The split is on both separators on every platform, not `Path::file_name`,
/// whose separator set is the *local* one: a Linux session applying it to a
/// Windows client's `C:\Users\alice\report.pdf` kept the whole path as the
/// name, and two clients published the session's `reports/final\v2.pdf` under
/// different names. `unique_name` computes its collisions from these, so the
/// rule has to be one rule everywhere. Nothing that names no file -- an empty
/// tail, `.` or `..` -- is a name.
pub fn base_name(path: &Path) -> Option<String> {
    let path = path.to_string_lossy();
    match path.rsplit(['/', '\\']).next().unwrap_or_default() {
        "" | "." | ".." => None,
        name => Some(name.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_typical_file_manager_list() {
        let list = "file:///home/alice/a.txt\r\nfile:///home/alice/b%20c.txt\r\n";
        assert_eq!(
            parse(list),
            vec![
                PathBuf::from("/home/alice/a.txt"),
                PathBuf::from("/home/alice/b c.txt")
            ]
        );
    }

    #[test]
    fn skips_comments_blanks_and_other_schemes() {
        let list = "# comment\n\nhttp://example.com/x\nfile:///tmp/ok\n";
        assert_eq!(parse(list), vec![PathBuf::from("/tmp/ok")]);
    }

    #[test]
    fn honours_the_host_component() {
        assert_eq!(
            parse("file://localhost/tmp/a"),
            vec![PathBuf::from("/tmp/a")]
        );
        // A URI naming another machine is not ours to open.
        assert!(parse("file://otherbox/tmp/a").is_empty());
    }

    #[test]
    fn roundtrips_awkward_names() {
        let paths = vec![
            PathBuf::from("/tmp/plain.txt"),
            PathBuf::from("/tmp/with space.txt"),
            PathBuf::from("/tmp/héllo#hash?.txt"),
            PathBuf::from("/tmp/100%.txt"),
        ];
        let list = build(&paths);
        assert!(list.ends_with("\r\n"));
        // A '#' must be encoded, or it would read as a comment on the way back.
        assert!(!list.contains("#"), "{list}");
        assert_eq!(parse(&list), paths);
    }

    #[test]
    fn percent_coding_is_reversible() {
        for raw in [&b"abc"[..], b"a b", b"\xff\xfe", b"100%", b"/a/b~c-d_e.f"] {
            assert_eq!(percent_decode(&percent_encode(raw)), raw);
        }
    }

    #[test]
    fn malformed_escapes_survive_rather_than_vanish() {
        // A stray '%' is kept literally instead of eating the filename.
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("a%zz"), b"a%zz");
        assert_eq!(percent_decode("%41"), b"A");
        // A sign is not a hex digit: `%+4` used to decode to byte 4.
        assert_eq!(percent_decode("rate%+4.txt"), b"rate%+4.txt");
        assert_eq!(percent_decode("a%-1b"), b"a%-1b");
        assert_eq!(percent_decode("a% 4b"), b"a% 4b");
        assert_eq!(percent_decode("%4"), b"%4");
    }

    #[test]
    fn a_huge_list_is_capped() {
        let mut list = String::new();
        for i in 0..(MAX_URIS + 100) {
            list.push_str(&format!("file:///tmp/f{i}\r\n"));
        }
        assert_eq!(parse(&list).len(), MAX_URIS);
    }

    #[test]
    fn base_name_of_a_path() {
        assert_eq!(base_name(Path::new("/a/b/c.txt")).as_deref(), Some("c.txt"));
        assert_eq!(base_name(Path::new("/")), None);
        assert_eq!(base_name(Path::new("/a/..")), None);
        assert_eq!(base_name(Path::new(".")), None);
    }

    /// The separator set is not the local platform's. A Windows client sends
    /// its native paths, and the Linux session must not keep the whole thing
    /// as the file's name; and the same session path must publish under the
    /// same name on every client, or the collision set differs per platform.
    #[test]
    fn base_name_splits_on_both_separators_everywhere() {
        assert_eq!(
            base_name(Path::new(r"C:\Users\alice\Documents\report.pdf")).as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            base_name(Path::new(r"/home/u/reports/final\v2.pdf")).as_deref(),
            Some("v2.pdf")
        );
        assert_eq!(
            base_name(Path::new(r"mixed\sub/name.txt")).as_deref(),
            Some("name.txt")
        );
        assert_eq!(base_name(Path::new(r"trailing\")), None);
    }
}

#[cfg(test)]
mod gnome_text_tests {
    use super::*;
    #[test]
    fn native_desktop_copy_and_cut_are_file_lists() {
        for action in ["copy", "cut"] {
            for newline in ["\n", "\r\n"] {
                let text = [
                    "x-special/nautilus-clipboard",
                    action,
                    "file:///tmp/a%20b.txt",
                    "file:///tmp/c.txt",
                    "",
                ]
                .join(newline);
                assert_eq!(
                    parse_gnome_text(&text).unwrap(),
                    vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c.txt")]
                );
            }
        }
    }
    #[test]
    fn ordinary_text_and_invalid_envelopes_are_not_file_copies() {
        for text in [
            "file:///tmp/a",
            "copy\nfile:///tmp/a",
            "x-special/nautilus-clipboard\ncopy\n",
            "x-special/nautilus-clipboard\ncopy\nhello",
            "x-special/nautilus-clipboard\nmove\nfile:///tmp/a",
            "x-special/nautilus-clipboard\ncopy\nfile:///tmp/a\nhttps://example.com",
            "x-special/nautilus-clipboard\ncopy\nfile://remote/tmp/a",
        ] {
            assert!(parse_gnome_text(text).is_none(), "{text:?}");
        }
    }
}
