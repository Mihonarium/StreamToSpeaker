//! Scenes of every shared part in every state, for the preview.
//!
//! The delay scenes put the control in a stand-in for the panel's Now card,
//! at the panel's width, so they show it at the size it has in the panel.

use egui::{Rect, Response, Sense, Ui, Vec2};

use super::button::{Button, IconButton};
use super::delay::{DelayControl, DelayFigure, DelayNote, DelayState, DelayView, Limits};
use super::icon::Icon;
use super::preview::{Scene, Stage};
use super::theme::{size, space, Theme};
use super::volume::{volume_row, VolumeView};

pub fn scenes() -> Vec<Scene> {
    vec![
        Scene { name: "parts-buttons", size: [1000.0, 400.0], draw: buttons },
        Scene { name: "parts-delay-states", size: [1180.0, 1660.0], draw: delay_states },
        Scene { name: "parts-delay-amount", size: [1560.0, 350.0], draw: delay_amount },
        Scene { name: "parts-delay-interaction", size: [1180.0, 600.0], draw: delay_interaction },
    ]
}

/// The states every control is shown in, left to right.
const STATES: [&str; 5] = ["Resting", "Pointed at", "Pressed", "Keyboard focus", "Disabled"];

fn caption(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.label(egui::RichText::new(text).font(t.font(size::CAPTION)).color(t.c.text_secondary));
}

