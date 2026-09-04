//! The session window's connection bar.
//!
//! The session window has no widget toolkit: it owns a `softbuffer` buffer and
//! blits the remote framebuffer into it 1:1. This module draws a bar into that
//! *presented* buffer, after the blit and before the local cursor.
//!
//! Compositing into the presented buffer rather than into the decoded
//! framebuffer is the whole correctness story here. The server sends
//! incremental frames that diff against the pixels it believes we hold; a bar
//! painted into `Client::framebuffer()` would make every later frame diff
//! against pixels the server never sent, and the difference would never be
//! repaired -- a permanent smear across the top of the screen. Nothing in this
//! module is given a `&mut Framebuffer`, so that mistake cannot be made by
//! accident, and `App::redraw` re-blits the whole window every frame, which is
//! also why hiding the bar needs no save/restore: the next blit simply
//! overwrites it.
//!
//! Two consequences follow from having no toolkit. There is no focus model, so
//! instead of faking a Tab ring every action carries a global accelerator and
//! the bar prints it next to the label -- that printing *is* the keyboard
//! story. And there is no font stack, so text is drawn from the 5x9 bitmap
//! table below, at an integer scale, with no anti-aliasing; for the same reason
//! nothing here is rounded, because a hand-drawn 6 px corner without
//! anti-aliasing is four visible steps.
//!
//! The bar shows only what the client actually knows: the identity from the
//! handshake, the remote size, the last real round-trip sample, and real upload
//! progress. No bandwidth figure (`Client::bytes_received` is a cumulative
//! counter, not a rate), no frame rate, no "connection quality".

use std::time::{Duration, Instant};

use lynxrdp_proto::Rect;

/// Colours, as the `0x00RRGGBB` words `softbuffer` writes.
///
/// These take the dark palette deliberately, whatever theme the launcher is
/// showing: what sits behind this bar is an arbitrary remote screen, not one
/// of our own surfaces, so the bar has to be legible over a white document
/// and a black terminal rather than agree with a window we are not drawing.
///
/// Everything semantic comes from [`crate::theme::DARK`] so the bar cannot
/// drift from the product's colours. Three values do not: the scrim and the
/// two text greys are a step brighter than the launcher's, because they are
/// measured against the scrim composited over an *unknown* screen rather than
/// against a known surface, and the worst case there is lighter than any
/// panel the launcher paints.
pub mod colour {
    use crate::theme::{packed, DARK};

    /// Bar background, composited at [`SCRIM_ALPHA`].
    ///
    /// A shade off `surface_sunken`: the extra blue keeps it from tinting
    /// warm when it lands on a white page.
    pub const SCRIM: u32 = 0x0E_1114;
    /// How opaque the scrim is over the remote screen.
    ///
    /// The worst case is a maximised white document, where the bar composites
    /// to `#26282B`; every foreground below still clears 5:1 against that,
    /// and 12.5:1 for the text.
    pub const SCRIM_ALPHA: u32 = 230;
    /// Machine strings: host, sizes, numbers.
    pub const TEXT: u32 = 0xE8_EDEF;
    /// Labels and units.
    pub const DIM: u32 = 0xA7_B3B8;
    /// The link answered a ping recently.
    pub const OK: u32 = packed(DARK.ok);
    /// The link has gone quiet.
    pub const WARN: u32 = packed(DARK.warn);
    /// The Disconnect label.
    pub const DANGER: u32 = packed(DARK.danger);
    /// Hover outline.
    pub const ACCENT: u32 = packed(DARK.accent_bright);
    /// The hairline under the bar, at [`HAIRLINE_ALPHA`].
    pub const HAIRLINE: u32 = 0xFF_FFFF;
    /// 14%: enough to separate the bar from a pale screen, not enough to
    /// read as a border on a dark one.
    pub const HAIRLINE_ALPHA: u32 = 36;

    /// Buttons darken under the pointer; nothing here ever lightens.
    ///
    /// Lightening was tried first and is wrong: white at 18% over the scrim
    /// drops the Disconnect label to 2.89:1 when the remote screen is white,
    /// which is exactly when the bar is hardest to read already. Darkening
    /// lifts the same label to 5.78:1.
    pub const PRESS: u32 = 0x00_0000;
    /// 22%.
    pub const HOVER_ALPHA: u32 = 56;
    /// 34%.
    pub const PRESSED_ALPHA: u32 = 87;
}

/// How long the pointer must stay in the hot zone before the bar comes up.
///
/// The top edge of the remote screen is somewhere users go on purpose --
/// a panel, a menu bar, a window title bar dragged up to maximise -- and a bar
/// that appeared the moment the pointer crossed the strip covered the thing
/// they were reaching for. The dwell is what separates "I am using the top of
/// my desktop" from "I want the connection bar": the first is a pass through
/// the strip, the second is a pause in it.
///
/// Presence, not stillness: the timer runs while the pointer is anywhere in
/// the strip and is reset only by leaving it. Requiring the pointer to hold
/// still would make the bar hard to summon with a real mouse, which jitters.
pub const REVEAL_DELAY: Duration = Duration::from_millis(600);
/// How long the bar stays up after the pointer leaves it.
pub const HIDE_DELAY: Duration = Duration::from_millis(700);
/// How long the bar shows itself on connect and on every state change, so it
/// is discoverable without documentation.
pub const FLASH: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------- geometry

/// Integer pixel scale for a window scale factor.
///
/// One integer drives every dimension, and it is always even so that the
/// half-unit offsets in the layout (the 3.5 unit text top) stay on whole
/// pixels. A fractional scale factor is rounded: this font has no fractional
/// positioning to spend a fractional scale on.
pub fn pixel_scale(window_scale: f64) -> u32 {
    let rounded = if window_scale.is_finite() {
        window_scale.round().clamp(1.0, 3.0) as u32
    } else {
        1
    };
    2 * rounded
}

/// Height of the bar in pixels.
pub fn bar_height(s: u32) -> u32 {
    16 * s
}

/// Height of the strip along the top edge that brings the bar up.
pub fn hot_zone_height(s: u32) -> u32 {
    4 * s
}

/// Width of `n` glyphs, without the trailing inter-glyph gap.
fn text_width(n: usize, s: u32) -> u32 {
    if n == 0 {
        0
    } else {
        n as u32 * 6 * s - s
    }
}

/// How many glyphs fit in `w` pixels.
fn fits(w: u32, s: u32) -> usize {
    ((w + s) / (6 * s)) as usize
}

// ------------------------------------------------------------------ status

/// Everything the bar is allowed to say, gathered once per frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// `user@host` from the handshake, already reduced to ASCII.
    pub who: String,
    /// Remote screen width.
    pub width: u32,
    /// Remote screen height.
    pub height: u32,
    /// The last measured round trip, or `None` before the first pong.
    pub rtt: Option<Duration>,
    /// How long the link has been quiet, when that is longer than the stall
    /// threshold. `Some` replaces the round-trip field.
    pub stalled: Option<Duration>,
    /// Uploads in flight: (files, percent complete).
    pub uploads: Option<(usize, u64)>,
}

