//! The panel by the clock: where this PC's sound plays, and the switch that
//! moves it.
//!
//! The panel draws the model's [`Snapshot`] and nothing else: every word
//! comes from it, every control sends the command the snapshot gives it,
//! and the rows keep the snapshot's order. It holds only what belongs to the
//! panel as a view: whether the delay section or the folded speakers are
//! open, which row has the keyboard, and its height while it is open.
//!
//! Nothing moves under the pointer. The top keeps its height in every
//! everyday state; the panel keeps the height it opened at, so whatever
//! opens (the delay's buttons, a notice, a row's question) takes its room
//! from the list, which scrolls.
//!
//! The keyboard follows the order people expect from Windows' own flyouts:
//! the panel opens with the keyboard on the current row; ↑ and ↓ move
//! between rows, Enter plays, Shift+F10 or a right-click opens a row's menu,
//! typing a name jumps to it; Tab goes on to the top's buttons, the volume,
//! the delay, the notice and the footer; Esc closes; Ctrl+, opens Settings.

pub mod scenes;
pub mod view;

use egui::{Color32, Event, Id, Key, Margin, Pos2, Rect, Sense, Ui, UiBuilder, Vec2};

use crate::model::command::{Command, DelayStep};
use crate::model::delay::{DelayBody, PanelDelay};
use crate::model::places::{Row, RowIcon, RowStatus};
use crate::model::snapshot::{Button as ModelButton, ButtonStyle, FirstRunWords, Snapshot, Third, TitleIcon, Tone as ModelTone, VolumeRow};
use crate::model::surfaces::Notice as ModelNotice;
use crate::ui::button::{Button, IconButton};
use crate::ui::delay::{delay_line, Direction, KeyStep, LineAction, LineNote, LineWords, StepWords};
use crate::ui::icon::Icon;
use crate::ui::inputs::Switch;
use crate::ui::menu::{menu_separator, open_menu, show_menu, MenuItem};
use crate::ui::notice::{notice_line, Notice};
use crate::ui::row::{PlaceRow, Problem, RowAction, Status, ROW_STYLE};
use crate::ui::text::{Mark, Para, ParaClick, Tone};
use crate::ui::theme::{size, Theme};
use crate::ui::volume::{volume_row, VolumeAction, VolumeView};

/// The panel's width, in points.
pub const WIDTH: f32 = 360.0;

/// Padding of the top, left and right.
const PAD_X: f32 = 16.0;

/// What the panel keeps between frames.
#[derive(Clone, Debug, Default)]
pub struct PanelState {
    /// The delay line: open or closed, and the room its line keeps.
    pub delay: crate::ui::delay::DelayState,
    /// *More speakers (12)* opened.
    pub more_open: bool,
    /// The height the panel opened at; it keeps it while open. `None` draws
    /// it at the height it needs.
    pub height: Option<f32>,
    /// Put the keyboard on the current row (when the panel opens).
    pub focus_current_row: bool,
    /// A row the keyboard is moving to; it takes the focus once it is the
    /// list's Tab stop.
    pending_focus: Option<Place>,
    /// The rows' widget ids in the last pass, to give one the focus before
    /// it is drawn (so the move is announced like a Tab).
    row_ids: Vec<(Place, Id)>,
    /// The row Tab stops on: the one the keyboard was last on.
    roving: Option<Place>,
    /// What was typed to find a row, and when.
    typed: (String, f64),
    /// The parts' heights in the last pass: top, list, bottom.
    heights: [f32; 3],
}

impl PanelState {
    /// The state of a panel that has just opened: everything closed, the
    /// keyboard on the current row.
    pub fn opened() -> Self {
        PanelState { focus_current_row: true, ..Default::default() }
    }
}

/// What the panel asks of its window and of the engine.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelOutput {
    /// Commands to send, in order.
    pub commands: Vec<Command>,
    /// Esc: close the panel.
    pub close: bool,
    /// The height the panel needs with its list in full: what it opens at
    /// (the window caps it to the display).
    pub natural_height: f32,
}

type Place = crate::model::state::Place;

