//! The byte corpus that keeps [`MIN_COMPATIBLE_VERSION`] honest.
//!
//! A version floor is three lines of code and a promise: while it stays where
//! it is, a peer built from an older commit can decode what this build sends.
//! Nothing in an ordinary test suite checks that promise. Round-trip tests pass
//! whatever the encoding is, because they encode and decode with the *same*
//! code — rename a field, swap two `u16`s, widen a count to `u64`, and every
//! test in the crate still passes while every deployed peer starts reading
//! plausible nonsense. The floor would then be a claim that gets less true with
//! every commit, which is worse than no floor at all, because it is believed.
//!
//! So the price of the floor is this: the exact bytes of every message, checked
//! into the tree as hex, compared against what this build produces. A change to
//! the shape of an existing message fails here, in CI, on the pull request that
//! made it — not in a session someone is working in.
//!
//! # If this file is failing
//!
//! Read the assertion; it names the message and the byte that moved, and it
//! spells out the two things you are allowed to do about it. The short version
//! is *undo it*, or *raise the floor* — and regenerating is not on the list,
//! because [`regenerate_corpus`] refuses to rewrite an existing entry unless
//! [`MIN_COMPATIBLE_VERSION`] has actually risen.
//!
//! # What is deliberately not in here
//!
//! Real codec output. A tile's `data` is opaque bytes chosen by hand, never the
//! result of `encode_tile`, because LZ4 and Zstd are free to emit different
//! (equally valid) bytes across versions of their crates. Pinning their output
//! would turn a dependency bump into a compatibility alarm, and the compressed
//! payload is not part of the message layout anyway — the length prefix in
//! front of it is, and that is pinned.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;

use lynxrdp_proto::codec::{TileEncoding, TileUpdate};
use lynxrdp_proto::message::{button, clipboard_format, features, reject, CursorImage, Kind};
use lynxrdp_proto::{CopyRect, FileEntry, Message, Rect, TransferPurpose, MIN_COMPATIBLE_VERSION};

/// Path of the corpus, relative to the crate root.
const CORPUS: &str = "tests/corpus/messages.hex";

/// The line naming the floor these bytes are pinned at.
const FLOOR_KEY: &str = "min-compatible-version";

/// How to rebuild the file, quoted verbatim in every failure message.
const REGENERATE: &str = "cargo test -p lynxrdp-proto --test wire_corpus -- --ignored regenerate";

