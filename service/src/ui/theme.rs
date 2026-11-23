//! The look every window shares: colours, type, spacing and shapes, in a
//! light and a dark form, and how they are applied to egui's style.
//!
//! Sizes here are the sizes Windows uses: text sizes are the em size in
//! pixels at 100 % scale, as in Windows' own apps (14 px Segoe UI for body
//! text). egui sizes a font by its line height instead of its em, so
//! [`Theme::font`] converts.

use egui::{Color32, FontFamily, FontId, Rounding, Shadow, Stroke, Vec2};

/// Light or dark, as Windows' app theme says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Light,
    Dark,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }
}

/// Every colour the parts use. Translucent colours are premultiplied, as
/// egui expects, and drawn over whatever is beneath them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Colors {
    /// Main text.
    pub text: Color32,
    /// Second lines, descriptions, labels.
    pub text_secondary: Color32,
    /// Disabled text, field placeholders.
    pub text_disabled: Color32,

    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_pressed: Color32,
    /// Links while pointed at or pressed.
    pub accent_strong: Color32,
    /// Text on an accent fill.
    pub on_accent: Color32,
    pub on_accent_pressed: Color32,
    /// Background of an informational bar.
    pub accent_soft: Color32,

    /// Window, panel and dialog background.
    pub surface: Color32,
    pub card: Color32,
    pub card_border: Color32,
    pub divider: Color32,
    /// Fill under a row, menu item or icon button while pointed at.
    pub hover: Color32,
    /// Fill under a row, menu item or icon button while pressed.
    pub pressed: Color32,
    /// Fill of the chosen row.
    pub selected: Color32,

    /// Buttons, fields and drop-downs.
    pub control: Color32,
    pub control_hover: Color32,
    pub control_pressed: Color32,
    pub control_border: Color32,
    /// The strong outline of a switch or radio button that is off, and of a
    /// field's bottom edge.
    pub control_strong: Color32,
    /// The empty part of a slider's track.
    pub track: Color32,

    /// Warnings ("isn't responding"): text and icons.
    pub amber: Color32,
    /// The amber of a badge or dot, the same in both modes.
    pub amber_mark: Color32,
    pub amber_bg: Color32,
    /// Errors ("Couldn't connect"): text and icons.
    pub red: Color32,
    pub red_bg: Color32,
    /// Done and ready marks.
    pub green: Color32,
    pub green_bg: Color32,
    /// The level meter while sound plays.
    pub live: Color32,

    /// The keyboard focus ring: a dark outer line and a light inner one in
    /// light mode, the other way round in dark mode, so it shows on any fill.
    pub focus_outer: Color32,
    pub focus_inner: Color32,

    /// The product mark's ink and its arcs.
    pub mark_ink: Color32,
    pub mark_arcs: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `hex` at `alpha` (0–1), premultiplied in gamma space: drawn over a
/// colour, it mixes with it the way a CSS `rgba()` colour does. (egui's own
/// constructors premultiply in linear space, which makes translucent white
/// much brighter than Windows draws it.)
pub fn translucent(hex: u32, alpha: f32) -> Color32 {
    let c = rgb(hex);
    let m = |v: u8| (v as f32 * alpha).round() as u8;
    Color32::from_rgba_premultiplied(m(c.r()), m(c.g()), m(c.b()), (alpha * 255.0).round() as u8)
}

fn black(alpha: f32) -> Color32 {
    translucent(0x000000, alpha)
}

fn white(alpha: f32) -> Color32 {
    translucent(0xffffff, alpha)
}

fn tint(hex: u32, alpha: f32) -> Color32 {
    translucent(hex, alpha)
}

