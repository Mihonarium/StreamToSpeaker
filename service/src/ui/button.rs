//! Buttons: the main act, a quiet one, a dangerous one, a link, and the
//! small icon-only buttons (settings, more, close).

use egui::{Color32, Rect, Response, Sense, Stroke, Ui, Vec2, Widget, WidgetInfo, WidgetType};

use super::icon::Icon;
use super::look::{look_as, Look};
use super::theme::{focus_ring, radius, size, space, Theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The one main act of a card or dialog: an accent fill.
    Primary,
    /// Every other act.
    Standard,
    /// An act that removes something the person may want back
    /// (*Forget Kitchen*): red words.
    Danger,
    /// A link: accent words, no frame.
    Link,
}

/// A button with words and, optionally, an icon before them.
#[must_use = "call .show(ui)"]
pub struct Button<'a> {
    text: &'a str,
    kind: Kind,
    small: bool,
    icon: Option<Icon>,
    enabled: bool,
    default: bool,
    width: Option<f32>,
    text_size: Option<f32>,
    name: Option<String>,
    max_width: Option<f32>,
}

impl<'a> Button<'a> {
    fn new(text: &'a str, kind: Kind) -> Self {
        Button { text, kind, small: false, icon: None, enabled: true, default: false, width: None, text_size: None, name: None, max_width: None }
    }

    pub fn primary(text: &'a str) -> Self {
        Self::new(text, Kind::Primary)
    }

    pub fn standard(text: &'a str) -> Self {
        Self::new(text, Kind::Standard)
    }

    pub fn danger(text: &'a str) -> Self {
        Self::new(text, Kind::Danger)
    }

    pub fn link(text: &'a str) -> Self {
        Self::new(text, Kind::Link)
    }

    /// 28 points tall instead of 32, with slightly smaller words.
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Outline it in the accent colour: Enter presses it.
    pub fn default_action(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// A fixed width instead of fitting the words.
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    /// No wider than this: longer words are cut with … and shown whole in
    /// the tooltip (a button that names a long speaker name).
    pub fn max_width(mut self, w: f32) -> Self {
        self.max_width = Some(w);
        self
    }

    /// The words' size (em, in pixels at 100 %), for a link that continues
    /// a sentence of that size.
    pub fn text_size(mut self, em_px: f32) -> Self {
        self.text_size = Some(em_px);
        self
    }

    /// What a screen reader says, when the words alone say less
    /// (*Remove 100 milliseconds of delay*).
    pub fn accessible_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        self.ui(ui)
    }
}

impl Widget for Button<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let t = Theme::of(ui.ctx());
        let link = self.kind == Kind::Link;
        let em = self.text_size.unwrap_or(if link {
            size::SECONDARY
        } else if self.small {
            size::SECONDARY
        } else {
            size::CONTROL
        });
        let font = t.font(em);
        let icon_size = if self.small { 14.0 } else { 15.0 };
        let icon_w = if self.icon.is_some() { icon_size + 6.0 } else { 0.0 };
        let pad = if link {
            0.0
        } else if self.small {
            10.0
        } else {
            space::M
        };
        let room = self.max_width.map_or(f32::INFINITY, |m| (m - icon_w - 2.0 * pad).max(20.0));
        let mut job = egui::text::LayoutJob::simple_singleline(self.text.to_owned(), font, Color32::PLACEHOLDER);
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        let galley = ui.fonts(|f| f.layout_job(job));
        let cut = galley.elided;
        let height = if link {
            galley.size().y.max(20.0)
        } else if self.small {
            space::SMALL_BUTTON_HEIGHT
        } else {
            space::BUTTON_HEIGHT
        };
        let natural = galley.size().x + icon_w + 2.0 * pad;
        let width = self.width.unwrap_or(natural);
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), sense);
        let response = if cut { response.on_hover_text(self.text) } else { response };
        let name = self.name.clone().unwrap_or_else(|| self.text.to_owned());
        response.widget_info(|| WidgetInfo::labeled(if link { WidgetType::Link } else { WidgetType::Button }, self.enabled, &name));
        let mut l = look_as(ui, &response, &name);
        l.enabled = self.enabled;
        if ui.is_rect_visible(rect) {
            let colors = colors(&t, self.kind, l);
            let p = ui.painter();
            let rounding = radius::CONTROL;
            if let Some(fill) = colors.fill {
                p.rect_filled(rect, rounding, fill);
            }
            if let Some(border) = colors.border {
                p.rect_stroke(rect.shrink(0.5), rounding, Stroke::new(1.0_f32, border));
            }
            if self.default && self.enabled {
                p.rect_stroke(rect.shrink(1.0), rounding, Stroke::new(2.0_f32, t.c.accent));
            }
            let content_w = galley.size().x + icon_w;
            let left = rect.center().x - content_w / 2.0;
            if let Some(icon) = self.icon {
                let r = Rect::from_center_size(egui::pos2(left + icon_size / 2.0, rect.center().y), Vec2::splat(icon_size));
                icon.paint(p, r, colors.text);
            }
            let text_pos = egui::pos2(left + icon_w, rect.center().y - galley.size().y / 2.0);
            if link && l.hovered && self.enabled {
                let y = text_pos.y + galley.size().y - 2.0;
                p.hline(text_pos.x..=text_pos.x + galley.size().x, y, Stroke::new(1.0_f32, colors.text));
            }
            p.galley(text_pos, galley, colors.text);
            if l.focused {
                focus_ring(p, if link { rect.expand2(Vec2::new(2.0, 0.0)) } else { rect }, rounding, true, &t);
            }
        }
        response
    }
}