/// Every message, with values chosen to make a layout change visible.
///
/// Two rules for anything added here. **Give every field a distinct, non-zero
/// value**: two adjacent `u16`s that both hold zero encode identically whether
/// or not somebody swaps them, so a lazy sample is a hole in the check. And
/// **write protocol version numbers as literals**, never as
/// `PROTOCOL_VERSION` — raising the version we speak is not a change to the
/// shape of `ClientHello`, and the corpus must not cry wolf over it.
///
/// Named constants are used for the *other* wire values (`reject::VERSION`,
/// the feature and clipboard-format bits) so a sample reads as the thing it
/// means. That does **not** pin them, and it is worth being clear about why:
/// a sample encodes whatever a constant currently *is*, so it notices a
/// renumbering only when one moves a byte -- and a bitmask hides exactly that,
/// because `A | B` is the same number whichever of the two is which. The
/// literals in `wire_constants_have_not_been_renumbered` are what pin those.
fn samples() -> Vec<(&'static str, Message)> {
    vec![
        (
            "TransferOptions/replace",
            Message::TransferOptions {
                id: 23,
                replace: true,
            },
        ),
        // ---- client to server ----
        (
            "ClientHello/all-features",
            Message::ClientHello {
                version: 3,
                client_name: "lynxrdp-client/corpus".into(),
                features: features::LOCAL_CURSOR
                    | features::CLIPBOARD
                    | features::RESIZE
                    | features::CLIPBOARD_IMAGE
                    | features::FILE_TRANSFER
                    | features::CLIPBOARD_FILES,
                width: 1920,
                height: 1080,
            },
        ),
        (
            "KeyEvent/press",
            Message::KeyEvent {
                keysym: 0xff0d,
                down: true,
            },
        ),
        (
            // The `false` half matters on its own: a bool is one byte and the
            // corpus is where "which byte" stops being an implementation
            // detail.
            "KeyEvent/release",
            Message::KeyEvent {
                keysym: 0x0041,
                down: false,
            },
        ),
        (
            "PointerMotion/typical",
            Message::PointerMotion { x: 1279, y: 719 },
        ),
        (
            "PointerButton/middle-press",
            Message::PointerButton {
                button: button::MIDDLE,
                down: true,
            },
        ),
        // Negative detents pin the two's-complement encoding of `i16`, which a
        // careless change to `u16` would quietly reverse.
        ("Scroll/negative-dx", Message::Scroll { dx: -3, dy: 5 }),
        ("FrameAck/max", Message::FrameAck { frame_id: u64::MAX }),
        (
            "ResizeRequest/typical",
            Message::ResizeRequest {
                width: 1024,
                height: 768,
            },
        ),
        (
            // Multi-byte UTF-8 and an astral-plane character: the length prefix
            // counts bytes, not characters, and this is where that is stated.
            "ClipboardText/utf8",
            Message::ClipboardText {
                text: "tab\there ✂ \u{1f600}".into(),
            },
        ),
        (
            "Pong/nonce",
            Message::Pong {
                nonce: 0x0102_0304_0506_0708,
            },
        ),
        (
            "Disconnect/reason",
            Message::Disconnect {
                reason: "closed by the user".into(),
            },
        ),
        // One byte on the wire. If it ever grows a payload, that is a break.
        ("RefreshRequest/empty", Message::RefreshRequest),
        // ---- server to client ----
        (
            "ServerHello/typical",
            Message::ServerHello {
                version: 3,
                server_name: "lynxrdp-session".into(),
                features: features::LOCAL_CURSOR | features::RESIZE | features::FILE_TRANSFER,
                session_id: 0x00de_adbe_ef00_1234,
                username: "alice".into(),
                width: 1920,
                height: 1080,
            },
        ),
        (
            "Rejected/version",
            Message::Rejected {
                code: reject::VERSION,
                reason: "protocol version 2 not supported (server floor is 3)".into(),
            },
        ),
        (
            "ScreenUpdate/copies-and-tiles",
            Message::ScreenUpdate {
                frame_id: 7,
                copies: vec![
                    CopyRect {
                        src_x: 11,
                        src_y: 22,
                        dest: Rect::new(33, 44, 55, 66),
                    },
                    CopyRect {
                        src_x: 640,
                        src_y: 480,
                        dest: Rect::new(1, 2, 3, 4),
                    },
                ],
                tiles: vec![TileUpdate {
                    rect: Rect::new(128, 192, 64, 32),
                    encoding: TileEncoding::Raw,
                    data: vec![0xde, 0xad, 0xbe, 0xef],
                }],
            },
        ),
        // The empty frame is a real frame: a `ScreenUpdate` with nothing in it
        // is how a bare frame id reaches the client, so its shape is load
        // bearing too.
        (
            "ScreenUpdate/empty",
            Message::ScreenUpdate {
                frame_id: 0,
                copies: vec![],
                tiles: vec![],
            },
        ),
        (
            // Every `TileEncoding` discriminant, in one entry. Renumbering one
            // is silent everywhere else in the tree -- both ends agree, because
            // both ends are this commit -- and catastrophic against a peer that
            // is not. See the `message` module docs for why a *new* encoding
            // needs more than this test can give it.
            "ScreenUpdate/every-tile-encoding",
            Message::ScreenUpdate {
                frame_id: 1,
                copies: vec![],
                tiles: [
                    TileEncoding::Solid,
                    TileEncoding::Raw,
                    TileEncoding::Lz4,
                    TileEncoding::Zstd,
                    TileEncoding::Palette,
                    TileEncoding::PaletteLz4,
                    TileEncoding::PaletteZstd,
                ]
                .into_iter()
                .enumerate()
                .map(|(i, encoding)| TileUpdate {
                    rect: Rect::new(i as u32 * 64, 0, 64, 64),
                    encoding,
                    data: vec![0x11, 0x22, 0x33],
                })
                .collect(),
            },
        ),
        (
            "ScreenResized/typical",
            Message::ScreenResized {
                width: 2560,
                height: 1440,
            },
        ),
        (
            "CursorShape/2x2",
            Message::CursorShape {
                cursor: CursorImage {
                    width: 2,
                    height: 2,
                    hot_x: 1,
                    hot_y: 0,
                    argb: vec![0xff00_0000, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff],
                },
            },
        ),
        (
            // Width zero means "hidden", so the pixel count is zero and the
            // decoder's `n == width * height` check has to accept it.
            "CursorShape/hidden",
            Message::CursorShape {
                cursor: CursorImage {
                    width: 0,
                    height: 0,
                    hot_x: 0,
                    hot_y: 0,
                    argb: vec![],
                },
            },
        ),
        (
            // `CursorShape/2x2` pins the ARGB ordering but not the four
            // dimension fields, because two of them hold the same 2 and a
            // third holds 0: an encoder that swapped `width` with `height`,
            // or `width` with `hot_y`, produces byte-identical output for it.
            // That is the sample rule at the top of this function being broken
            // by its own first cursor entry, so here is one where all four are
            // distinct and non-zero. Added rather than fixing the 2x2 entry in
            // place: the regenerator rightly refuses to rewrite or rename an
            // existing pin while the floor stands, and a new entry costs
            // nothing to anybody.
            "CursorShape/4x3-offset-hotspot",
            Message::CursorShape {
                cursor: CursorImage {
                    width: 4,
                    height: 3,
                    hot_x: 1,
                    hot_y: 2,
                    // Distinct so the count and the order stay pinned; the
                    // channel layout is the 2x2 entry's job, not this one's.
                    argb: (1_u32..=12)
                        .map(|i| 0xff00_0000 | (i * 0x0001_0101))
                        .collect(),
                },
            },
        ),
        (
            "CursorPosition/typical",
            Message::CursorPosition { x: 42, y: 4242 },
        ),
        (
            "Ping/nonce",
            Message::Ping {
                nonce: 0x0807_0605_0403_0201,
            },
        ),
        (
            "Notice/text",
            Message::Notice {
                text: "session resumed".into(),
            },
        ),
        // ---- transfers, either direction ----
        // One entry per `TransferPurpose`, for the same reason as the tile
        // encodings: the discriminants are wire values.
        (
            "TransferOffer/clipboard-image",
            Message::TransferOffer {
                id: 0x1122_3344_5566_7788,
                purpose: TransferPurpose::ClipboardImage,
                name: "clipboard.png".into(),
                size: 65_537,
            },
        ),
        (
            "TransferOffer/file-upload",
            Message::TransferOffer {
                id: 2,
                purpose: TransferPurpose::FileUpload,
                name: "/home/alice/notes.md".into(),
                size: 4096,
            },
        ),
        (
            "TransferOffer/file-download",
            Message::TransferOffer {
                id: 3,
                purpose: TransferPurpose::FileDownload,
                name: "report.pdf".into(),
                size: 1_073_741_825,
            },
        ),
        (
            "TransferAccept/accepted",
            Message::TransferAccept {
                id: 4,
                accepted: true,
                reason: String::new(),
            },
        ),
        (
            "TransferAccept/declined",
            Message::TransferAccept {
                id: 5,
                accepted: false,
                reason: "larger than the receiver will hold".into(),
            },
        ),
        (
            "TransferData/chunk",
            Message::TransferData {
                id: 6,
                seq: 0x0a0b_0c0d,
                data: vec![0, 1, 2, 253, 254, 255],
            },
        ),
        (
            "TransferAck/chunk",
            Message::TransferAck {
                id: 7,
                seq: 0x0a0b_0c0d,
            },
        ),
        (
            "TransferEnd/ok",
            Message::TransferEnd {
                id: 8,
                ok: true,
                message: String::new(),
            },
        ),
        (
            "TransferEnd/failed",
            Message::TransferEnd {
                id: 9,
                ok: false,
                message: "truncated: 12 of 34 bytes".into(),
            },
        ),
        (
            "FileRequest/path",
            Message::FileRequest {
                id: 10,
                path: "/home/alice/photos/holiday.jpg".into(),
            },
        ),
        (
            "FileList/two-entries",
            Message::FileList {
                id: 11,
                files: vec![
                    FileEntry {
                        path: "a.txt".into(),
                        size: 12,
                    },
                    FileEntry {
                        path: "sub/b.bin".into(),
                        size: 0x0011_2233_4455_6677,
                    },
                ],
            },
        ),
        (
            "FileList/empty",
            Message::FileList {
                id: 12,
                files: vec![],
            },
        ),
        (
            "ClipboardOffer/all-formats",
            Message::ClipboardOffer {
                formats: clipboard_format::TEXT | clipboard_format::PNG | clipboard_format::FILES,
            },
        ),
        (
            "ClipboardRequest/png",
            Message::ClipboardRequest {
                format: clipboard_format::PNG,
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// corpus file
// ---------------------------------------------------------------------------

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String is infallible; the Result exists for other sinks.
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn unhex(s: &str, name: &str) -> Vec<u8> {
    assert!(
        s.len() % 2 == 0 && s.bytes().all(|c| c.is_ascii_hexdigit()),
        "{CORPUS}: entry `{name}` is not an even-length run of hex digits"
    );
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("checked above"))
        .collect()
}

/// The floor the file is pinned at, and its entries in file order.
struct Corpus {
    floor: u16,
    entries: Vec<(String, String)>,
}

impl Corpus {
    fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, h)| h.as_str())
    }
}