impl Colors {
    pub fn light() -> Self {
        Colors {
            text: rgb(0x1b1b1b),
            text_secondary: rgb(0x5e5e5e),
            text_disabled: rgb(0x8a8a8a),
            accent: rgb(0x005fb8),
            accent_hover: rgb(0x1a6fbf),
            accent_pressed: rgb(0x337fc6),
            accent_strong: rgb(0x003e92),
            on_accent: rgb(0xffffff),
            on_accent_pressed: rgb(0xd5e5f4),
            accent_soft: rgb(0xe8f1fb),
            surface: rgb(0xf9f9f9),
            card: rgb(0xffffff),
            card_border: black(0.065),
            divider: black(0.08),
            hover: black(0.04),
            pressed: black(0.07),
            selected: tint(0x005fb8, 0.08),
            control: rgb(0xffffff),
            control_hover: rgb(0xf6f6f6),
            control_pressed: rgb(0xf0f0f0),
            control_border: black(0.14),
            control_strong: rgb(0x5e5e5e),
            track: rgb(0xc4c4c4),
            amber: rgb(0x9d5d00),
            amber_mark: rgb(0xf2a900),
            amber_bg: rgb(0xfff4ce),
            red: rgb(0xc42b1c),
            red_bg: rgb(0xfde7e9),
            green: rgb(0x0f7b0f),
            green_bg: rgb(0xdff6dd),
            live: rgb(0x15a349),
            focus_outer: rgb(0x1b1b1b),
            focus_inner: rgb(0xffffff),
            mark_ink: rgb(0x545cc4),
            mark_arcs: rgb(0xfe6045),
        }
    }

    pub fn dark() -> Self {
        Colors {
            text: rgb(0xffffff),
            text_secondary: rgb(0xcfcfcf),
            text_disabled: rgb(0x9a9a9a),
            accent: rgb(0x60cdff),
            accent_hover: rgb(0x5bbeeb),
            accent_pressed: rgb(0x57aed6),
            accent_strong: rgb(0x99ebff),
            on_accent: rgb(0x000000),
            on_accent_pressed: rgb(0x1d3f50),
            accent_soft: rgb(0x1e3446),
            surface: rgb(0x2b2b2b),
            card: rgb(0x323232),
            card_border: white(0.07),
            divider: white(0.09),
            hover: white(0.05),
            pressed: white(0.03),
            selected: tint(0x60cdff, 0.10),
            control: rgb(0x3a3a3a),
            control_hover: rgb(0x424242),
            control_pressed: rgb(0x343434),
            control_border: white(0.12),
            control_strong: rgb(0xcfcfcf),
            track: rgb(0x6a6a6a),
            amber: rgb(0xfce100),
            amber_mark: rgb(0xf2a900),
            amber_bg: rgb(0x433519),
            red: rgb(0xff99a4),
            red_bg: rgb(0x442726),
            green: rgb(0x6ccb5f),
            green_bg: rgb(0x393d1b),
            live: rgb(0x15a349),
            focus_outer: rgb(0xffffff),
            focus_inner: rgb(0x000000),
            mark_ink: rgb(0xc9ccf4),
            mark_arcs: rgb(0xfe6045),
        }
    }
}

/// Text sizes: the em size in pixels at 100 % scale.
pub mod size {
    /// Second lines of rows, hints, the panel's head.
    pub const CAPTION: f32 = 12.0;
    /// The notice line, the list heading, notes under the delay buttons.
    pub const SMALL: f32 = 12.5;
    /// The Now card's second line, the panel's footer, info bars.
    pub const SECONDARY: f32 = 13.0;
    /// Buttons, menu items, choices, drop-downs.
    pub const CONTROL: f32 = 13.5;
    /// Row names, setting titles.
    pub const BODY: f32 = 14.0;
    /// The Now card's title.
    pub const TITLE: f32 = 16.0;
    /// A dialog's title.
    pub const DIALOG: f32 = 20.0;
    /// A Settings page's title.
    pub const PAGE: f32 = 26.0;
}

/// Lengths in points (pixels at 100 % scale).
pub mod space {
    pub const XS: f32 = 4.0;
    pub const S: f32 = 8.0;
    pub const M: f32 = 12.0;
    pub const L: f32 = 16.0;
    pub const XL: f32 = 24.0;

