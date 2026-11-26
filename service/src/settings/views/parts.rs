//! The few shapes every view is made of: a plain sentence with its
//! control at the right, a slider line, a choice of a few plain outcomes, a
//! field under its sentence, links, and a thin line between groups. No
//! headings, no cards. Long words wrap; nothing is cut off.

use crate::ui::button::Button;
use egui::{Rect, Sense, Ui, Vec2};

use crate::ui::inputs::{DropDown, Switch, TextField};
use crate::ui::text::{Mark, Para, Tone};
use crate::ui::theme::{size, Theme};
use crate::ui::volume::Slider;

use crate::settings::PageState;

/// The panel's side padding, which a thin line crosses.
const PAD: f32 = 16.0;

/// A sentence that says what turning the switch on does, and the switch.
pub fn switch_line(ui: &mut Ui, label: &str, on: bool) -> Option<bool> {
    control_line(ui, label, None, |ui| Switch::new(on, label).without_words().show(ui))
}

/// A sentence (and a grey line under it) with a control at the right,
/// laid out right to left at its own width; the sentence wraps in the
/// rest.
pub fn control_line<R>(ui: &mut Ui, label: &str, under: Option<&str>, control: impl FnOnce(&mut Ui) -> R) -> R {
    crate::settings::widgets::row(ui, None, label, under, control)
}

/// A sentence (and a grey line under it) with a drop-down of choices. The
/// drop-down sits at the right while it fits there at the width of its
/// longest choice; otherwise the sentence comes first and the drop-down
/// takes the whole width under it, so no choice is cut off.
pub fn drop_line(ui: &mut Ui, id: impl std::hash::Hash, label: &str, under: Option<&str>, options: &[&str], chosen: usize) -> Option<usize> {
    let t = Theme::of(ui.ctx());
    let font = t.font(size::CONTROL);
    let widest = options.iter().map(|o| ui.painter().layout_no_wrap((*o).to_owned(), font.clone(), t.c.text).size().x).fold(0.0, f32::max);
    // The drop-down's arrow and padding; the room a line leaves its
    // control (see `widgets::row`).
    let natural = widest + 46.0;
    let width = ui.available_width();
    if natural <= width - 154.0 {
        return control_line(ui, label, under, |ui| DropDown::new(id, label, options, chosen).min_width(natural).show(ui));
    }
    plain(ui, label);
    if let Some(u) = under {
        sentence(ui, u);
    }
    DropDown::new(id, label, options, chosen).min_width(width).show(ui)
}

/// Words in the main colour, wrapping.
pub fn plain(ui: &mut Ui, text: &str) {
    crate::settings::widgets::body(ui, text);
}

/// A grey sentence, wrapping.
pub fn sentence(ui: &mut Ui, text: &str) {
    let _ = Para::new(text).size(size::SECONDARY).show(ui);
}

/// A grey sentence with links after it; the link clicked.
pub fn sentence_links(ui: &mut Ui, text: &str, links: &[&str]) -> Option<usize> {
    let mut p = Para::new(text).size(size::SECONDARY);
    for l in links {
        p = p.link(l);
    }
    match p.show(ui) {
        Some(crate::ui::text::ParaClick::Link(i)) => Some(i),
        _ => None,
    }
}

/// A sentence with a mark that says what kind it is (a warning, a fact).
pub fn marked(ui: &mut Ui, mark: Mark, tone: Tone, text: &str, links: &[&str]) -> Option<usize> {
    let mut p = Para::new(text).mark(mark).tone(tone).size(size::SECONDARY);
    for l in links {
        p = p.link(l);
    }
    match p.show(ui) {
        Some(crate::ui::text::ParaClick::Link(i)) => Some(i),
        _ => None,
    }
}

/// A thin line across the whole panel between two groups; it has no
/// title.
pub fn thin(ui: &mut Ui) {
    let t = Theme::of(ui.ctx());
    ui.add_space(6.0);
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    let line = Rect::from_min_max(egui::pos2(r.left() - PAD, r.top()), egui::pos2(r.right() + PAD, r.bottom()));
    ui.painter().rect_filled(line, 0.0, t.c.divider);
    ui.add_space(6.0);
}

/// A slider with its sentence at the left and its value at the right
/// (*Bass* ━━●━ *0 dB*). When the sentence is too long to
/// share the line it goes above the slider. `spoken` is what a reader of
/// the display says (*Bass, minus 2 decibels*).
pub fn slider_line(ui: &mut Ui, label: &str, shown: &str, value: f32, range: std::ops::RangeInclusive<f32>, step: f32, spoken: &str) -> Option<f32> {
    let t = Theme::of(ui.ctx());
    let label_w = ui.painter().layout_no_wrap(label.to_owned(), t.font(size::BODY), t.c.text).size().x;
    let value_w = ui.painter().layout_no_wrap(shown.to_owned(), t.font(size::BODY), t.c.text).size().x.max(36.0);
    let label_col = 54.0;
    let stacked = label_w > label_col;
    if stacked {
        plain(ui, label);
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        if !stacked && !label.is_empty() {
            ui.allocate_ui(Vec2::new(label_col, 24.0), |ui| {
                ui.set_min_width(label_col);
                plain(ui, label);
            });
        }
        let slider_w = (ui.available_width() - value_w - 10.0).max(60.0);
        let mut out = None;
        ui.allocate_ui(Vec2::new(slider_w, 24.0), |ui| {
            out = Slider::new(value, range, spoken).step(step).show(ui);
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(shown).font(t.font(size::BODY)).color(t.c.text));
        });
        out
    })
    .inner
}

/// A field under its sentence, the whole width; the text kept when typing
/// ends (focus leaves, or Enter) and it differs from `value`.
pub fn field_line(ui: &mut Ui, st: &mut PageState, key: &str, label: &str, value: &str) -> Option<String> {
    plain(ui, label);
    edit_field(ui, st, key, label, value)
}

pub fn edit_field(ui: &mut Ui, st: &mut PageState, key: &str, label: &str, value: &str) -> Option<String> {
    let text = st.typing.entry(key.to_string()).or_insert_with(|| value.to_string());
    let resp = TextField::new(text, label).show(ui);
    let done = resp.lost_focus();
    if !resp.has_focus() && !done && *text != value {
        *text = value.to_string();
    }
    if done && text.trim() != value {
        return Some(text.trim().to_string());
    }
    None
}

/// Space between two lines of a view.
pub fn gap(ui: &mut Ui) {
    ui.add_space(6.0);
}

/// A row of buttons, wrapping.
pub fn buttons<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    crate::settings::widgets::buttons(ui, add)
}


pub fn page_button(label: &str) -> Button<'_> {
    Button::standard(label).text_size(size::BODY)
}

pub fn page_link(label: &str) -> Button<'_> {
    Button::link(label).text_size(size::BODY)
}

pub fn wrapped_link(ui: &mut Ui, text: &str) -> egui::Response {
    let theme = Theme::of(ui.ctx());
    ui.add(egui::Link::new(crate::ui::text::page_widget_text(ui, egui::RichText::new(text).font(theme.font(size::BODY)))))
}
