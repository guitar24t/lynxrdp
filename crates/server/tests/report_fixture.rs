//! The monitoring report *payload*, pinned across the Rust/Python seam.
//!
//! `reporting/seal.rs` and `tools/lynxrdp-monitor/lynxrdp_monitor/crypto.py`
//! are already pinned to each other: both assert the same known-answer key, so
//! changing one constant without the other fails a test on both sides. Nothing
//! pinned what goes *inside* the envelope. Rename `node` to `hostname` in
//! `Report::to_json`, or drop a field, and every suite in this repository
//! stays green while `parse_report` in `model.py` starts returning `None` --
//! which shows up in the field as every deployed viewer displaying an empty
//! table, with no error anywhere, until somebody notices the monitoring has
//! been dead for a month.
//!
//! So: one sealed datagram, committed once, opened from both sides. This file
//! asserts its plaintext byte for byte against what `Report::to_json` produces
//! today; `tests/test_report_fixture.py` in the monitor asserts the fields the
//! viewer actually reads out of the same bytes. Neither side can be updated to
//! match a change without the other failing.
//!
//! The fixture lives in `tests/fixtures/report-v1.hex`, in *this* tree, and the
//! Python suite reaches across the repository for it. That looks wrong and is
//! deliberate; the file says so itself at the top, at length, because the
//! obvious tidy-up -- give each suite its own copy -- reinstates exactly the
//! silent drift this exists to catch.

use std::path::PathBuf;

use lynxrdp_server::reporting::{seal, Report};

/// Where the one copy lives. `CARGO_MANIFEST_DIR` rather than a relative path
/// because the working directory of a test is not something to rely on.
fn fixture_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/report-v1.hex"
    ))
}

/// The report the fixture holds.
///
/// The same values as `sample()` in `crates/server/src/reporting/mod.rs`, which
/// is private to that file's own test module and cannot be called from here. A
/// literal `version` rather than `SERVER_NAME`, so bumping the crate version
/// does not invalidate a fixture that has nothing to do with it.
///
/// Constructing the struct field by field is part of the guard: adding a field
/// to `Report` stops this file compiling, which is the right moment to decide
/// whether the viewer needs to know about it.
fn fixture_report() -> Report {
    Report {
        node: "desk01".into(),
        ip: "10.0.0.5".into(),
        port: 3390,
        version: "LynxRDP/0.1.0".into(),
        sessions: 2,
        uptime_secs: 3600,
        time: 1_756_900_000,
    }
}

/// Decode the fixture's text: hex digits, with `#` comment lines and all
/// whitespace ignored.
///
/// Hand-rolled because the server links no hex crate and one datagram is not
/// worth adding a dependency the packaging then has to carry. Strict: a stray
/// character means the fixture is corrupt, which is worth a failure rather than
/// a silent skip past it.
fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    let mut nibbles: Vec<u8> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        for c in line.chars() {
            if c.is_whitespace() {
                continue;
            }
            let v = c
                .to_digit(16)
                .ok_or_else(|| format!("line {}: {c:?} is not a hex digit", n + 1))?;
            nibbles.push(v as u8);
        }
    }
    if nibbles.len() % 2 != 0 {
        return Err(format!("odd number of hex digits ({})", nibbles.len()));
    }
    Ok(nibbles.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

/// 32 bytes to a line, which is what the committed file uses.
fn hex_lines(bytes: &[u8]) -> String {
    let mut out = String::new();
    for row in bytes.chunks(32) {
        out.push_str(&row.iter().map(|b| format!("{b:02x}")).collect::<String>());
        out.push('\n');
    }
    out
}

#[test]
fn hex_decoding_ignores_comments_and_layout() {
    assert_eq!(
        decode_hex("# a note\n00ff\n  10\t20 \n#another\n").unwrap(),
        vec![0x00, 0xff, 0x10, 0x20]
    );
    assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
    assert_eq!(decode_hex("# only comments\n").unwrap(), Vec::<u8>::new());
    // A truncated or mistyped fixture must not decode to "almost right".
    assert!(decode_hex("abc").is_err());
    assert!(decode_hex("0g").is_err());
    // '#' only starts a comment at the start of a line; anywhere else it is
    // simply not a hex digit.
    assert!(decode_hex("00 # trailing").is_err());
}

#[test]
fn hex_round_trips_through_the_line_layout() {
    let bytes: Vec<u8> = (0..70u8).collect();
    assert_eq!(decode_hex(&hex_lines(&bytes)).unwrap(), bytes);
}

#[test]
fn the_fixture_holds_exactly_what_the_server_would_send() {
    let path = fixture_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let datagram = decode_hex(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

    assert!(
        datagram.len() > seal::HEADER_LEN + seal::TAG_LEN,
        "the fixture is {} bytes, too short to carry a report",
        datagram.len()
    );
    assert_eq!(
        &datagram[..4],
        &seal::MAGIC[..],
        "the fixture lost its magic"
    );
    assert_eq!(
        datagram[4],
        seal::FORMAT_VERSION,
        "the fixture is format version {}, this build speaks {}; bumping the \
         version is a wire change and needs a new fixture and a viewer that \
         understands it",
        datagram[4],
        seal::FORMAT_VERSION
    );

    let plaintext = seal::open(&datagram)
        .unwrap_or_else(|e| panic!("the committed fixture no longer opens: {e:#}"));
    // Through String only for a readable failure; String equality is byte
    // equality, so this is still an assertion about the bytes.
    let got = String::from_utf8(plaintext)
        .unwrap_or_else(|e| panic!("the fixture's plaintext is not UTF-8: {e}"));
    let expected = fixture_report().to_json();
    assert_eq!(
        got, expected,
        "the report payload changed. Every deployed lynxrdp-monitor parses the \
         old shape, so this is a wire change: regenerate the fixture (see its \
         header) and update tools/lynxrdp-monitor/tests/test_report_fixture.py \
         in the same commit"
    );
}

/// Rewrite the committed fixture from the current `Report::to_json`.
///
/// Ignored, *and* gated on an environment variable: `cargo test -- --ignored`
/// over the workspace is a thing people run, and a golden file that quietly
/// regenerates itself is no longer golden. Sealing goes through `seal::seal`
/// rather than a second copy of the AEAD here, so the bytes this writes are
/// bytes the real server could have sent; the nonce is fresh, so every byte
/// after the header changes on each run.
#[test]
#[ignore = "rewrites the committed fixture; set LYNXRDP_WRITE_REPORT_FIXTURE"]
fn regenerate_the_fixture() {
    assert!(
        std::env::var_os("LYNXRDP_WRITE_REPORT_FIXTURE").is_some(),
        "refusing to rewrite the fixture without LYNXRDP_WRITE_REPORT_FIXTURE=1"
    );
    let path = fixture_path();
    let existing = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    // Keep the header, which is the whole explanation of why there is one copy
    // of this file, and replace only the data lines below it.
    let mut out = String::new();
    for line in existing.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            out.push_str(line);
            out.push('\n');
        } else {
            break;
        }
    }
    let datagram = seal::seal(fixture_report().to_json().as_bytes()).expect("sealing");
    out.push_str(&hex_lines(&datagram));
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    eprintln!("rewrote {}", path.display());
}