/// Draw the panel into `ui`, which is `WIDTH` wide. `level` is the meter's
/// level now (0–1).
pub fn show(ui: &mut Ui, snap: &Snapshot, level: f32, st: &mut PanelState) -> PanelOutput {
    let mut out = PanelOutput::default();
    crate::ui::look::follow_input(ui.ctx());
    let t = Theme::of(ui.ctx());
    let width = ui.available_width();
    let origin = ui.cursor().min;
    let [top_h, list_h_last, bottom_h] = st.heights;

    // The list's room: all of the fixed height the top and bottom leave, or
    // what the list needs.
    let list_room = match st.height {
        Some(h) => (h - top_h - 1.0 - bottom_h).max(40.0),
        None => f32::INFINITY,
    };

    // Drawn list first, then the top, then the bottom, though they sit top,
    // list, bottom: the keyboard's Tab order follows the order things are
    // made, and from the list Tab goes on to the top.
    let list_top = origin.y + top_h + 1.0;
    let list_rect = Rect::from_min_size(egui::pos2(origin.x, list_top), Vec2::new(width, if list_room.is_finite() { list_room } else { 10_000.0 }));
    let mut list_ui = ui.new_child(UiBuilder::new().id_salt("panel-list").max_rect(list_rect).layout(egui::Layout::top_down(egui::Align::Min)));
    let list_used = draw_list(&mut list_ui, snap, st, list_room, &mut out);

    let down = egui::Layout::top_down(egui::Align::Min);
    let mut top_ui = ui.new_child(UiBuilder::new().id_salt("panel-top").max_rect(Rect::from_min_size(origin, Vec2::new(width, 10_000.0))).layout(down));
    let top_used = draw_top(&mut top_ui, snap, level, st, &mut out);

    let list_shown = if list_room.is_finite() { list_room } else { list_used };
    let bottom_top = list_top + list_shown;
    let mut bottom_ui = ui.new_child(UiBuilder::new().id_salt("panel-bottom").max_rect(Rect::from_min_size(egui::pos2(origin.x, bottom_top), Vec2::new(width, 10_000.0))).layout(down));
    let bottom_used = draw_bottom(&mut bottom_ui, snap, &mut out);

    // The hairline between the top and the list.
    ui.painter().rect_filled(Rect::from_min_size(egui::pos2(origin.x, origin.y + top_h), Vec2::new(width, 1.0)), 0.0, t.c.divider);

    let measured = [top_used, list_used, bottom_used];
    if measured.iter().zip(st.heights.iter()).any(|(a, b)| (a - b).abs() > 0.5) {
        st.heights = measured;
        ui.ctx().request_discard("the panel's parts changed height");
    }
    let _ = list_h_last;
    out.natural_height = top_used + 1.0 + list_used + bottom_used;
    let total = top_h + 1.0 + list_shown + bottom_h;
    ui.allocate_rect(Rect::from_min_size(origin, Vec2::new(width, total)), Sense::hover());

    // Keys for the whole panel.
    let (esc, settings) = ui.input(|i| {
        let ctrl_comma = i.events.iter().any(|e| matches!(e, Event::Key { key: Key::Comma, pressed: true, modifiers, .. } if modifiers.command));
        (i.key_pressed(Key::Escape), ctrl_comma)
    });
    if esc && !crate::ui::menu::menu_is_open(ui.ctx(), row_menu_id()) {
        out.close = true;
    }
    if settings {
        out.commands.push(snap.footer.settings.command.clone());
    }
    out
}

fn row_menu_id() -> Id {
    Id::new("panel-row-menu")
}

// ---------------------------------------------------------------- the top