impl Status {
    /// Build a status line, folding anything non-ASCII in `who` to `?`.
    ///
    /// The font is ASCII, so an internationalised hostname or a non-ASCII
    /// username cannot be drawn here. Nothing is lost: the window title holds
    /// the real value and is drawn with a real OS font.
    pub fn new(who: &str, size: (u32, u32), rtt: Option<Duration>) -> Self {
        Self {
            who: who
                .chars()
                .map(|c| {
                    if c.is_ascii_graphic() || c == ' ' {
                        c
                    } else {
                        '?'
                    }
                })
                .collect(),
            width: size.0,
            height: size.1,
            rtt,
            stalled: None,
            uploads: None,
        }
    }

    /// The fields right of the state dot, most important first.
    ///
    /// Truncation drops them from the end, so the order here is also the order
    /// in which they are given up: uploads, then the link figure, then the
    /// remote size, and only then is `user@host` itself shortened.
    fn fields(&self) -> Vec<Vec<Span>> {
        let mut out = vec![vec![Span::new(&self.who, colour::TEXT)]];
        out.push(vec![Span::new(
            &format!("{}x{}", self.width, self.height),
            colour::DIM,
        )]);
        if let Some(d) = self.stalled {
            // Elapsed time since the last proof the link was alive, measured
            // rather than guessed, and named rather than left to the dot's
            // colour alone.
            out.push(vec![Span::new(
                &format!("stalled {} s", d.as_secs()),
                colour::WARN,
            )]);
        } else {
            let value = match self.rtt {
                // Never averaged, never smoothed: this is one sample, and it
                // can be a whole ping interval old.
                Some(rtt) => format!("{:.0} ms", rtt.as_secs_f64() * 1000.0),
                None => "--".to_string(),
            };
            out.push(vec![
                Span::new("rtt ", colour::DIM),
                Span::new(&value, colour::TEXT),
            ]);
        }
        if let Some((n, pct)) = self.uploads {
            let unit = if n == 1 { "file" } else { "files" };
            out.push(vec![
                Span::new("up ", colour::DIM),
                Span::new(&format!("{n} {unit} {pct}%"), colour::TEXT),
            ]);
        }
        out
    }
}

// ------------------------------------------------------------------ layout

/// What a bar button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Toggle the window's fullscreen state.
    Fullscreen,
    /// Send Ctrl+Alt+Del into the session.
    SecureAttention,
    /// End the session.
    Disconnect,
}

/// The buttons, left to right; `Disconnect` is laid out hard against the
/// right edge and the others fill leftwards from it.
const BUTTONS: [(Action, &str, &str, u32); 3] = [
    (Action::Fullscreen, "Fullscreen", "C-A-Enter", colour::TEXT),
    (
        Action::SecureAttention,
        "Ctrl+Alt+Del",
        "C-A-End",
        colour::TEXT,
    ),
    (Action::Disconnect, "Disconnect", "C-A-Q", colour::DANGER),
];

/// A run of text in one colour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Left edge, in window pixels.
    pub x: u32,
    /// The characters to draw.
    pub text: String,
    /// `0x00RRGGBB`.
    pub colour: u32,
}

impl Span {
    fn new(text: &str, colour: u32) -> Self {
        Self {
            x: 0,
            text: text.to_string(),
            colour,
        }
    }

    fn width(&self, s: u32) -> u32 {
        text_width(self.text.chars().count(), s)
    }
}

/// A laid-out button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    /// What pressing it does.
    pub action: Action,
    /// Where it is.
    pub rect: Rect,
    /// The label.
    pub label: &'static str,
    /// The accelerator, printed after the label when there is room.
    pub shortcut: &'static str,
    /// Whether the accelerator is being printed.
    pub shortcut_shown: bool,
    /// Label colour.
    pub colour: u32,
}

/// One frame's worth of bar geometry. Pure: no window, no clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The whole bar.
    pub bar: Rect,
    /// The state square.
    pub dot: Rect,
    /// `OK` or `WARN`.
    pub dot_colour: u32,
    /// Text runs, already positioned.
    pub spans: Vec<Span>,
    /// Buttons, in `BUTTONS` order (leftmost first).
    pub buttons: Vec<Button>,
    /// The scale everything was laid out at.
    pub s: u32,
    /// Top of every glyph box.
    pub text_y: u32,
}

impl Layout {
    /// Index of the button under a window point, if any.
    pub fn button_at(&self, x: u32, y: u32) -> Option<usize> {
        self.buttons.iter().position(|b| {
            x >= b.rect.x && x < b.rect.right() && y >= b.rect.y && y < b.rect.bottom()
        })
    }

    /// Right edge of the text run, for tests and for asserting no overlap.
    pub fn text_right(&self) -> u32 {
        self.spans
            .last()
            .map(|sp| sp.x + sp.width(self.s))
            .unwrap_or(0)
    }
}

fn button_width(label: &str, shortcut: &str, with_shortcut: bool, s: u32) -> u32 {
    let mut w = 3 * s + text_width(label.chars().count(), s) + 3 * s;
    if with_shortcut {
        w += 6 * s + text_width(shortcut.chars().count(), s);
    }
    w
}

