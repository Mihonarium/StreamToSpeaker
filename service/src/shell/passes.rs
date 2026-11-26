//! What a window's content does over the passes egui runs in one frame.
//!
//! egui runs a frame a second time when its first pass asks to (the panel,
//! settings rows and cards ask when their parts change height), keeps the
//! second pass's drawing, and gives that pass no input: a key, a click or a
//! request to close the window comes to the first pass only, and
//! `show_viewport_immediate` returns only what the last pass returned. So
//! every window of the shell (the panel, a dialog, a small window by the
//! clock) gathers what each pass saw and sent in a [`Passes`], and acts on
//! it once, after the frame.

use super::{Ask, WindowContent};
use crate::model::command::Command;
use crate::model::Snapshot;

/// What a window's content did over the passes of one frame.
#[derive(Default)]
pub struct Passes {
    /// Its window was asked to close (its ×, Alt+F4), in some pass.
    pub asked_to_close: bool,
    /// The content was done in some pass: the panel's Esc, a dialog's
    /// Cancel or its work finished, a small window clicked.
    pub done: bool,
    /// What the content sends the engine, from every pass.
    pub commands: Vec<Command>,
    /// What the content asks of the shell, from every pass.
    pub asks: Vec<Ask>,
}

impl Passes {
    /// At the start of a pass over a window that can be asked to close: note
    /// the request, and keep eframe from closing the window (the shell
    /// closes it).
    pub fn notice_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.asked_to_close = true;
        }
    }

    /// At the end of a pass: what the content did in it. True for the pass
    /// whose drawing egui keeps, the last of the frame.
    pub fn end(&mut self, ctx: &egui::Context, done: bool, commands: Vec<Command>, asks: Vec<Ask>) -> bool {
        self.done |= done;
        self.commands.extend(commands);
        self.asks.extend(asks);
        !ctx.will_discard()
    }

    /// Whether the window goes after this frame: its content was done or its
    /// window was asked to close.
    pub fn ends_the_window(&self) -> bool {
        self.done || self.asked_to_close
    }
}

/// One pass over a dialog's window, which `content` draws.
pub fn dialog_pass(ctx: &egui::Context, passes: &mut Passes, content: &mut dyn WindowContent, snap: &Snapshot) {
    passes.notice_close(ctx);
    let mut asks = Vec::new();
    let done = content.show(ctx, snap, &mut |a| asks.push(a));
    passes.end(ctx, done, Vec::new(), asks);
}

#[cfg(test)]
mod tests {
    use super::super::popup::popup_pass;
    use super::*;
    use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, RawInput, ViewportEvent, ViewportId, ViewportInfo};

    fn input(events: Vec<Event>, close_request: bool) -> RawInput {
        let mut info = ViewportInfo { focused: Some(true), ..Default::default() };
        if close_request {
            info.events.push(ViewportEvent::Close);
        }
        RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(360.0, 260.0))),
            viewports: std::iter::once((ViewportId::ROOT, info)).collect(),
            events,
            ..Default::default()
        }
    }

    /// One frame whose first pass asks egui for a second, `pass` drawing the
    /// window in each.
    fn frame_twice(ctx: &egui::Context, input: RawInput, mut pass: impl FnMut(&egui::Context, &mut Passes)) -> Passes {
        let mut passes = Passes::default();
        let mut n = 0;
        let _ = ctx.run(input, |ctx| {
            n += 1;
            if n == 1 {
                ctx.request_discard("a part changed height");
            }
            pass(ctx, &mut passes);
        });
        assert_eq!(n, 2, "egui ran the frame twice");
        passes
    }

    /// A dialog that is done on Enter and asks to go back on Backspace.
    struct EnterDialog;

    impl WindowContent for EnterDialog {
        fn title(&self, _snap: &Snapshot) -> String {
            "A dialog".into()
        }
        fn sizes(&self) -> (egui::Vec2, egui::Vec2) {
            (egui::vec2(360.0, 260.0), egui::vec2(360.0, 260.0))
        }
        fn show(&mut self, ctx: &egui::Context, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) -> bool {
            egui::CentralPanel::default().show(ctx, |_ui| {});
            if ctx.input(|i| i.key_pressed(Key::Backspace)) {
                ask(Ask::Back);
            }
            ctx.input(|i| i.key_pressed(Key::Enter))
        }
    }

    fn key(key: Key) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
    }

    fn snap() -> std::sync::Arc<Snapshot> {
        crate::model::port::Port::snapshot(&crate::model::fake::ScriptedEngine::new("kitchen-playing"))
    }

    fn context() -> egui::Context {
        crate::ui::preview::context(crate::fonts::definitions(&[], &mut |_| None).0, crate::ui::theme::Mode::Light)
    }

    #[test]
    fn a_dialog_keeps_what_the_first_of_two_passes_saw() {
        let ctx = context();
        let snap = snap();
        let mut dialog = EnterDialog;
        // Its work finished (Enter).
        let p = frame_twice(&ctx, input(vec![key(Key::Enter)], false), |ctx, passes| dialog_pass(ctx, passes, &mut dialog, &*snap));
        assert!(p.done && !p.asked_to_close && p.ends_the_window());
        // Its × or Alt+F4.
        let p = frame_twice(&ctx, input(Vec::new(), true), |ctx, passes| dialog_pass(ctx, passes, &mut dialog, &*snap));
        assert!(p.asked_to_close && p.ends_the_window());
        // What it asked.
        let p = frame_twice(&ctx, input(vec![key(Key::Backspace)], false), |ctx, passes| dialog_pass(ctx, passes, &mut dialog, &*snap));
        assert!(matches!(p.asks.as_slice(), [Ask::Back]));
        assert!(!p.ends_the_window());
    }

    #[test]
    fn a_small_window_clicked_in_the_first_of_two_passes_goes() {
        let ctx = context();
        use super::super::popup::Slots;
        use crate::model::{fake::ScriptedEngine, samples, Command, Port, Surface};
        let port = ScriptedEngine::new("kitchen-playing");
        let mut slots = Slots::default();
        for (at, command) in [(Pos2::new(70.0, 92.0), samples::to_pc()), (Pos2::new(294.0, 22.0), Command::ClosePopup)] {
            let snap = port.snapshot();
            let popup = slots.current(&snap).unwrap();
            let click = vec![
                Event::PointerMoved(at),
                Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
                Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
            ];
            let p = frame_twice(&ctx, input(Vec::new(), false), |ctx, passes| { popup_pass(ctx, passes, popup, snap.lang); });
            assert!(!p.ends_the_window());
            let p = frame_twice(&ctx, input(click, false), |ctx, passes| { popup_pass(ctx, passes, popup, snap.lang); });
            assert!(p.ends_the_window());
            assert_eq!(p.commands, [command]);
            slots.act(&port, p.commands.into_iter().next().unwrap());
            assert!(slots.current(&snap).is_none());
            let mut next = snap.scene.clone();
            next.situation.popups.popup.as_mut().unwrap().seq += 1;
            port.publish(next);
        }
        assert_eq!(port.received(), [(Surface::Notification, samples::to_pc()), (Surface::Notification, Command::ClosePopup)]);
    }
}