fn draw_top(ui: &mut Ui, snap: &Snapshot, level: f32, st: &mut PanelState, out: &mut PanelOutput) -> f32 {
    let t = Theme::of(ui.ctx());
    let start = ui.cursor().min.y;
    let first = snap.first_run.as_ref();
    let top = &snap.top;
    egui::Frame::none().inner_margin(Margin { left: PAD_X, right: PAD_X, top: 14.0, bottom: 6.0 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);

        // The sentence, with its icon before it and the meter, Cancel or ×
        // at its right end.
        let (sentence, icon) = match first {
            Some(f) => (f.sentence.as_str(), TitleIcon::None),
            None => (top.sentence.as_str(), top.icon),
        };
        let corner = match first {
            Some(f) => Some(&f.close),
            None => top.corner.as_ref(),
        };
        sentence_line(ui, sentence, icon, top.meter && first.is_none(), level, corner, first.is_some(), out);

        // A problem's detail, its buttons and their note; first run's words.
        let detail = first.map(|f| f.detail.as_str()).or(top.detail.as_deref());
        if let Some(d) = detail {
            ui.add_space(4.0);
            let _ = Para::new(d).size(size::SECONDARY).show(ui);
        }
        if let Some(f) = first {
            first_run_controls(ui, f, out);
        } else if !top.actions.is_empty() {
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                for b in &top.actions {
                    if model_button(ui, b).clicked() {
                        out.commands.push(b.command.clone());
                    }
                }
            });
        }
        if first.is_none() {
            if let Some(n) = &top.note {
                ui.add_space(8.0);
                let _ = Para::new(n).size(size::SECONDARY).show(ui);
            }
        }

        // The volume row. Its mute button's icon lines up with the words
        // above, so the row starts a little to their left.
        if let Some(v) = &top.volume {
            ui.add_space(8.0);
            volume(ui, v, out);
        }

        // The third line: the place's output, or the delay line.
        if first.is_none() {
            match &top.third {
                Some(Third::Text(text)) => {
                    ui.add_space(4.0);
                    third_words(ui, text);
                }
                Some(Third::Delay(d)) => {
                    ui.add_space(4.0);
                    delay(ui, d, &mut st.delay, out);
                }
                None => {}
            }
        }
    });
    let _ = t;
    ui.cursor().min.y - start
}

fn sentence_line(ui: &mut Ui, sentence: &str, icon: TitleIcon, meter: bool, level: f32, corner: Option<&ModelButton>, first_run: bool, out: &mut PanelOutput) {
    let t = Theme::of(ui.ctx());
    let (line, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    let mut left = line.left();
    let p = ui.painter().clone();
    match icon {
        TitleIcon::Warning => {
            Icon::Warning.paint_weight(&p, Rect::from_center_size(egui::pos2(left + 9.0, line.center().y), Vec2::splat(18.0)), t.c.amber, 1.6);
            left += 26.0;
        }
        TitleIcon::Error => {
            Icon::ErrorCross.paint_weight(&p, Rect::from_center_size(egui::pos2(left + 9.0, line.center().y), Vec2::splat(18.0)), t.c.red, 1.7);
            left += 26.0;
        }
        TitleIcon::None => {}
    }
    // What sits at the right end, and its width.
    let mut right = line.right();
    if let Some(b) = corner {
        if first_run {
            let r = Rect::from_min_size(egui::pos2(right - 28.0, line.center().y - 14.0), Vec2::splat(28.0));
            let name = b.spoken.clone().unwrap_or_else(|| b.label.clone());
            if crate::ui::inside(ui, r, |ui: &mut Ui| IconButton::new(Icon::Close, &name).size(28.0, 14.0).color(t.c.text_secondary).show(ui)).clicked() {
                out.commands.push(b.command.clone());
            }
            right -= 36.0;
        } else {
            let g = ui.painter().layout_no_wrap(b.label.clone(), t.font(size::SECONDARY), Color32::PLACEHOLDER);
            let w = g.size().x + 4.0;
            let r = Rect::from_min_size(egui::pos2(right - w, line.center().y - 10.0), Vec2::new(w, 20.0));
            if crate::ui::inside(ui, r, |ui: &mut Ui| Button::link(&b.label).show(ui)).clicked() {
                out.commands.push(b.command.clone());
            }
            right -= w + 12.0;
        }
    } else if meter {
        paint_meter(&p, egui::pos2(right, line.center().y + 7.0), level, t.c.live);
        right -= 26.0;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(sentence.to_owned(), t.strong(size::TITLE), t.c.text);
    job.wrap = egui::text::TextWrapping::truncate_at_width((right - left).max(40.0));
    let galley = ui.fonts(|f| f.layout_job(job));
    let cut = galley.elided;
    p.galley(egui::pos2(left, line.center().y - galley.size().y / 2.0), galley, t.c.text);
    if cut {
        let r = ui.interact(Rect::from_min_max(egui::pos2(left, line.top()), egui::pos2(right, line.bottom())), ui.id().with("sentence"), Sense::hover());
        let _ = r.on_hover_text(sentence);
    }
}

/// The level meter: four bars ending at `bottom_right`, as tall as the
/// level is loud.
fn paint_meter(p: &egui::Painter, bottom_right: Pos2, level: f32, color: Color32) {
    let level = level.clamp(0.0, 1.0);
    for (i, peak) in [5.0f32, 11.0, 8.0, 14.0].into_iter().enumerate() {
        let h = (2.0 + (peak - 2.0) * (0.25 + 0.75 * level)).round();
        let x = bottom_right.x - 18.0 + i as f32 * 5.0;
        p.rect_filled(Rect::from_min_max(egui::pos2(x, bottom_right.y - h), egui::pos2(x + 3.0, bottom_right.y)), 1.0, color);
    }
}

fn model_button(ui: &mut Ui, b: &ModelButton) -> egui::Response {
    let mut button = match b.style {
        ButtonStyle::Primary => Button::primary(&b.label),
        ButtonStyle::Secondary | ButtonStyle::Icon => Button::standard(&b.label),
        ButtonStyle::Link => Button::link(&b.label),
    }
    .enabled(b.enabled)
    .max_width(ui.available_width());
    if let Some(s) = &b.spoken {
        button = button.accessible_name(s.clone());
    }
    button.show(ui)
}

fn first_run_controls(ui: &mut Ui, f: &FirstRunWords, out: &mut PanelOutput) {
    if let Some(sw) = &f.switch {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 76.0).max(40.0);
            ui.allocate_ui(Vec2::new(w, 0.0), |ui| {
                let _ = Para::new(&sw.label).size(size::CONTROL).tone(Tone::Primary).show(ui);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Switch::new(sw.on, &sw.label).show(ui).is_some() {
                    out.commands.push(sw.command.clone());
                }
            });
        });
    }
    if !f.actions.is_empty() || f.link.is_some() {
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            for b in &f.actions {
                if model_button(ui, b).clicked() {
                    out.commands.push(b.command.clone());
                }
            }
            if let Some(l) = &f.link {
                ui.add_space(4.0);
                // Its own width, so that it wraps to a line of its own when
                // it doesn't fit beside the buttons.
                let row = ui.max_rect().width();
                let font = Theme::of(ui.ctx()).font(size::SECONDARY);
                let w = ui.fonts(|fo| fo.layout_no_wrap(l.label.clone(), font, Color32::PLACEHOLDER).size().x).min(row);
                ui.allocate_ui_with_layout(Vec2::new(w, 32.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if Button::link(&l.label).max_width(row).show(ui).clicked() {
                        out.commands.push(l.command.clone());
                    }
                });
            }
        });
    }
}