fn parse(text: &str) -> Corpus {
    let mut floor = None;
    let mut entries: Vec<(String, String)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("{CORPUS}:{}: expected `name = hex`", n + 1));
        let (key, value) = (key.trim(), value.trim());
        if key == FLOOR_KEY {
            floor = Some(
                value
                    .parse()
                    .unwrap_or_else(|_| panic!("{CORPUS}:{}: `{FLOOR_KEY}` is not a u16", n + 1)),
            );
            continue;
        }
        assert!(
            !entries.iter().any(|(existing, _)| existing == key),
            "{CORPUS}:{}: duplicate entry `{key}`",
            n + 1
        );
        entries.push((key.to_string(), value.to_string()));
    }
    Corpus {
        floor: floor.unwrap_or_else(|| panic!("{CORPUS}: no `{FLOOR_KEY}` line")),
        entries,
    }
}

fn load() -> Corpus {
    let path = corpus_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\n\nIf the corpus is genuinely missing, create it with:\n    {REGENERATE}",
            path.display()
        )
    });
    parse(&text)
}

fn render(floor: u16, entries: &[(String, String)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\
# LynxRDP wire corpus. Generated; see crates/proto/tests/wire_corpus.rs.
#
# The exact bytes of every protocol message, as this build encodes them. These
# are what a peer compiled from an older commit will be handed, and the floor
# below is the promise that such a peer can still read them. An entry that
# changes without that number changing is a compatibility break, so the test
# fails and regeneration refuses.
#
# One entry per line: NAME = lowercase hex of `Message::encode`, no framing
# (the 4-byte little-endian length prefix belongs to `frame.rs`, which pins it
# with its own tests).
#
# Regenerate with:
#     {REGENERATE}

{FLOOR_KEY} = {floor}

"
    ));
    let width = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, hexed) in entries {
        out.push_str(&format!("{name:width$} = {hexed}\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// What to do about a mismatch, appended to every failure. The point of the
/// corpus is that the next person does not have to work this out.
fn what_to_do() -> String {
    format!(
        "\
The corpus pins the encoding of every message at
MIN_COMPATIBLE_VERSION = {MIN_COMPATIBLE_VERSION}, which is this build's promise to keep
talking to peers compiled from an older commit. Such a peer decodes these bytes
with the layout it was built with, so a field that moved, changed width or
changed meaning is not an error to it -- it is a plausible wrong value it will
act on, in a session someone is working in.

Do exactly one of:

  1. Undo the change to the message. This is almost always the answer. A new
     field can go in a *new* message instead: a tag at or above
     frame::EXTENSION_TAG_MIN is discarded whole by peers that do not know it,
     and a new structural message can be gated on a bit in message::features.

  2. If the shape genuinely must change, accept that it is a compatibility
     break: raise PROTOCOL_VERSION *and* MIN_COMPATIBLE_VERSION together in
     crates/proto/src/lib.rs -- old peers are then refused at the handshake
     with a reason, which is the outcome the floor exists to produce -- and
     only then regenerate:

         {REGENERATE}

Regeneration refuses to rewrite an entry while the floor is unchanged, so it
cannot be used to make this failure go away on its own."
    )
}

fn diff_view(want: &[u8], got: &[u8], at: usize) -> String {
    let start = at.saturating_sub(12);
    let end = at + 12;
    let window = |v: &[u8]| hex(&v[start.min(v.len())..end.min(v.len())]);
    let (lead, pad) = if start > 0 { ("...", "   ") } else { ("", "") };
    let caret = format!("{}^^", " ".repeat((at - start) * 2));
    format!(
        "  corpus:     {lead}{}\n  this build: {lead}{}\n              {pad}{caret}",
        window(want),
        window(got)
    )
}

/// Every message still encodes to exactly the bytes on disk, and those bytes
/// still decode to exactly the message.
///
/// Both directions, because they fail differently: a changed encoder breaks the
/// peer reading us, a changed decoder breaks us reading the peer, and either
/// one alone is enough to corrupt a session.
#[test]
fn wire_encoding_has_not_changed() {
    let corpus = load();
    let mut problems: Vec<String> = Vec::new();

    for (name, msg) in samples() {
        let encoded = msg.encode();
        let Some(stored_hex) = corpus.get(name) else {
            problems.push(format!(
                "`{name}` is missing from the corpus.\n\
                 \n  this build: {}\n\n\
                 A message with no pinned bytes is unprotected. If this entry is new, \
                 adding it is always safe -- run:\n    {REGENERATE}",
                hex(&encoded)
            ));
            continue;
        };
        let stored = unhex(stored_hex, name);
        if stored != encoded {
            let at = stored
                .iter()
                .zip(&encoded)
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| stored.len().min(encoded.len()));
            problems.push(format!(
                "the wire encoding of `{name}` changed.\n\n\
                 {}\n\n  first difference at byte {at} ({} bytes on disk, {} now)\n\n{}",
                diff_view(&stored, &encoded, at),
                stored.len(),
                encoded.len(),
                what_to_do()
            ));
            continue;
        }
        match Message::decode(&stored) {
            Ok(back) if back == msg => {}
            Ok(back) => problems.push(format!(
                "`{name}` still encodes correctly but decodes to a different message.\n\
                 \n  expected: {msg:?}\n  got:      {back:?}\n\n{}",
                what_to_do()
            )),
            Err(e) => problems.push(format!(
                "`{name}` no longer decodes: {e}\n\n\
                 The bytes are unchanged, so it is the decoder that moved -- a peer \
                 sending exactly what this build sends can no longer be understood.\n\n{}",
                what_to_do()
            )),
        }
    }

    assert!(
        problems.is_empty(),
        "\n\n{}\n",
        problems.join("\n\n----------------------------------------------------------\n\n")
    );
}

/// A message with no corpus entry is a message with no compatibility check, so
/// the set of tags is derived from `Kind::from_u8` rather than listed here: add
/// a message and this fails until it is pinned.
#[test]
fn every_message_kind_is_pinned() {
    let sampled: BTreeSet<u8> = samples().iter().map(|(_, m)| m.kind() as u8).collect();
    let missing: Vec<u8> = (0..=u8::MAX)
        .filter(|&tag| Kind::from_u8(tag).is_ok() && !sampled.contains(&tag))
        .collect();
    assert!(
        missing.is_empty(),
        "message tags {missing:?} are decodable but have no corpus sample.\n\n\
         Add one to `samples()` in {file} -- values distinct and non-zero, so a \
         reordering of the fields actually moves a byte -- and then run:\n    {REGENERATE}",
        file = file!(),
    );
}

/// The wire values that live in no message body, and that no sample can pin.
///
/// The corpus catches a field that moved. It is blind to a *constant* that
/// changed number whenever the sample carrying it is a bitmask, because
/// `A | B` encodes identically whichever of the two is which:
/// `ClientHello/all-features` ORs all six feature bits into `0x3f` and
/// `ClipboardOffer/all-formats` ORs all three formats into `0x07`, so any
/// permutation inside either set is invisible there. Measured, not assumed --
/// swapping `features::CLIPBOARD` with `features::CLIPBOARD_IMAGE`, which is
/// an old client offering clipboard *text* being read by a new server as an
/// offer of *images*, leaves every other test in this file green.
///
/// So they are written out as literals. This is the hand-written list that
/// `every_message_kind_is_pinned` goes out of its way to avoid, and it has to
/// be: a `pub const` cannot be enumerated the way a tag can, and there is no
/// point at which the compiler will notice a new one is missing from here. The
/// enums get the better treatment, below.
#[test]
fn wire_constants_have_not_been_renumbered() {
    // `assert_eq!` alone would report `left: 8, right: 2` and leave the reader
    // to work out that those are feature bits and why it matters. The line
    // number says which constant; this says what it costs.
    const RENUMBERED: &str = "\
This constant is a value on the wire, not an internal identifier. A peer built \
from an older commit keeps sending and expecting the old number, and neither \
end can tell: the message parses, the field is in range, and the meaning is \
simply wrong. Nothing else in the corpus catches it, because a sample encodes \
whatever the constant currently is -- and a bitmask sample cannot even see a \
permutation, since `A | B` is one number.

Put the value back. A new button, code, feature bit or clipboard format takes \
the next unused number; the existing ones are spent.";

    // X11 button numbering, handed to XTEST unchanged. Two of these swapped is
    // a peer one commit out of step opening context menus on left-click, with
    // nothing in any log to say so.
    assert_eq!(button::LEFT, 1_u8, "{RENUMBERED}");
    assert_eq!(button::MIDDLE, 2_u8, "{RENUMBERED}");
    assert_eq!(button::RIGHT, 3_u8, "{RENUMBERED}");
    assert_eq!(button::BACK, 8_u8, "{RENUMBERED}");
    assert_eq!(button::FORWARD, 9_u8, "{RENUMBERED}");

    // The code is the whole of what a refused client can act on; the reason
    // beside it is free text an old client may not even display.
    assert_eq!(reject::VERSION, 1_u16, "{RENUMBERED}");
    assert_eq!(reject::UNAUTHORIZED, 2_u16, "{RENUMBERED}");
    assert_eq!(reject::SESSION_FAILED, 3_u16, "{RENUMBERED}");
    assert_eq!(reject::UNAVAILABLE, 4_u16, "{RENUMBERED}");

    // Feature bits are the escape hatch the `message` module docs point at for
    // adding things without moving the floor. That only works while a bit
    // means the same thing at both ends forever, which is this assertion.
    assert_eq!(features::LOCAL_CURSOR, 0x01_u32, "{RENUMBERED}");
    assert_eq!(features::CLIPBOARD, 0x02_u32, "{RENUMBERED}");
    assert_eq!(features::RESIZE, 0x04_u32, "{RENUMBERED}");
    assert_eq!(features::CLIPBOARD_IMAGE, 0x08_u32, "{RENUMBERED}");
    assert_eq!(features::FILE_TRANSFER, 0x10_u32, "{RENUMBERED}");
    assert_eq!(features::CLIPBOARD_FILES, 0x20_u32, "{RENUMBERED}");

    assert_eq!(clipboard_format::TEXT, 0x01_u32, "{RENUMBERED}");
    assert_eq!(clipboard_format::PNG, 0x02_u32, "{RENUMBERED}");
    assert_eq!(clipboard_format::FILES, 0x04_u32, "{RENUMBERED}");
}

/// A tile encoding with no corpus sample is one nobody was made to think about.
///
/// Derived from `TileEncoding::from_u8` for the same reason the message tags
/// are, but this one matters more: the tile tag is the single place the version
/// floor cannot help, and the `Versioning` section of the `message` module docs
/// exists to explain what to do instead. Failing here is how that section gets
/// read *before* the encoding ships, rather than after a client three commits
/// old drops its connection in the middle of somebody's session.
///
/// Renumbering an existing encoding is already caught, by the bytes of
/// `ScreenUpdate/every-tile-encoding`. Adding one is what this catches.
#[test]
fn every_tile_encoding_is_pinned() {
    let pinned: BTreeSet<u8> = samples()
        .iter()
        .filter_map(|(_, m)| match m {
            Message::ScreenUpdate { tiles, .. } => Some(tiles),
            _ => None,
        })
        .flatten()
        .map(|t| t.encoding as u8)
        .collect();
    let missing: Vec<u8> = (0..=u8::MAX)
        .filter(|&tag| TileEncoding::from_u8(tag).is_ok() && !pinned.contains(&tag))
        .collect();
    assert!(
        missing.is_empty(),
        "tile encodings {missing:?} decode but appear in no corpus sample.\n\n\
         Add them to `ScreenUpdate/every-tile-encoding` in {file}, then run:\n    \
         {REGENERATE}\n\n\
         Adding the sample is the easy half. Read the `Versioning` section of the \
         `message` module docs before shipping the encoding itself: a tile tag is \
         nested inside a ScreenUpdate, so an older client can neither skip it nor \
         ever repaint the rectangle it lost. Emit a new encoding only to clients \
         that advertised a message::features bit for it, or raise PROTOCOL_VERSION \
         and MIN_COMPATIBLE_VERSION together and accept losing those clients.",
        file = file!(),
    );
}

/// One layer further down, and for the same reason: a `TransferPurpose` the
/// receiver does not know is a `TransferOffer` it can only refuse.
///
/// Unlike a tile encoding that is survivable -- refusing an offer is a defined
/// outcome with a reason string -- so a new purpose needs a sample here and a
/// thought about what an older peer does with it, not a version bump.
#[test]
fn every_transfer_purpose_is_pinned() {
    let pinned: BTreeSet<u8> = samples()
        .iter()
        .filter_map(|(_, m)| match m {
            Message::TransferOffer { purpose, .. } => Some(*purpose as u8),
            _ => None,
        })
        .collect();
    let missing: Vec<u8> = (0..=u8::MAX)
        .filter(|&tag| TransferPurpose::from_u8(tag).is_some() && !pinned.contains(&tag))
        .collect();
    assert!(
        missing.is_empty(),
        "transfer purposes {missing:?} decode but appear in no corpus sample.\n\n\
         Add a `TransferOffer` sample for each to `samples()` in {file}, then \
         run:\n    {REGENERATE}",
        file = file!(),
    );
}

/// The floor recorded in the file must be the floor the code claims.
///
/// This is the hinge of the whole arrangement. Raising `MIN_COMPATIBLE_VERSION`
/// without regenerating fails here; regenerating is what unlocks rewriting the
/// entries; and so the only way to change a message's shape is to have already
/// given up the old peers, on purpose, in a diff someone reviewed.
#[test]
fn corpus_is_pinned_at_the_current_floor() {
    let corpus = load();
    assert_eq!(
        corpus.floor, MIN_COMPATIBLE_VERSION,
        "{CORPUS} says `{FLOOR_KEY} = {}` but crates/proto/src/lib.rs says \
         MIN_COMPATIBLE_VERSION = {MIN_COMPATIBLE_VERSION}.\n\n\
         If the floor was just raised, that is the moment the old encodings stop \
         being a promise: regenerate the corpus, review the resulting diff as the \
         compatibility break it is, and commit both together.\n    {REGENERATE}",
        corpus.floor,
    );
}

/// A message removed from `samples()` but left in the file would quietly stop
/// being checked, and one removed from the protocol is itself a break.
#[test]
fn corpus_has_no_stale_entries() {
    let corpus = load();
    let names: BTreeSet<&str> = samples().iter().map(|(n, _)| *n).collect();
    let stale: Vec<&str> = corpus
        .entries
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !names.contains(n))
        .collect();
    assert!(
        stale.is_empty(),
        "{CORPUS} pins entries that `samples()` no longer produces: {stale:?}\n\n\
         Either the sample was renamed -- rename it back, or regenerate after \
         raising the floor -- or a message was removed from the protocol, which is \
         a compatibility break in its own right.\n\n{}",
        what_to_do()
    );
}

/// Rewrite the corpus from the current encoders.
///
/// Ignored by default: it is a tool, not a check. It will happily *add* new
/// entries, because a message an old peer has never seen cannot break it. It
/// refuses to change or drop an existing one unless `MIN_COMPATIBLE_VERSION`
/// has risen above the floor recorded in the file -- otherwise "just regenerate
/// it" becomes the path of least resistance and the corpus stops meaning
/// anything.
#[test]
#[ignore = "regenerates the checked-in corpus; run deliberately"]
fn regenerate_corpus() {
    let path = corpus_path();
    let existing = std::fs::read_to_string(&path).ok().map(|t| parse(&t));
    let old_floor = existing
        .as_ref()
        .map_or(MIN_COMPATIBLE_VERSION, |c| c.floor);
    let floor_has_risen = old_floor < MIN_COMPATIBLE_VERSION;

    let entries: Vec<(String, String)> = samples()
        .into_iter()
        .map(|(name, msg)| (name.to_string(), hex(&msg.encode())))
        .collect();

    // Once the floor has moved, the old encodings are no longer promised to
    // anybody and rewriting them is the whole point of the exercise.
    let still_promised = if floor_has_risen {
        None
    } else {
        existing.as_ref()
    };
    if let Some(old) = still_promised {
        let mut refused: Vec<String> = Vec::new();
        for (name, hexed) in &entries {
            if old.get(name).is_some_and(|prev| prev != hexed) {
                refused.push(format!("  {name}: encoding changed"));
            }
        }
        for (name, _) in &old.entries {
            if !entries.iter().any(|(n, _)| n == name) {
                refused.push(format!("  {name}: no longer produced"));
            }
        }
        assert!(
            refused.is_empty(),
            "refusing to regenerate: the floor is still {MIN_COMPATIBLE_VERSION}, but \
             these entries would change:\n\n{}\n\n{}",
            refused.join("\n"),
            what_to_do()
        );
    }

    std::fs::create_dir_all(path.parent().expect("corpus lives in a directory"))
        .expect("creating the corpus directory");
    std::fs::write(&path, render(MIN_COMPATIBLE_VERSION, &entries)).expect("writing the corpus");
    println!(
        "wrote {} entries to {}{}",
        entries.len(),
        path.display(),
        if floor_has_risen {
            format!(" (floor {old_floor} -> {MIN_COMPATIBLE_VERSION})")
        } else {
            String::new()
        }
    );
}