    /// A place in the panel's list: tall enough for a finger.
    pub const ROW_HEIGHT: f32 = 52.0;
    /// The square behind a row's kind icon.
    pub const ROW_ICON_TILE: f32 = 32.0;
    pub const BUTTON_HEIGHT: f32 = 32.0;
    /// Small buttons, the delay buttons and the amount field.
    pub const SMALL_BUTTON_HEIGHT: f32 = 28.0;
    pub const FIELD_HEIGHT: f32 = 32.0;
    pub const MENU_ITEM_HEIGHT: f32 = 32.0;
    /// Padding inside a card.
    pub const CARD_PAD_X: f32 = 14.0;
    pub const CARD_PAD_Y: f32 = 12.0;
}

/// Corner radii in points.
pub mod radius {
    pub const WINDOW: f32 = 8.0;
    pub const CARD: f32 = 6.0;
    pub const ROW: f32 = 5.0;
    pub const CONTROL: f32 = 4.0;
}

/// Segoe UI's line height (ascender to descender, no line gap) per em.
/// egui sizes a font by that height, Windows and CSS by the em.
pub const SEGOE_UI_HEIGHT_PER_EM: f32 = (2210.0 + 514.0) / 2048.0;

/// The font family of Segoe UI Semibold, set up by [`crate::fonts`].
pub const SEMIBOLD: &str = crate::fonts::SEMIBOLD_FAMILY;

/// The whole look in one mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub mode: Mode,
    pub c: Colors,
}

impl Theme {
    pub fn new(mode: Mode) -> Self {
        let c = match mode {
            Mode::Light => Colors::light(),
            Mode::Dark => Colors::dark(),
        };
        Theme { mode, c }
    }

    /// The theme [`apply`] stored in `ctx`, or the light one.
    pub fn of(ctx: &egui::Context) -> Self {
        ctx.data(|d| d.get_temp::<Theme>(theme_id())).unwrap_or_else(|| Theme::new(Mode::Light))
    }

    /// Regular text whose em is `em_px` pixels at 100 % scale.
    pub fn font(&self, em_px: f32) -> FontId {
        FontId::new(em_px * SEGOE_UI_HEIGHT_PER_EM, FontFamily::Proportional)
    }

    /// Semibold text whose em is `em_px` pixels at 100 % scale.
    pub fn strong(&self, em_px: f32) -> FontId {
        FontId::new(em_px * SEGOE_UI_HEIGHT_PER_EM, FontFamily::Name(SEMIBOLD.into()))
    }

    /// The shadow of a flyout, menu or dialog.
    pub fn shadow(&self) -> Shadow {
        let alpha = match self.mode {
            Mode::Light => 0.22,
            Mode::Dark => 0.55,
        };
        Shadow { offset: Vec2::new(0.0, 8.0), blur: 28.0, spread: 0.0, color: black(alpha) }
    }

    /// The thin border every card, flyout and dialog has.
    pub fn card_stroke(&self) -> Stroke {
        Stroke::new(1.0_f32, self.c.card_border)
    }
}

fn theme_id() -> egui::Id {
    egui::Id::new("stream_to_speaker::ui::theme")
}

/// Use `mode` for everything drawn in `ctx` from the next frame: egui's own
/// widgets get the same colours, sizes and corners as the parts, and
/// [`Theme::of`] returns it.
pub fn apply(ctx: &egui::Context, mode: Mode) {
    let theme = Theme::new(mode);
    ctx.data_mut(|d| d.insert_temp(theme_id(), theme));
    ctx.style_mut(|style| style_for(style, &theme));
}

