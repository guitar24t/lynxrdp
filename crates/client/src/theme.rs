//! Colour, type and spacing tokens for the client's windows.
//!
//! One module so the two windows cannot drift apart. The launcher is egui and
//! takes [`Tokens`]; the session window owns raw `softbuffer` pixels and has no
//! widget toolkit at all, so every colour is also published as three sRGB bytes
//! ([`Palette`]) and as the `0x00RRGGBB` word a framebuffer wants ([`packed`]).
//! Both forms are the same numbers -- a second hand-typed copy of the palette
//! next to `blit` is exactly how the two ends of a product stop matching.
//!
//! The accent is the app icon's cyan (`assets/lynxrdp.svg`, `#12a3cb`). It is
//! already the product's colour; a second invented one would be worse. Note the
//! asymmetry between the themes: dark uses the brand cyan with near-black text
//! on it, light uses a darkened cyan with white text. Brand cyan cannot carry
//! white text (2.95:1) nor sit on white (2.6:1), so one accent for both themes
//! would fail contrast in one of them. That is deliberate, not an oversight.
//!
//! Every foreground token reaches 4.5:1 on its own surface and every outline
//! token reaches 3:1, measured rather than asserted -- `contrast_ratio` and the
//! test below are what keep that true when a colour is next adjusted. The
//! launcher shipped a red error colour at 3.29:1 until that test existed.

use eframe::egui;

// ---- colours ---------------------------------------------------------

/// One colour, as the three sRGB bytes a config file and a framebuffer both
/// speak.
pub type Rgb = [u8; 3];

/// A colour as the `0x00RRGGBB` word `softbuffer` writes.
pub const fn packed(colour: Rgb) -> u32 {
    ((colour[0] as u32) << 16) | ((colour[1] as u32) << 8) | colour[2] as u32
}

/// The token set, in the form a caller with no egui can use.
///
/// The field names are the meanings, not the colours: code should ask for
/// `danger` and not for "the red one", so that a theme can move a hue without
/// every call site becoming a lie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Window and panel background.
    pub surface: Rgb,
    /// Cards, popups and striped rows: a step towards the viewer.
    pub surface_raised: Rgb,
    /// Text-entry and read-only wells: a step away from the viewer.
    pub surface_sunken: Rgb,
    /// Hairlines and separators. Decorative: nothing is conveyed by these
    /// alone, which is why they are allowed under 3:1.
    pub border: Rgb,
    /// Outlines that carry meaning -- input edges, hovered controls.
    pub border_strong: Rgb,
    pub text: Rgb,
    /// Supporting text: addresses under names, hints, the status line.
    pub text_dim: Rgb,
    /// Disabled controls. Still above 3:1, because a user has to be able to
    /// *read* a disabled item to find out why it is off.
    pub text_disabled: Rgb,
    /// Primary action and links.
    pub accent: Rgb,
    /// Focus rings and hover, where the accent needs to be louder.
    pub accent_bright: Rgb,
    /// Selected-row fill. Barely a tint: selection is also marked by an edge.
    pub accent_weak: Rgb,
    /// Text on top of `accent`.
    pub on_accent: Rgb,
    /// Row and button hover.
    pub hover_fill: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    pub danger: Rgb,
}

/// Dark theme, built to sit beside a terminal at 08:00.
pub const DARK: Palette = Palette {
    surface: [0x14, 0x18, 0x1A],
    surface_raised: [0x1C, 0x22, 0x25],
    surface_sunken: [0x0E, 0x11, 0x13],
    border: [0x2A, 0x32, 0x36],
    border_strong: [0x5A, 0x6B, 0x73],
    text: [0xE4, 0xEA, 0xEC],
    text_dim: [0x9B, 0xA8, 0xAE],
    text_disabled: [0x6E, 0x7B, 0x81],
    accent: [0x12, 0xA3, 0xCB],
    accent_bright: [0x35, 0xB4, 0xDA],
    accent_weak: [0x12, 0x33, 0x3F],
    on_accent: [0x0B, 0x11, 0x13],
    hover_fill: [0x25, 0x2D, 0x31],
    ok: [0x4C, 0xAF, 0x82],
    warn: [0xE0, 0xA8, 0x4A],
    danger: [0xF0, 0x73, 0x6A],
};

