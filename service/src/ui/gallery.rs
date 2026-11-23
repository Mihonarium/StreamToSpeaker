//! Scenes of every shared part in every state, for the preview.
//!
//! The delay scenes put the control in a stand-in for the panel's Now card,
//! at the panel's width, so they show it at the size it has in the panel.

use egui::{Rect, Response, Sense, Ui, Vec2};

use super::button::{Button, IconButton};
use super::card::{card, divider, list_heading, section_heading, SettingRow, SETTINGS_CARD_PAD};
use super::delay::{DelayControl, DelayFigure, DelayNote, DelayState, DelayView, Limits};
use super::icon::Icon;
use super::inputs::{Check, Choice, DropDown, Switch, TextField};
use super::menu::{menu_frame, menu_header, menu_separator, MenuItem};
use super::notice::{info_bar, notice_line, BarKind, InfoBar, Notice};
use super::preview::{Scene, Stage};
use super::row::{PlaceRow, Trailing};
use super::text::{Mark, Tone};
use super::theme::{size, space, Theme};
use super::volume::{volume_row, VolumeView};

pub fn scenes() -> Vec<Scene> {
    vec![
        Scene { name: "parts-buttons", size: [1000.0, 400.0], draw: buttons },
        Scene { name: "parts-delay-states", size: [1180.0, 1660.0], draw: delay_states },
        Scene { name: "parts-delay-amount", size: [1560.0, 350.0], draw: delay_amount },
        Scene { name: "parts-delay-interaction", size: [1180.0, 620.0], draw: delay_interaction },
        Scene { name: "parts-rows", size: [820.0, 1000.0], draw: rows },
        Scene { name: "parts-notices", size: [1060.0, 640.0], draw: notices },
        Scene { name: "parts-inputs", size: [1060.0, 620.0], draw: inputs },
        Scene { name: "parts-cards", size: [700.0, 840.0], draw: cards },
        Scene { name: "parts-menus", size: [960.0, 360.0], draw: menus },
        Scene { name: "parts-icons", size: [700.0, 200.0], draw: icons },
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

/// The panel's width.
const PANEL_W: f32 = 380.0;

/// A column the panel's width and colour, with rows inset as the panel's
/// list insets them.
fn panel_column(ui: &mut Ui, caption: &str, add: impl FnOnce(&mut Ui)) {
    let t = Theme::of(ui.ctx());
    ui.allocate_ui(Vec2::new(PANEL_W, 0.0), |ui| {
        ui.vertical(|ui| {
            board_caption(ui, caption);
            egui::Frame::none()
                .fill(t.c.surface)
                .stroke(t.card_stroke())
                .rounding(8.0)
                .inner_margin(egui::Margin { left: 8.0, right: 8.0, top: 4.0, bottom: 8.0 })
                .show(ui, |ui| {
                    ui.set_width(PANEL_W - 18.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    add(ui);
                });
        });
    });
}

fn rows(ui: &mut Ui, st: &mut Stage) {
    let t = Theme::of(ui.ctx());
    ui.add_space(16.0);
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
        ui.spacing_mut().item_spacing.x = 24.0;
        panel_column(ui, "Play on: the everyday list", |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                list_heading(ui, "Play on");
            });
            ui.add_space(4.0);
            let rows = [
                PlaceRow { trailing: Trailing::Expand { open: false }, ..PlaceRow::new(Icon::Laptop, "This PC", "Speakers (Realtek Audio)") },
                PlaceRow { current: true, menu: true, ..PlaceRow::new(Icon::Speaker, "Kitchen", "Sonos · about 0.15 s delay") },
                PlaceRow { menu: true, ..PlaceRow::new(Icon::Group, "Living Room + Dining Room", "Sonos group · about 0.2 s delay") },
                PlaceRow { menu: true, ..PlaceRow::new(Icon::Pair, "Bedroom", "HomePod pair · about 0.2 s delay") },
                PlaceRow { menu: true, ..PlaceRow::new(Icon::Speaker, "Office", "AirPlay · about 2 s delay") },
                PlaceRow { dimmed: true, menu: true, ..PlaceRow::new(Icon::Speaker, "Garage", "Not found right now") },
            ];
            for r in &rows {
                let _ = r.show(ui);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Through Music Assistant").font(t.font(size::CAPTION)).color(t.c.text_secondary));
            });
            ui.add_space(2.0);
            let _ = PlaceRow::new(Icon::Hub, "Music Assistant", "Chooses the speaker in Music Assistant").show(ui);
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                list_heading(ui, "This PC's outputs");
            });
            ui.add_space(4.0);
            let _ = PlaceRow { current: true, trailing: Trailing::Expand { open: true }, ..PlaceRow::new(Icon::Laptop, "This PC", "Speakers (Realtek Audio)") }.show(ui);
            let _ = PlaceRow { indent: true, trailing: Trailing::Check, ..PlaceRow::new(Icon::Laptop, "Speakers", "Realtek Audio") }.show(ui);
            let _ = PlaceRow { indent: true, ..PlaceRow::new(Icon::Headphones, "Headphones", "WH-1000XM4") }.show(ui);
            let _ = PlaceRow { indent: true, ..PlaceRow::new(Icon::Monitor, "Monitor", "HDMI · NVIDIA High Definition Audio") }.show(ui);
        });
        panel_column(ui, "Rows in the states that matter", |ui| {
            ui.add_space(4.0);
            let a = ui.cursor().min;
            let _ = PlaceRow { tone: Tone::Amber, menu: true, ..PlaceRow::new(Icon::Speaker, "Kitchen", "Isn't responding — trying again (2 of 3)") }.show(ui);
            st.hover_named(Rect::from_min_max(a, ui.cursor().min + Vec2::new(PANEL_W, 0.0)), "Kitchen.");
            let _ = PlaceRow { tone: Tone::Red, trailing: Trailing::Button("Try again"), ..PlaceRow::new(Icon::Tv, "Den TV", "Couldn't connect — no answer.") }.show(ui);
            let _ = PlaceRow { trailing: Trailing::Busy, ..PlaceRow::new(Icon::Pair, "Bedroom", "Asking Bedroom to play…") }.show(ui);
            let _ = PlaceRow { dimmed: true, ..PlaceRow::new(Icon::Pair, "Bedroom", "Waiting for the second HomePod") }.show(ui);
            let _ = PlaceRow { tone: Tone::Amber, ..PlaceRow::new(Icon::Speaker, "Kitchen (192.168.1.20)", "Sonos · two devices answer as Kitchen") }.show(ui);
            let _ = PlaceRow { dimmed: true, ..PlaceRow::new(Icon::Speaker, "Bathroom", "Can't play to this speaker") }.show(ui);
            let _ = PlaceRow::new(Icon::Speaker, "Office", "AirPlay · 1.74 s delay, measured").show(ui);
            let _ = PlaceRow { trailing: Trailing::Expand { open: false }, ..PlaceRow::new(Icon::List, "More speakers on this network (12)", "Never used from this PC") }.show(ui);
            let b = ui.cursor().min;
            let _ = PlaceRow { menu: true, ..PlaceRow::new(Icon::Group, "Upstairs", "Pressed") }.show(ui);
            st.press_named(Rect::from_min_max(b, ui.cursor().min + Vec2::new(PANEL_W, 0.0)), "Upstairs.");
            let c = ui.cursor().min;
            let _ = PlaceRow { menu: true, ..PlaceRow::new(Icon::Speaker, "Study", "Keyboard focus: ⋯ shows too") }.show(ui);
            st.focus_named(Rect::from_min_max(c, ui.cursor().min + Vec2::new(PANEL_W, 0.0)), "Study.");
            let _ = PlaceRow { menu: true, ..PlaceRow::new(Icon::Group, "Living Room + Dining Room + Kitchen + Garden Terrace", "Sonos group · about 0.2 s delay") }.show(ui);
            let _ = PlaceRow { menu: true, ..PlaceRow::new(Icon::Speaker, "客厅的音箱", "Sonos · about 0.2 s delay") }.show(ui);
        });
    });
}

