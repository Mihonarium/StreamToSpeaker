//! Switches, choices, tick boxes, drop-downs and text fields, as Windows 11
//! draws them.

use egui::{Color32, Rect, Sense, Stroke, Ui, Vec2, WidgetInfo, WidgetType};

use super::delay::field_shapes;
use super::icon::Icon;
use super::look::{look_as, Look};
use super::menu::MenuItem;
use super::theme::{focus_ring, radius, size, space, Theme};

/// An on/off switch with *On* or *Off* beside it. `name` is what it turns
/// on, for screen readers (*Start Stream To Speaker when I sign in*).
#[must_use = "call .show(ui)"]
pub struct Switch<'a> {
    on: bool,
    name: &'a str,
    enabled: bool,
    state_words: bool,
}

impl<'a> Switch<'a> {
    pub fn new(on: bool, name: &'a str) -> Self {
        Switch { on, name, enabled: true, state_words: true }
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Leave out the *On* / *Off* words.
    pub fn without_words(mut self) -> Self {
        self.state_words = false;
        self
    }

    /// Draw it; the new value when the person flipped it.
    pub fn show(self, ui: &mut Ui) -> Option<bool> {
        let t = Theme::of(ui.ctx());
        let words = if self.on { "On" } else { "Off" };
        let galley = self.state_words.then(|| ui.painter().layout_no_wrap(words.into(), t.font(size::CONTROL), t.c.text));
        let words_w = galley.as_ref().map_or(0.0, |g| g.size().x + 12.0);
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(words_w + 40.0, 24.0), sense);
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, self.enabled, self.on, self.name));
        let l = look_as(ui, &resp, self.name);
        let p = ui.painter();
        if let Some(g) = galley {
            let color = if self.enabled { t.c.text } else { t.c.text_disabled };
            p.galley(egui::pos2(rect.left(), rect.center().y - g.size().y / 2.0), g, color);
        }
        let track = Rect::from_min_size(egui::pos2(rect.right() - 40.0, rect.center().y - 10.0), Vec2::new(40.0, 20.0));
        let (fill, border, knob) = match (self.enabled, self.on) {
            (false, true) => (Some(t.c.text_disabled), None, t.c.card),
            (false, false) => (None, Some(t.c.text_disabled), t.c.text_disabled),
            (true, true) => (Some(if l.pressed { t.c.accent_pressed } else if l.hovered { t.c.accent_hover } else { t.c.accent }), None, t.c.on_accent),
            (true, false) => (if l.hovered || l.pressed { Some(t.c.hover) } else { None }, Some(t.c.control_strong), t.c.control_strong),
        };
        if let Some(f) = fill {
            p.rect_filled(track, 10.0, f);
        }
        if let Some(b) = border {
            p.rect_stroke(track.shrink(0.5), 10.0, Stroke::new(1.0_f32, b));
        }
        let (kw, kh) = if l.pressed && self.enabled {
            (17.0, 14.0)
        } else if l.hovered && self.enabled {
            (14.0, 14.0)
        } else {
            (12.0, 12.0)
        };
        let cx = if self.on { track.right() - 10.0 } else { track.left() + 10.0 };
        let cx = if l.pressed && self.enabled { if self.on { cx - 1.5 } else { cx + 1.5 } } else { cx };
        p.rect_filled(Rect::from_center_size(egui::pos2(cx, track.center().y), Vec2::new(kw, kh)), kh / 2.0, knob);
        if l.focused {
            focus_ring(p, track, 10.0, true, &t);
        }
        (self.enabled && resp.clicked()).then_some(!self.on)
    }
}

/// The text beside a choice or tick box: a title and, under it, a grey
/// description. Returns the height used.
fn titled(ui: &Ui, left: f32, top: f32, width: f32, title: &str, description: Option<&str>, enabled: bool) -> (Vec<(egui::Pos2, std::sync::Arc<egui::Galley>, Color32)>, f32) {
    let t = Theme::of(ui.ctx());
    let title_color = if enabled { t.c.text } else { t.c.text_disabled };
    let desc_color = if enabled { t.c.text_secondary } else { t.c.text_disabled };
    let g = ui.painter().layout(title.to_owned(), t.font(size::CONTROL), title_color, width);
    let mut h = g.size().y;
    let mut out = vec![(egui::pos2(left, top), g, title_color)];
    if let Some(d) = description {
        let dg = ui.painter().layout(d.to_owned(), t.font(size::CAPTION), desc_color, width);
        out.push((egui::pos2(left, top + h), dg.clone(), desc_color));
        h += dg.size().y;
    }
    (out, h)
}

/// One of a set of choices (a radio button), with its title and an
/// optional description under it.
#[must_use = "call .show(ui)"]
pub struct Choice<'a> {
    selected: bool,
    title: &'a str,
    description: Option<&'a str>,
    enabled: bool,
}