/// Light theme. Not the dark one inverted: the accent is darkened so it can
/// carry white text, and the sunken well is white rather than darker than the
/// surface, because a recessed *lighter* field is what every other light UI
/// uses for a text box.
pub const LIGHT: Palette = Palette {
    surface: [0xF2, 0xF5, 0xF6],
    surface_raised: [0xFF, 0xFF, 0xFF],
    surface_sunken: [0xFF, 0xFF, 0xFF],
    border: [0xD2, 0xD9, 0xDD],
    border_strong: [0x7F, 0x8B, 0x91],
    text: [0x17, 0x1D, 0x21],
    text_dim: [0x59, 0x66, 0x6D],
    text_disabled: [0x78, 0x84, 0x8A],
    accent: [0x0A, 0x6E, 0x8E],
    accent_bright: [0x08, 0x60, 0x7C],
    accent_weak: [0xDB, 0xEF, 0xF7],
    on_accent: [0xFF, 0xFF, 0xFF],
    hover_fill: [0xE7, 0xEC, 0xEE],
    ok: [0x16, 0x70, 0x4A],
    warn: [0x8A, 0x53, 0x00],
    danger: [0xB0, 0x2A, 0x20],
};

/// The palette for a theme.
pub const fn palette(dark: bool) -> Palette {
    if dark {
        DARK
    } else {
        LIGHT
    }
}

/// The same tokens as egui colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub surface: egui::Color32,
    pub surface_raised: egui::Color32,
    pub surface_sunken: egui::Color32,
    pub border: egui::Color32,
    pub border_strong: egui::Color32,
    pub text: egui::Color32,
    pub text_dim: egui::Color32,
    pub text_disabled: egui::Color32,
    pub accent: egui::Color32,
    pub accent_bright: egui::Color32,
    pub accent_weak: egui::Color32,
    pub on_accent: egui::Color32,
    pub hover_fill: egui::Color32,
    pub ok: egui::Color32,
    pub warn: egui::Color32,
    pub danger: egui::Color32,
}

/// One `Rgb` as an egui colour.
const fn c(colour: Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(colour[0], colour[1], colour[2])
}

/// The tokens for a theme.
pub const fn tokens(dark: bool) -> Tokens {
    let p = palette(dark);
    Tokens {
        surface: c(p.surface),
        surface_raised: c(p.surface_raised),
        surface_sunken: c(p.surface_sunken),
        border: c(p.border),
        border_strong: c(p.border_strong),
        text: c(p.text),
        text_dim: c(p.text_dim),
        text_disabled: c(p.text_disabled),
        accent: c(p.accent),
        accent_bright: c(p.accent_bright),
        accent_weak: c(p.accent_weak),
        on_accent: c(p.on_accent),
        hover_fill: c(p.hover_fill),
        ok: c(p.ok),
        warn: c(p.warn),
        danger: c(p.danger),
    }
}

/// The tokens matching whichever theme a `Ui` is currently drawing in.
///
/// Both themes are installed at once (see [`apply`]) and egui resolves
/// `System` per frame, so a widget must ask the visuals it is being drawn
/// with rather than remember a choice made at startup.
pub fn of(visuals: &egui::Visuals) -> Tokens {
    tokens(visuals.dark_mode)
}

/// The colour a focus ring is drawn in.
///
/// Split out because it is the one token that is not simply "the accent": in
/// dark the ring has to be brighter than the accent to read against the
/// surface, in light it has to be the accent itself, since `accent_bright` is
/// *darker* there.
pub fn focus_ring(t: &Tokens, dark: bool) -> egui::Color32 {
    if dark {
        t.accent_bright
    } else {
        t.accent
    }
}

/// The WCAG 2 contrast ratio between two sRGB colours, 1.0 ..= 21.0.
///
/// Here rather than in a test so that the palette carries its own definition
/// of "readable" and the numbers in this file can be checked rather than
/// believed.
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let luminance = |colour: Rgb| {
        let channel = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(colour[0]) + 0.7152 * channel(colour[1]) + 0.0722 * channel(colour[2])
    };
    let (a, b) = (luminance(a), luminance(b));
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

// ---- spacing and shape -----------------------------------------------
//
// One unit of 4 points, and everything below is a multiple of it. The point
// of naming them is that zoom (the low-vision path, 0.75x .. 2.0x) scales
// points, so nothing may be derived from a window size.

/// The spacing unit everything else is a multiple of.
pub const UNIT: f32 = 4.0;

/// Height of a button, an input, a checkbox or a menu item. Also the minimum
/// hit target: 28 points is the floor for anything clickable.
pub const CONTROL_HEIGHT: f32 = 7.0 * UNIT;

