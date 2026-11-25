//! The small windows that must not take the focus: the confirmation by the
//! clock (after a delay shortcut) and a notification. Each is a window of
//! its own, on top, off the taskbar and out of Alt+Tab, in the corner by
//! the clock, shown without being activated, so the window someone types
//! in keeps the keyboard. Their contents come later; a stand-in shows one
//! line.

use egui::{Ui, Vec2, ViewportBuilder, WindowLevel};

use super::placement::{self, Monitor, PxRect};
use super::Ask;
use crate::model::snapshot::Snapshot;
use crate::ui::text::Para;
use crate::ui::theme::size;

/// Which slot a small window goes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PopupKind {
    /// The confirmation by the clock: after a shortcut changed the delay.
    Confirmation,
    /// A notification: sound left the place chosen, a speaker is back.
    Notification,
}

impl PopupKind {
    /// Its window's title: what a screen reader calls it.
    pub fn title(self) -> &'static str {
        match self {
            PopupKind::Confirmation => "Stream To Speaker confirmation",
            PopupKind::Notification => "Stream To Speaker notification",
        }
    }
}

/// What a small window that must not take the focus shows.
pub trait PopupContent {
    /// Its size, in points.
    fn size(&self) -> Vec2;
    /// Draw it; true once it should go (its time is up, or it was
    /// clicked). `now` is in seconds.
    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, now: f64, ask: &mut dyn FnMut(Ask)) -> bool;
}

/// How another part of the app shows one: `door(kind, content)`; a newer
/// one of the same kind replaces the one showing.
pub type PopupDoor = Box<dyn Fn(PopupKind, Box<dyn PopupContent + Send>) + Send + Sync>;

/// The window for a popup of `kind`, for a frame: hidden until it is placed
/// (`shown` false), then shown without being activated. Its builder says
/// what is wanted now, as the panel's does (see
/// [`super::panel_window::PanelWindow::builder`]): eframe applies it again
/// every frame.
pub fn popup_builder(kind: PopupKind, size: Vec2, shown: bool) -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(kind.title())
        .with_inner_size(size)
        .with_visible(shown)
        .with_active(false)
        .with_decorations(false)
        .with_resizable(false)
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_taskbar(false)
}

/// One pass over a small window: `content` drawn in its card. Whether it
/// is done (clicked, or its time up) is gathered in `passes` over the
/// frame's passes (see [`super::passes`]).
pub fn popup_pass(ctx: &egui::Context, passes: &mut super::passes::Passes, content: &mut dyn PopupContent, snap: &Snapshot, now: f64) {
    let t = crate::ui::theme::Theme::of(ctx);
    let done = egui::CentralPanel::default()
        .frame(egui::Frame::none().fill(t.c.surface).stroke(t.card_stroke()))
        .show(ctx, |ui| content.show(ui, snap, now, &mut |_| {}))
        .inner;
    passes.end(ctx, done, Vec::new(), Vec::new());
}

/// Where a popup of `size` points goes: the corner by the clock, on the
/// monitor of the tray icon; above `under` when another popup or the panel
/// is already there.
pub fn popup_rect(monitors: &[Monitor], icon: Option<PxRect>, size: Vec2, under: Option<PxRect>) -> Option<PxRect> {
    let p = placement::place(monitors, icon, (size.x, size.y))?;
    let Some(under) = under else { return Some(p.rect) };
    let gap = (placement::GAP * p.scale).round() as i32;
    let h = p.rect.height();
    // Above what is there, if it fits; else beside it, toward the middle.
    let m = placement::monitor_for(monitors, icon)?;
    if under.top - gap - h >= m.work.top + gap {
        let bottom = under.top - gap;
        Some(PxRect::new(p.rect.left, bottom - h, p.rect.right, bottom))
    } else {
        let right = under.left - gap;
        Some(PxRect::new(right - p.rect.width(), p.rect.top, right, p.rect.bottom))
    }
}