fn notices(ui: &mut Ui, st: &mut Stage) {
    let _ = st;
    ui.add_space(16.0);
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
        ui.spacing_mut().item_spacing.x = 24.0;
        panel_column(ui, "The notice line, one at a time", |ui| {
            let list: [Notice; 7] = [
                Notice { mark: Mark::Info, lead: "Restart Windows to finish updating.", text: "", actions: &["Restart…"], dismissable: false },
                Notice { mark: Mark::Warning, lead: "Not looking for speakers:", text: "the network adapter “Wi-Fi” is off.", actions: &["Settings"], dismissable: false },
                Notice { mark: Mark::Warning, lead: "Kitchen Sub asked for the sound and was refused.", text: "", actions: &["Allow"], dismissable: false },
                Notice { mark: Mark::Lab, lead: "Lab settings are on.", text: "", actions: &["Turn off"], dismissable: false },
                Notice { mark: Mark::Info, lead: "Version 0.2.1 is ready to install.", text: "", actions: &["What's new"], dismissable: false },
                Notice { mark: Mark::Info, lead: "Updated to 0.2.1.", text: "", actions: &["What's new"], dismissable: true },
                Notice {
                    mark: Mark::Person,
                    lead: "",
                    text: "Stream To Speaker is free. Donations pay for signing its sound driver.",
                    actions: &["Donate"],
                    dismissable: true,
                },
            ];
            for n in &list {
                ui.add_space(10.0);
                let _ = notice_line(ui, n);
            }
        });
        ui.allocate_ui(Vec2::new(600.0, 0.0), |ui| {
            ui.vertical(|ui| {
                board_caption(ui, "Bars at the top of a page or dialog");
                ui.spacing_mut().item_spacing.y = 10.0;
                let bars = [
                    InfoBar {
                        kind: BarKind::Warning,
                        title: Some("Settings for testing speakers that aren't fully supported yet."),
                        text: "They can stop a speaker from playing. While any of them differs from its default, the panel says “Lab settings are on”.",
                        action: Some("Reset everything in Lab"),
                    },
                    InfoBar { kind: BarKind::Error, title: None, text: "That password didn't work. You can check it in the speaker's own settings.", action: None },
                    InfoBar {
                        kind: BarKind::Warning,
                        title: None,
                        text: "Pairing with a code is paused because wrong codes were entered too many times. If that wasn't you, something on your network may be trying to pair.",
                        action: Some("Allow pairing again"),
                    },
                    InfoBar { kind: BarKind::Info, title: None, text: "Music Assistant sets the delay for each of its speakers, so it isn't changed here.", action: None },
                    InfoBar { kind: BarKind::Done, title: None, text: "Paired with Music Assistant (Home).", action: None },
                ];
                for b in &bars {
                    let _ = info_bar(ui, b);
                }
            });
        });
    });
}