/// Height of the menu bar panel: a control plus its 4-point margins.
pub const MENU_BAR_HEIGHT: f32 = CONTROL_HEIGHT + 2.0 * UNIT;

/// The heading-and-New row above the connection list.
pub const TOOLBAR_HEIGHT: f32 = 10.0 * UNIT;

/// The Connect/Edit/Delete bar under it.
pub const ACTION_BAR_HEIGHT: f32 = 13.0 * UNIT;

/// A connection row with its name and address on two lines.
pub const ROW_HEIGHT: f32 = 12.0 * UNIT;

/// A connection row with both on one line.
pub const ROW_HEIGHT_COMPACT: f32 = 8.0 * UNIT;

/// Gap between rows.
pub const ROW_GAP: f32 = UNIT;

/// Gap between the list and the window edge.
pub const LIST_MARGIN: f32 = 3.0 * UNIT;

/// Where a row's text starts: the list margin plus the selected-row edge,
/// less one, so selected and unselected rows read down the same line.
pub const ROW_TEXT_INSET: f32 = LIST_MARGIN + SELECTED_EDGE - 1.0;

/// Width of the accent bar down the left of a selected row.
///
/// Selection is fill *and* edge because the fill alone is 1.34:1 (dark) and
/// 1.08:1 (light) against the surface -- a colour difference that small is
/// not a state anyone can be asked to see.
pub const SELECTED_EDGE: f32 = 3.0;

/// Width of a focus ring.
pub const FOCUS_RING: f32 = 2.0;

/// Height of the command-line preview strip.
pub const COMMAND_STRIP_HEIGHT: f32 = CONTROL_HEIGHT;

/// Corner radius for controls, rows and menus.
pub const RADIUS: u8 = 6;

/// Corner radius for windows and modals.
pub const RADIUS_WINDOW: u8 = 8;

/// Zoom limits for the View menu. Below 0.75 the 12-point status text stops
/// being text; above 2.0 the action bar no longer fits the minimum window.
pub const ZOOM_MIN: f32 = 0.75;
pub const ZOOM_MAX: f32 = 2.0;
pub const ZOOM_STEP: f32 = 0.1;

/// A connection's name in the list.
pub fn row_title_font() -> egui::FontId {
    egui::FontId::proportional(15.0)
}

/// The `user@host` under it. Monospace, like every other string a user could
/// mistype or has to compare character by character.
pub fn row_address_font() -> egui::FontId {
    egui::FontId::monospace(12.5)
}

/// The one-line row's name.
pub fn row_compact_title_font() -> egui::FontId {
    egui::FontId::proportional(14.0)
}

/// The one-line row's address, and the right-hand detail column.
pub fn row_detail_font() -> egui::FontId {
    egui::FontId::monospace(12.0)
}

// ---- applying it -----------------------------------------------------

/// Install both themes on a context.
///
/// Both, through `set_style_of`, rather than `set_visuals`: that only touches
/// the theme in use, so a user whose desktop is light -- or who picks Light
/// from the View menu -- would get egui's stock palette instead of ours, and
/// nothing in a dark-mode test would notice.
pub fn apply(ctx: &egui::Context) {
    ctx.set_style_of(egui::Theme::Dark, style(true));
    ctx.set_style_of(egui::Theme::Light, style(false));
}