/// A stand-in: one line, for a few seconds.
pub struct StandInPopup {
    pub kind: PopupKind,
    pub words: String,
    /// When it goes, in seconds; set on its first frame.
    until: Option<f64>,
}

impl StandInPopup {
    pub fn new(kind: PopupKind, words: impl Into<String>) -> Self {
        StandInPopup { kind, words: words.into(), until: None }
    }

    /// How long it shows, in seconds.
    pub fn lasts(&self) -> f64 {
        match self.kind {
            PopupKind::Confirmation => 3.0,
            PopupKind::Notification => 8.0,
        }
    }
}

impl PopupContent for StandInPopup {
    fn size(&self) -> Vec2 {
        match self.kind {
            PopupKind::Confirmation => Vec2::new(280.0, 72.0),
            PopupKind::Notification => Vec2::new(360.0, 96.0),
        }
    }

    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, now: f64, _ask: &mut dyn FnMut(Ask)) -> bool {
        let until = *self.until.get_or_insert(now + self.lasts());
        egui::Frame::none().inner_margin(egui::Margin::symmetric(16.0, 12.0)).show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 2.0;
                let _ = Para::new("").lead(&snap.short_status).size(size::CONTROL).show(ui);
                let _ = Para::new(&self.words).size(size::SECONDARY).show(ui);
            });
        });
        let clicked = ui.input(|i| i.pointer.any_click());
        clicked || now >= until
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> Monitor {
        Monitor { bounds: PxRect::new(0, 0, 1920, 1080), work: PxRect::new(0, 0, 1920, 1032), scale: 1.0, primary: true }
    }

    #[test]
    fn a_popup_sits_in_the_corner_by_the_clock_and_never_takes_the_focus() {
        let r = popup_rect(&[monitor()], None, Vec2::new(280.0, 72.0), None).unwrap();
        assert_eq!(r, PxRect::new(1920 - 12 - 280, 1032 - 12 - 72, 1920 - 12, 1032 - 12));
        let b = popup_builder(PopupKind::Confirmation, Vec2::new(280.0, 72.0), false);
        assert_eq!((b.active, b.visible, b.taskbar, b.decorations), (Some(false), Some(false), Some(false), Some(false)));
        assert_eq!(b.window_level, Some(WindowLevel::AlwaysOnTop));
        assert_ne!(PopupKind::Confirmation.title(), super::super::PANEL_TITLE);
    }

    #[test]
    fn a_second_popup_or_the_panel_pushes_it_above() {
        let panel = PxRect::new(1548, 500, 1908, 1020);
        let r = popup_rect(&[monitor()], None, Vec2::new(360.0, 96.0), Some(panel)).unwrap();
        assert_eq!((r.bottom, r.height()), (500 - 12, 96));
        // No room above a tall panel: beside it.
        let tall = PxRect::new(1548, 20, 1908, 1020);
        let r = popup_rect(&[monitor()], None, Vec2::new(360.0, 96.0), Some(tall)).unwrap();
        assert_eq!(r.right, 1548 - 12);
    }

    #[test]
    fn the_stand_in_goes_after_its_time() {
        let snap = crate::model::port::Port::snapshot(&crate::model::scripted::ScriptedEngine::new(crate::model::scripted::Scenario::Playing));
        let ctx = crate::ui::preview::context(crate::fonts::definitions(&[], &mut |_| None).0, crate::ui::theme::Mode::Light);
        let mut p = StandInPopup::new(PopupKind::Confirmation, "50 ms removed");
        let mut run = |now: f64| {
            let mut gone = false;
            let _ = ctx.run(crate::ui::preview::input(p.size(), 1.0, now), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| gone = p.show(ui, &snap, now, &mut |_| {}));
            });
            gone
        };
        assert!(!run(10.0));
        assert!(!run(12.9));
        assert!(run(13.0));
    }
}