fn volume(ui: &mut Ui, v: &VolumeRow, out: &mut PanelOutput) {
    let w = ui.available_width() + 6.0;
    let r = Rect::from_min_size(ui.cursor().min - Vec2::new(6.0, 0.0), Vec2::new(w, 32.0));
    let name = v.tooltip.clone().unwrap_or_else(|| v.spoken.clone());
    let view = VolumeView { level: v.level, muted: v.muted, limit: v.limit, label: None, name: &name, name_as_tooltip: v.tooltip.is_some() };
    let action = crate::ui::inside(ui, r, |ui: &mut Ui| {
        ui.set_width(w);
        volume_row(ui, &view)
    });
    ui.allocate_exact_size(Vec2::new(w - 6.0, 32.0), Sense::hover());
    match action {
        Some(VolumeAction::Set(level)) => out.commands.push(Command::SetVolume { owner: v.owner.clone(), level }),
        Some(VolumeAction::ToggleMute) => out.commands.push(v.mute.command.clone()),
        None => {}
    }
}

fn third_words(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::hover());
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), t.font(size::SECONDARY), t.c.text_secondary);
    job.wrap = egui::text::TextWrapping::truncate_at_width(r.width());
    let g = ui.fonts(|f| f.layout_job(job));
    ui.painter().galley(egui::pos2(r.left(), r.center().y - g.size().y / 2.0), g, t.c.text_secondary);
}

fn mark_and_tone(tone: ModelTone) -> (Mark, Tone) {
    match tone {
        ModelTone::Plain => (Mark::None, Tone::Secondary),
        ModelTone::Info => (Mark::Info, Tone::Secondary),
        ModelTone::Warning => (Mark::Warning, Tone::Amber),
        ModelTone::Error => (Mark::Error, Tone::Red),
        ModelTone::Busy => (Mark::Busy, Tone::Secondary),
    }
}

