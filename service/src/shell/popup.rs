//! The small windows that must not take the focus: the confirmation by the
//! clock (before switching or playing a chime) and a notification. Each is a window of
//! its own, on top, off the taskbar and out of Alt+Tab, in the corner by
//! the clock, shown without being activated, so the window someone types
//! in keeps the keyboard. Their contents and lifetime come from the published view.

use egui::{Ui, Vec2, ViewportBuilder, WindowLevel};

use super::placement::{self, Monitor, PxRect};
use crate::model::{Command, Surface};
#[cfg(test)]
use crate::model::Port;
use crate::model::panel::Act;
use crate::model::text::t;
use crate::model::Snapshot;
use crate::ui::{button::{Button, IconButton}, icon::Icon, theme::Theme};

/// Which slot a small window goes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PopupKind {
    /// The confirmation by the clock: before switching or playing a chime.
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

pub const WIDTH: f32 = 328.0;

#[derive(Clone, Copy)]
pub struct Contents<'a> {
    pub kind: PopupKind,
    pub seq: u64,
    title: &'a str,
    sentence: Option<&'a str>,
    yes: Option<&'a Act>,
    no: Option<&'a Act>,
    link: Option<&'a Act>,
    close: &'a Command,
}

impl<'a> Contents<'a> {
    pub fn from_view(view: &'a Snapshot) -> Option<Self> {
        if let Some(q) = &view.confirmation {
            let v = &q.what;
            return Some(Self { kind: PopupKind::Confirmation, seq: q.seq, title: &v.title,
                sentence: Some(&v.sentence), yes: Some(&v.yes), no: Some(&v.no), link: None, close: &v.no.command });
        }
        let p = view.popup.as_ref()?;
        let v = &p.what;
        Some(Self { kind: PopupKind::Notification, seq: p.seq, title: &v.title,
            sentence: v.sentence.as_deref(), yes: v.button.as_ref(), no: None, link: v.link.as_ref(), close: &v.close.command })
    }
}

/// Keep a clicked or withdrawn publication from reopening on a repaint.
#[derive(Default)]
pub struct Slots {
    last: [Option<u64>; 2],
    active: Option<(PopupKind, u64)>,
}

impl Slots {
    pub fn current<'a>(&mut self, view: &'a Snapshot) -> Option<Contents<'a>> {
        let candidate = Contents::from_view(view);
        let key = candidate.map(|p| (p.kind, p.seq));
        if self.active != key {
            self.active = key.filter(|(kind, seq)| self.last[*kind as usize].is_none_or(|last| *seq > last));
            if let Some((kind, seq)) = self.active {
                self.last[kind as usize] = Some(seq);
            }
        }
        candidate.filter(|_| self.active.is_some())
    }

    pub fn take_action(&mut self, command: Command) -> Option<(Surface, Command)> {
        self.active.take().map(|_| (Surface::Notification, command))
    }

    #[cfg(test)]
    pub fn act(&mut self, port: &dyn crate::model::Port, command: Command) {
        if let Some((from, command)) = self.take_action(command) { port.send(from, command); }
    }
}

/// The same content-sized card in the native window and the preview gallery.
pub fn card(ui: &mut Ui, content: Contents<'_>, lang: crate::model::Lang) -> (f32, Option<Command>) {
    let t = Theme::of(ui.ctx());
    let mut clicked = None;
    let frame = egui::Frame::none().fill(t.c.surface).stroke(t.card_stroke()).rounding(8.0)
        .inner_margin(egui::Margin { left: 15.0, right: 15.0, top: 6.0, bottom: 15.0 });
    let response = frame.show(ui, |ui| {
        ui.set_width(WIDTH - 32.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.allocate_ui_with_layout(Vec2::new(WIDTH - 32.0, 32.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let image = super::icons::app_mark(ui);
            ui.add(egui::Image::new((image.id(), Vec2::splat(16.0))).tint(t.c.text));
            ui.label(egui::RichText::new(t!(lang, AppName)).font(t.font(13.0)).color(t.c.text_secondary));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if IconButton::new(Icon::Close, &t!(lang, Close)).size(32.0, 14.0).color(t.c.text_secondary).show(ui).clicked() {
                    clicked = Some(content.close.clone());
                }
            });
        });
        ui.add_space(3.0);
        ui.add(egui::Label::new(egui::RichText::new(content.title).font(t.strong(15.0)).color(t.c.text)).wrap());
        if let Some(sentence) = content.sentence {
            ui.add_space(2.0);
            ui.scope(|ui| {
                if content.link.is_some() { ui.set_max_width(214.0); }
                ui.add(egui::Label::new(egui::RichText::new(sentence).font(t.font(14.0)).color(t.c.text_secondary)).wrap());
            });
        }
        if let Some(link) = content.link {
            ui.add_space(10.0);
            crate::ui::look::follow_input(ui.ctx());
            if Button::link(&link.label).text_size(14.0).show(ui).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                clicked = Some(link.command.clone());
            }
        }
        if let Some(first) = content.yes {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let reserve = content.no.map_or(0.0, |act| {
                    ui.painter().layout_no_wrap(act.label.clone(), t.font(14.0), t.c.text).size().x + 2.0 * crate::ui::theme::space::M + 8.0
                });
                for (act, room) in [(Some(first), ui.available_width() - reserve), (content.no, reserve - 8.0)] {
                    if let Some(a) = act {
                        if Button::standard(&a.label).text_size(14.0).max_width(room).accessible_name(a.spoken.as_ref().unwrap_or(&a.label)).show(ui).clicked() {
                            clicked = Some(a.command.clone());
                        }
                    }
                }
            });
        }
    });
    (response.response.rect.height(), clicked)
}