/// Lay the bar out for a window `width` pixels wide at scale `s`.
///
/// Two passes, because a single greedy one gives up too much: it drops a field
/// to make room for a button and then cannot take the field back when a later
/// concession frees the space up again.
///
/// The first pass decides how much of the button row survives, charging the
/// text at what it would ideally like. What it gives up, in order:
///
/// 1. the upload figure -- transient, and the window title carries it too;
/// 2. the printed accelerators -- an affordance, not information, and the keys
///    keep working unprinted;
/// 3. the round-trip (or stall) figure, then the remote size;
/// 4. whole buttons, from the left: Fullscreen first, because every window
///    manager has its own, then Ctrl+Alt+Del, and Disconnect last.
///
/// `user@host` is never shortened to make room for a button. It is the one
/// thing on the bar that stops a user typing into the wrong machine, and it is
/// the only thing here with no keyboard equivalent; every button has a global
/// accelerator whether or not it is drawn. It is shortened only when no button
/// is left and it still does not fit.
///
/// The second pass then fits as many fields as actually go in the space the
/// buttons left, dropping from the right in the same order.
pub fn bar_layout(width: u32, s: u32, st: &Status) -> Layout {
    let h = bar_height(s);
    let pad = 4 * s;
    let gap = 2 * s;
    let sep = 18 * s;
    let text_y = 7 * s / 2;
    let dot = Rect::new(pad, 5 * s, 6 * s, 6 * s);
    let text_x = pad + 6 * s + 3 * s;

    let block = |fields: &[Vec<Span>]| -> u32 {
        let chars: usize = fields
            .iter()
            .map(|f| f.iter().map(|sp| sp.text.chars().count()).sum::<usize>())
            .sum();
        text_width(chars, s) + sep * fields.len().saturating_sub(1) as u32
    };
    let buttons_width = |first: usize, with_shortcut: bool| -> u32 {
        let shown = &BUTTONS[first.min(BUTTONS.len())..];
        if shown.is_empty() {
            return 0;
        }
        shown
            .iter()
            .map(|(_, l, sc, _)| button_width(l, sc, with_shortcut, s))
            .sum::<u32>()
            + gap * (shown.len() - 1) as u32
    };

    // Pass one: concede until the ideal text and the button row both fit.
    let mut wanted = st.fields();
    let mut with_shortcut = true;
    let mut first = 0usize;
    for step in 0..=6 {
        let btns = buttons_width(first, with_shortcut);
        let need = text_x + block(&wanted) + pad + if btns == 0 { 0 } else { btns + pad };
        if need <= width {
            break;
        }
        match step {
            0 => {
                if wanted.len() > 3 {
                    wanted.pop();
                }
            }
            1 => with_shortcut = false,
            2 | 3 => {
                if wanted.len() > 1 {
                    wanted.pop();
                }
            }
            _ => first += 1,
        }
    }
    first = first.min(BUTTONS.len());

    let mut buttons = Vec::new();
    let mut right = width.saturating_sub(pad);
    for (action, label, shortcut, colour) in BUTTONS[first..].iter().rev() {
        let w = button_width(label, shortcut, with_shortcut, s);
        let x = right.saturating_sub(w);
        buttons.push(Button {
            action: *action,
            rect: Rect::new(x, 2 * s, w, 12 * s),
            label,
            shortcut,
            shortcut_shown: with_shortcut,
            colour: *colour,
        });
        right = x.saturating_sub(gap);
    }
    buttons.reverse();

    // Pass two: the fields take what the buttons actually left, keeping one
    // pad between the text and whatever bounds it. With no buttons that bound
    // is the window edge, which is why `limit` is the full width there rather
    // than the width less a margin: subtracting the margin twice would let
    // pass two drop a field pass one had just decided fitted.
    let limit = buttons.first().map(|b| b.rect.x).unwrap_or(width);
    let avail = limit.saturating_sub(pad).saturating_sub(text_x);

    let mut fields = st.fields();
    while fields.len() > 1 && block(&fields) > avail {
        fields.pop();
    }
    if fields.len() == 1 && block(&fields) > avail {
        // Only `user@host` is left and it is still too wide. Shorten it and
        // mark it: a silently clipped hostname is a different hostname.
        let room = fits(avail, s);
        let text = &mut fields[0][0].text;
        if room < 3 {
            text.clear();
        } else {
            text.truncate(
                text.char_indices()
                    .nth(room - 2)
                    .map(|(i, _)| i)
                    .unwrap_or(text.len()),
            );
            text.push_str("..");
        }
    }

    let mut spans = Vec::new();
    let mut x = text_x;
    for (i, field) in fields.into_iter().enumerate() {
        if i > 0 {
            x += sep;
        }
        for mut sp in field {
            if sp.text.is_empty() {
                continue;
            }
            sp.x = x;
            x += sp.width(s) + s;
            spans.push(sp);
        }
        x = x.saturating_sub(s);
    }

    Layout {
        bar: Rect::new(0, 0, width, h),
        dot,
        dot_colour: if st.stalled.is_some() {
            colour::WARN
        } else {
            colour::OK
        },
        spans,
        buttons,
        s,
        text_y,
    }
}

// ----------------------------------------------------------------- drawing

/// The window buffer, as a clipped drawing target.
struct Canvas<'a> {
    buf: &'a mut [u32],
    w: u32,
    h: u32,
}