fn delay(ui: &mut Ui, d: &PanelDelay, state: &mut crate::ui::delay::DelayState, out: &mut PanelOutput) {
    let buttons: Option<[&ModelButton; 4]> = match &d.body {
        DelayBody::Buttons { remove_large, remove_small, add_large, add_small } => Some([remove_large, add_large, remove_small, add_small]),
        DelayBody::Fixed { .. } => None,
    };
    let steps = buttons.map(|b| b.map(|b| StepWords { label: &b.label, spoken: b.spoken.as_deref().unwrap_or(&b.label), enabled: b.enabled }));
    let fixed = match &d.body {
        DelayBody::Fixed { text, action } => Some((text.as_str(), action.label.as_str())),
        DelayBody::Buttons { .. } => None,
    };
    // The line under the buttons: its own action, then Reset.
    let mut links: Vec<&ModelButton> = Vec::new();
    if let Some(a) = d.line.as_ref().and_then(|l| l.action.as_ref()) {
        links.push(a);
    }
    if let Some(r) = &d.reset {
        links.push(r);
    }
    let line = match &d.line {
        Some(l) => {
            let (mark, tone) = mark_and_tone(l.tone);
            Some(LineNote { text: &l.text, mark, tone, links: links.iter().map(|b| b.label.as_str()).collect() })
        }
        None if !links.is_empty() => Some(LineNote { text: "", mark: Mark::None, tone: Tone::Secondary, links: links.iter().map(|b| b.label.as_str()).collect() }),
        None => None,
    };
    let words = LineWords { label: &d.label, value: &d.value, value_is_figure: !d.reconnecting, spoken: &d.spoken, steps, fixed, line };
    let Some(action) = delay_line(ui, &words, state) else { return };
    let command = match action {
        LineAction::Step(i) => buttons.map(|b| b[i].command.clone()),
        LineAction::Fixed => match &d.body {
            DelayBody::Fixed { action, .. } => Some(action.command.clone()),
            DelayBody::Buttons { .. } => None,
        },
        LineAction::Link(i) => links.get(i).map(|b| b.command.clone()),
        LineAction::Key(dir, step) => buttons.and_then(|b| {
            let small = if dir == Direction::Remove { b[2] } else { b[3] };
            let large = if dir == Direction::Remove { b[0] } else { b[1] };
            match step {
                KeyStep::Small => Some(small.command.clone()),
                KeyStep::Large => Some(large.command.clone()),
                KeyStep::Fine => match &small.command {
                    Command::ChangeDelay { speaker, .. } => Some(Command::ChangeDelay {
                        speaker: speaker.clone(),
                        step: if dir == Direction::Remove { DelayStep::Remove(5) } else { DelayStep::Add(5) },
                    }),
                    _ => None,
                },
            }
        }),
    };
    out.commands.extend(command);
}

/// The panel's delay block for `d`, wherever it is drawn (the top of the
/// panel, a speaker's view); the commands its controls send.
pub fn delay_block(ui: &mut Ui, d: &PanelDelay, state: &mut crate::ui::delay::DelayState) -> Vec<Command> {
    let mut out = PanelOutput::default();
    delay(ui, d, state, &mut out);
    out.commands
}

// ---------------------------------------------------------------- the list

