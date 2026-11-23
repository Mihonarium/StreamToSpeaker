//! The app's look and the parts every window is built from.
//!
//! [`theme`] holds the colours, type and spacing, light and dark. The parts
//! ([`button`], …) draw from plain values (words, numbers, flags) and return
//! what the person did; none of them reads or changes the app's state.
//! [`preview`] draws any of it to a picture without a window; [`scenes`]
//! lists everything there is a picture of.

pub mod button;
pub mod card;
pub mod delay;
pub mod gallery;
pub mod icon;
pub mod inputs;
pub mod look;
pub mod menu;
pub mod notice;
pub mod preview;
pub mod row;
pub mod text;
pub mod theme;
pub mod volume;

/// Add a widget at `rect`, inside space the caller has already taken, without
/// moving the caller's cursor (which `Ui::put` does).
pub(crate) fn inside<R>(ui: &mut egui::Ui, rect: egui::Rect, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
    add(&mut child)
}

/// Every scene the preview can draw. Each surface adds its own here.
pub fn scenes() -> Vec<preview::Scene> {
    let mut all = Vec::new();
    all.extend(gallery::scenes());
    // The panel.
    all.extend(crate::panel::scenes::scenes());
    all.extend(crate::shell::scenes::scenes());
    all
}