fn inputs(ui: &mut Ui, st: &mut Stage) {
    sheet(ui, |ui| {
        egui::Grid::new("inputs").num_columns(6).spacing(Vec2::new(22.0, 16.0)).min_col_width(120.0).show(ui, |ui| {
            ui.label("");
            for s in STATES {
                caption(ui, s);
            }
            ui.end_row();
            for on in [false, true] {
                caption(ui, if on { "Switch, on" } else { "Switch, off" });
                for i in 0..STATES.len() {
                    let r = ui.push_id((on, i), |ui| Switch::new(on, "Start at sign-in").enabled(i != 4).show(ui)).response;
                    let _ = r;
                    let area = ui.min_rect();
                    let _ = area;
                }
                ui.end_row();
            }
            for chosen in [false, true] {
                caption(ui, if chosen { "Choice, chosen" } else { "Choice" });
                for i in 0..STATES.len() {
                    ui.push_id((chosen, i), |ui| {
                        ui.set_max_width(150.0);
                        let _ = Choice::new(chosen, "Steady").description("about 2 s").enabled(i != 4).show(ui);
                    });
                }
                ui.end_row();
            }
            for ticked in [false, true] {
                caption(ui, if ticked { "Tick box, ticked" } else { "Tick box" });
                for i in 0..STATES.len() {
                    ui.push_id((ticked, i, "c"), |ui| {
                        ui.set_max_width(150.0);
                        let _ = Check::new(ticked, "The log file").enabled(i != 4).show(ui);
                    });
                }
                ui.end_row();
            }
            caption(ui, "Drop-down");
            for i in 0..STATES.len() {
                ui.push_id((i, "d"), |ui| {
                    let _ = DropDown::new(("dd", i), "Look for speakers on", &["All networks", "Wi-Fi", "Ethernet 2"], 0).min_width(140.0).enabled(i != 4).show(ui);
                });
            }
            ui.end_row();
            caption(ui, "Text field");
            for i in 0..STATES.len() {
                ui.push_id((i, "f"), |ui| {
                    let mut text = if i == 0 { String::new() } else { "192.168.20.40".to_string() };
                    let r = TextField::new(&mut text, "Address").hint("an address").width(150.0).enabled(i != 4).show(ui);
                    match i {
                        1 => st.hover(&r),
                        3 => st.focus(&r),
                        _ => {}
                    }
                });
            }
            ui.end_row();
        });
        // States of the inner parts, by their names, column by column.
        let grid = ui.min_rect();
        let col = |i: usize| Rect::from_min_max(egui::pos2(grid.left() + 120.0 + 22.0 + (i as f32) * 172.0 - 4.0, grid.top()), egui::pos2(grid.left() + 120.0 + 22.0 + (i as f32 + 1.0) * 172.0 - 8.0, grid.bottom()));
        for name in ["Start at sign-in", "Steady", "The log file", "Look for speakers on"] {
            st.hover_named(col(1), name);
            st.press_named(col(2), name);
            st.focus_named(col(3), name);
        }
    });
}