impl<'a> Choice<'a> {
    pub fn new(selected: bool, title: &'a str) -> Self {
        Choice { selected, title, description: None, enabled: true }
    }

    pub fn description(mut self, d: &'a str) -> Self {
        self.description = Some(d);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Draw it; true when the person chose it (and it wasn't chosen).
    pub fn show(self, ui: &mut Ui) -> bool {
        let (rect, l, resp) = mark_row(ui, self.title, self.description, self.enabled, WidgetType::RadioButton, self.selected);
        let t = Theme::of(ui.ctx());
        let p = ui.painter();
        let c = egui::pos2(rect.left() + 9.0, rect.top() + 1.0 + 9.0);
        if self.selected {
            let fill = if !self.enabled {
                t.c.text_disabled
            } else if l.pressed {
                t.c.accent_pressed
            } else if l.hovered {
                t.c.accent_hover
            } else {
                t.c.accent
            };
            p.circle_filled(c, 9.0, fill);
            let dot = if l.pressed {
                3.0
            } else if l.hovered {
                5.0
            } else {
                4.0
            };
            p.circle_filled(c, dot, t.c.on_accent);
        } else {
            let fill = if l.pressed {
                t.c.control_pressed
            } else if l.hovered {
                t.c.control_hover
            } else {
                t.c.control
            };
            p.circle(c, 8.5, fill, Stroke::new(1.0_f32, if self.enabled { t.c.control_strong } else { t.c.text_disabled }));
            if l.pressed {
                p.circle_filled(c, 5.0, t.c.text_secondary);
            }
        }
        if l.focused {
            focus_ring(p, Rect::from_center_size(c, Vec2::splat(18.0)), 9.0, true, &t);
        }
        self.enabled && !self.selected && resp.clicked()
    }
}

/// A tick box with its title and an optional description.
#[must_use = "call .show(ui)"]
pub struct Check<'a> {
    checked: bool,
    title: &'a str,
    description: Option<&'a str>,
    enabled: bool,
}

impl<'a> Check<'a> {
    pub fn new(checked: bool, title: &'a str) -> Self {
        Check { checked, title, description: None, enabled: true }
    }

    pub fn description(mut self, d: &'a str) -> Self {
        self.description = Some(d);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Draw it; the new value when the person ticked or unticked it.
    pub fn show(self, ui: &mut Ui) -> Option<bool> {
        let (rect, l, resp) = mark_row(ui, self.title, self.description, self.enabled, WidgetType::Checkbox, self.checked);
        let t = Theme::of(ui.ctx());
        let p = ui.painter();
        let b = Rect::from_min_size(egui::pos2(rect.left(), rect.top() + 1.0), Vec2::splat(18.0));
        if self.checked {
            let fill = if !self.enabled {
                t.c.text_disabled
            } else if l.pressed {
                t.c.accent_pressed
            } else if l.hovered {
                t.c.accent_hover
            } else {
                t.c.accent
            };
            p.rect_filled(b, 3.0, fill);
            Icon::Check.paint_weight(p, b.shrink(2.0), t.c.on_accent, 2.4);
        } else {
            let fill = if l.pressed {
                t.c.control_pressed
            } else if l.hovered {
                t.c.control_hover
            } else {
                t.c.control
            };
            p.rect_filled(b, 3.0, fill);
            p.rect_stroke(b.shrink(0.5), 3.0, Stroke::new(1.0_f32, if self.enabled { t.c.control_strong } else { t.c.text_disabled }));
        }
        if l.focused {
            focus_ring(p, b, 3.0, true, &t);
        }
        (self.enabled && resp.clicked()).then_some(!self.checked)
    }
}

/// A row for a choice or tick box: an 18-point mark at the left, the title
/// and description beside it; the whole row is clickable.
fn mark_row(ui: &mut Ui, title: &str, description: Option<&str>, enabled: bool, typ: WidgetType, on: bool) -> (Rect, Look, egui::Response) {
    let width = ui.available_width();
    let text_left = 28.0;
    let probe = ui.cursor().min;
    let (galleys, text_h) = titled(ui, probe.x + text_left, probe.y, width - text_left, title, description, enabled);
    let height = text_h.max(20.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), sense);
    resp.widget_info(|| WidgetInfo::selected(typ, enabled, on, title));
    let l = look_as(ui, &resp, title);
    let shift = rect.min - probe;
    for (pos, g, color) in galleys {
        ui.painter().galley(pos + shift, g, color);
    }
    (rect, l, resp)
}

/// A drop-down: the chosen value in a box with a chevron; a click opens
/// the list.
#[must_use = "call .show(ui)"]
pub struct DropDown<'a> {
    id: egui::Id,
    options: &'a [&'a str],
    chosen: usize,
    name: &'a str,
    min_width: f32,
    enabled: bool,
}