fn board_caption(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.label(egui::RichText::new(text.to_uppercase()).font(t.strong(11.5)).color(t.c.text_secondary));
    ui.add_space(6.0);
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

/// The panel's width, less the Now card's side margins.
const NOW_CARD_W: f32 = 364.0;

/// A stand-in for the panel's Now card (title, second line, volume), with
/// `body` under a divider.
fn now_card(ui: &mut Ui, title: &str, line2: &str, level: u8, body: impl FnOnce(&mut Ui)) {
    let t = Theme::of(ui.ctx());
    ui.allocate_ui(Vec2::new(NOW_CARD_W, 0.0), |ui| {
        egui::Frame::none()
            .fill(t.c.card)
            .stroke(t.card_stroke())
            .rounding(6.0)
            .inner_margin(egui::Margin::symmetric(space::CARD_PAD_X, space::CARD_PAD_Y))
            .show(ui, |ui| {
                ui.set_width(NOW_CARD_W - 2.0 * space::CARD_PAD_X - 2.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 21.0), Sense::hover());
                ui.painter().text(r.left_center(), egui::Align2::LEFT_CENTER, title, t.strong(size::TITLE), t.c.text);
                for (i, h) in [5.0, 11.0, 8.0, 14.0].into_iter().enumerate() {
                    let x = r.right() - 15.0 + i as f32 * 5.0;
                    let bar = Rect::from_min_max(egui::pos2(x - 3.0, r.center().y + 7.0 - h), egui::pos2(x, r.center().y + 7.0));
                    ui.painter().rect_filled(bar, 1.0, t.c.live);
                }
                ui.add_space(2.0);
                ui.label(egui::RichText::new(line2).font(t.font(size::SECONDARY)).color(t.c.text_secondary));
                ui.add_space(10.0);
                let _ = volume_row(ui, &VolumeView { level, muted: false, limit: None, label: None, name: "Volume" });
                ui.add_space(10.0);
                let (d, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                ui.painter().rect_filled(d, 0.0, t.c.divider);
                ui.add_space(10.0);
                body(ui);
            });
    });
}

fn view(speaker: &'static str, ms: u32, change_ms: i32, note: Option<DelayNote>) -> DelayView<'static> {
    DelayView {
        speaker,
        delay: DelayFigure::about(ms),
        change_ms,
        limits: Limits { max_removed_ms: 200, max_added_ms: 1000 },
        small_step_ms: 25,
        large_step_ms: 100,
        note,
        fixed_with_short_ms: None,
        other_amount_link: false,
    }
}

/// One card of a board: its caption, the Now card's words and level, the
/// control, its state, and what to force on it (`state:name`).
struct Case {
    caption: &'static str,
    title: &'static str,
    line2: &'static str,
    level: u8,
    view: DelayView<'static>,
    state: DelayState,
    force: Option<(&'static str, &'static str)>,
}

fn case(caption: &'static str, view: DelayView<'static>, note_app: &'static str) -> Case {
    let title = match view.speaker {
        "Kitchen" => "Playing on Kitchen",
        "Office" => "Playing on Office",
        "Den TV" => "Playing on Den TV",
        _ => "Playing on Desk Speaker",
    };
    let level = match view.speaker {
        "Kitchen" => 32,
        "Desk Speaker" => 40,
        _ => 30,
    };
    Case { caption, title, line2: note_app, level, view, state: DelayState::default(), force: None }
}

/// Cards in rows of `per_row`, each under an upper-case caption.
fn board(ui: &mut Ui, st: &mut Stage, per_row: usize, cases: Vec<Case>) {
    ui.add_space(16.0);
    let mut cases = cases.into_iter().peekable();
    let mut n = 0;
    while cases.peek().is_some() {
        ui.horizontal_top(|ui| {
            ui.add_space(16.0);
            ui.spacing_mut().item_spacing.x = 20.0;
            for _ in 0..per_row {
                let Some(mut c) = cases.next() else { break };
                n += 1;
                ui.push_id(n, |ui| {
                    ui.allocate_ui(Vec2::new(NOW_CARD_W, 0.0), |ui| {
                        ui.vertical(|ui| {
                            board_caption(ui, c.caption);
                            now_card(ui, c.title, c.line2, c.level, |ui| {
                                let _ = DelayControl::new(&c.view, &mut c.state).show(ui);
                            });
                            if c.state.amount_is_open() {
                                st.focus_named(ui.min_rect(), "Milliseconds to remove or add");
                            }
                            if let Some((state, name)) = c.force {
                                let area = ui.min_rect();
                                match state {
                                    "press" => st.press_named(area, name),
                                    "focus" => st.focus_named(area, name),
                                    _ => st.hover_named(area, name),
                                }
                            }
                        });
                    });
                });
            }
        });
        ui.add_space(18.0);
    }
}

fn delay_states(ui: &mut Ui, st: &mut Stage) {
    let office = |ms, change, note| DelayView { limits: Limits { max_removed_ms: 280, max_added_ms: 699 }, ..view("Office", ms, change, note) };
    let steady = |ms, change, note| DelayView { limits: Limits { max_removed_ms: 1000, max_added_ms: 1000 }, ..view("Office", ms, change, note) };
    let spotify = "Spotify is playing";
    let edge = "Edge is playing";
    board(
        ui,
        st,
        3,
        vec![
            case("At zero, before any change was made", view("Kitchen", 200, 0, Some(DelayNote::Newcomer)), spotify),
            case("A Sonos, after Remove 100 ms and Remove 25 ms", view("Kitchen", 75, -125, Some(DelayNote::Removing { to_go_ms: 75 })), spotify),
            Case { state: DelayState::with_amount("150", false), ..case("Typing an amount (click the delay)", view("Kitchen", 150, -50, None), spotify) },
            case("At the limit: Remove 100 ms stops at 200 removed", view("Kitchen", 0, -200, Some(DelayNote::MostRemoved)), spotify),
            case("Sound breaking up: after Add 100 ms", view("Kitchen", 300, 100, Some(DelayNote::Adding { to_go_ms: 50 })), spotify),
            case("Back to Kitchen's own delay (↺)", view("Kitchen", 200, 0, Some(DelayNote::Adding { to_go_ms: 125 })), spotify),
            case("AirPlay on Short delay: a change reconnects", office(250, -50, Some(DelayNote::WillReconnect)), edge),
            case("S2t · during the reconnect", office(250, -50, Some(DelayNote::Reconnecting)), "Reconnecting to remove 50 ms: a few seconds of silence."),
            case("If Office doesn't accept the change", steady(1800, -200, Some(DelayNote::NotAccepted { asked_ms: -300, now_ms: -200 })), edge),
            case("AirPlay on Steady: the first Remove press", steady(1900, -100, Some(DelayNote::SteadyOffer { steady_ms: 2000, short_ms: 300 })), edge),
            case("Just switched to Short delay", office(300, 0, Some(DelayNote::ChoiceSwitched { to_short: true, now_ms: 0, other_ms: -300 })), edge),
            case("AirPlay 2 on Steady: nothing can be removed yet", DelayView { fixed_with_short_ms: Some(300), ..view("Den TV", 2000, 0, None) }, edge),
            case(
                "A Sendspin speaker: add only",
                DelayView { limits: Limits { max_removed_ms: 0, max_added_ms: 800 }, ..view("Desk Speaker", 500, 0, Some(DelayNote::AsLittleAsItCan)) },
                edge,
            ),
            case("After a dropout from removing too much", view("Kitchen", 50, -150, Some(DelayNote::DroppedOut { had_ms: -200, now_ms: -150 })), spotify),
            case("Dropouts the app can see", view("Kitchen", 200, 0, Some(DelayNote::DropoutsAdd { times: 3, minutes: 10 })), spotify),
        ],
    );
}

fn delay_amount(ui: &mut Ui, st: &mut Stage) {
    let spotify = "Spotify is playing";
    board(
        ui,
        st,
        4,
        vec![
            Case { state: DelayState::with_amount("", false), ..case("Just opened: click the delay, or type a digit", view("Kitchen", 150, -50, None), spotify) },
            Case { state: DelayState::with_amount("150", false), ..case("150 typed: Enter removes 150 ms", view("Kitchen", 150, -50, None), spotify) },
            Case { state: DelayState::with_amount("40", true), ..case("+40 typed: the + makes Enter add", view("Kitchen", 150, -50, None), spotify) },
            Case { state: DelayState::with_amount("100", false), ..case("Past the limit: said before Enter", view("Kitchen", 50, -150, None), spotify) },
        ],
    );
}

fn delay_interaction(ui: &mut Ui, st: &mut Stage) {
    let spotify = "Spotify is playing";
    let k = |change: i32| view("Kitchen", (200 + change).max(0) as u32, change, None);
    board(
        ui,
        st,
        3,
        vec![
            Case { force: Some(("hover", "Remove 25 milliseconds")), ..case("Pointer on Remove 25 ms", k(-50), spotify) },
            Case { force: Some(("press", "Add 100 milliseconds")), ..case("Pressing Add 100 ms", k(-50), spotify) },
            Case { force: Some(("hover", "Delay on Kitchen")), ..case("Pointer on the delay: click to type", k(-50), spotify) },
            Case { force: Some(("focus", "Delay on Kitchen")), ..case("Keyboard focus on the delay: ←/→, Enter", k(-50), spotify) },
            Case { force: Some(("focus", "Back to Kitchen")), ..case("Keyboard focus on ↺", k(-50), spotify) },
            Case {
                force: Some(("focus", "Add 25 milliseconds")),
                ..case("Focus on Add 25 ms; Remove at its limit", view("Kitchen", 0, -200, Some(DelayNote::MostRemoved)), spotify)
            },
        ],
    );
}