/// Blend `src` over `dst` at `a`/255, using the same integer form as the
/// cursor compositor so the two agree pixel for pixel.
fn blend(dst: u32, src: u32, a: u32) -> u32 {
    let inv = 255 - a;
    let ch = |shift: u32| -> u32 {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        ((s * a + d * inv + 127) / 255).min(255)
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

impl Canvas<'_> {
    fn fill(&mut self, r: Rect, colour: u32, alpha: u32) {
        if alpha == 0 {
            return;
        }
        let r = r.intersect(&Rect::new(0, 0, self.w, self.h));
        for y in r.y..r.bottom() {
            let row = (y * self.w) as usize;
            for x in r.x..r.right() {
                let i = row + x as usize;
                self.buf[i] = if alpha >= 255 {
                    colour
                } else {
                    blend(self.buf[i], colour, alpha)
                };
            }
        }
    }

    fn outline(&mut self, r: Rect, t: u32, colour: u32) {
        if r.width <= 2 * t || r.height <= 2 * t {
            return;
        }
        self.fill(Rect::new(r.x, r.y, r.width, t), colour, 255);
        self.fill(Rect::new(r.x, r.bottom() - t, r.width, t), colour, 255);
        self.fill(Rect::new(r.x, r.y, t, r.height), colour, 255);
        self.fill(Rect::new(r.right() - t, r.y, t, r.height), colour, 255);
    }

    /// Draw `text` with its top-left at `(x, y)`; returns the advance.
    fn text(&mut self, x: u32, y: u32, s: u32, text: &str, colour: u32) -> u32 {
        let mut cx = x;
        for ch in text.chars() {
            let rows = glyph(ch);
            for (r, bits) in rows.iter().enumerate() {
                if *bits == 0 {
                    continue;
                }
                for c in 0..5u32 {
                    if bits & (1 << (4 - c)) != 0 {
                        self.fill(Rect::new(cx + c * s, y + r as u32 * s, s, s), colour, 255);
                    }
                }
            }
            cx += 6 * s;
        }
        cx.saturating_sub(x + s)
    }
}

/// Composite the bar into a presented window buffer.
///
/// `hover` and `pressed` are button indices into `layout.buttons`.
pub fn paint(
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
    layout: &Layout,
    hover: Option<usize>,
    pressed: Option<usize>,
) {
    if dst.len() < (dst_w as usize) * (dst_h as usize) {
        return;
    }
    let s = layout.s;
    let mut c = Canvas {
        buf: dst,
        w: dst_w,
        h: dst_h,
    };
    c.fill(layout.bar, colour::SCRIM, colour::SCRIM_ALPHA);
    c.fill(
        Rect::new(
            0,
            layout.bar.bottom().saturating_sub(s),
            layout.bar.width,
            s,
        ),
        colour::HAIRLINE,
        colour::HAIRLINE_ALPHA,
    );
    c.fill(layout.dot, layout.dot_colour, 255);
    for sp in &layout.spans {
        c.text(sp.x, layout.text_y, s, &sp.text, sp.colour);
    }
    for (i, b) in layout.buttons.iter().enumerate() {
        if pressed == Some(i) {
            c.fill(b.rect, colour::PRESS, colour::PRESSED_ALPHA);
        } else if hover == Some(i) {
            c.fill(b.rect, colour::PRESS, colour::HOVER_ALPHA);
        }
        if hover == Some(i) || pressed == Some(i) {
            c.outline(b.rect, s, colour::ACCENT);
        }
        let x = b.rect.x + 3 * s;
        let w = c.text(x, layout.text_y, s, b.label, b.colour);
        if b.shortcut_shown {
            c.text(x + w + 6 * s, layout.text_y, s, b.shortcut, colour::DIM);
        }
    }
}

// ------------------------------------------------------------------- state

/// Show/hide state and pointer bookkeeping for one session window.
#[derive(Debug, Default)]
pub struct Overlay {
    visible: bool,
    pinned: bool,
    focused: bool,
    /// The pointer is on the bar itself, which holds it up with no delay --
    /// a bar that hid out from under the pointer aiming at its buttons would
    /// be unusable.
    on_bar: bool,
    /// When the pointer entered the hot zone, or `None` when it is outside.
    /// The bar comes up once this is [`REVEAL_DELAY`] old.
    in_zone_since: Option<Instant>,
    /// Whether the pointer was holding the bar up at the last tick, so that
    /// the grace period below is armed by the pointer *leaving* and by
    /// nothing else.
    was_pointed: bool,
    hide_at: Option<Instant>,
    flash_until: Option<Instant>,
    hover: Option<usize>,
    armed: Option<usize>,
    /// The layout drawn last frame, used for hit testing.
    layout: Option<Layout>,
    /// The rect painted last frame, so the area a hidden bar used to cover is
    /// still presented once more and the remote pixels underneath come back.
    painted: Option<Rect>,
}

impl Overlay {
    /// A hidden bar that shows itself once, so a new session's user sees it.
    pub fn new(now: Instant) -> Self {
        Self {
            flash_until: Some(now + FLASH),
            ..Self::default()
        }
    }

    /// Whether the bar is on screen.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Whether the bar is pinned open.
    pub fn pinned(&self) -> bool {
        self.pinned
    }

    /// Pin or unpin. Per-window and not persisted: there is no session-side
    /// config file, and a pin is an intent about this window right now.
    pub fn toggle_pin(&mut self) -> bool {
        self.pinned = !self.pinned;
        self.pinned
    }

    /// Show the bar for [`FLASH`] regardless of the pointer.
    pub fn flash(&mut self, now: Instant) {
        self.flash_until = Some(now + FLASH);
    }

    /// Window focus changed. An unfocused window does not raise the bar on
    /// hover: the pointer is probably only crossing it on its way somewhere.
    pub fn set_focused(&mut self, focused: bool) {
        if focused == self.focused {
            return;
        }
        self.focused = focused;
        // Either direction throws the pointer state away, the dwell included.
        // What the pointer was doing over an unfocused window -- crossing it
        // on the way somewhere, resting on it -- was not a request for this
        // bar, so the clock starts when the window is the user's again and
        // they move in the strip, not before.
        self.on_bar = false;
        self.in_zone_since = None;
        self.hover = None;
        self.armed = None;
    }

    /// The pointer left the window.
    pub fn pointer_left(&mut self) {
        self.on_bar = false;
        self.in_zone_since = None;
        self.hover = None;
    }

    /// The session has taken the pointer -- a button is down on the remote
    /// screen.
    ///
    /// This cancels a dwell in progress, and it is why a press in the hot zone
    /// is a press and not a slow reveal: clicking a panel at the top of the
    /// remote desktop, or dragging a window up there, must not raise the bar
    /// over the thing being clicked. The dwell restarts from the next move
    /// after the button comes up.
    pub fn pointer_taken(&mut self) {
        self.on_bar = false;
        self.in_zone_since = None;
    }

    /// Note the pointer position. Returns true when it is over the bar, in
    /// which case the caller must not forward the event to the session.
    ///
    /// `now` is what the dwell is measured from, so it is taken here rather
    /// than at the next tick: the strip is a few pixels tall and a pointer
    /// crossing it may be seen exactly once, and starting the clock a tick
    /// late would make [`REVEAL_DELAY`] mean anything up to a tick more.
    pub fn track(&mut self, x: u32, y: u32, s: u32, now: Instant) -> bool {
        self.on_bar = self.visible && y < bar_height(s);
        if self.on_bar || y < hot_zone_height(s) {
            // Re-entering restarts the dwell; staying does not, so the timer
            // survives movement along the strip.
            self.in_zone_since.get_or_insert(now);
        } else {
            self.in_zone_since = None;
        }
        self.hover = if self.on_bar {
            self.layout.as_ref().and_then(|l| l.button_at(x, y))
        } else {
            None
        };
        self.on_bar
    }

    /// Whether a dwell is running that has not yet raised the bar, so the
    /// caller knows to keep waking up finely enough to honour it.
    pub fn revealing(&self) -> bool {
        !self.visible && self.focused && self.in_zone_since.is_some()
    }

    /// A press landed on the bar.
    pub fn press(&mut self) {
        self.armed = self.hover;
    }

    /// A release landed on the bar; returns the action when it completed a
    /// press and release inside the same button.
    pub fn release(&mut self) -> Option<Action> {
        let armed = self.armed.take()?;
        if self.hover != Some(armed) {
            return None;
        }
        self.layout
            .as_ref()
            .and_then(|l| l.buttons.get(armed))
            .map(|b| b.action)
    }

    /// Advance the show/hide state machine. Returns true when visibility
    /// changed, which the caller turns into a full redraw so the pixels the
    /// bar covered are repainted.
    ///
    /// The 700 ms grace period is armed by the pointer leaving and by nothing
    /// else: it exists so that crossing the bar's own bottom edge does not
    /// make it flicker. An expiring flash or an unpin hides the bar at once,
    /// because in both cases the user has already had their 1500 ms.
    ///
    /// The pointer holds the bar up once it has been in the hot zone for
    /// [`REVEAL_DELAY`], or at once when it is on the bar already -- the delay
    /// is the price of raising a bar, not of keeping one that is up.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.flash_until.is_some_and(|t| now >= t) {
            self.flash_until = None;
        }
        let dwelt = self
            .in_zone_since
            .is_some_and(|t| now.saturating_duration_since(t) >= REVEAL_DELAY);
        let pointed = self.focused && (self.on_bar || dwelt);
        if pointed {
            self.hide_at = None;
        } else if self.was_pointed {
            self.hide_at = Some(now + HIDE_DELAY);
        }
        self.was_pointed = pointed;
        let in_grace = self.hide_at.is_some_and(|t| now < t);
        let want =
            self.pinned || self.flash_until.is_some() || pointed || (self.visible && in_grace);
        if !want {
            self.hide_at = None;
        }
        if want == self.visible {
            return false;
        }
        self.visible = want;
        if !want {
            self.hover = None;
            self.armed = None;
        }
        true
    }

    /// Draw the bar, if it is showing, into a presented window buffer.
    ///
    /// Returns the rectangles that must be presented: what was drawn now and
    /// what was drawn last frame. The second is what makes a hidden bar
    /// actually disappear -- the blit has already restored those pixels, but
    /// nothing would upload them without naming the region.
    pub fn draw(
        &mut self,
        dst: &mut [u32],
        dst_w: u32,
        dst_h: u32,
        s: u32,
        status: &Status,
    ) -> (Option<Rect>, Option<Rect>) {
        let was = self.painted.take();
        if !self.visible || dst_w == 0 || dst_h == 0 {
            self.layout = None;
            return (None, was);
        }
        let layout = bar_layout(dst_w, s, status);
        paint(dst, dst_w, dst_h, &layout, self.hover, self.armed);
        let bar = layout.bar;
        self.layout = Some(layout);
        self.painted = Some(bar);
        (Some(bar), was)
    }
}