impl<'a> DropDown<'a> {
    /// `name` is what it chooses, for screen readers (*Look for speakers
    /// on*).
    pub fn new(id_source: impl std::hash::Hash, name: &'a str, options: &'a [&'a str], chosen: usize) -> Self {
        DropDown { id: egui::Id::new(id_source), options, chosen, name, min_width: 180.0, enabled: true }
    }

    pub fn min_width(mut self, w: f32) -> Self {
        self.min_width = w;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Draw it; the index the person picked, when they picked one.
    pub fn show(self, ui: &mut Ui) -> Option<usize> {
        let t = Theme::of(ui.ctx());
        let font = t.font(size::CONTROL);
        let widest = self
            .options
            .iter()
            .map(|o| ui.painter().layout_no_wrap((*o).to_owned(), font.clone(), Color32::PLACEHOLDER).size().x)
            .fold(0.0, f32::max);
        let width = (widest + 20.0 + 26.0).max(self.min_width).min(ui.available_width());
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, space::BUTTON_HEIGHT), sense);
        let current = self.options.get(self.chosen).copied().unwrap_or("");
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::ComboBox, self.enabled, format!("{}: {current}", self.name)));
        let l = look_as(ui, &resp, self.name);
        let popup = self.id.with("list");
        let open = ui.memory(|m| m.is_popup_open(popup));
        let p = ui.painter();
        let fill = if !self.enabled {
            t.c.control.gamma_multiply(0.4)
        } else if l.pressed || open {
            t.c.control_pressed
        } else if l.hovered {
            t.c.control_hover
        } else {
            t.c.control
        };
        p.rect_filled(rect, radius::CONTROL, fill);
        p.rect_stroke(rect.shrink(0.5), radius::CONTROL, Stroke::new(1.0_f32, t.c.control_border));
        let color = if self.enabled { t.c.text } else { t.c.text_disabled };
        let g = p.layout(current.to_owned(), font, color, width - 46.0);
        p.galley(egui::pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, color);
        Icon::ChevronDown.paint(p, Rect::from_center_size(egui::pos2(rect.right() - 18.0, rect.center().y), Vec2::splat(16.0)), t.c.text_secondary);
        if l.focused {
            focus_ring(p, rect, radius::CONTROL, true, &t);
        }
        if self.enabled && resp.clicked() {
            ui.memory_mut(|m| m.toggle_popup(popup));
        }
        let mut picked = None;
        let _ = egui::popup::popup_below_widget(ui, popup, &resp, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
            ui.set_min_width(width - 8.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, o) in self.options.iter().enumerate() {
                if MenuItem::new(o).selected(i == self.chosen).show(ui) {
                    picked = Some(i);
                }
            }
        });
        if picked.is_some() {
            ui.memory_mut(|m| m.close_popup());
        }
        picked.filter(|i| *i != self.chosen)
    }
}

/// A one-line text field.
#[must_use = "call .show(ui)"]
pub struct TextField<'a> {
    text: &'a mut String,
    name: &'a str,
    hint: Option<&'a str>,
    password: bool,
    width: Option<f32>,
    enabled: bool,
}

impl<'a> TextField<'a> {
    /// `name` is what the field is for, for screen readers (*Address*).
    pub fn new(text: &'a mut String, name: &'a str) -> Self {
        TextField { text, name, hint: None, password: false, width: None, enabled: true }
    }

    /// Grey words shown while it is empty: a format (*192.168.1.20 or
    /// kitchen.local*), never a value to copy from another device.
    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = Some(hint);
        self
    }

    /// Show dots instead of the characters.
    pub fn password(mut self, password: bool) -> Self {
        self.password = password;
        self
    }

    pub fn width(mut self, w: f32) -> Self {
        self.width = Some(w);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn show(self, ui: &mut Ui) -> egui::Response {
        let t = Theme::of(ui.ctx());
        let width = self.width.unwrap_or(ui.available_width());
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, space::FIELD_HEIGHT), Sense::hover());
        let bg = ui.painter().add(egui::Shape::Noop);
        let mut edit = egui::TextEdit::singleline(self.text)
            .frame(false)
            .font(t.font(size::BODY))
            .text_color(if self.enabled { t.c.text } else { t.c.text_disabled })
            .interactive(self.enabled)
            .password(self.password)
            .margin(egui::Margin::ZERO)
            .vertical_align(egui::Align::Center)
            .desired_width(width - 20.0);
        if let Some(h) = self.hint {
            edit = edit.hint_text(egui::RichText::new(h).color(t.c.text_disabled));
        }
        let edit_rect = rect.shrink2(Vec2::new(10.0, 2.0));
        let resp = super::inside(ui, edit_rect, |ui: &mut Ui| ui.add_sized(edit_rect.size(), edit));
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, self.name));
        let mut l = look_as(ui, &resp, self.name);
        l.enabled = self.enabled;
        ui.painter().set(bg, field_shapes(rect, l, &t));
        resp
    }
}