/// One row of the list as the panel draws it.
enum Item<'a> {
    Place(&'a Row),
    Searching,
    Fold,
    LookingMore(&'a str),
}

fn draw_list(ui: &mut Ui, snap: &Snapshot, st: &mut PanelState, room: f32, out: &mut PanelOutput) -> f32 {
    let list = &snap.list;
    let mut items: Vec<Item> = vec![Item::Place(&list.this_pc)];
    items.extend(list.speakers.iter().map(Item::Place));
    if list.searching.is_some() {
        items.push(Item::Searching);
    }
    if let Some(m) = &list.more {
        items.push(Item::Fold);
        if st.more_open {
            items.extend(m.rows.iter().map(Item::Place));
        }
    }
    if let Some(ma) = &list.music_assistant {
        items.push(Item::Place(&ma.row));
    }
    if let Some(words) = snap.first_run.as_ref().and_then(|f| f.looking_more.as_deref()) {
        items.push(Item::LookingMore(words));
    }

    // The row the keyboard stops on: the one it was last on, else the
    // current place.
    let current = items.iter().find_map(|i| match i {
        Item::Place(r) if r.current => Some(r.place.clone()),
        _ => None,
    });
    if st.focus_current_row {
        st.pending_focus = current.clone();
        st.focus_current_row = false;
    }
    if let Some(p) = &st.pending_focus {
        st.roving = Some(p.clone());
        if let Some((_, id)) = st.row_ids.iter().find(|(q, _)| q == p) {
            let id = *id;
            ui.memory_mut(|m| m.request_focus(id));
        }
    }
    let roving = st.roving.clone().filter(|p| items.iter().any(|i| matches!(i, Item::Place(r) if &r.place == p))).or(current.clone());

    let mut content_h = 0.0;
    let mut responses: Vec<(Place, egui::Response)> = Vec::new();
    let draw = |ui: &mut Ui, st: &mut PanelState, out: &mut PanelOutput, responses: &mut Vec<(Place, egui::Response)>| {
        egui::Frame::none().inner_margin(Margin { left: 8.0, right: 8.0, top: 6.0, bottom: 6.0 }).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            for item in &items {
                match item {
                    Item::Place(row) => {
                        let tab = roving.as_ref() == Some(&row.place);
                        let (action, resp) = place_row(ui, row, tab);
                        if tab && st.pending_focus.as_ref() == Some(&row.place) {
                            if resp.has_focus() {
                                st.pending_focus = None;
                            } else {
                                resp.request_focus();
                            }
                        }
                        if resp.has_focus() {
                            st.roving = Some(row.place.clone());
                        }
                        match action {
                            Some(RowAction::Choose) => out.commands.extend(row.command.clone()),
                            Some(RowAction::Button) => {
                                if let Some(e) = &row.error {
                                    out.commands.push(e.retry.command.clone());
                                }
                            }
                            Some(RowAction::Menu(at)) => {
                                if !row.menu.is_empty() {
                                    ui.ctx().data_mut(|d| d.insert_temp(row_menu_id().with("row"), row.place.clone()));
                                    open_menu(ui.ctx(), row_menu_id(), at);
                                }
                            }
                            Some(RowAction::Options) => out.commands.extend(row.options.as_ref().map(|o| o.command.clone())),
                            Some(RowAction::Expand) | None => {}
                        }
                        if let Some(q) = &row.question {
                            question(ui, q, out);
                        }
                        responses.push((row.place.clone(), resp));
                    }
                    Item::Searching => searching(ui, snap, out),
                    Item::Fold => {
                        if let Some(m) = &snap.list.more {
                            let r = PlaceRow { status: Status::Expand { open: st.more_open }, tab_stop: false, ..PlaceRow::new(Icon::List, &m.title, &m.tooltip) };
                            if matches!(r.show(ui), Some(RowAction::Choose) | Some(RowAction::Expand)) {
                                st.more_open = !st.more_open;
                            }
                        }
                    }
                    Item::LookingMore(words) => {
                        let r = PlaceRow { status: Status::Busy, tab_stop: false, ..PlaceRow::new(Icon::Search, words, "") };
                        let _ = r.show(ui);
                    }
                }
            }
        });
    };
    if room.is_finite() {
        let scroll = egui::ScrollArea::vertical().id_salt("panel-list-scroll").max_height(room).auto_shrink([false, false]).show(ui, |ui| {
            draw(ui, st, out, &mut responses);
        });
        content_h = scroll.content_size.y;
    } else {
        let start = ui.cursor().min.y;
        draw(ui, st, out, &mut responses);
        content_h += ui.cursor().min.y - start;
    }
    st.row_ids = responses.iter().map(|(p, r)| (p.clone(), r.id)).collect();

    // ↑ and ↓ move between rows; typing jumps to a name.
    if let Some(i) = responses.iter().position(|(_, r)| r.has_focus()) {
        let id = responses[i].1.id;
        ui.memory_mut(|m| m.set_focus_lock_filter(id, egui::EventFilter { vertical_arrows: true, horizontal_arrows: true, ..Default::default() }));
        let (up, down, now) = ui.input(|inp| (inp.key_pressed(Key::ArrowUp), inp.key_pressed(Key::ArrowDown), inp.time));
        let to = if up && i > 0 {
            Some(i - 1)
        } else if down && i + 1 < responses.len() {
            Some(i + 1)
        } else {
            None
        };
        let typed: String = ui.input(|inp| {
            inp.events
                .iter()
                .filter_map(|e| match e {
                    Event::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        });
        let to = to.or_else(|| {
            if typed.is_empty() {
                return None;
            }
            // A pause of a second starts a new name.
            if now - st.typed.1 > 1.0 {
                st.typed.0.clear();
            }
            st.typed.0.push_str(&typed.to_lowercase());
            st.typed.1 = now;
            let wanted = st.typed.0.clone();
            let titles: Vec<String> = items
                .iter()
                .filter_map(|it| match it {
                    Item::Place(r) => Some(r.title.to_lowercase()),
                    _ => None,
                })
                .collect();
            // From the next row on, so the same letter again moves on.
            let n = titles.len();
            (1..=n).map(|k| (i + k) % n).find(|j| titles[*j].starts_with(&wanted)).or_else(|| titles.iter().position(|tt| tt.starts_with(&wanted)))
        });
        if let Some(j) = to {
            // The row becomes the Tab stop on the next pass, then takes
            // the focus.
            st.roving = Some(responses[j].0.clone());
            st.pending_focus = Some(responses[j].0.clone());
            ui.ctx().request_discard("the keyboard moved to another row");
        }
        if ui.input(|inp| inp.key_pressed(Key::F10) && inp.modifiers.shift) {
            // Handled by the row itself; nothing more here.
        }
    }

    // The row menu, while open.
    let menu_place: Option<Place> = ui.ctx().data(|d| d.get_temp(row_menu_id().with("row")));
    if let Some(place) = menu_place {
        if let Some(row) = snap.list.rows().find(|r| r.place == place) {
            let picked = show_menu(ui.ctx(), row_menu_id(), 220.0, |ui| {
                let mut picked = None;
                for item in &row.menu {
                    if MenuItem::new(&item.label).enabled(item.enabled).checked(item.checked).show(ui) {
                        picked = Some(item.command.clone());
                    }
                }
                (picked.is_some(), picked)
            });
            if let Some(Some(c)) = picked {
                out.commands.push(c);
            }
        }
    }
    let _ = menu_separator;
    content_h
}

fn row_icon(icon: RowIcon) -> Icon {
    match icon {
        RowIcon::Pc => Icon::Laptop,
        RowIcon::Speaker => Icon::Speaker,
        RowIcon::Group => Icon::Group,
        RowIcon::HomePodPair => Icon::Pair,
        RowIcon::Tv => Icon::Tv,
        RowIcon::MusicAssistant => Icon::Hub,
    }
}

fn place_row(ui: &mut Ui, row: &Row, tab_stop: bool) -> (Option<RowAction>, egui::Response) {
    let status = match &row.status {
        Some(RowStatus::Text(t)) => Status::Words(t),
        Some(RowStatus::Spinner) => Status::Busy,
        Some(RowStatus::Attention) => Status::Warning,
        None => Status::None,
    };
    let problem = row.error.as_ref().map(|e| Problem { text: &e.text, tone: Tone::Red, button: Some(&e.retry.label) });
    let r = PlaceRow {
        status,
        problem,
        current: row.current,
        dimmed: row.dimmed,
        menu: !row.menu.is_empty(),
        spoken: Some(row.spoken.clone()),
        tab_stop,
        options: row.options.as_ref().map(|o| (o.label.as_str(), o.spoken.as_deref().unwrap_or(o.label.as_str()))),
        ..PlaceRow::new(row_icon(row.icon), &row.title, &row.tooltip)
    };
    ui.push_id(("row", &row.title), |ui| r.show_full(ui, ROW_STYLE)).inner
}

/// A busy speaker's question, in place under its row.
fn question(ui: &mut Ui, q: &crate::model::places::Question, out: &mut PanelOutput) {
    let t = Theme::of(ui.ctx());
    ui.add_space(2.0);
    egui::Frame::none().fill(t.c.card).stroke(t.card_stroke()).rounding(6.0).inner_margin(Margin::symmetric(12.0, 10.0)).outer_margin(Margin::symmetric(4.0, 2.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        let _ = Para::new("").lead(&q.title).size(size::CONTROL).show(ui);
        let _ = Para::new(&q.line).size(size::SECONDARY).show(ui);
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            if model_button(ui, &q.confirm).clicked() {
                out.commands.push(q.confirm.command.clone());
            }
            if model_button(ui, &q.cancel).clicked() {
                out.commands.push(q.cancel.command.clone());
            }
        });
    });
    ui.add_space(4.0);
}