fn cards(ui: &mut Ui, st: &mut Stage) {
    egui::Frame::none().inner_margin(egui::Margin::same(16.0)).show(ui, |ui| {
        ui.set_width(640.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        let _ = SettingRow::new("Look for speakers on")
            .icon(Icon::Wifi)
            .description("Looking on Wi-Fi “HomeNet” (192.168.1.23) and Ethernet 2 (10.8.0.4).")
            .show(ui, |ui| DropDown::new("net", "Look for speakers on", &["All networks", "Wi-Fi", "Ethernet 2"], 0).show(ui));
        section_heading(ui, "Your speakers");
        let _ = SettingRow::new("Kitchen").icon(Icon::Speaker).description("Sonos · Playing now · 50 ms removed").opens_page().show(ui, |_| ());
        let a = ui.cursor().min;
        let _ = SettingRow::new("Office").icon(Icon::Speaker).description("AirPlay speaker · Steady · 300 ms removed").opens_page().show(ui, |_| ());
        st.hover_named(Rect::from_min_max(a, ui.cursor().min + Vec2::new(640.0, 0.0)), "Office");
        let _ = SettingRow::new("Neighbour's TV").icon(Icon::Tv).description("Apple TV · hidden from the panel").show(ui, |ui| Button::standard("Show").show(ui));
        section_heading(ui, "Start-up");
        let _ = SettingRow::new("Start Stream To Speaker when I sign in")
            .icon(Icon::Play)
            .description("It waits by the clock without opening anything.")
            .show(ui, |ui| Switch::new(true, "Start Stream To Speaker when I sign in").show(ui));
        let _ = SettingRow::new("Sendspin speakers (experimental)")
            .icon(Icon::Flask)
            .description("ESPHome and other speakers that play from Music Assistant. This PC plays to them directly. Music Assistant has its own page.")
            .show(ui, |ui| Switch::new(false, "Sendspin speakers (experimental)").show(ui));
        section_heading(ui, "What plays on your speaker");
        let _ = card(ui, SETTINGS_CARD_PAD, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let _ = Choice::new(true, "Everything this PC plays (recommended)").description("Choosing a speaker makes it Windows' sound output; choosing This PC switches back.").show(ui);
            let _ = Choice::new(false, "Only apps I send there").description("In Windows' Volume mixer, set an app's output to “Stream To Speaker”. The app never changes Windows' own output.").show(ui);
            divider(ui);
            let mut text = String::new();
            let _ = TextField::new(&mut text, "Show as").hint("Kitchen").width(260.0).show(ui);
        });
    });
}