/// egui's style, set to the theme.
pub fn style_for(style: &mut egui::Style, t: &Theme) {
    use egui::style::{Selection, TextCursorStyle, WidgetVisuals, Widgets};
    use egui::{TextStyle, Visuals};
    let c = &t.c;

    style.text_styles = [
        (TextStyle::Small, t.font(size::CAPTION)),
        (TextStyle::Body, t.font(size::BODY)),
        (TextStyle::Button, t.font(size::CONTROL)),
        (TextStyle::Heading, t.strong(size::DIALOG)),
        (TextStyle::Monospace, FontId::monospace(size::SECONDARY * SEGOE_UI_HEIGHT_PER_EM)),
    ]
    .into();

    let control = |fill: Color32, text: Color32| WidgetVisuals {
        bg_fill: fill,
        weak_bg_fill: fill,
        bg_stroke: Stroke::new(1.0_f32, c.control_border),
        rounding: Rounding::same(radius::CONTROL),
        fg_stroke: Stroke::new(1.0_f32, text),
        expansion: 0.0,
    };
    let mut v = match t.mode {
        Mode::Light => Visuals::light(),
        Mode::Dark => Visuals::dark(),
    };
    v.widgets = Widgets {
        noninteractive: WidgetVisuals {
            bg_fill: c.card,
            weak_bg_fill: c.card,
            bg_stroke: Stroke::new(1.0_f32, c.divider),
            rounding: Rounding::same(radius::CONTROL),
            fg_stroke: Stroke::new(1.0_f32, c.text),
            expansion: 0.0,
        },
        inactive: control(c.control, c.text),
        hovered: control(c.control_hover, c.text),
        active: control(c.control_pressed, c.text_secondary),
        open: control(c.control_pressed, c.text),
    };
    v.selection = Selection {
        bg_fill: Color32::from_rgba_premultiplied(
            (c.accent.r() as u16 * 90 / 255) as u8,
            (c.accent.g() as u16 * 90 / 255) as u8,
            (c.accent.b() as u16 * 90 / 255) as u8,
            90,
        ),
        stroke: Stroke::new(1.0_f32, c.accent),
    };
    v.text_cursor = TextCursorStyle { stroke: Stroke::new(1.0_f32, c.text), ..Default::default() };
    v.hyperlink_color = c.accent;
    v.faint_bg_color = c.hover;
    v.extreme_bg_color = c.control;
    v.code_bg_color = c.hover;
    v.warn_fg_color = c.amber;
    v.error_fg_color = c.red;
    v.window_fill = c.surface;
    v.panel_fill = c.surface;
    v.window_stroke = t.card_stroke();
    v.window_rounding = Rounding::same(radius::WINDOW);
    v.window_shadow = t.shadow();
    v.popup_shadow = t.shadow();
    v.menu_rounding = Rounding::same(radius::WINDOW);
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.striped = false;
    v.slider_trailing_fill = true;
    style.visuals = v;

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(space::S, 6.0);
    s.button_padding = Vec2::new(space::M, 5.0);
    s.interact_size = Vec2::new(40.0, 24.0);
    s.menu_margin = egui::Margin::same(space::XS);
    s.window_margin = egui::Margin::same(space::L);
    s.scroll = egui::style::ScrollStyle::floating();
    // Windows' own controls change state in about 80 ms.
    style.animation_time = 0.083;
}

/// Draw the keyboard focus ring around `rect`: two lines, so it shows on
/// any fill. `outside` puts it just outside the control, as Windows does
/// for buttons; otherwise it sits just inside, for rows that touch their
/// neighbours.
pub fn focus_ring(painter: &egui::Painter, rect: egui::Rect, rounding: f32, outside: bool, t: &Theme) {
    let (outer, inner) = if outside { (rect.expand(3.0), rect.expand(1.0)) } else { (rect.shrink(1.0), rect.shrink(3.0)) };
    let r = if outside { rounding + 3.0 } else { (rounding - 1.0).max(0.0) };
    painter.rect_stroke(outer.shrink(1.0), Rounding::same(r - 1.0), Stroke::new(2.0_f32, t.c.focus_outer));
    painter.rect_stroke(inner.shrink(0.5), Rounding::same((r - 2.5).max(0.0)), Stroke::new(1.0_f32, t.c.focus_inner));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_text_is_fourteen_pixel_segoe_ui() {
        // 14 px em; Segoe UI's line is 1.33 em, which is the size egui wants.
        let f = Theme::new(Mode::Light).font(size::BODY);
        assert!((f.size - 18.62).abs() < 0.01, "{}", f.size);
    }

    #[test]
    fn apply_stores_the_theme_for_the_parts() {
        let ctx = egui::Context::default();
        apply(&ctx, Mode::Dark);
        assert_eq!(Theme::of(&ctx).mode, Mode::Dark);
        assert_eq!(ctx.style().visuals.panel_fill, Colors::dark().surface);
        apply(&ctx, Mode::Light);
        assert_eq!(Theme::of(&ctx).c, Colors::light());
    }
}