pub fn popup_pass(ctx: &egui::Context, passes: &mut super::passes::Passes, content: Contents<'_>, lang: crate::model::Lang) -> f32 {
    passes.notice_close(ctx);
    let (height, action) = egui::CentralPanel::default().frame(egui::Frame::none())
        .show(ctx, |ui| card(ui, content, lang)).inner;
    let action = action.or_else(|| passes.asked_to_close.then(|| content.close.clone()));
    passes.end(ctx, action.is_some(), action.into_iter().collect(), Vec::new());
    height
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publications_replace_withdraw_and_give_priority_to_the_question() {
        use crate::model::{fake::ScriptedEngine, samples};
        let engine = ScriptedEngine::new("kitchen-playing");
        let mut slots = Slots::default();
        let mut scene = engine.snapshot().scene.clone();
        scene.situation.popups.confirmation = samples::named("question").situation.popups.confirmation;
        engine.publish(scene.clone());
        let v = engine.snapshot();
        let q = slots.current(&v).unwrap();
        assert_eq!(q.kind, PopupKind::Confirmation);
        slots.act(&engine, q.close.clone());
        assert!(slots.current(&v).is_none());
        scene.situation.popups.confirmation = None;
        engine.publish(scene.clone());
        let v = engine.snapshot();
        let p = slots.current(&v).unwrap();
        assert_eq!(p.kind, PopupKind::Notification);
        slots.act(&engine, p.yes.unwrap().command.clone());
        assert!(slots.current(&v).is_none());
        scene.situation.popups.popup.as_mut().unwrap().seq += 1;
        engine.publish(scene.clone());
        let v = engine.snapshot();
        assert_eq!(slots.current(&v).unwrap().seq, p.seq + 1);
        scene.situation.popups.popup.as_mut().unwrap().seq += 1;
        engine.publish(scene.clone());
        let v = engine.snapshot();
        let newer = slots.current(&v).unwrap();
        assert_eq!(newer.seq, p.seq + 2);
        slots.act(&engine, newer.close.clone());
        scene.situation.popups.popup.as_mut().unwrap().seq += 1;
        engine.publish(scene.clone());
        assert!(slots.current(&engine.snapshot()).is_some());
        let previous = scene.situation.popups.popup.clone();
        scene.situation.popups.popup = None;
        engine.publish(scene.clone());
        assert!(slots.current(&engine.snapshot()).is_none());
        scene.situation.popups.popup = previous;
        scene.situation.popups.popup.as_mut().unwrap().seq = p.seq;
        engine.publish(scene);
        assert!(slots.current(&engine.snapshot()).is_none());
        assert_eq!(engine.received(), [
            (Surface::Notification, Command::Confirm { yes: false }),
            (Surface::Notification, samples::to_pc()),
            (Surface::Notification, Command::ClosePopup),
        ]);
    }

    #[test]
    fn clicking_the_first_close_link_quits_from_the_notification() {
        use crate::model::fake::ScriptedEngine;
        use crate::ui::{preview, theme::Mode};
        for (sample, expected) in [("kitchen-playing", "Quit, and play the sound on this PC"), ("pc-plain", "Quit the app")] {
            let port = ScriptedEngine::new(sample);
            port.send(Surface::Panel, Command::PanelShown(true));
            port.send(Surface::Panel, Command::PanelShown(false));
            port.send(Surface::Panel, Command::WindowClosed);
            let snap = port.snapshot();
            let mut slots = Slots::default();
            let content = slots.current(&snap).unwrap();
            assert_eq!(content.link.unwrap().label, expected);
            let ctx = preview::context(crate::fonts::definitions(&[], &mut |_| None).0, Mode::Light);
            let input = || preview::input(egui::vec2(WIDTH, 220.0), 1.0, 0.0);
            let mut passes = super::super::passes::Passes::default();
            let draw = |ctx: &egui::Context, p: &mut super::super::passes::Passes| { popup_pass(ctx, p, content, snap.lang); };
            let output = ctx.run(input(), |ctx| draw(ctx, &mut passes));
            let pos = output.shapes.iter().find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text() == expected => Some(t.pos + t.galley.size()/2.0),
                _ => None,
            }).unwrap();
            let mut click = input();
            click.events.push(egui::Event::PointerMoved(pos));
            for pressed in [true, false] {
                click.events.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
            }
            let _ = ctx.run(click, |ctx| draw(ctx, &mut passes));
            assert_eq!(passes.commands, [Command::Quit]);
            let (from, action) = slots.take_action(passes.commands.pop().unwrap()).unwrap();
            port.send(from, action);
            assert_eq!(port.received().last(), Some(&(Surface::Notification, Command::Quit)));
            assert!(slots.current(&snap).is_none());
            port.send(Surface::Panel, Command::PanelShown(true));
            port.send(Surface::Panel, Command::WindowClosed);
            assert!(port.snapshot().popup.is_none());
        }
    }

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

}
