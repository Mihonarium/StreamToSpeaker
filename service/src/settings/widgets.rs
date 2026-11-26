//! The few shapes the views and dialogs share beyond the common parts: a
//! sentence with its control at the right, numbered steps, check lines,
//! copy buttons, monospace text.

use egui::{Color32, Rect, Sense, Ui, Vec2};

use crate::ui::button::Button;
use crate::ui::icon::Icon;
use crate::ui::text::{page_widget_text, page_wrap, Para};
use crate::ui::theme::{size, Theme};

use super::PageState;

/// Words in the main colour, wrapping.
pub fn body(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.add(egui::Label::new(page_widget_text(ui, egui::RichText::new(text).font(t.font(size::BODY)).color(t.c.text))).wrap());
}

/// Words in semibold, wrapping.
pub fn strong(ui: &mut Ui, text: &str, em: f32) {
    let t = Theme::of(ui.ctx());
    ui.add(egui::Label::new(page_widget_text(ui, egui::RichText::new(text).font(t.strong(em)).color(t.c.text))).wrap());
}

/// A grey line of help (12 points), wrapping.
pub fn note(ui: &mut Ui, text: &str) {
    let _ = Para::new(text).show(ui);
}

/// A grey line of help with links after it; the link clicked.
pub fn note_links(ui: &mut Ui, text: &str, links: &[&str]) -> Option<usize> {
    let mut p = Para::new(text);
    for l in links {
        p = p.link(l);
    }
    match p.show(ui) {
        Some(crate::ui::text::ParaClick::Link(i)) => Some(i),
        _ => None,
    }
}

/// A row of buttons, wrapping when the room is narrow.
pub fn buttons<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        add(ui)
    })
    .inner
}

/// A line: an optional icon, a sentence and a grey line under it on the
/// left, a control on the right (right to left, at its own width).
pub fn row<R>(ui: &mut Ui, icon: Option<Icon>, title: &str, description: Option<&str>, control: impl FnOnce(&mut Ui) -> R) -> R {
    let t = Theme::of(ui.ctx());
    let id = ui.next_auto_id().with("settings-row");
    let width = ui.available_width();
    let icon_w = if icon.is_some() { 46.0 } else { 0.0 };
    let (control_w, control_h): (f32, f32) = ui.ctx().data(|d| d.get_temp(id)).unwrap_or((0.0, 0.0));
    let gap = if control_w > 0.0 { 14.0 } else { 0.0 };
    let text_w = (width - icon_w - control_w - gap).max(140.0);
    let title_g = page_wrap(ui, ui.painter().layout(title.to_owned(), t.font(size::BODY), t.c.text, text_w));
    let desc_g = description.map(|d| page_wrap(ui, ui.painter().layout(d.to_owned(), t.font(size::CAPTION), t.c.text_secondary, text_w)));
    let text_h = title_g.size().y + desc_g.as_ref().map_or(0.0, |g| g.size().y + 2.0);
    let height = text_h.max(control_h).max(24.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let p = ui.painter();
    if let Some(i) = icon {
        i.paint(p, Rect::from_center_size(egui::pos2(rect.left() + 16.0, rect.center().y), Vec2::splat(20.0)), t.c.text);
    }
    let tx = rect.left() + icon_w;
    let ty = rect.center().y - text_h / 2.0;
    let title_h = title_g.size().y;
    p.galley(egui::pos2(tx, ty), title_g, t.c.text);
    if let Some(g) = desc_g {
        p.galley(egui::pos2(tx, ty + title_h + 2.0), g, t.c.text_secondary);
    }
    let room = (width - icon_w - 140.0 - 14.0).max(0.0);
    let c_rect = Rect::from_min_max(egui::pos2(rect.right() - room, rect.top()), rect.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(c_rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
    let out = control(&mut child);
    let used = child.min_rect();
    let size = if used.is_positive() { (used.width(), used.height()) } else { (0.0, 0.0) };
    if (size.0 - control_w).abs() > 0.5 || (size.1 - control_h).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(id, size));
        ui.ctx().request_discard("a settings row's control changed size");
    }
    out
}

/// How a step of a set-up or check stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepMark {
    Done,
    Current,
    Later,
}