/// The search while nothing is found: a row that says where it looks, then
/// what was found wrong and what can be done.
fn searching(ui: &mut Ui, snap: &Snapshot, out: &mut PanelOutput) {
    let Some(s) = &snap.list.searching else { return };
    let row = PlaceRow { status: Status::Busy, tab_stop: false, ..PlaceRow::new(Icon::Search, &s.title, &s.tooltip) };
    let _ = row.show(ui);
    if let Some(n) = &s.none_found {
        let t = Theme::of(ui.ctx());
        egui::Frame::none().fill(t.c.card).stroke(t.card_stroke()).rounding(6.0).inner_margin(Margin::symmetric(12.0, 10.0)).outer_margin(Margin::symmetric(4.0, 4.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            let _ = Para::new("").lead(&n.title).size(size::CONTROL).show(ui);
            for c in &n.causes {
                let _ = Para::new(c).mark(Mark::Warning).tone(Tone::Amber).size(size::SECONDARY).show(ui);
            }
            if !n.buttons.is_empty() {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    for b in &n.buttons {
                        if model_button(ui, b).clicked() {
                            out.commands.push(b.command.clone());
                        }
                    }
                });
            }
        });
    }
}

// ---------------------------------------------------------------- the bottom

fn draw_bottom(ui: &mut Ui, snap: &Snapshot, out: &mut PanelOutput) -> f32 {
    let start = ui.cursor().min.y;
    ui.spacing_mut().item_spacing.y = 0.0;
    if let Some(n) = &snap.notice {
        notice(ui, n, out);
    }
    footer(ui, snap, out);
    ui.cursor().min.y - start
}

