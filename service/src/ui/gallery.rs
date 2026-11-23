//! Scenes of every shared part in every state, for the preview.

use egui::{Response, Ui, Vec2};

use super::button::{Button, IconButton};
use super::icon::Icon;
use super::preview::{Scene, Stage};
use super::theme::{size, Theme};

pub fn scenes() -> Vec<Scene> {
    vec![Scene { name: "parts-buttons", size: [1000.0, 400.0], draw: buttons }]
}

/// The states every control is shown in, left to right.
const STATES: [&str; 5] = ["Resting", "Pointed at", "Pressed", "Keyboard focus", "Disabled"];

fn caption(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.label(egui::RichText::new(text).font(t.font(size::CAPTION)).color(t.c.text_secondary));
}

/// Put `r` in the state of column `i` of [`STATES`].
fn in_state(st: &mut Stage, i: usize, r: &Response) {
    match i {
        1 => st.hover(r),
        2 => st.press(r),
        3 => st.focus(r),
        _ => {}
    }
}

/// A card-coloured sheet with a margin, the way parts sit in the windows.
fn sheet(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let t = Theme::of(ui.ctx());
    egui::Frame::none()
        .fill(t.c.card)
        .stroke(t.card_stroke())
        .rounding(6.0)
        .inner_margin(egui::Margin::same(16.0))
        .outer_margin(egui::Margin::same(12.0))
        .show(ui, add);
}

fn buttons(ui: &mut Ui, st: &mut Stage) {
    sheet(ui, |ui| {
        egui::Grid::new("buttons").num_columns(6).spacing(Vec2::new(16.0, 14.0)).show(ui, |ui| {
            ui.label("");
            for s in STATES {
                caption(ui, s);
            }
            ui.end_row();
            type Make = fn() -> Button<'static>;
            let kinds: [(&str, Make); 6] = [
                ("Main act", || Button::primary("Try Kitchen again")),
                ("Quiet", || Button::standard("Stay on this PC")),
                ("Dangerous", || Button::danger("Forget Kitchen")),
                ("Small, icon", || Button::standard("Search again").small().icon(Icon::Refresh)),
                ("Enter presses it", || Button::standard("Remove 150 ms").small().default_action(true).width(130.0)),
                ("Link", || Button::link("Speaker missing?")),
            ];
            for (label, make) in kinds {
                caption(ui, label);
                for i in 0..STATES.len() {
                    let r = make().enabled(i != 4).show(ui);
                    in_state(st, i, &r);
                }
                ui.end_row();
            }
            caption(ui, "Icon only");
            for i in 0..STATES.len() {
                let r = IconButton::new(Icon::Gear, "Settings").enabled(i != 4).show(ui);
                in_state(st, i, &r);
            }
            ui.end_row();
        });
    });
}