/// The complete style for one theme.
fn style(dark: bool) -> egui::Style {
    let t = tokens(dark);
    let mut style = egui::Style {
        visuals: if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        },
        ..Default::default()
    };

    // Ubuntu-Light at egui's default 12.5 is too thin to scan a list of
    // hostnames at a glance; 14 is the floor for body text here. There is
    // exactly one weight of each bundled font -- `RichText::strong` changes
    // colour, not weight -- so hierarchy is size and colour only.
    style.text_styles = [
        (egui::TextStyle::Heading, egui::FontId::proportional(20.0)),
        (egui::TextStyle::Body, egui::FontId::proportional(14.0)),
        (egui::TextStyle::Button, egui::FontId::proportional(14.0)),
        (egui::TextStyle::Small, egui::FontId::proportional(12.0)),
        (egui::TextStyle::Monospace, egui::FontId::monospace(13.0)),
    ]
    .into();

    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(2.0 * UNIT, 1.5 * UNIT);
    s.button_padding = egui::vec2(2.5 * UNIT, 1.5 * UNIT);
    s.interact_size = egui::vec2(12.0 * UNIT, CONTROL_HEIGHT);
    s.window_margin = egui::Margin::same(12); // 3 units
    s.menu_margin = egui::Margin::same(8); // 2 units
    s.menu_spacing = UNIT;
    s.indent = 5.0 * UNIT;
    s.scroll.bar_width = 2.5 * UNIT;

    let v = &mut style.visuals;
    v.panel_fill = t.surface;
    v.window_fill = t.surface_raised;
    v.extreme_bg_color = t.surface_sunken;
    v.faint_bg_color = t.surface_raised;
    v.code_bg_color = t.surface_sunken;
    v.window_stroke = egui::Stroke::new(1.0, t.border);
    v.window_corner_radius = egui::CornerRadius::same(RADIUS_WINDOW);
    v.menu_corner_radius = egui::CornerRadius::same(RADIUS_WINDOW);
    v.hyperlink_color = t.accent;
    v.warn_fg_color = t.warn;
    v.error_fg_color = t.danger;
    v.selection.bg_fill = t.accent_weak;
    v.selection.stroke = egui::Stroke::new(1.0, t.accent);
    v.button_frame = true;
    v.striped = false;
    // Keep egui's geometry, replace only the colour: the stock dark shadow is
    // strong enough to make a menu look like it is floating a centimetre off
    // the window.
    let shadow = egui::Color32::from_black_alpha(if dark { 110 } else { 30 });
    v.window_shadow.color = shadow;
    v.popup_shadow.color = shadow;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = t.surface;
    w.noninteractive.weak_bg_fill = t.surface;
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0, t.border);
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0, t.text);
    w.inactive.bg_fill = t.surface_raised;
    w.inactive.weak_bg_fill = t.surface_raised;
    w.inactive.bg_stroke = egui::Stroke::new(1.0, t.border);
    w.inactive.fg_stroke = egui::Stroke::new(1.0, t.text);
    w.hovered.bg_fill = t.hover_fill;
    w.hovered.weak_bg_fill = t.hover_fill;
    w.hovered.bg_stroke = egui::Stroke::new(1.0, t.border_strong);
    w.hovered.fg_stroke = egui::Stroke::new(1.0, t.text);
    // `active` is also what egui hands a *focused* widget, so this stroke is
    // the keyboard focus ring: 2 points of accent rather than the stock one
    // point of pure white, which on this surface is nearly invisible.
    w.active.bg_fill = t.hover_fill;
    w.active.weak_bg_fill = t.hover_fill;
    w.active.bg_stroke = egui::Stroke::new(FOCUS_RING, focus_ring(&t, dark));
    w.active.fg_stroke = egui::Stroke::new(1.0, t.text);
    w.open.bg_fill = t.hover_fill;
    w.open.weak_bg_fill = t.hover_fill;
    w.open.bg_stroke = egui::Stroke::new(1.0, t.border_strong);
    w.open.fg_stroke = egui::Stroke::new(1.0, t.text);
    for widget in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(RADIUS);
        // egui grows a hovered or pressed widget by a point. In a list that
        // means every row twitches as the pointer runs down it, which reads
        // as a rendering fault rather than as feedback.
        widget.expansion = 0.0;
    }

    style
}