// -------------------------------------------------------------------- font

/// The glyph rows for `ch`, folding anything outside ASCII to the box.
fn glyph(ch: char) -> &'static [u8; 9] {
    let i = match ch {
        ' '..='~' => ch as usize - 0x20,
        _ => 95,
    };
    &GLYPHS[i]
}

/// ASCII `0x20..=0x7F`, 9 rows per glyph, 5 columns in bits 4..0 (bit 4 is
/// leftmost). Rows 0..=6 are cap height to baseline; rows 7..=8 carry the
/// descenders of `gjpqy,;` and the underscore.
///
/// `0x7F` is a hollow box and is also what every unmapped codepoint draws as.
pub const GLYPHS: [[u8; 9]; 96] = decode(ART);

/// The font, drawn for this repo rather than lifted from anywhere: each line
/// is one glyph as its nine 5-column rows, left to right, `#` for ink.
///
/// It is kept as art because that is the only reviewable form -- a wrong bit
/// in a hex table is invisible, a wrong pixel here is not -- and it costs
/// nothing at runtime: [`decode`] runs at compile time and the binary carries
/// only the 864-byte table.
const ART: &str = concat!(
    "..... ..... ..... ..... ..... ..... ..... ..... .....", // 20 space
    "..#.. ..#.. ..#.. ..#.. ..#.. ..... ..#.. ..... .....", // 21 '!'
    ".#.#. .#.#. ..... ..... ..... ..... ..... ..... .....", // 22 '"'
    ".#.#. .#.#. ##### .#.#. ##### .#.#. .#.#. ..... .....", // 23 '#'
    "..#.. .#### #.#.. .###. ..#.# ####. ..#.. ..... .....", // 24 '$'
    "##..# ##..# ...#. ..#.. .#... #..## #..## ..... .....", // 25 '%'
    ".##.. #..#. #.#.. .#... #.#.# #..#. .##.# ..... .....", // 26 '&'
    "..#.. ..#.. ..... ..... ..... ..... ..... ..... .....", // 27 '''
    "...#. ..#.. .#... .#... .#... ..#.. ...#. ..... .....", // 28 '('
    ".#... ..#.. ...#. ...#. ...#. ..#.. .#... ..... .....", // 29 ')'
    "..... ..#.. #.#.# .###. #.#.# ..#.. ..... ..... .....", // 2A '*'
    "..... ..#.. ..#.. ##### ..#.. ..#.. ..... ..... .....", // 2B '+'
    "..... ..... ..... ..... ..... ..#.. ..#.. .#... .....", // 2C ','
    "..... ..... ..... .###. ..... ..... ..... ..... .....", // 2D '-'
    "..... ..... ..... ..... ..... ..... ..#.. ..... .....", // 2E '.'
    "....# ....# ...#. ..#.. .#... #.... #.... ..... .....", // 2F '/'
    ".###. #...# #..## #.#.# ##..# #...# .###. ..... .....", // 30 '0'
    "..#.. .##.. ..#.. ..#.. ..#.. ..#.. .###. ..... .....", // 31 '1'
    ".###. #...# ....# ...#. ..#.. .#... ##### ..... .....", // 32 '2'
    "##### ...#. ..#.. ...#. ....# #...# .###. ..... .....", // 33 '3'
    "...#. ..##. .#.#. #..#. ##### ...#. ...#. ..... .....", // 34 '4'
    "##### #.... ####. ....# ....# #...# .###. ..... .....", // 35 '5'
    "..##. .#... #.... ####. #...# #...# .###. ..... .....", // 36 '6'
    "##### ....# ...#. ..#.. .#... .#... .#... ..... .....", // 37 '7'
    ".###. #...# #...# .###. #...# #...# .###. ..... .....", // 38 '8'
    ".###. #...# #...# .#### ....# ...#. .##.. ..... .....", // 39 '9'
    "..... ..#.. ..#.. ..... ..#.. ..#.. ..... ..... .....", // 3A ':'
    "..... ..#.. ..#.. ..... ..#.. ..#.. .#... ..... .....", // 3B ';'
    "...#. ..#.. .#... #.... .#... ..#.. ...#. ..... .....", // 3C '<'
    "..... ..... ##### ..... ##### ..... ..... ..... .....", // 3D '='
    ".#... ..#.. ...#. ....# ...#. ..#.. .#... ..... .....", // 3E '>'
    ".###. #...# ....# ...#. ..#.. ..... ..#.. ..... .....", // 3F '?'
    ".###. #...# #.### #.#.# #.### #.... .###. ..... .....", // 40 '@'
    "..#.. .#.#. #...# #...# ##### #...# #...# ..... .....", // 41 'A'
    "####. #...# #...# ####. #...# #...# ####. ..... .....", // 42 'B'
    ".###. #...# #.... #.... #.... #...# .###. ..... .....", // 43 'C'
    "###.. #..#. #...# #...# #...# #..#. ###.. ..... .....", // 44 'D'
    "##### #.... #.... ####. #.... #.... ##### ..... .....", // 45 'E'
    "##### #.... #.... ####. #.... #.... #.... ..... .....", // 46 'F'
    ".###. #...# #.... #.### #...# #...# .#### ..... .....", // 47 'G'
    "#...# #...# #...# ##### #...# #...# #...# ..... .....", // 48 'H'
    ".###. ..#.. ..#.. ..#.. ..#.. ..#.. .###. ..... .....", // 49 'I'
    "..### ...#. ...#. ...#. ...#. #..#. .##.. ..... .....", // 4A 'J'
    "#...# #..#. #.#.. ##... #.#.. #..#. #...# ..... .....", // 4B 'K'
    "#.... #.... #.... #.... #.... #.... ##### ..... .....", // 4C 'L'
    "#...# ##.## #.#.# #.#.# #...# #...# #...# ..... .....", // 4D 'M'
    "#...# #...# ##..# #.#.# #..## #...# #...# ..... .....", // 4E 'N'
    ".###. #...# #...# #...# #...# #...# .###. ..... .....", // 4F 'O'
    "####. #...# #...# ####. #.... #.... #.... ..... .....", // 50 'P'
    ".###. #...# #...# #...# #.#.# #..#. .##.# ..... .....", // 51 'Q'
    "####. #...# #...# ####. #.#.. #..#. #...# ..... .....", // 52 'R'
    ".#### #.... #.... .###. ....# ....# ####. ..... .....", // 53 'S'
    "##### ..#.. ..#.. ..#.. ..#.. ..#.. ..#.. ..... .....", // 54 'T'
    "#...# #...# #...# #...# #...# #...# .###. ..... .....", // 55 'U'
    "#...# #...# #...# #...# #...# .#.#. ..#.. ..... .....", // 56 'V'
    "#...# #...# #...# #.#.# #.#.# ##.## #...# ..... .....", // 57 'W'
    "#...# #...# .#.#. ..#.. .#.#. #...# #...# ..... .....", // 58 'X'
    "#...# #...# .#.#. ..#.. ..#.. ..#.. ..#.. ..... .....", // 59 'Y'
    "##### ....# ...#. ..#.. .#... #.... ##### ..... .....", // 5A 'Z'
    ".###. .#... .#... .#... .#... .#... .###. ..... .....", // 5B '['
    "#.... #.... .#... ..#.. ...#. ....# ....# ..... .....", // 5C '\'
    ".###. ...#. ...#. ...#. ...#. ...#. .###. ..... .....", // 5D ']'
    "..#.. .#.#. #...# ..... ..... ..... ..... ..... .....", // 5E '^'
    "..... ..... ..... ..... ..... ..... ..... ##### .....", // 5F '_'
    ".#... ..#.. ..... ..... ..... ..... ..... ..... .....", // 60 '`'
    "..... ..... .###. ....# .#### #...# .#### ..... .....", // 61 'a'
    "#.... #.... ####. #...# #...# #...# ####. ..... .....", // 62 'b'
    "..... ..... .###. #.... #.... #.... .###. ..... .....", // 63 'c'
    "....# ....# .#### #...# #...# #...# .#### ..... .....", // 64 'd'
    "..... ..... .###. #...# ##### #.... .###. ..... .....", // 65 'e'
    "..##. .#... ####. .#... .#... .#... .#... ..... .....", // 66 'f'
    "..... ..... .#### #...# #...# #...# .#### ....# .###.", // 67 'g'
    "#.... #.... ####. #...# #...# #...# #...# ..... .....", // 68 'h'
    "..#.. ..... .##.. ..#.. ..#.. ..#.. .###. ..... .....", // 69 'i'
    "...#. ..... ...#. ...#. ...#. ...#. ...#. #..#. .##..", // 6A 'j'
    "#.... #.... #..#. #.#.. ##... #.#.. #..#. ..... .....", // 6B 'k'
    ".##.. ..#.. ..#.. ..#.. ..#.. ..#.. .###. ..... .....", // 6C 'l'
    "..... ..... ##.#. #.#.# #.#.# #.#.# #.#.# ..... .....", // 6D 'm'
    "..... ..... ####. #...# #...# #...# #...# ..... .....", // 6E 'n'
    "..... ..... .###. #...# #...# #...# .###. ..... .....", // 6F 'o'
    "..... ..... ####. #...# #...# #...# ####. #.... #....", // 70 'p'
    "..... ..... .#### #...# #...# #...# .#### ....# ....#", // 71 'q'
    "..... ..... #.##. ##..# #.... #.... #.... ..... .....", // 72 'r'
    "..... ..... .#### #.... .###. ....# ####. ..... .....", // 73 's'
    ".#... .#... ####. .#... .#... .#..# ..##. ..... .....", // 74 't'
    "..... ..... #...# #...# #...# #..## .##.# ..... .....", // 75 'u'
    "..... ..... #...# #...# #...# .#.#. ..#.. ..... .....", // 76 'v'
    "..... ..... #...# #.#.# #.#.# #.#.# .#.#. ..... .....", // 77 'w'
    "..... ..... #...# .#.#. ..#.. .#.#. #...# ..... .....", // 78 'x'
    "..... ..... #...# #...# #...# #...# .#### ....# .###.", // 79 'y'
    "..... ..... ##### ...#. ..#.. .#... ##### ..... .....", // 7A 'z'
    "...## ..#.. ..#.. .#... ..#.. ..#.. ...## ..... .....", // 7B '{'
    "..#.. ..#.. ..#.. ..#.. ..#.. ..#.. ..#.. ..... .....", // 7C '|'
    "##... ..#.. ..#.. ...#. ..#.. ..#.. ##... ..... .....", // 7D '}'
    "..... ..... .##.# #..#. ..... ..... ..... ..... .....", // 7E '~'
    "##### #...# #...# #...# #...# #...# ##### ..... .....", // 7F box (fallback)
);

