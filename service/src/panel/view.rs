//! A view of the panel, in place of the everyday content and at the same
//! width: a bar (the back arrow, a title, a second line, one button), the
//! view's content, which scrolls when it is long, and a line of links at
//! the bottom. Going back is the frame's: the arrow, Esc, Backspace outside
//! a field, Alt+Left.

use egui::{Event, Key, Margin, Rect, Sense, Ui, UiBuilder, Vec2};

use super::PAD_X;
use crate::model::snapshot::Snapshot;
use crate::model::text::t;
use crate::shell::{Ask, ViewContent};
use crate::ui::button::{Button, IconButton};
use crate::ui::icon::Icon;
use crate::ui::text::Para;
use crate::ui::theme::{size, Theme};

/// What the frame keeps between frames.
#[derive(Clone, Debug, Default)]
pub struct ViewState {
    /// The most the view may take, in points (the window's room); its
    /// content scrolls beyond it. `None`: as tall as it needs.
    pub cap: Option<f32>,
    /// Put the keyboard on the back arrow (when the view opens).
    pub focus_back: bool,
    /// The parts' heights in the last pass: bar, content, links.
    heights: [f32; 3],
}

impl ViewState {
    /// A view that has just opened: the keyboard on its back arrow.
    pub fn opened() -> Self {
        ViewState { focus_back: true, ..Default::default() }
    }
}

/// What a frame of a view asks.
#[derive(Default)]
pub struct ViewOutput {
    /// What the view asked, in order.
    pub asks: Vec<Ask>,
    /// Go back one view.
    pub back: bool,
    /// The height the view needs with its content in full.
    pub natural_height: f32,
}

/// Whether the keys that go back were pressed: Esc, Alt+Left, and
/// Backspace while no field takes the keys.
fn back_keys(ui: &Ui) -> bool {
    let typing = ui.ctx().wants_keyboard_input();
    ui.input(|i| {
        i.events.iter().any(|e| match e {
            Event::Key { key: Key::Escape, pressed: true, .. } => true,
            Event::Key { key: Key::ArrowLeft, pressed: true, modifiers, .. } => modifiers.alt,
            Event::Key { key: Key::Backspace, pressed: true, .. } => !typing,
            _ => false,
        })
    })
}