struct Colors {
    fill: Option<Color32>,
    border: Option<Color32>,
    text: Color32,
}

fn colors(t: &Theme, kind: Kind, l: Look) -> Colors {
    let c = &t.c;
    if !l.enabled {
        return match kind {
            Kind::Link => Colors { fill: None, border: None, text: c.text_disabled },
            _ => Colors {
                fill: Some(c.control.gamma_multiply(0.4)),
                border: Some(c.control_border.gamma_multiply(0.6)),
                text: c.text_disabled,
            },
        };
    }
    match kind {
        Kind::Primary => {
            let (fill, text) = if l.pressed {
                (c.accent_pressed, c.on_accent_pressed)
            } else if l.hovered {
                (c.accent_hover, c.on_accent)
            } else {
                (c.accent, c.on_accent)
            };
            Colors { fill: Some(fill), border: None, text }
        }
        Kind::Standard | Kind::Danger => {
            let words = if kind == Kind::Danger { c.red } else { c.text };
            let (fill, text) = if l.pressed {
                (c.control_pressed, if kind == Kind::Danger { c.red } else { c.text_secondary })
            } else if l.hovered {
                (c.control_hover, words)
            } else {
                (c.control, words)
            };
            Colors { fill: Some(fill), border: Some(c.control_border), text }
        }
        Kind::Link => {
            let text = if l.pressed {
                c.accent_strong.gamma_multiply(0.8)
            } else if l.hovered {
                c.accent_strong
            } else {
                c.accent
            };
            Colors { fill: None, border: None, text }
        }
    }
}

/// A square button with only an icon (settings, more, close, back to the
/// speaker's own delay). `name` is said by screen readers and shown as the
/// tooltip.
#[must_use = "call .show(ui)"]
pub struct IconButton<'a> {
    icon: Icon,
    name: &'a str,
    /// What a reader of the display says, when it says more than the
    /// tooltip.
    spoken: Option<&'a str>,
    /// Whether Tab stops on it.
    tab_stop: bool,
    side: f32,
    icon_size: f32,
    enabled: bool,
    color: Option<Color32>,
}

impl<'a> IconButton<'a> {
    pub fn new(icon: Icon, name: &'a str) -> Self {
        IconButton { icon, name, spoken: None, tab_stop: true, side: 32.0, icon_size: 17.0, enabled: true, color: None }
    }

    /// Whether Tab stops on it (a row's › only on the row Tab stops on).
    pub fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    /// Say `spoken` to a reader of the display instead of the tooltip's
    /// words (*Options for Kitchen, Sonos*).
    pub fn spoken(mut self, spoken: &'a str) -> Self {
        self.spoken = Some(spoken);
        self
    }

    /// The square's side and the icon's size, in points.
    pub fn size(mut self, side: f32, icon_size: f32) -> Self {
        self.side = side;
        self.icon_size = icon_size;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The icon's colour when it isn't the main text colour.
    pub fn color(mut self, color: Color32) -> Self {
        self.color = Some(color);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = Theme::of(ui.ctx());
        let sense = match (self.enabled, self.tab_stop) {
            (true, true) => Sense::click(),
            (true, false) => Sense { click: true, drag: false, focusable: false },
            (false, _) => Sense::hover(),
        };
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(self.side), sense);
        let spoken = self.spoken.unwrap_or(self.name);
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, spoken));
        let l = look_as(ui, &response, spoken);
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if l.enabled && (l.hovered || l.pressed) {
                p.rect_filled(rect, radius::CONTROL, if l.pressed { t.c.pressed } else { t.c.hover });
            }
            let color = if !self.enabled {
                t.c.text_disabled
            } else if l.pressed {
                t.c.text_secondary
            } else {
                self.color.unwrap_or(t.c.text)
            };
            self.icon.paint(p, Rect::from_center_size(rect.center(), Vec2::splat(self.icon_size)), color);
            if l.focused {
                focus_ring(p, rect, radius::CONTROL, true, &t);
            }
        }
        if self.enabled && !self.name.is_empty() {
            response.on_hover_text(self.name)
        } else {
            response
        }
    }
}