/// Nine rows of five columns, separated by single spaces.
const GLYPH_CHARS: usize = 9 * 5 + 8;

const _: () = assert!(ART.len() == 96 * GLYPH_CHARS, "the font art is malformed");

const fn decode(art: &str) -> [[u8; 9]; 96] {
    let b = art.as_bytes();
    let mut out = [[0u8; 9]; 96];
    let mut g = 0;
    while g < 96 {
        let mut r = 0;
        while r < 9 {
            let mut c = 0;
            let mut bits = 0u8;
            while c < 5 {
                if b[g * GLYPH_CHARS + r * 6 + c] == b'#' {
                    bits |= 1 << (4 - c);
                }
                c += 1;
            }
            out[g][r] = bits;
            r += 1;
        }
        g += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        Status::new(
            "alice@server1",
            (1920, 1080),
            Some(Duration::from_millis(42)),
        )
    }

    fn texts(l: &Layout) -> Vec<String> {
        l.spans.iter().map(|s| s.text.clone()).collect()
    }

    fn joined(l: &Layout) -> String {
        texts(l).join("")
    }

    #[test]
    fn the_font_covers_ascii_in_five_columns() {
        assert_eq!(GLYPHS.len(), 96);
        for (i, g) in GLYPHS.iter().enumerate() {
            let ch = char::from(0x20 + i as u8);
            for row in g {
                assert_eq!(row & 0xE0, 0, "{ch:?} draws outside five columns");
            }
            if ch != ' ' {
                assert!(g.iter().any(|r| *r != 0), "{ch:?} is blank");
            }
        }
        assert!(GLYPHS[0].iter().all(|r| *r == 0), "space is not blank");
    }

    #[test]
    fn glyphs_a_user_must_compare_are_all_distinct() {
        // The bar exists to be read character by character -- 0 against O, 1
        // against l, 5 against S -- so any two of these sharing a bit pattern
        // is a defect, not a stylistic choice.
        let interesting: Vec<char> = ('0'..='9')
            .chain('a'..='z')
            .chain('A'..='Z')
            .chain("@.-+:%".chars())
            .collect();
        for (i, a) in interesting.iter().enumerate() {
            for b in &interesting[i + 1..] {
                assert_ne!(glyph(*a), glyph(*b), "{a:?} and {b:?} draw the same");
            }
        }
    }

    #[test]
    fn anything_the_font_cannot_draw_becomes_the_box() {
        assert_eq!(glyph('\u{e9}'), &GLYPHS[95]);
        assert_eq!(glyph('\u{1F600}'), &GLYPHS[95]);
        // ...but only after Status has already folded it to a question mark,
        // which is the visible form a reader can recognise.
        let st = Status::new("josé@héte", (800, 600), None);
        assert_eq!(st.who, "jos?@h?te");
    }

    #[test]
    fn the_round_trip_is_two_dashes_until_a_pong_arrives() {
        let st = Status::new("a@b", (800, 600), None);
        let l = bar_layout(1600, 2, &st);
        assert!(joined(&l).contains("rtt --"), "{:?}", texts(&l));
        assert_eq!(l.dot_colour, colour::OK);
    }

    #[test]
    fn a_stall_replaces_the_round_trip_with_a_word_as_well_as_a_colour() {
        // Colour alone is not a status: the dot turns amber *and* the field
        // says the word, so the state survives a monochrome screenshot and a
        // reader who cannot tell green from amber.
        let mut st = status();
        st.stalled = Some(Duration::from_secs(12));
        let l = bar_layout(1600, 2, &st);
        assert!(joined(&l).contains("stalled 12 s"), "{:?}", texts(&l));
        assert!(!joined(&l).contains("rtt"));
        assert_eq!(l.dot_colour, colour::WARN);
    }

    #[test]
    fn buttons_never_overlap_the_text_at_any_width() {
        let mut st = status();
        st.uploads = Some((3, 42));
        for width in (120..2600).step_by(7) {
            for s in [2, 4, 6] {
                let l = bar_layout(width, s, &st);
                if let Some(first) = l.buttons.first() {
                    assert!(
                        l.text_right() <= first.rect.x,
                        "text runs into the buttons at {width}/{s}"
                    );
                    assert!(l.buttons.last().unwrap().rect.right() <= width);
                }
                for w in l.buttons.windows(2) {
                    assert!(w[0].rect.right() <= w[1].rect.x, "buttons overlap");
                }
                // Nothing runs past the right margin either, buttons or not:
                // a field that reached the window edge would read as clipped
                // even when it is complete.
                assert!(
                    l.text_right() + 4 * s <= width.max(4 * s),
                    "the text reaches the edge at {width}/{s}"
                );
            }
        }
    }

    #[test]
    fn a_narrowing_bar_gives_up_fields_in_the_documented_order() {
        let mut st = status();
        st.uploads = Some((3, 42));
        let seen = |w: u32| joined(&bar_layout(w, 2, &st));
        // Wide enough for everything.
        let all = seen(2400);
        assert!(all.contains("alice@server1") && all.contains("1920x1080"));
        assert!(all.contains("rtt ") && all.contains("up 3 files 42%"));
        // Uploads go first, then the link figure, then the size, and the host
        // is the last thing standing.
        let mut dropped = Vec::new();
        let mut width = 2400;
        while width > 140 {
            let t = seen(width);
            for field in ["up 3 files 42%", "rtt ", "1920x1080"] {
                if !t.contains(field) && !dropped.contains(&field) {
                    dropped.push(field);
                }
            }
            width -= 4;
        }
        assert_eq!(dropped, vec!["up 3 files 42%", "rtt ", "1920x1080"]);
        assert!(seen(160).contains(".."), "the host should shorten last");
    }

    #[test]
    fn the_host_is_never_shortened_while_a_button_could_go_instead() {
        let st = status();
        for width in (140..2600).step_by(3) {
            let l = bar_layout(width, 2, &st);
            if l.spans.first().is_some_and(|s| s.text.ends_with("..")) {
                assert!(
                    l.buttons.is_empty(),
                    "the host was cut at {width} while a button remained"
                );
            }
        }
    }

    #[test]
    fn a_480_pixel_bar_still_names_the_host_in_full() {
        // The narrowest window anyone would plausibly drag a session down to.
        let st = status();
        let l = bar_layout(480, 2, &st);
        assert_eq!(l.spans[0].text, "alice@server1");
        assert!(joined(&l).contains("1920x1080"));
        // Only Disconnect survives, and without its printed accelerator.
        assert_eq!(l.buttons.len(), 1);
        assert_eq!(l.buttons[0].action, Action::Disconnect);
        assert!(!l.buttons[0].shortcut_shown);
    }

    #[test]
    fn accelerators_are_printed_when_there_is_room_for_them() {
        let l = bar_layout(1920, 2, &status());
        assert_eq!(l.buttons.len(), 3);
        assert!(l.buttons.iter().all(|b| b.shortcut_shown));
        assert_eq!(l.buttons[2].action, Action::Disconnect);
        assert_eq!(l.buttons[2].colour, colour::DANGER);
    }

    #[test]
    fn the_bar_paints_only_its_own_rows() {
        // Everything below the bar is the remote screen. One stray row would
        // be a line of our colour sitting on the user's desktop until the
        // server happened to redraw that scanline.
        let (w, h, s) = (900u32, 600u32, 2u32);
        let mut buf = vec![0x00AA_BBCCu32; (w * h) as usize];
        let l = bar_layout(w, s, &status());
        paint(&mut buf, w, h, &l, Some(0), None);
        for y in bar_height(s)..h {
            for x in 0..w {
                assert_eq!(buf[(y * w + x) as usize], 0x00AA_BBCC, "touched {x},{y}");
            }
        }
        assert!(buf[..(bar_height(s) * w) as usize]
            .iter()
            .any(|p| *p != 0x00AA_BBCC));
    }

    #[test]
    fn painting_a_short_buffer_is_a_no_op_rather_than_a_panic() {
        let l = bar_layout(900, 2, &status());
        let mut buf = vec![0u32; 10];
        paint(&mut buf, 900, 600, &l, None, None);
        assert!(buf.iter().all(|p| *p == 0));
    }

    #[test]
    fn the_bar_names_the_region_it_stops_covering() {
        // The blit has already restored those pixels; without naming the
        // region nothing would present them and the bar would linger.
        let now = Instant::now();
        let mut o = Overlay::new(now);
        let mut buf = vec![0u32; 400 * 100];
        assert!(o.tick(now));
        let (drawn, was) = o.draw(&mut buf, 400, 100, 2, &status());
        assert_eq!(drawn, Some(Rect::new(0, 0, 400, 32)));
        assert_eq!(was, None);
        let later = now + FLASH + Duration::from_millis(1);
        assert!(o.tick(later));
        assert!(!o.visible());
        let (drawn, was) = o.draw(&mut buf, 400, 100, 2, &status());
        assert_eq!(drawn, None);
        assert_eq!(was, Some(Rect::new(0, 0, 400, 32)));
        // ...and only once.
        assert_eq!(o.draw(&mut buf, 400, 100, 2, &status()), (None, None));
    }

    #[test]
    fn the_pointer_raises_the_bar_only_while_the_window_is_focused() {
        let mut now = Instant::now();
        let mut o = Overlay::new(now);
        now += FLASH;
        o.tick(now);
        assert!(!o.visible(), "the opening flash should have expired");

        o.track(100, 2, 2, now);
        now += REVEAL_DELAY;
        o.tick(now);
        assert!(!o.visible(), "an unfocused window should not raise the bar");

        // Focus restarts the dwell, so the time already spent in the strip
        // over an unfocused window buys nothing.
        o.set_focused(true);
        assert!(!o.tick(now));
        o.track(100, 2, 2, now);
        now += REVEAL_DELAY;
        assert!(o.tick(now));
        assert!(o.visible());

        // Leaving starts the delay rather than hiding at once, so crossing the
        // bar's own edge does not make it flicker.
        o.track(100, 200, 2, now);
        now += Duration::from_millis(1);
        assert!(!o.tick(now));
        assert!(o.visible());
        now += HIDE_DELAY;
        assert!(o.tick(now));
        assert!(!o.visible());
    }

    #[test]
    fn reaching_for_the_top_of_the_remote_screen_does_not_raise_the_bar() {
        // The whole point of the dwell: a pointer on its way to a panel or a
        // title bar passes through the strip, and passing through must not
        // put our bar over the thing it was going to.
        let mut now = Instant::now();
        let mut o = Overlay::new(now);
        o.set_focused(true);
        now += FLASH;
        o.tick(now);
        assert!(!o.visible(), "the opening flash should have expired");

        o.track(100, 2, 2, now);
        now += REVEAL_DELAY / 2;
        assert!(!o.tick(now), "half a dwell is not a dwell");
        o.track(100, 300, 2, now);
        now += REVEAL_DELAY * 2;
        assert!(!o.tick(now));
        assert!(!o.visible(), "leaving the strip abandons the dwell");

        // Staying does raise it, and moving along the strip is staying: the
        // clock is reset by leaving and by nothing else.
        let entered = now;
        o.track(100, 2, 2, now);
        now += REVEAL_DELAY / 2;
        o.track(600, 5, 2, now);
        assert!(!o.tick(now));
        now = entered + REVEAL_DELAY;
        assert!(o.tick(now));
        assert!(o.visible());
    }

    #[test]
    fn a_press_in_the_hot_zone_belongs_to_the_session() {
        // Clicking something at the top of the remote desktop and holding --
        // a menu, a drag -- would otherwise finish the dwell under the button
        // and cover what was clicked.
        let mut now = Instant::now();
        let mut o = Overlay::new(now);
        o.set_focused(true);
        now += FLASH;
        o.tick(now);

        o.track(100, 2, 2, now);
        o.pointer_taken();
        now += REVEAL_DELAY * 3;
        assert!(!o.tick(now));
        assert!(!o.visible());
    }

    #[test]
    fn a_pinned_bar_never_hides() {
        let mut now = Instant::now();
        let mut o = Overlay::new(now);
        assert!(o.toggle_pin());
        o.tick(now);
        now += FLASH + HIDE_DELAY * 10;
        o.tick(now);
        assert!(o.visible());
        assert!(!o.toggle_pin());
        o.tick(now);
        assert!(!o.visible());
    }

    #[test]
    fn only_a_press_and_a_release_inside_one_button_acts() {
        let now = Instant::now();
        let mut o = Overlay::new(now);
        o.set_focused(true);
        o.tick(now);
        let mut buf = vec![0u32; 1600 * 100];
        o.draw(&mut buf, 1600, 100, 2, &status());
        let l = bar_layout(1600, 2, &status());
        let disconnect = l
            .buttons
            .iter()
            .find(|b| b.action == Action::Disconnect)
            .unwrap();
        let (bx, by) = (disconnect.rect.x + 4, disconnect.rect.y + 4);

        // A press that drifts off the button before release does nothing.
        o.track(bx, by, 2, now);
        o.press();
        o.track(10, by, 2, now);
        assert_eq!(o.release(), None);

        // A release with no press does nothing either.
        o.track(bx, by, 2, now);
        assert_eq!(o.release(), None);

        o.track(bx, by, 2, now);
        o.press();
        assert_eq!(o.release(), Some(Action::Disconnect));
    }

    #[test]
    fn the_pointer_is_only_claimed_while_the_bar_is_showing() {
        let now = Instant::now();
        let mut o = Overlay::new(now);
        o.set_focused(true);
        assert!(
            !o.track(100, 10, 2, now),
            "hidden bar must not swallow the pointer"
        );
        o.tick(now);
        assert!(o.visible());
        assert!(o.track(100, 10, 2, now));
        assert!(
            !o.track(100, 40, 2, now),
            "below the bar is the remote screen"
        );
    }

    /// The scrim composited over an arbitrary remote screen.
    fn over(src: u32, dst: u32, a: u32) -> u32 {
        (0..3)
            .map(|i| {
                let sh = i * 8;
                let (s, d) = ((src >> sh) & 0xff, (dst >> sh) & 0xff);
                ((s * a + d * (255 - a) + 127) / 255).min(255) << sh
            })
            .fold(0, |acc, c| acc | c)
    }

    fn rgb(c: u32) -> [u8; 3] {
        [(c >> 16) as u8, (c >> 8) as u8, c as u8]
    }

    #[test]
    fn every_foreground_is_readable_over_any_remote_screen() {
        // The bar sits on pixels we do not control, so contrast has to hold
        // against the extremes rather than against a surface of our own. The
        // worst case is a maximised white document; a black terminal is the
        // easy one. Measured, not asserted -- this is the test that rejected
        // lightening the button hover, which fails on white alone.
        for screen in [0x00FF_FFFF, 0x0000_0000, 0x0026_5E8A] {
            let base = over(colour::SCRIM, screen, colour::SCRIM_ALPHA);
            // The plain scrim, and both darkened button fills: a label has
            // to stay readable while the pointer is on it and while it is
            // held down, not only at rest.
            for fill in [
                base,
                over(colour::PRESS, base, colour::HOVER_ALPHA),
                over(colour::PRESS, base, colour::PRESSED_ALPHA),
            ] {
                for (name, fg, floor) in [
                    ("text", colour::TEXT, 4.5),
                    ("dim", colour::DIM, 4.5),
                    ("ok", colour::OK, 4.5),
                    ("warn", colour::WARN, 4.5),
                    ("danger", colour::DANGER, 4.5),
                    // A hover outline conveys state on its own, so it takes
                    // the non-text floor from WCAG 1.4.11.
                    ("accent", colour::ACCENT, 3.0),
                ] {
                    let got = crate::theme::contrast_ratio(rgb(fg), rgb(fill));
                    assert!(
                        got >= floor,
                        "{name} is {got:.2}:1 over screen {screen:06X}, floor {floor}"
                    );
                }
            }
        }
        // The specific case that decided the hover direction: white at 18%
        // over the scrim would drop Disconnect below AA on a white screen.
        let white_screen = over(colour::SCRIM, 0x00FF_FFFF, colour::SCRIM_ALPHA);
        let lightened = over(0x00FF_FFFF, white_screen, 46);
        assert!(crate::theme::contrast_ratio(rgb(colour::DANGER), rgb(lightened)) < 3.0);
    }

    #[test]
    fn the_scale_is_even_and_bounded() {
        // Even, so the layout's half units land on whole pixels; bounded, so a
        // bogus scale factor cannot make the bar taller than the window.
        for (input, want) in [
            (0.5, 2),
            (1.0, 2),
            (1.4, 2),
            (1.6, 4),
            (2.0, 4),
            (3.0, 6),
            (9.0, 6),
            (f64::NAN, 2),
        ] {
            assert_eq!(pixel_scale(input), want, "scale {input}");
        }
        for s in [2, 4, 6] {
            assert_eq!(bar_height(s) % 2, 0);
            assert_eq!(7 * s % 2, 0, "the text top must be a whole pixel");
        }
    }
}