/// Draw `view` into `ui`, which is the panel's width, at `depth` (how many
/// views are under it: each keeps its own scroll position).
pub fn show_view(ui: &mut Ui, view: &mut dyn ViewContent, snap: &Snapshot, st: &mut ViewState, depth: usize) -> ViewOutput {
    let mut out = ViewOutput::default();
    crate::ui::look::follow_input(ui.ctx());
    let t = Theme::of(ui.ctx());
    let width = ui.available_width();
    let origin = ui.cursor().min;
    let [bar_h, content_h_last, links_h] = st.heights;
    let has_links = view.has_links(snap);
    let links_room = if has_links { links_h + 1.0 } else { 0.0 };
    let down = egui::Layout::top_down(egui::Align::Min);
    let mut asks: Vec<Ask> = Vec::new();

    // The bar: the title and its line, with the arrow and the button on
    // their middle.
    let bar = view.bar(snap);
    let mut back = false;
    let mut pressed = false;
    let bar_used = {
        let top = origin.y + 10.0;
        let left = origin.x + 6.0;
        let right = origin.x + width - PAD_X;
        let button_w = bar.button.as_ref().map(|b| {
            let font = t.font(size::CONTROL);
            (ui.fonts(|f| f.layout_no_wrap(b.clone(), font, t.c.text).size().x) + 2.0 * 12.0).min(width * 0.45)
        });
        let title_x = left + 32.0 + 6.0;
        let title_w = (right - title_x - button_w.map_or(0.0, |w| w + 10.0)).max(60.0);
        let mut title_ui = ui.new_child(UiBuilder::new().id_salt(("view-title", depth)).max_rect(Rect::from_min_size(egui::pos2(title_x, top), Vec2::new(title_w, 10_000.0))).layout(down));
        title_ui.spacing_mut().item_spacing.y = 0.0;
        let _ = Para::new("").lead(&bar.title).size(size::TITLE).show(&mut title_ui);
        if let Some(line) = &bar.line {
            let _ = Para::new(line).size(size::SECONDARY).show(&mut title_ui);
        }
        let title_h = title_ui.min_rect().height().max(32.0);
        let mid = top + title_h / 2.0;
        let back_word = t!(snap.lang, SetBackButton);
        let arrow = crate::ui::inside(ui, Rect::from_min_size(egui::pos2(left, mid - 16.0), Vec2::splat(32.0)), |ui: &mut Ui| {
            IconButton::new(Icon::ChevronLeft, &back_word).size(32.0, 16.0).show(ui)
        });
        if st.focus_back {
            arrow.request_focus();
            st.focus_back = false;
        }
        if arrow.clicked() {
            back = true;
        }
        if let (Some(label), Some(w)) = (&bar.button, button_w) {
            let r = Rect::from_min_size(egui::pos2(right - w, mid - 16.0), Vec2::new(w, 32.0));
            if crate::ui::inside(ui, r, |ui: &mut Ui| Button::standard(label).max_width(w).show(ui)).clicked() {
                pressed = true;
            }
        }
        title_h + 20.0
    };
    if pressed {
        view.bar_button(snap, &mut |a| asks.push(a));
    }
    ui.painter().rect_filled(Rect::from_min_size(egui::pos2(origin.x, origin.y + bar_h), Vec2::new(width, 1.0)), 0.0, t.c.divider);

    // The content: all the room the cap leaves, scrolling beyond it.
    let content_top = origin.y + bar_h + 1.0;
    let room = st.cap.map(|cap| (cap - bar_h - 1.0 - links_room).max(60.0));
    let mut content_ui = ui.new_child(
        UiBuilder::new()
            .id_salt(("view-content", depth))
            .max_rect(Rect::from_min_size(egui::pos2(origin.x, content_top), Vec2::new(width, room.unwrap_or(10_000.0))))
            .layout(down),
    );
    let draw = |ui: &mut Ui, view: &mut dyn ViewContent, asks: &mut Vec<Ask>| -> f32 {
        egui::Frame::none()
            .inner_margin(Margin { left: PAD_X, right: PAD_X, top: 12.0, bottom: 14.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                view.show(ui, snap, &mut |a| asks.push(a));
            })
            .response
            .rect
            .height()
    };
    let content_used = match room {
        Some(room) => {
            egui::ScrollArea::vertical().id_salt(("view-scroll", depth)).max_height(room).auto_shrink([false, true]).show(&mut content_ui, |ui| draw(ui, view, &mut asks)).inner
        }
        None => draw(&mut content_ui, view, &mut asks),
    };
    let content_shown = room.map_or(content_used, |r| content_used.min(r));

    // The links.
    let mut links_used = 0.0;
    if has_links {
        let links_top = content_top + content_shown;
        ui.painter().rect_filled(Rect::from_min_size(egui::pos2(origin.x, links_top), Vec2::new(width, 1.0)), 0.0, t.c.divider);
        let mut links_ui = ui.new_child(UiBuilder::new().id_salt(("view-links", depth)).max_rect(Rect::from_min_size(egui::pos2(origin.x, links_top + 1.0), Vec2::new(width, 10_000.0))).layout(down));
        let start = links_ui.cursor().min.y;
        egui::Frame::none().inner_margin(Margin { left: PAD_X, right: PAD_X, top: 8.0, bottom: 10.0 }).show(&mut links_ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(18.0, 6.0);
                view.links(ui, snap, &mut |a| asks.push(a));
            });
        });
        links_used = links_ui.cursor().min.y - start;
    }

    let measured = [bar_used, content_used, links_used];
    if measured.iter().zip(st.heights.iter()).any(|(a, b)| (a - b).abs() > 0.5) {
        st.heights = measured;
        ui.ctx().request_discard("a view's parts changed height");
    }
    let _ = content_h_last;
    out.natural_height = bar_used + 1.0 + content_used + if has_links { 1.0 + links_used } else { 0.0 };
    let total = bar_h + 1.0 + content_shown + links_room;
    ui.allocate_rect(Rect::from_min_size(origin, Vec2::new(width, total)), Sense::hover());

    out.back = back || back_keys(ui);
    out.asks = asks;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::port::Port;
    use crate::model::scripted::{Scenario, ScriptedEngine};
    use crate::shell::ViewBar;
    use crate::ui::preview;
    use crate::ui::theme::Mode;
    use egui::Modifiers;

    /// A view with a field, a button in its bar and a link.
    #[derive(Default)]
    struct Probe {
        typed: String,
        bar_presses: u32,
        rows: usize,
        focus_field: bool,
    }

    impl ViewContent for Probe {
        fn bar(&self, _snap: &Snapshot) -> ViewBar {
            ViewBar { title: "Kitchen".into(), line: Some("Sonos · playing now".into()), button: Some("Test sound".into()) }
        }
        fn bar_button(&mut self, _snap: &Snapshot, _ask: &mut dyn FnMut(Ask)) {
            self.bar_presses += 1;
        }
        fn show(&mut self, ui: &mut Ui, _snap: &Snapshot, _ask: &mut dyn FnMut(Ask)) {
            let field = ui.add(egui::TextEdit::singleline(&mut self.typed));
            if std::mem::take(&mut self.focus_field) {
                field.request_focus();
            }
            for i in 0..self.rows {
                ui.label(format!("Line {i}"));
            }
        }
        fn has_links(&self, _snap: &Snapshot) -> bool {
            true
        }
        fn links(&mut self, ui: &mut Ui, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
            if ui.button("Details").clicked() {
                ask(Ask::Back);
            }
        }
        fn holds_unused_text(&self) -> bool {
            !self.typed.is_empty()
        }
    }

    struct H {
        ctx: egui::Context,
        snap: std::sync::Arc<Snapshot>,
        st: ViewState,
        view: Probe,
        frame: u32,
    }

    impl H {
        fn new(view: Probe) -> Self {
            let ctx = preview::context(crate::fonts::definitions(&[], &mut |_| None).0, Mode::Light);
            H { ctx, snap: ScriptedEngine::new(Scenario::Playing).snapshot(), st: ViewState::opened(), view, frame: 0 }
        }

        fn run(&mut self, events: Vec<egui::Event>) -> (bool, f32) {
            self.frame += 1;
            let mut input = preview::input(Vec2::new(crate::panel::WIDTH, 900.0), 1.0, self.frame as f64 * 0.05);
            input.events = events;
            let (snap, st, view) = (self.snap.clone(), &mut self.st, &mut self.view);
            let mut res = (false, 0.0);
            let _ = self.ctx.run(input, |ctx| {
                egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
                    let o = show_view(ui, view, &snap, st, 0);
                    res = (o.back, o.natural_height);
                });
            });
            res
        }

        fn key(&mut self, key: Key, modifiers: Modifiers) -> bool {
            let ev = |pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
            let a = self.run(vec![ev(true)]).0;
            let b = self.run(vec![ev(false)]).0;
            a || b
        }

        fn settle(&mut self) -> f32 {
            let mut h = 0.0;
            for _ in 0..4 {
                h = self.run(vec![]).1;
            }
            h
        }
    }

    #[test]
    fn esc_alt_left_and_backspace_outside_a_field_go_back() {
        let mut h = H::new(Probe::default());
        h.settle();
        assert!(h.key(Key::Escape, Modifiers::NONE));
        assert!(h.key(Key::ArrowLeft, Modifiers::ALT));
        assert!(!h.key(Key::ArrowLeft, Modifiers::NONE), "Left alone stays");
        assert!(h.key(Key::Backspace, Modifiers::NONE), "the keyboard is on the back arrow, not a field");
        // In the field, Backspace deletes.
        h.view.focus_field = true;
        h.view.typed = "Kitch".into();
        h.settle();
        assert!(h.ctx.wants_keyboard_input());
        assert!(!h.key(Key::Backspace, Modifiers::NONE), "Backspace in a field edits it");
        assert_eq!(h.view.typed, "Kitc");
        assert!(h.view.holds_unused_text());
    }

    #[test]
    fn the_view_opens_with_the_keyboard_on_its_back_arrow() {
        let mut h = H::new(Probe::default());
        h.settle();
        assert!(h.ctx.memory(|m| m.focused()).is_some());
        assert!(h.key(Key::Enter, Modifiers::NONE), "Enter on the arrow goes back");
    }

    #[test]
    fn its_content_scrolls_within_the_cap_and_the_height_is_what_it_needs() {
        let mut h = H::new(Probe { rows: 60, ..Default::default() });
        let natural = h.settle();
        assert!(natural > 900.0, "{natural}");
        h.st.cap = Some(400.0);
        let capped = h.settle();
        assert_eq!(capped, natural, "the need doesn't change with the cap");
        let mut h = H::new(Probe { rows: 2, ..Default::default() });
        let small = h.settle();
        assert!(small < 300.0, "{small}");
    }
}