fn menus(ui: &mut Ui, st: &mut Stage) {
    ui.add_space(16.0);
    ui.horizontal_top(|ui| {
        ui.add_space(16.0);
        ui.spacing_mut().item_spacing.x = 24.0;
        let frame = menu_frame(ui.ctx());
        ui.vertical(|ui| {
            board_caption(ui, "The panel's ⋯ menu");
            frame.show(ui, |ui| {
                ui.set_width(250.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                let _ = MenuItem::new("Help").show(ui);
                let _ = MenuItem::new("About").show(ui);
                menu_separator(ui);
                let _ = MenuItem::new("Quit (sound returns to this PC)").show(ui);
            });
        });
        ui.vertical(|ui| {
            board_caption(ui, "A row's menu: pointer on one, one greyed");
            frame.show(ui, |ui| {
                ui.set_width(250.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                let _ = MenuItem::new("Speaker settings").show(ui);
                let a = ui.cursor().min;
                let _ = MenuItem::new("Play a test sound").show(ui);
                st.hover_named(Rect::from_min_max(a, ui.cursor().min + Vec2::new(250.0, 0.0)), "Play a test sound");
                let _ = MenuItem::new("Reconnect").enabled(false).show(ui);
                let _ = MenuItem::new("Hide from this list").show(ui);
            });
        });
        ui.vertical(|ui| {
            board_caption(ui, "A state line, ticks and a submenu");
            frame.show(ui, |ui| {
                ui.set_width(280.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                menu_header(ui, "Playing on Kitchen");
                menu_separator(ui);
                let _ = MenuItem::new("This PC").show(ui);
                let _ = MenuItem::new("Kitchen").checked(true).show(ui);
                let _ = MenuItem::new("Living Room + Dining Room").show(ui);
                menu_separator(ui);
                let _ = MenuItem::new("Delay: about 0.15 s · 50 ms removed").submenu(true).show(ui);
                let b = ui.cursor().min;
                let _ = MenuItem::new("Settings").show(ui);
                st.focus_named(Rect::from_min_max(b, ui.cursor().min + Vec2::new(280.0, 0.0)), "Settings");
            });
        });
    });
}

fn icons(ui: &mut Ui, st: &mut Stage) {
    let _ = st;
    let t = Theme::of(ui.ctx());
    sheet(ui, |ui| {
        let all = [
            Icon::Speaker, Icon::Group, Icon::Pair, Icon::Tv, Icon::Laptop, Icon::Monitor, Icon::Headphones, Icon::Hub,
            Icon::ChevronRight, Icon::ChevronLeft, Icon::ChevronDown, Icon::ChevronUp, Icon::More, Icon::Gear, Icon::Volume,
            Icon::Mute, Icon::Check, Icon::Warning, Icon::Error, Icon::ErrorCross, Icon::Info, Icon::Close, Icon::Back,
            Icon::Search, Icon::Copy, Icon::Refresh, Icon::Reset, Icon::Wifi, Icon::Bell, Icon::Phone, Icon::Lock,
            Icon::Shield, Icon::Play, Icon::Music, Icon::Clock, Icon::Timer, Icon::List, Icon::Wrench, Icon::User,
            Icon::Plus, Icon::Sliders, Icon::Flask, Icon::Help, Icon::External, Icon::Keyboard,
        ];
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);
            for icon in all {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                icon.paint(ui.painter(), r.shrink(3.0), t.c.text);
            }
            let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
            super::icon::paint_mark(ui.painter(), r.shrink(2.0), t.c.mark_ink, t.c.mark_arcs, false);
            let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
            super::icon::paint_spinner(ui.painter(), r.shrink(4.0), t.c.accent, 0.25);
        });
    });
}
