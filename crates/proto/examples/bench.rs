//! Codec benchmark over synthetic but representative desktop content.
//!
//! Run with `cargo run --release -p lynxrdp-proto --example bench`.
//!
//! The scenes imitate what this protocol is actually used for — an editor
//! or terminal full of text — rather than photographic or video content,
//! and each scenario is measured twice: once with copy detection enabled
//! and once without, so the contribution of each feature is visible.
//!
//! Every scenario declares its own damage rectangles. That is not a detail:
//! a benchmark that always hands the encoder one full-screen rectangle
//! exercises none of the per-region work, which is how four milliseconds a
//! frame of scroll detection once sat here unnoticed.

use std::time::Instant;

use lynxrdp_proto::codec::{Encoder, FrameUpdate};
use lynxrdp_proto::{Framebuffer, Rect};

const W: u32 = 1920;
const H: u32 = 1080;
/// Height of one line of text, in pixels.
const LINE: u32 = 18;

/// Colours of a typical dark-on-light editor with syntax highlighting.
const BG: u32 = 0xFDFDFD;
const GUTTER: u32 = 0xF0F0F0;
const CHROME: u32 = 0x2B2B3B;
const INK: [u32; 5] = [0x202020, 0x0B5FA5, 0x8B1A1A, 0x0A7A32, 0x7A3FA5];

/// The scrolling half of a split window.
fn left_pane() -> Rect {
    Rect::new(0, 28, W / 2 - 2, H - 52)
}

/// A cursor blinking in the other half, well to the right of the scroll.
fn cursor() -> Rect {
    Rect::new(W / 2 + 220, 460, 10, 14)
}

/// Draw a screen of "code": a chrome bar, a gutter, and lines of coloured
/// word-shaped blocks. `scroll` shifts which text lands on which line.
fn editor(scroll: u32) -> Framebuffer {
    let mut fb = Framebuffer::new(W, H);
    fb.fill(&fb.bounds(), BG);
    fb.fill(&Rect::new(0, 0, W, 28), CHROME);
    fb.fill(&Rect::new(0, H - 24, W, 24), CHROME);
    fb.fill(&Rect::new(0, 28, 60, H - 52), GUTTER);

    let first = 28 + LINE;
    let mut line = 0u32;
    let mut y = first;
    while y + LINE < H - 24 {
        let n = line + scroll;
        // Indentation and a handful of "words" per line, deterministic in n.
        let indent = 70 + (n * 7 % 4) * 24;
        let words = 3 + (n * 13 % 6);
        let mut x = indent;
        for w in 0..words {
            let len = 24 + (n.wrapping_mul(31).wrapping_add(w * 17) % 9) * 8;
            let ink = INK[((n.wrapping_add(w * 3)) % INK.len() as u32) as usize];
            if x + len < W - 40 {
                // A word is a run of glyph-height marks, not a solid bar, so
                // tiles contain real edges rather than one flat rectangle.
                for gx in (x..x + len).step_by(2) {
                    fb.fill(&Rect::new(gx, y, 1, 11), ink);
                }
            }
            x += len + 12;
        }
        y += LINE;
        line += 1;
    }
    fb
}

/// A split window: the left pane scrolls, the right pane holds a second
/// file that does nothing but blink a cursor.
///
/// The two damage rectangles sit side by side, so their union spans the
/// static pane as well — every row of that box then contains pixels that did
/// not move, and detection over the union finds nothing at all.
fn split_panes(scroll: u32, tick: u32) -> Framebuffer {
    let mut fb = editor(scroll);
    let other = editor(500);
    let right = Rect::new(W / 2, 28, W / 2, H - 52);
    fb.copy_rect_from(&other, &right);
    fb.fill(&Rect::new(W / 2 - 2, 28, 2, H - 52), CHROME);
    fb.fill(&cursor(), if tick % 2 == 0 { INK[0] } else { BG });
    fb
}

struct Measurement {
    label: &'static str,
    frames: usize,
    bytes: usize,
    copies: usize,
    micros: u128,
}

