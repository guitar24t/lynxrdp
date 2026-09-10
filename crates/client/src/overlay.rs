//! The session window's connection bar.
//!
//! The session window owns a `softbuffer` presentation buffer and
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
//! Chrome uses antialiased vector controls and proportional fonts. Global
//! accelerators remain available without cluttering the button labels.
//!
//! The bar shows only what the client actually knows: the identity from the
//! handshake, the remote size, the last real round-trip sample, and real upload
//! progress. No bandwidth figure (`Client::bytes_received` is a cumulative
//! counter, not a rate), no frame rate, no "connection quality".

use std::time::{Duration, Instant};

use lynxrdp_proto::Rect;

/// Presentation colours from the same dark theme as the graphical widgets.
pub mod colour {
    use crate::theme::{packed, DARK};
    pub const SCRIM: u32 = 0x0E_1114;
    pub const SCRIM_ALPHA: u32 = 245;
    pub const TEXT: u32 = packed(DARK.text);
    pub const DIM: u32 = packed(DARK.text_dim);
    pub const OK: u32 = packed(DARK.ok);
    pub const WARN: u32 = packed(DARK.warn);
    pub const DANGER: u32 = packed(DARK.danger);
    pub const ACCENT: u32 = packed(DARK.accent_bright);
    pub const BUTTON: u32 = packed(DARK.surface_raised);
    pub const HOVER: u32 = packed(DARK.hover_fill);
    pub const PRESS: u32 = packed(DARK.surface_sunken);
    pub const BORDER: u32 = packed(DARK.border_strong);
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

/// Bounded presentation scale, independent of the remote desktop's zoom.
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
    24 * s
}

/// Height of the strip along the top edge that brings the bar up.
pub fn hot_zone_height(s: u32) -> u32 {
    4 * s
}

// ------------------------------------------------------------------ status