/// A numbered step (Music Assistant's set-up): a circle with the number or
/// a tick, the title, then whatever `body` draws under it.
pub fn step(ui: &mut Ui, n: u8, mark: StepMark, title: &str, body: impl FnOnce(&mut Ui)) {
    let t = Theme::of(ui.ctx());
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(36.0, 30.0), Sense::hover());
        let c = Rect::from_center_size(egui::pos2(r.left() + 14.0, r.top() + 13.0), Vec2::splat(26.0));
        let p = ui.painter();
        match mark {
            StepMark::Done => {
                p.circle_filled(c.center(), 13.0, t.c.green);
                Icon::Check.paint_weight(p, c.shrink(6.0), t.c.card, 2.4);
            }
            StepMark::Current => {
                p.circle_stroke(c.center(), 12.0, egui::Stroke::new(1.6_f32, t.c.accent));
                let g = p.layout_no_wrap(n.to_string(), t.strong(size::SECONDARY), t.c.accent);
                p.galley(c.center() - g.size() / 2.0, g, t.c.accent);
            }
            StepMark::Later => {
                p.circle_stroke(c.center(), 12.0, egui::Stroke::new(1.0_f32, t.c.text_disabled));
                let g = p.layout_no_wrap(n.to_string(), t.font(size::CAPTION), t.c.text_disabled);
                p.galley(c.center() - g.size() / 2.0, g, t.c.text_disabled);
            }
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            let color = if mark == StepMark::Later { t.c.text_disabled } else { t.c.text };
            ui.add(egui::Label::new(page_widget_text(ui, egui::RichText::new(title).font(t.strong(size::BODY)).color(color))).wrap());
            body(ui);
        });
    });
}

/// What a line of a check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Good,
    Bad,
    Warning,
    /// The PC can't tell.
    Unknown,
    Info,
}

/// One line of a check: the mark, a statement, a grey detail, then
/// `actions` (buttons) under it.
pub fn check_line(ui: &mut Ui, found: Found, title: &str, detail: Option<&str>, actions: impl FnOnce(&mut Ui)) {
    let t = Theme::of(ui.ctx());
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(30.0, 20.0), Sense::hover());
        let m = Rect::from_center_size(egui::pos2(r.left() + 10.0, r.top() + 10.0), Vec2::splat(18.0));
        let p = ui.painter();
        match found {
            Found::Good => Icon::Check.paint_weight(p, m, t.c.green, 2.0),
            Found::Bad => Icon::ErrorCross.paint_weight(p, m, t.c.red, 1.7),
            Found::Warning => Icon::Warning.paint_weight(p, m, t.c.amber, 1.6),
            Found::Unknown => Icon::Help.paint_weight(p, m, t.c.text_secondary, 1.6),
            Found::Info => Icon::Info.paint_weight(p, m, t.c.accent, 1.6),
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.add(egui::Label::new(page_widget_text(ui, egui::RichText::new(title).font(t.font(size::BODY)).color(t.c.text))).wrap());
            if let Some(d) = detail {
                let _ = Para::new(d).size(size::SECONDARY).show(ui);
            }
            ui.add_space(2.0);
            actions(ui);
        });
    });
}

/// A copy button: *Copy* until clicked, then *Copied* for two seconds.
/// Copies `text` itself.
pub fn copy_button(ui: &mut Ui, st: &mut PageState, key: &str, label: &str, copied: &str, text: &str) {
    let now = ui.input(|i| i.time);
    let just = st.copied.as_ref().is_some_and(|(k, at)| k == key && now - at < 2.0);
    let shown = if just { copied } else { label };
    if Button::standard(shown).icon(Icon::Copy).accessible_name(label).show(ui).clicked() {
        ui.ctx().copy_text(text.to_owned());
        st.copied = Some((key.to_string(), now));
    }
    if just {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
}

/// Text in the monospace face, selectable (addresses, ids, a token).
pub fn mono(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.add(egui::Label::new(egui::RichText::new(text).monospace().size(size::SECONDARY).color(t.c.text)).wrap());
}

/// The fill of a tinted panel (the code's box).
pub fn tint(ui: &Ui) -> Color32 {
    Theme::of(ui.ctx()).c.accent_soft
}

/// `long` when it fits the width left in `ui`, else `short`: a button that
/// names a speaker drops the name when the name is too long for the line.
pub fn fitting<'a>(ui: &Ui, long: &'a str, short: &'a str) -> &'a str {
    let t = Theme::of(ui.ctx());
    let w = ui.painter().layout_no_wrap(long.to_owned(), t.font(size::CONTROL), Color32::PLACEHOLDER).size().x + 2.0 * crate::ui::theme::space::M + 24.0;
    if w <= ui.available_width() && w <= ui.max_rect().width() {
        long
    } else {
        short
    }
}