fn run(
    label: &'static str,
    frames: &[Framebuffer],
    regions: &[Rect],
    allow_copy: bool,
) -> Measurement {
    let mut enc = Encoder::new(W, H);
    // Prime the encoder with the first frame; only later frames are measured.
    // A first frame is a full refresh whatever the scenario's damage is.
    enc.encode_frame(&frames[0], &[Rect::new(0, 0, W, H)], false);

    let mut bytes = 0usize;
    let mut copies = 0usize;
    let start = Instant::now();
    for f in &frames[1..] {
        let FrameUpdate { copies: c, tiles } = enc.encode_frame(f, regions, allow_copy);
        bytes += tiles.iter().map(|t| t.data.len()).sum::<usize>() + c.len() * 12;
        copies += c.len();
    }
    let micros = start.elapsed().as_micros();
    Measurement {
        label,
        frames: frames.len() - 1,
        bytes,
        copies,
        micros,
    }
}

fn report(with: &Measurement, without: &Measurement) {
    let per_frame = |r: &Measurement| r.bytes as f64 / r.frames as f64 / 1024.0;
    let ms = |r: &Measurement| r.micros as f64 / r.frames as f64 / 1000.0;
    let saving = if with.bytes > 0 {
        without.bytes as f64 / with.bytes as f64
    } else {
        0.0
    };
    println!(
        "{:<30} {:>11.1} {:>13.1} {:>7.1}x {:>7} {:>11.2} {:>8.2}",
        with.label,
        per_frame(without),
        per_frame(with),
        saving,
        with.copies,
        ms(without),
        ms(with),
    );
}

fn main() {
    let bounds = vec![Rect::new(0, 0, W, H)];
    let split = vec![left_pane(), cursor()];

    // Scrolling a file one line at a time, the commonest editor motion.
    let line_scroll: Vec<Framebuffer> = (0..25).map(editor).collect();
    // Paging through a file.
    let page_scroll: Vec<Framebuffer> = (0..10).map(|i| editor(i * 40)).collect();
    // The same scroll in one pane of a split window, with a cursor blinking
    // in the other: two damage rectangles, side by side.
    let split_scroll: Vec<Framebuffer> = (0..25).map(|i| split_panes(i, i)).collect();
    // Typing: one line changes, everything else is still.
    let typing: Vec<Framebuffer> = (0..25)
        .map(|i| {
            let mut fb = editor(0);
            let y = 28 + LINE * 12;
            fb.fill(&Rect::new(300, y, 8 * i, 11), INK[0]);
            fb
        })
        .collect();
    // Switching between two entirely different screens.
    let switching: Vec<Framebuffer> = (0..10)
        .map(|i| if i % 2 == 0 { editor(0) } else { editor(500) })
        .collect();
    // A completely still screen.
    let idle: Vec<Framebuffer> = (0..25).map(|_| editor(3)).collect();

    println!("LynxRDP codec, {W}x{H} synthetic editor content");
    println!(
        "{:<30} {:>11} {:>13} {:>8} {:>7} {:>11} {:>8}",
        "scenario", "no-copy KiB", "with-copy KiB", "saving", "copies", "ms no-copy", "ms copy"
    );
    println!("{}", "-".repeat(93));

    for (label, frames, regions) in [
        ("scroll, one line at a time", &line_scroll, &bounds),
        ("scroll, one page at a time", &page_scroll, &bounds),
        ("split panes, 2 damage rects", &split_scroll, &split),
        ("typing on one line", &typing, &bounds),
        ("switching between screens", &switching, &bounds),
        ("idle (no changes)", &idle, &bounds),
    ] {
        let with = run(label, frames, regions, true);
        let without = run(label, frames, regions, false);
        report(&with, &without);
    }

    // Show what a single full screen costs, which bounds a reconnect.
    let mut enc = Encoder::new(W, H);
    let screen = editor(0);
    let t = Instant::now();
    let full = enc.encode_frame(&screen, &[Rect::new(0, 0, W, H)], false);
    let raw = (W as usize) * (H as usize) * 3;
    println!(
        "\nfull screen: {:.0} KiB in {} tiles ({:.1} ms), vs {:.0} KiB raw RGB — {:.0}x",
        full.payload_bytes() as f64 / 1024.0,
        full.tiles.len(),
        t.elapsed().as_secs_f64() * 1000.0,
        raw as f64 / 1024.0,
        raw as f64 / full.payload_bytes().max(1) as f64,
    );
}
