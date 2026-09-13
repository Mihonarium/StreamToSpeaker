//! Layout skeletons shared by the notice strips and the status banner:
//! the two shapes that have bitten us in the field, kept free of any
//! Windows-only code so they can be laid out headlessly on any platform
//! and tested (`tests/gui_layout.rs` runs one egui frame at 580/720/900
//! px and inspects the emitted rectangles). `gui.rs` is `cfg(windows)` as
//! a whole; everything here is plain egui.
//!
//! Two egui facts these helpers exist to encode:
//!
//! 1. A label added *before* a right-to-left button group claims the row
//!    at its full single-line width (labels don't wrap inside horizontal
//!    layouts), so the buttons get painted over its tail. Lay the buttons
//!    out first and wrap the text into what's left.
//! 2. A vertically-centred horizontal layout dropped straight into an
//!    unbounded ui (a scroll area's content) centres its widgets within an
//!    infinite height — at y = ∞ — and the enclosing frame paints the whole
//!    page. Every such row must live inside `ui.horizontal`, which bounds
//!    the row height.

use egui;

/// Spacing scale shared with `gui.rs`.
pub mod sp {
    pub const XS: f32 = 8.0;
    pub const S: f32 = 12.0;
    pub const M: f32 = 16.0;
    pub const L: f32 = 24.0;

    // M14 — three "container padding" tiers, named so we never reach
    // for raw 18 / 16 / 24 / 12 / 8 again.
    //
    //   CARD_*  — sectional cards + status banner. 18 horizontal is
    //             slightly wider than sp::M to give section_label a
    //             touch more breathing room from the card border.
    //   MODAL   — confirm / settings modals. sp::L on all sides; the
    //             extra padding signals "this is a heavier surface
    //             demanding attention" (Refactoring UI: heavier
    //             surfaces get heavier padding).
    //   PILL_*  — inline pills, segmented controls, status chips.
    //             Tight by design (small surfaces need small padding).
    pub const CARD_H: f32 = 18.0;
    pub const CARD_V: f32 = M;
    pub const MODAL: f32 = L;
    pub const PILL_H: f32 = S;
    pub const PILL_V: f32 = XS;
}

/// A notice strip's content row: a message with right-aligned actions,
/// laid out so the message can never run under the buttons. The buttons
/// are placed first (right-to-left), then the message wraps in whatever
/// width is left; if that would be too narrow to read, the row stacks —
/// message on top, buttons right-aligned beneath. `buttons_w` is the
/// buttons' total width including gaps (the caller knows it from the min
/// widths it passes to the button helpers).
///
/// Why: a label added *before* a right-to-left button group claims the
/// row at its full single-line width — labels don't wrap inside
/// horizontal layouts — so past a certain window width the buttons were
/// painted straight over the text. A fixed "stack below N px" threshold
/// only moved that collision to a different window width (the donation
/// strip overlapped at the default 720 px window).
///
/// Every right-to-left row here is wrapped in `ui.horizontal`, which
/// bounds the row's height. A vertically-centred horizontal layout dropped
/// straight into an unbounded ui (the scroll area's content) centres its
/// widgets within an infinite height — i.e. at y = ∞ — and the enclosing
/// frame then paints the whole page. `main`'s stacked branch did this at
/// the 580 px minimum width; the first cut of this helper did it at every
/// width. Verified with a headless egui layout harness.
pub fn notice_row(
    ui: &mut egui::Ui,
    msg: egui::RichText,
    buttons_w: f32,
    buttons: impl FnOnce(&mut egui::Ui),
) {
    const MIN_TEXT_W: f32 = 240.0;
    if ui.available_width() - buttons_w - sp::S < MIN_TEXT_W {
        ui.vertical(|ui| {
            ui.add(egui::Label::new(msg).wrap());
            ui.add_space(sp::S);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), buttons);
            });
        });
    } else {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                buttons(ui);
                ui.add_space(sp::S);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(msg).wrap());
                });
            });
        });
    }
}


/// Status-banner body: a text column that wraps, with an optional
/// fixed-width, right-aligned column of buttons laid out FIRST so the text
/// can never run under it. `buttons` is `(column width, contents)`; the
/// contents are laid out top-down and right-aligned inside that width
/// (rows inside it may use right-to-left layouts freely).
///
/// Call this inside a `ui.horizontal` (after the icon): the horizontal
/// bounds the row height (fact 2 above). The column is an explicit
/// `allocate_ui_with_layout` rather than `ui.vertical` because a plain
/// vertical here claims the full remaining width — its bounding box
/// reaches the left edge — which left the text column laid out after it
/// with ~0 px and a headline one word per line.
pub fn banner_row<B, T>(ui: &mut egui::Ui, buttons: Option<(f32, B)>, text: T)
where
    B: FnOnce(&mut egui::Ui),
    T: FnOnce(&mut egui::Ui),
{
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if let Some((width, buttons)) = buttons {
            let col_h = ui.available_height();
            ui.allocate_ui_with_layout(
                egui::vec2(width, col_h),
                egui::Layout::top_down(egui::Align::Max),
                buttons,
            );
            ui.add_space(sp::S);
        }
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), text);
    });
}
