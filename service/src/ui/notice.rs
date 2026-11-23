//! Messages that sit in the layout, never over it: the notice line above a
//! footer, and the coloured bar at the top of a page or dialog.

use egui::{Margin, Ui, Vec2};

use super::button::Button;
use super::text::{Mark, Para, ParaClick, Tone};
use super::theme::{size, Theme};

/// One line above the panel's footer: a background fact with its action
/// (*Restart Windows to finish updating.* Restart…).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice<'a> {
    pub mark: Mark,
    /// The fact, in semibold.
    pub lead: &'a str,
    /// More words after it, in grey (may be empty).
    pub text: &'a str,
    /// Its actions, as links after the words.
    pub actions: &'a [&'a str],
    /// Whether a × closes it.
    pub dismissable: bool,
}

/// Draw the notice line: a divider above it and its padding included.
/// Returns the action clicked (its index), or a dismissal.
pub fn notice_line(ui: &mut Ui, n: &Notice) -> Option<ParaClick> {
    let t = Theme::of(ui.ctx());
    let (line, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(line, 0.0, t.c.divider);
    egui::Frame::none()
        .inner_margin(Margin { left: 16.0, right: 16.0, top: 9.0, bottom: 9.0 })
        .show(ui, |ui| {
            let mut para = Para::new(n.text).mark(n.mark).lead(n.lead).size(size::SMALL);
            for a in n.actions {
                para = para.link(a);
            }
            if n.dismissable {
                para = para.dismiss("Close this message");
            }
            para.show(ui)
        })
        .inner
}

/// The kind of an [`InfoBar`]: its colour and icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Info,
    Warning,
    Error,
    Done,
}

/// A coloured bar with an icon, a title, words and an optional button:
/// *Settings for testing speakers that aren't fully supported yet.*
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoBar<'a> {
    pub kind: BarKind,
    pub title: Option<&'a str>,
    pub text: &'a str,
    pub action: Option<&'a str>,
}

/// Draw the bar; true when its button was clicked.
pub fn info_bar(ui: &mut Ui, b: &InfoBar) -> bool {
    let t = Theme::of(ui.ctx());
    let (fill, mark) = match b.kind {
        BarKind::Info => (t.c.accent_soft, Mark::Info),
        BarKind::Warning => (t.c.amber_bg, Mark::Warning),
        BarKind::Error => (t.c.red_bg, Mark::Error),
        BarKind::Done => (t.c.green_bg, Mark::Done),
    };
    let mut clicked = false;
    egui::Frame::none()
        .fill(fill)
        .stroke(t.card_stroke())
        .rounding(6.0)
        .inner_margin(Margin { left: 14.0, right: 14.0, top: 11.0, bottom: 11.0 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let button_w = b.action.map(|a| {
                    let g = ui.painter().layout_no_wrap(a.to_owned(), t.font(size::CONTROL), egui::Color32::PLACEHOLDER);
                    g.size().x + 24.0 + 12.0
                });
                let words_w = ui.available_width() - button_w.unwrap_or(0.0);
                ui.allocate_ui(Vec2::new(words_w, 0.0), |ui| {
                    let mut para = Para::new(b.text).mark(mark).size(size::SECONDARY).tone(Tone::Primary);
                    if let Some(title) = b.title {
                        para = para.lead(title);
                    }
                    let _ = para.show(ui);
                });
                if let Some(a) = b.action {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        clicked = Button::primary(a).show(ui).clicked();
                    });
                }
            });
        });
    clicked
}