/// Everything the bar is allowed to say, gathered once per frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// `user@host` from the handshake.
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
    /// Build a status line, preserving international host and user names.
    pub fn new(who: &str, size: (u32, u32), rtt: Option<Duration>) -> Self {
        Self {
            who: who.to_owned(),
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
    /// Open local file transfer controls.
    Transfers,
    /// Toggle the window's fullscreen state.
    Fullscreen,
    /// Send Ctrl+Alt+Del into the session.
    SecureAttention,
    /// End the session.
    Disconnect,
}

/// The buttons, left to right; `Disconnect` is laid out hard against the
/// right edge and the others fill leftwards from it.
const BUTTONS: [(Action, &str, &str, u32); 4] = [
    (Action::Transfers, "Transfers", "C-A-T", colour::TEXT),
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
        crate::gui_paint::text_width(&self.text, 7.0 * s as f32)
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
    /// The connection status dot.
    pub dot: Rect,
    /// `OK` or `WARN`.
    pub dot_colour: u32,
    /// Text runs, already positioned.
    pub spans: Vec<Span>,
    /// Buttons, in `BUTTONS` order (leftmost first).
    pub buttons: Vec<Button>,
    /// The scale everything was laid out at.
    pub s: u32,
    /// Legacy alignment origin retained for layout consumers.
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

/// Responsive toolbar: at narrow widths every action keeps an icon button.
/// Identity is retained before optional connection statistics.
pub fn bar_layout(width: u32, s: u32, st: &Status) -> Layout {
    let h = bar_height(s);
    let pad = 4 * s;
    let gap = 3 * s;
    let text_x = 13 * s;
    let font_width = |text: &str| crate::gui_paint::text_width(text, 7.0 * s as f32);
    let label_widths: Vec<_> = BUTTONS
        .iter()
        .map(|(_, label, _, _)| font_width(label) + 19 * s)
        .collect();
    let labeled =
        text_x + font_width(&st.who) + 2 * pad + label_widths.iter().sum::<u32>() + gap * 3
            <= width;
    let mut buttons = Vec::new();
    let mut right = width.saturating_sub(pad);
    for (i, (action, label, shortcut, colour)) in BUTTONS.iter().enumerate().rev() {
        let w = if labeled { label_widths[i] } else { 22 * s };
        if right < w + pad {
            break;
        }
        buttons.push(Button {
            action: *action,
            rect: Rect::new(right - w, 3 * s, w, 18 * s),
            label,
            shortcut,
            shortcut_shown: false,
            colour: *colour,
        });
        right = right.saturating_sub(w + gap);
    }
    buttons.reverse();
    let limit = buttons
        .first()
        .map_or(width, |b| b.rect.x)
        .saturating_sub(pad);
    let available = limit.saturating_sub(text_x);
    let mut fields = st.fields();
    let field_width = |f: &Vec<Span>| f.iter().map(|sp| sp.width(s)).sum::<u32>();
    while fields.len() > 1
        && fields.iter().map(field_width).sum::<u32>() + 12 * s * (fields.len() - 1) as u32
            > available
    {
        fields.pop();
    }
    if let Some(first) = fields.first_mut() {
        let text = &mut first[0].text;
        if font_width(text) > available {
            while !text.is_empty() && font_width(&format!("{text}...")) > available {
                text.pop();
            }
            if !text.is_empty() {
                text.push_str("...");
            }
        }
    }
    let mut x = text_x;
    let mut spans = Vec::new();
    for field in fields {
        for mut sp in field {
            if sp.text.is_empty() {
                continue;
            }
            sp.x = x;
            x += sp.width(s);
            spans.push(sp);
        }
        x += 12 * s;
    }
    Layout {
        bar: Rect::new(0, 0, width, h),
        dot: Rect::new(pad, 9 * s, 6 * s, 6 * s),
        dot_colour: if st.stalled.is_some() {
            colour::WARN
        } else {
            colour::OK
        },
        spans,
        buttons,
        s,
        text_y: 7 * s / 2,
    }
}

// ----------------------------------------------------------------- drawing
/// Composite graphical chrome into the presentation buffer.
pub fn paint(
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
    layout: &Layout,
    hover: Option<usize>,
    pressed: Option<usize>,
) {
    use crate::gui_paint::{self, color};
    use eframe::egui::{self, Align2, Color32, FontId, Stroke, StrokeKind};
    if dst.len() < dst_w as usize * dst_h as usize {
        return;
    }
    let s = layout.s as f32;
    let rect = |r: Rect| {
        egui::Rect::from_min_size(
            egui::pos2(r.x as f32, r.y as f32),
            egui::vec2(r.width as f32, r.height as f32),
        )
    };
    gui_paint::paint(dst, dst_w, dst_h, |p| {
        let p = p.with_clip_rect(rect(layout.bar));
        p.rect_filled(
            rect(layout.bar),
            0,
            Color32::from_rgba_unmultiplied(14, 17, 20, colour::SCRIM_ALPHA as u8),
        );
        let center_y = layout.bar.height as f32 / 2.0;
        p.circle_filled(
            egui::pos2(layout.dot.x as f32 + 3.0 * s, center_y),
            2.0 * s,
            color(layout.dot_colour),
        );
        let compact_hover = hover
            .and_then(|i| layout.buttons.get(i))
            .filter(|b| b.rect.width == 22 * layout.s);
        if let Some(b) = compact_hover {
            let limit = layout.buttons.first().map_or(dst_w, |b| b.rect.x) as f32 - 4.0 * s;
            let clip = egui::Rect::from_min_max(
                egui::pos2(13.0 * s, 0.0),
                egui::pos2(limit, layout.bar.height as f32),
            );
            p.with_clip_rect(clip).text(
                egui::pos2(13.0 * s, center_y),
                Align2::LEFT_CENTER,
                b.label,
                FontId::proportional(7.0 * s),
                color(b.colour),
            );
        }
        for span in layout.spans.iter().filter(|_| compact_hover.is_none()) {
            p.text(
                egui::pos2(span.x as f32, center_y),
                Align2::LEFT_CENTER,
                &span.text,
                FontId::proportional(7.0 * s),
                color(span.colour),
            );
        }
        for (i, b) in layout.buttons.iter().enumerate() {
            let r = rect(b.rect);
            let fill = if pressed == Some(i) {
                color(colour::PRESS)
            } else if hover == Some(i) {
                color(colour::HOVER)
            } else {
                color(colour::BUTTON)
            };
            p.rect_filled(r, (3.0 * s) as u8, fill);
            p.rect_stroke(
                r,
                (3.0 * s) as u8,
                Stroke::new(
                    0.5 * s,
                    if hover == Some(i) {
                        color(colour::ACCENT)
                    } else {
                        color(colour::BORDER)
                    },
                ),
                StrokeKind::Inside,
            );
            // Vector icons remain crisp at every display scale.
            let c = egui::pos2(
                r.left()
                    + if b.rect.width == 22 * layout.s {
                        11.0 * s
                    } else {
                        8.0 * s
                    },
                r.center().y,
            );
            let stroke = Stroke::new(0.8 * s, color(b.colour));
            let line = |a: [f32; 2], b: [f32; 2]| {
                p.line_segment(
                    [
                        c + egui::vec2(a[0] * s, a[1] * s),
                        c + egui::vec2(b[0] * s, b[1] * s),
                    ],
                    stroke,
                );
            };
            match b.action {
                Action::Transfers => {
                    line([-2.0, 3.0], [-2.0, -3.0]);
                    line([-4.0, -1.0], [-2.0, -3.0]);
                    line([-2.0, -3.0], [0.0, -1.0]);
                    line([2.0, -3.0], [2.0, 3.0]);
                    line([0.0, 1.0], [2.0, 3.0]);
                    line([2.0, 3.0], [4.0, 1.0]);
                }
                Action::Fullscreen => {
                    for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                        line([x * 3.0, y], [x * 3.0, y * 3.0]);
                        line([x, y * 3.0], [x * 3.0, y * 3.0]);
                    }
                }
                Action::SecureAttention => {
                    p.rect_stroke(
                        egui::Rect::from_center_size(c, egui::vec2(8.0 * s, 5.0 * s)),
                        s as u8,
                        stroke,
                        StrokeKind::Inside,
                    );
                    line([-2.0, 0.0], [2.0, 0.0]);
                }
                Action::Disconnect => {
                    line([-2.5, -2.5], [2.5, 2.5]);
                    line([-2.5, 2.5], [2.5, -2.5]);
                }
            }
            if b.rect.width != 22 * layout.s {
                p.text(
                    egui::pos2(r.left() + 15.0 * s, r.center().y),
                    Align2::LEFT_CENTER,
                    b.label,
                    FontId::proportional(7.0 * s),
                    color(b.colour),
                );
            }
        }
    });
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
    /// Native fullscreen chrome is outside the view's mouse tracking area.
    /// It reveals the bar without claiming a pointer event or arming a button.
    native_hover: bool,
    /// Physical pixels covered by a revealed native title bar.
    top_inset: u32,
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

    /// Follow native fullscreen chrome. Returns whether the bar moved.
    pub fn set_native_chrome(&mut self, hovered: bool, top_inset: u32) -> bool {
        self.native_hover = hovered;
        // macOS retracts its title bar as the pointer moves down onto ours.
        // Keep our buttons still while the user is reaching for them.
        if self.top_inset == top_inset || (top_inset < self.top_inset && self.on_bar) {
            return false;
        }
        self.top_inset = top_inset;
        self.on_bar = false;
        self.in_zone_since = None;
        self.hover = None;
        self.armed = None;
        true
    }

    /// The absolute presentation rectangle, including native title-bar space.
    pub fn bounds(&self, width: u32, height: u32, s: u32) -> Option<Rect> {
        (self.visible && width > 0 && self.top_inset < height)
            .then(|| Rect::new(0, self.top_inset, width, bar_height(s)))
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
        self.native_hover = false;
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
        self.native_hover = false;
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
        let y = y.checked_sub(self.top_inset);
        self.on_bar = self.visible && y.is_some_and(|y| y < bar_height(s));
        if self.on_bar || y.is_some_and(|y| y < hot_zone_height(s)) {
            // Re-entering restarts the dwell; staying does not, so the timer
            // survives movement along the strip.
            self.in_zone_since.get_or_insert(now);
        } else {
            self.in_zone_since = None;
        }
        self.hover = if self.on_bar {
            self.layout
                .as_ref()
                .and_then(|l| y.and_then(|y| l.button_at(x, y)))
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
        let pointed = self.focused && (self.native_hover || self.on_bar || dwelt);
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
        let Some(bar) = self.bounds(dst_w, dst_h, s) else {
            self.layout = None;
            return (None, was);
        };
        let layout = bar_layout(dst_w, s, status);
        // Keep layout/hit-testing bar-relative. Only the presentation slice
        // moves; no decoded remote pixels or remote input coordinates do.
        if let Some(dst) = dst.get_mut(self.top_inset as usize * dst_w as usize..) {
            paint(
                dst,
                dst_w,
                dst_h - self.top_inset,
                &layout,
                self.hover,
                self.armed,
            );
        }
        self.layout = Some(layout);
        self.painted = Some(bar);
        (Some(bar), was)
    }
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
    fn international_host_names_are_preserved() {
        assert_eq!(
            Status::new("jos\u{e9}@host", (800, 600), None).who,
            "jos\u{e9}@host"
        );
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
    fn narrow_windows_keep_all_four_graphical_actions() {
        for width in [320, 480, 640] {
            let l = bar_layout(width, 2, &status());
            assert_eq!(l.buttons.len(), 4);
            assert_eq!(l.buttons[0].action, Action::Transfers);
            assert!(!l.spans[0].text.is_empty());
        }
    }

    #[test]
    fn graphical_buttons_do_not_print_accelerator_codes() {
        let l = bar_layout(1920, 2, &status());
        assert_eq!(l.buttons.len(), 4);
        assert!(l.buttons.iter().all(|b| !b.shortcut_shown));
        assert_eq!(l.buttons[3].action, Action::Disconnect);
        assert_eq!(l.buttons[3].colour, colour::DANGER);
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
        assert_eq!(drawn, Some(Rect::new(0, 0, 400, 48)));
        assert_eq!(was, None);
        let later = now + FLASH + Duration::from_millis(1);
        assert!(o.tick(later));
        assert!(!o.visible());
        let (drawn, was) = o.draw(&mut buf, 400, 100, 2, &status());
        assert_eq!(drawn, None);
        assert_eq!(was, Some(Rect::new(0, 0, 400, 48)));
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
    fn native_chrome_reveals_immediately_and_leaving_keeps_the_grace_period() {
        let mut now = Instant::now();
        let mut o = Overlay::new(now);
        o.set_focused(true);
        now += FLASH;
        o.tick(now);
        assert!(!o.visible());
        o.pointer_left(); // macOS took the pointer out of the content view.
        o.set_native_chrome(true, 64);
        assert!(o.tick(now));
        assert!(o.visible(), "native chrome must not add a second dwell");
        o.press();
        assert_eq!(o.release(), None, "native hover must not arm a control");
        now += FLASH * 2;
        o.tick(now);
        assert!(o.visible(), "stay up as long as native chrome is hovered");
        o.set_native_chrome(false, 0);
        o.tick(now);
        assert!(o.visible());
        now += HIDE_DELAY;
        assert!(o.tick(now));
        assert!(!o.visible());
    }

    #[test]
    fn native_chrome_respects_focus_and_remote_drags() {
        let mut now = Instant::now();
        let mut o = Overlay::new(now);
        now += FLASH;
        o.tick(now);
        o.set_native_chrome(true, 0);
        assert!(!o.tick(now));
        o.set_focused(true);
        assert!(!o.tick(now), "focus discards stale native hover");
        o.set_native_chrome(true, 0);
        o.pointer_taken();
        assert!(!o.tick(now), "a remote drag cancels native reveal too");
    }

    #[test]
    fn an_inset_bar_paints_and_hit_tests_at_the_same_offset() {
        let now = Instant::now();
        let mut o = Overlay::new(now);
        o.set_focused(true);
        o.set_native_chrome(true, 32);
        o.tick(now);
        let (w, h, s) = (1600, 160, 2);
        let mut plain = vec![0x123456; (w * h) as usize];
        let mut inset = plain.clone();
        let layout = bar_layout(w, s, &status());
        paint(&mut plain, w, h, &layout, None, None);
        let (drawn, old) = o.draw(&mut inset, w, h, s, &status());
        assert_eq!(drawn, Some(Rect::new(0, 32, w, bar_height(s))));
        assert_eq!(drawn, o.bounds(w, h, s));
        assert_eq!(old, None);
        assert!(inset[..(32 * w) as usize].iter().all(|p| *p == 0x123456));
        assert_eq!(
            &inset[(32 * w) as usize..((32 + bar_height(s)) * w) as usize],
            &plain[..(bar_height(s) * w) as usize]
        );
        let button = &layout.buttons[0];
        let (x, y) = (button.rect.x + 2, button.rect.y + 2);
        assert!(!o.track(x, y, s, now), "the native title bar isn't ours");
        assert!(o.track(x, y + 32, s, now));
        o.press();
        // macOS retracts its row when moving onto our buttons; don't move a
        // target out from under the pointer while completing the click.
        assert!(!o.set_native_chrome(false, 0));
        assert_eq!(o.release(), Some(button.action));
        o.track(x, 150, s, now);
        assert!(o.set_native_chrome(false, 0));
        let (drawn, old) = o.draw(&mut inset, w, h, s, &status());
        assert_eq!(drawn, Some(Rect::new(0, 0, w, bar_height(s))));
        assert_eq!(old, Some(Rect::new(0, 32, w, bar_height(s))));
    }

    #[test]
    fn moving_or_hiding_an_inset_bar_retires_its_old_damage_and_click_target() {
        let now = Instant::now();
        let mut o = Overlay::new(now);
        o.set_focused(true);
        o.tick(now);
        let mut pixels = vec![0; 1600 * 100];
        o.draw(&mut pixels, 1600, 100, 2, &status());
        let button = &bar_layout(1600, 2, &status()).buttons[0];
        o.track(button.rect.x + 2, button.rect.y + 2, 2, now);
        o.press();
        o.set_native_chrome(true, 32);
        assert_eq!(o.release(), None, "moving geometry cancels an armed click");
        let (_, was) = o.draw(&mut pixels, 1600, 100, 2, &status());
        assert_eq!(was, Some(Rect::new(0, 0, 1600, 48)));
        o.set_native_chrome(false, 32);
        o.tick(now + FLASH + HIDE_DELAY);
        let (drawn, was) = o.draw(&mut pixels, 1600, 100, 2, &status());
        assert_eq!(drawn, None);
        assert_eq!(was, Some(Rect::new(0, 32, 1600, 48)));
        o.toggle_pin();
        o.tick(now + FLASH + HIDE_DELAY);
        o.set_native_chrome(false, 100);
        assert_eq!(o.bounds(1600, 100, 2), None);
        assert_eq!(o.draw(&mut pixels, 1600, 100, 2, &status()).0, None);
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
            !o.track(100, 60, 2, now),
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
            // The scrim and actual button fills: a label has
            // to stay readable while the pointer is on it and while it is
            // held down, not only at rest.
            for fill in [base, colour::HOVER, colour::PRESS] {
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