/// A `Button` styled as the one thing to do on this screen.
///
/// Filled rather than merely outlined, because the primary action has to be
/// findable without reading every button on the bar.
pub fn primary_button(t: &Tokens, label: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(label.to_string()).color(t.on_accent)).fill(t.accent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Foreground tokens and the surface each is drawn on, with the ratio the
    /// palette promises. This is the test that would have caught the launcher's
    /// old `#C0392B` error colour at 3.29:1.
    #[test]
    fn every_foreground_token_is_readable_on_its_surface() {
        for dark in [true, false] {
            let p = palette(dark);
            let text_pairs: &[(&str, Rgb, Rgb)] = &[
                ("text", p.text, p.surface),
                ("text on sunken", p.text, p.surface_sunken),
                ("text on selection", p.text, p.accent_weak),
                ("text on hover", p.text, p.hover_fill),
                ("text_dim", p.text_dim, p.surface),
                ("text_dim on sunken", p.text_dim, p.surface_sunken),
                ("text_dim on hover", p.text_dim, p.hover_fill),
                ("text_dim on selection", p.text_dim, p.accent_weak),
                ("accent", p.accent, p.surface),
                ("accent_bright", p.accent_bright, p.surface),
                ("on_accent", p.on_accent, p.accent),
                ("ok", p.ok, p.surface),
                ("warn", p.warn, p.surface),
                ("danger", p.danger, p.surface),
                ("danger on hover", p.danger, p.hover_fill),
                // The destructive button in the delete modal is the one place
                // a token is used as a *fill* under text rather than as text,
                // and it is the pair a table of foreground-on-surface ratios
                // would never have checked.
                ("on_accent on danger", p.on_accent, p.danger),
                // Modals and menus sit on `surface_raised`, not on `surface`,
                // so every colour that carries words inside one is measured
                // there too.
                ("text on raised", p.text, p.surface_raised),
                ("text_dim on raised", p.text_dim, p.surface_raised),
                ("danger on raised", p.danger, p.surface_raised),
                ("warn on raised", p.warn, p.surface_raised),
            ];
            for (what, fg, bg) in text_pairs {
                let ratio = contrast_ratio(*fg, *bg);
                assert!(
                    ratio >= 4.5,
                    "{what} is {ratio:.2}:1 in the {} theme, under the 4.5 floor for text",
                    if dark { "dark" } else { "light" }
                );
            }

            // Outlines and state marks carry meaning without being text, so
            // WCAG 1.4.11 asks 3:1 of them rather than 4.5:1. Disabled text is
            // held to the same line even though disabled controls are exempt:
            // a user has to be able to read a greyed item to learn why it is
            // off.
            let mark_pairs: &[(&str, Rgb, Rgb)] = &[
                ("border_strong", p.border_strong, p.surface),
                (
                    "focus ring",
                    if dark { p.accent_bright } else { p.accent },
                    p.surface,
                ),
                ("text_disabled", p.text_disabled, p.surface),
                ("selected edge", p.accent, p.surface),
            ];
            for (what, fg, bg) in mark_pairs {
                let ratio = contrast_ratio(*fg, *bg);
                assert!(
                    ratio >= 3.0,
                    "{what} is {ratio:.2}:1 in the {} theme, under the 3.0 floor",
                    if dark { "dark" } else { "light" }
                );
            }
        }
    }

    #[test]
    fn the_contrast_helper_agrees_with_the_extremes() {
        assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 0.01);
        assert!((contrast_ratio([0x80, 0x80, 0x80], [0x80, 0x80, 0x80]) - 1.0).abs() < 0.001);
    }

    #[test]
    fn the_packed_form_is_the_same_colour_as_the_egui_one() {
        // The session window writes `packed` words straight into a
        // framebuffer while the launcher paints Color32s. A divergence here
        // is two products wearing one name.
        let t = tokens(true);
        assert_eq!(packed(DARK.accent), 0x0012_A3CB);
        assert_eq!(
            [t.accent.r(), t.accent.g(), t.accent.b()],
            DARK.accent,
            "the egui and byte forms of the accent disagree"
        );
        assert_eq!(packed([0xFF, 0xFF, 0xFF]), 0x00FF_FFFF);
        assert_eq!(packed([0, 0, 0]), 0);
    }

    #[test]
    fn apply_styles_both_themes_and_not_just_the_one_in_use() {
        // `set_visuals` would leave the other theme on egui's stock palette,
        // and a dark-mode-only test would never notice.
        let ctx = egui::Context::default();
        apply(&ctx);
        for (theme, dark) in [(egui::Theme::Dark, true), (egui::Theme::Light, false)] {
            let style = ctx.style_of(theme);
            assert_eq!(style.visuals.panel_fill, tokens(dark).surface);
            assert_eq!(style.visuals.error_fg_color, tokens(dark).danger);
            assert_eq!(style.spacing.interact_size.y, CONTROL_HEIGHT);
            assert_eq!(
                style.text_styles[&egui::TextStyle::Monospace],
                egui::FontId::monospace(13.0)
            );
        }
    }

    #[test]
    fn no_widget_state_grows_under_the_pointer() {
        // A list whose rows change size as the pointer crosses them reads as
        // a rendering fault. egui's default expansion is 1.0 on hover.
        let style = style(true);
        let w = &style.visuals.widgets;
        for widget in [
            &w.noninteractive,
            &w.inactive,
            &w.hovered,
            &w.active,
            &w.open,
        ] {
            assert_eq!(widget.expansion, 0.0);
            assert_eq!(widget.corner_radius, egui::CornerRadius::same(RADIUS));
        }
        // The focus ring is thick enough to see; egui's stock is 1.0.
        assert_eq!(w.active.bg_stroke.width, FOCUS_RING);
    }
}