fn notice(ui: &mut Ui, n: &ModelNotice, out: &mut PanelOutput) {
    let (mark, _) = mark_and_tone(n.tone);
    // A plain notice (the donation's) is said in plain words, no lead.
    let (lead, text) = match (n.tone, &n.detail) {
        (ModelTone::Plain, Some(d)) => (String::new(), format!("{} {}", n.text, d)),
        (ModelTone::Plain, None) => (String::new(), n.text.clone()),
        (_, d) => (n.text.clone(), d.clone().unwrap_or_default()),
    };
    let mark = if n.tone == ModelTone::Plain { Mark::Person } else { mark };
    let labels: Vec<&str> = n.actions.iter().map(|a| a.label.as_str()).collect();
    let words = Notice { mark, lead: &lead, text: &text, actions: &labels, dismissable: n.dismiss.is_some() };
    match notice_line(ui, &words) {
        Some(ParaClick::Link(i)) => out.commands.extend(n.actions.get(i).map(|a| a.command.clone())),
        Some(ParaClick::Dismiss) => out.commands.extend(n.dismiss.as_ref().map(|d| d.command.clone())),
        None => {}
    }
}

fn footer(ui: &mut Ui, snap: &Snapshot, out: &mut PanelOutput) {
    let t = Theme::of(ui.ctx());
    let (line, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(line, 0.0, t.c.divider);
    let f = &snap.footer;
    let first = snap.first_run.as_ref();
    egui::Frame::none().inner_margin(Margin { left: PAD_X, right: 10.0, top: 6.0, bottom: 8.0 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.set_min_height(32.0);
            ui.spacing_mut().item_spacing.x = 16.0;
            // In first run the footer carries only what helps with the step
            // at hand: *Speaker missing?* while picking, *Can't hear it?*
            // after the chime.
            let picking = first.is_some_and(|f| f.not_now.is_some());
            if let Some(c) = &f.cant_hear {
                if first.is_none() || !picking {
                    if Button::link(&c.label).show(ui).clicked() {
                        out.commands.push(c.command.clone());
                    }
                }
            }
            if first.is_none() || picking {
                if Button::link(&f.speaker_missing.label).show(ui).clicked() {
                    out.commands.push(f.speaker_missing.command.clone());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match first {
                Some(fr) => {
                    if let Some(n) = &fr.not_now {
                        if Button::link(&n.label).show(ui).clicked() {
                            out.commands.push(n.command.clone());
                        }
                    }
                }
                None => {
                    let name = f.settings.spoken.clone().unwrap_or_else(|| f.settings.label.clone());
                    if IconButton::new(Icon::Gear, &name).show(ui).clicked() {
                        out.commands.push(f.settings.command.clone());
                    }
                }
            });
        });
    });
}
