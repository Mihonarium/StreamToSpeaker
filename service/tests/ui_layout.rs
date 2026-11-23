//! Headless layout tests for the shared parts of the windows: each part is
//! laid out at the narrowest width it is used at, at 100 % and 200 %, with
//! words a third longer than the English and with a long Chinese name, and
//! the emitted shapes are inspected: no two pieces of text overlap, no text
//! runs past the part's width or out of its button, and no text is squeezed
//! into a column of single words. The delay control is also checked to keep
//! its size while it is used, and to report what a click or typing does.

use egui::{Color32, Event, Pos2, Rect, Shape, Vec2};
use stream_to_speaker::ui::button::Button;
use stream_to_speaker::ui::card::SettingRow;
use stream_to_speaker::ui::delay::{DelayAction, DelayControl, DelayFigure, DelayLine, DelayNote, DelayState, DelayView, Limits, Reconnect};
use stream_to_speaker::ui::icon::Icon;
use stream_to_speaker::ui::inputs::{DropDown, Switch};
use stream_to_speaker::ui::menu::MenuItem;
use stream_to_speaker::ui::notice::{info_bar, notice_line, BarKind, InfoBar, Notice};
use stream_to_speaker::ui::preview;
use stream_to_speaker::ui::row::{PlaceRow, Problem, RowAction, RowStyle, Status};
use stream_to_speaker::ui::text::{Mark, Tone};
use stream_to_speaker::ui::theme::{Mode, Theme};

/// The width inside the 360-point panel's top (16 points each side), a
/// row's width in its list, and the delay control's width on a speaker's
/// page: the narrowest each part is used at. The full delay control is
/// also checked at the panel's width, in case the panel shows it in full.
const PANEL_TOP: f32 = 326.0;
const PANEL_ROW: f32 = 342.0;
const SPEAKER_PAGE_DELAY: f32 = 470.0;
/// Settings at its smallest (640 points, the navigation as icons).
const SETTINGS_CONTENT: f32 = 550.0;
const SCALES: [f32; 2] = [1.0, 2.0];

const CHINESE: &str = "客厅里的索诺斯音箱和餐厅的音箱";
const LONG_NAME: &str = "Living Room + Dining Room + Kitchen";

/// `s` a third longer: the first word stretched, as translations stretch.
fn longer(s: &str) -> String {
    let extra = s.chars().count().div_ceil(3);
    match s.split_once(' ') {
        Some((first, rest)) => format!("{first}{} {rest}", "x".repeat(extra)),
        None => format!("{s}{}", "x".repeat(extra)),
    }
}

struct Text {
    text: String,
    rect: Rect,
    words_per_row: Vec<usize>,
}

struct Drawn {
    texts: Vec<Text>,
    fills: Vec<(Rect, Color32)>,
    used: Rect,
}

fn fonts() -> egui::FontDefinitions {
    stream_to_speaker::fonts::definitions(&[], &mut |_| None).0
}

/// Lay out `add` in a column `width` points wide, at `scale`, over a few
/// passes (fonts settle on the first), with `events` in the last pass.
fn draw_with(width: f32, scale: f32, passes: &mut dyn FnMut(usize) -> Vec<Event>, add: &mut dyn FnMut(&mut egui::Ui)) -> Drawn {
    let ctx = preview::context(fonts(), Mode::Light);
    let mut out = None;
    let mut used = Rect::NOTHING;
    for pass in 0..4 {
        let mut input = preview::input(Vec2::new(width + 40.0, 1400.0), scale, pass as f64 * 0.1);
        input.events = passes(pass);
        out = Some(ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none().inner_margin(20.0)).show(ctx, |ui| {
                ui.set_width(width);
                used = ui.scope(|ui| add(ui)).response.rect;
            });
        }));
    }
    let out = out.unwrap();
    let mut d = Drawn { texts: vec![], fills: vec![], used };
    for cs in &out.shapes {
        collect(&cs.shape, &mut d);
    }
    d
}

fn draw(width: f32, scale: f32, mut add: impl FnMut(&mut egui::Ui)) -> Drawn {
    draw_with(width, scale, &mut |_| vec![], &mut add)
}

fn collect(shape: &Shape, d: &mut Drawn) {
    match shape {
        Shape::Vec(v) => v.iter().for_each(|s| collect(s, d)),
        Shape::Text(t) if !t.galley.text().trim().is_empty() => {
            let words_per_row = t
                .galley
                .rows
                .iter()
                .map(|r| r.glyphs.iter().map(|g| g.chr).collect::<String>().split_whitespace().count())
                .collect();
            d.texts.push(Text {
                text: t.galley.text().to_string(),
                rect: Rect::from_min_size(t.pos, t.galley.size()),
                words_per_row,
            });
        }
        Shape::Rect(r) if r.fill != Color32::TRANSPARENT => d.fills.push((r.rect, r.fill)),
        _ => {}
    }
}

/// The checks every part must pass.
fn assert_clean(d: &Drawn, what: &str) {
    let left = d.used.left();
    let right = left + (d.used.width());
    for (i, a) in d.texts.iter().enumerate() {
        assert!(a.rect.left() >= left - 0.5 && a.rect.right() <= right + 0.5, "{what}: {:?} runs past the part ({:?})", a.text, d.used);
        for b in &d.texts[i + 1..] {
            let x = a.rect.intersect(b.rect);
            assert!(
                !(a.rect.intersects(b.rect) && x.width() > 1.0 && x.height() > 1.0),
                "{what}: {:?} at {:?} overlaps {:?} at {:?}",
                a.text,
                a.rect,
                b.text,
                b.rect
            );
        }
        let words = a.text.split_whitespace().count();
        let rows = a.words_per_row.len();
        assert!(
            !(rows >= 3 && words >= 3 && a.words_per_row.iter().all(|w| *w <= 1)),
            "{what}: {:?} is squeezed into a column of single words",
            a.text
        );
    }
}

/// Text inside a control's fill must stay inside it.
fn assert_text_inside_controls(d: &Drawn, control: Color32, what: &str) {
    for (rect, fill) in &d.fills {
        if *fill != control {
            continue;
        }
        for t in &d.texts {
            if rect.contains(t.rect.center()) {
                assert!(rect.expand(0.5).contains_rect(t.rect), "{what}: {:?} ({:?}) spills out of its button {:?}", t.text, t.rect, rect);
            }
        }
    }
}

fn view<'a>(speaker: &'a str, change: i32, note: Option<DelayNote>) -> DelayView<'a> {
    DelayView {
        speaker,
        delay: DelayFigure::about((200 + change).max(0) as u32),
        change_ms: change,
        limits: Limits { max_removed_ms: 200, max_added_ms: 1000 },
        small_step_ms: 25,
        large_step_ms: 100,
        note,
        fixed_with_short_ms: None,
        other_amount_link: false,
        reconnecting: None,
    }
}

fn every_note() -> Vec<DelayNote> {
    vec![
        DelayNote::NotAccepted { asked_ms: -300, now_ms: -200 },
        DelayNote::DroppedOut { had_ms: -200, now_ms: -150 },
        DelayNote::SteadyOffer { steady_ms: 2000, short_ms: 300 },
        DelayNote::MostRemoved,
        DelayNote::MostAdded,
        DelayNote::SteadyMostRemoved,
        DelayNote::AsLittleAsItCan,
        DelayNote::Removing { to_go_ms: 1000 },
        DelayNote::WillReconnect,
        DelayNote::Reconnecting,
        DelayNote::ChoiceSwitched { to_short: true, now_ms: -100, other_ms: -300 },
        DelayNote::DropoutsAdd { times: 12, minutes: 10 },
        DelayNote::DropoutsSteady { times: 12, minutes: 10 },
        DelayNote::Newcomer,
    ]
}

#[test]
fn the_delay_control_fits_the_speakers_page_and_the_panel_in_every_state() {
    let control = Theme::new(Mode::Light).c.control;
    for (scale, width) in SCALES.into_iter().flat_map(|s| [(s, PANEL_TOP), (s, SPEAKER_PAGE_DELAY)]) {
        for speaker in ["Kitchen", LONG_NAME, CHINESE] {
            for note in every_note().into_iter().map(Some).chain([None]) {
                for change in [-200, -50, 0, 1000] {
                    let v = view(speaker, change, note.clone());
                    let v = DelayView { other_amount_link: width > PANEL_TOP, ..v };
                    let d = draw(width, scale, |ui| {
                        let _ = DelayControl::new(&v, &mut DelayState::default()).show(ui);
                    });
                    let what = format!("{speaker} {note:?} {change} at {scale}, {width} wide");
                    assert_clean(&d, &what);
                    assert_text_inside_controls(&d, control, &what);
                }
            }
            // Large steps from the Lab, and the field for any amount.
            let big = DelayView { small_step_ms: 500, large_step_ms: 1000, ..view(speaker, -150, None) };
            let d = draw(width, scale, |ui| {
                let _ = DelayControl::new(&big, &mut DelayState::default()).show(ui);
            });
            assert_text_inside_controls(&d, control, "large steps");
            for (digits, add) in [("", false), ("9999", false), ("9999", true), ("40", true)] {
                let v = view(speaker, -150, None);
                let d = draw(width, scale, |ui| {
                    let _ = DelayControl::new(&v, &mut DelayState::with_amount(digits, add)).show(ui);
                });
                assert_clean(&d, &format!("{speaker} amount {digits:?} at {scale}"));
            }
            let fixed = DelayView { fixed_with_short_ms: Some(300), ..view(speaker, 0, None) };
            let d = draw(width, scale, |ui| {
                let _ = DelayControl::new(&fixed, &mut DelayState::default()).show(ui);
            });
            assert_clean(&d, &format!("{speaker} fixed at {scale}"));
        }
    }
}

fn bottom_of(v: &DelayView, state: &mut DelayState) -> f32 {
    let d = draw(PANEL_TOP, 1.0, |ui| {
        let _ = DelayControl::new(v, &mut state.clone()).show(ui);
    });
    d.used.bottom()
}

#[test]
fn the_field_for_any_amount_takes_the_room_of_the_four_buttons() {
    // The line under the buttons stays where it is when the field opens, so
    // the person's next click lands where they expect.
    let v = view("Kitchen", -50, Some(DelayNote::Removing { to_go_ms: 25 }));
    let buttons = bottom_of(&v, &mut DelayState::default());
    let field = bottom_of(&view("Kitchen", -50, None), &mut DelayState::with_amount("150", false));
    assert!((buttons - field).abs() < 0.5, "buttons end at {buttons}, the field at {field}");
}

#[test]
fn the_head_line_keeps_its_height_whatever_it_says() {
    let mut tops = vec![];
    for change in [0, -50, -200, 1000] {
        let v = view(LONG_NAME, change, None);
        let d = draw(PANEL_TOP, 1.0, |ui| {
            let _ = DelayControl::new(&v, &mut DelayState::default()).show(ui);
        });
        let first_button = d.texts.iter().find(|t| t.text.starts_with("Remove")).expect("a Remove button").rect.top();
        tops.push(first_button);
    }
    assert!(tops.windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "the buttons moved: {tops:?}");
}

/// Run the control over several frames with the pointer at the centre of
/// the text `target`, pressing on the last frames; returns what it reported.
fn click(v: &DelayView, state: &mut DelayState, target: &str) -> Option<DelayAction> {
    let d = draw(PANEL_TOP, 1.0, |ui| {
        let _ = DelayControl::new(v, &mut state.clone()).show(ui);
    });
    let at = d.texts.iter().find(|t| t.text == target).unwrap_or_else(|| panic!("no {target:?}")).rect.center();
    let ctx = preview::context(fonts(), Mode::Light);
    let mut got = None;
    let frames: Vec<Vec<Event>> = vec![
        vec![],
        vec![Event::PointerMoved(at)],
        vec![Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }],
        vec![Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }],
        vec![],
    ];
    for (i, events) in frames.into_iter().enumerate() {
        let mut input = preview::input(Vec2::new(PANEL_TOP + 40.0, 1400.0), 1.0, i as f64 * 0.1);
        input.events = events;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none().inner_margin(20.0)).show(ctx, |ui| {
                ui.set_width(PANEL_TOP);
                if let Some(a) = DelayControl::new(v, state).show(ui) {
                    got = Some(a);
                }
            });
        });
    }
    got
}

#[test]
fn the_buttons_report_what_they_say_cut_to_the_limit() {
    let v = view("Kitchen", -50, None);
    assert_eq!(click(&v, &mut DelayState::default(), "Remove 25 ms"), Some(DelayAction::Remove(25)));
    assert_eq!(click(&v, &mut DelayState::default(), "Add 100 ms"), Some(DelayAction::Add(100)));
    // At 150 ms removed, Remove 100 ms stops at the limit of 200.
    let near = view("Kitchen", -150, None);
    assert_eq!(click(&near, &mut DelayState::default(), "Remove 100 ms"), Some(DelayAction::Remove(50)));
    // At the limit the column is greyed: nothing happens.
    let at = view("Kitchen", -200, None);
    assert_eq!(click(&at, &mut DelayState::default(), "Remove 25 ms"), None);
}

#[test]
fn clicking_the_delay_opens_the_field_and_its_buttons_repeat_the_number() {
    let v = view("Kitchen", -50, None);
    let mut state = DelayState::default();
    assert_eq!(click(&v, &mut state, "about 0.15 s"), None);
    assert!(state.amount_is_open());
    let mut typed = DelayState::with_amount("40", true);
    assert_eq!(click(&v, &mut typed, "Add 40 ms"), Some(DelayAction::Add(40)));
    assert!(!typed.amount_is_open(), "choosing closes the field");
    let mut closing = DelayState::with_amount("150", false);
    assert_eq!(click(&v, &mut closing, "Amount"), None, "a click on the field's own label changes nothing");
    assert!(closing.amount_is_open());
}

#[test]
fn the_line_under_the_buttons_keeps_its_room_while_the_pointer_is_on_the_control() {
    // A press shows "Removing… 25 ms to go"; when it ends the line goes. If
    // the pointer is still on the buttons, the room stays, so nothing below
    // jumps up between two presses.
    let ctx = preview::context(fonts(), Mode::Light);
    let mut state = DelayState::default();
    let with = view("Kitchen", -25, Some(DelayNote::Removing { to_go_ms: 25 }));
    let without = view("Kitchen", -25, None);
    let on_buttons = Pos2::new(20.0 + 60.0, 20.0 + 40.0);
    let mut bottoms = vec![];
    for (i, (v, pointer)) in [(&with, Some(on_buttons)), (&with, Some(on_buttons)), (&without, Some(on_buttons)), (&without, None), (&without, None)]
        .into_iter()
        .enumerate()
    {
        let mut input = preview::input(Vec2::new(PANEL_TOP + 40.0, 600.0), 1.0, i as f64 * 0.1);
        input.events = match pointer {
            Some(p) => vec![Event::PointerMoved(p)],
            None => vec![Event::PointerGone],
        };
        let mut bottom = 0.0;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none().inner_margin(20.0)).show(ctx, |ui| {
                ui.set_width(PANEL_TOP);
                let _ = DelayControl::new(v, &mut state).show(ui);
                bottom = ui.cursor().min.y;
            });
        });
        bottoms.push(bottom);
    }
    assert!(bottoms[2] >= bottoms[1] - 0.5, "the room went while the pointer was on the control: {bottoms:?}");
    assert!(bottoms[4] < bottoms[1] - 5.0, "the room is given back once the pointer leaves: {bottoms:?}");
}

#[test]
fn the_panels_delay_line_fits_closed_and_open_in_every_state() {
    let control = Theme::new(Mode::Light).c.control;
    for scale in SCALES {
        for speaker in ["Kitchen", LONG_NAME, CHINESE] {
            for note in every_note().into_iter().map(Some).chain([None]) {
                for change in [-200, -50, 0, 1000] {
                    for open in [false, true] {
                        let v = view(speaker, change, note.clone());
                        let reconnecting = DelayView { reconnecting: Some(Reconnect::BackToOwn), ..v.clone() };
                        for v in [v, reconnecting] {
                            let d = draw(PANEL_TOP, scale, |ui| {
                                let mut state = if open { DelayState::opened() } else { DelayState::default() };
                                let _ = DelayLine::new(&v, &mut state).show(ui);
                            });
                            let what = format!("line {speaker} {note:?} {change} open={open} at {scale}");
                            assert_clean(&d, &what);
                            assert_text_inside_controls(&d, control, &what);
                            if !open {
                                assert!((d.used.height() - 32.0).abs() < 0.5, "{what}: the closed line is {} tall", d.used.height());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Run a delay line over several frames and click the text `target`.
fn click_line(v: &DelayView, state: &mut DelayState, target: &str) -> Option<DelayAction> {
    let d = draw(PANEL_TOP, 1.0, |ui| {
        let _ = DelayLine::new(v, &mut state.clone()).show(ui);
    });
    let found = d.texts.iter().find(|t| t.text == target || t.text.ends_with(target)).unwrap_or_else(|| panic!("no {target:?}"));
    // For a link at the end of a line, aim at the link's own words.
    let at = if found.text == target { found.rect.center() } else { Pos2::new(found.rect.right() - 10.0, found.rect.center().y) };
    let ctx = preview::context(fonts(), Mode::Light);
    let mut got = None;
    let frames: Vec<Vec<Event>> = vec![
        vec![],
        vec![Event::PointerMoved(at)],
        vec![Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }],
        vec![Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }],
    ];
    for (i, events) in frames.into_iter().enumerate() {
        let mut input = preview::input(Vec2::new(PANEL_TOP + 40.0, 800.0), 1.0, i as f64 * 0.1);
        input.events = events;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none().inner_margin(20.0)).show(ctx, |ui| {
                ui.set_width(PANEL_TOP);
                if let Some(a) = DelayLine::new(v, state).show(ui) {
                    got = Some(a);
                }
            });
        });
    }
    got
}

#[test]
fn the_delay_line_opens_and_its_buttons_and_reset_report_what_they_say() {
    let v = view("Kitchen", -50, None);
    let mut state = DelayState::default();
    assert_eq!(click_line(&v, &mut state, "about 0.15 s"), None);
    assert!(state.is_open(), "a click on the line opens it");
    assert_eq!(click_line(&v, &mut state, "Remove 25 ms"), Some(DelayAction::Remove(25)));
    assert_eq!(click_line(&v, &mut state, "Reset"), Some(DelayAction::BackToOwn));
    let near = view("Kitchen", -150, None);
    assert_eq!(click_line(&near, &mut DelayState::opened(), "Remove 100 ms"), Some(DelayAction::Remove(50)));
    let mut open = DelayState::opened();
    assert_eq!(click_line(&v, &mut open, "about 0.15 s"), None);
    assert!(!open.is_open(), "a second click closes it");
}

#[test]
fn the_arrow_keys_work_on_the_closed_delay_line() {
    // Tab to the line, then ← and Shift+→, without opening it.
    let v = view("Kitchen", -50, None);
    let ctx = preview::context(fonts(), Mode::Light);
    let mut state = DelayState::default();
    let key = |key, shift| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers { shift, ..Default::default() } };
    let mut got = vec![];
    for (i, events) in [vec![], vec![key(egui::Key::Tab, false)], vec![], vec![key(egui::Key::ArrowLeft, false)], vec![key(egui::Key::ArrowRight, true)]].into_iter().enumerate() {
        let mut input = preview::input(Vec2::new(PANEL_TOP + 40.0, 400.0), 1.0, i as f64 * 0.1);
        input.events = events;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none().inner_margin(20.0)).show(ctx, |ui| {
                ui.set_width(PANEL_TOP);
                if let Some(a) = DelayLine::new(&v, &mut state).show(ui) {
                    got.push(a);
                }
            });
        });
    }
    assert_eq!(got, [DelayAction::Remove(25), DelayAction::Add(100)]);
    assert!(!state.is_open());
}

#[test]
fn rows_fit_the_panel_with_long_names_statuses_and_problems() {
    let names = [("Kitchen".to_string(), "Not found".to_string()), (LONG_NAME.to_string(), "In use".to_string()), (CHINESE.to_string(), "1 of 2 found".to_string())];
    for scale in SCALES {
        for style in [RowStyle::OneLine, RowStyle::TwoLines] {
            for (name, status) in names.iter().cloned().chain(names.iter().map(|(n, s)| (longer(n), longer(s)))) {
                let error = longer("Couldn't connect — no answer.");
                let statuses = [Status::None, Status::Words(&status), Status::Busy, Status::Warning, Status::Expand { open: false }, Status::Check];
                for st in statuses {
                    for problem in [None, Some(Problem { text: &error, tone: Tone::Red, button: Some("Try again") })] {
                        let row = PlaceRow { status: st, problem, menu: true, current: true, ..PlaceRow::new(Icon::Group, &name, "Sonos group · about 0.2 s delay") };
                        let d = draw(PANEL_ROW, scale, |ui| {
                            let _ = row.show_in(ui, style);
                            let _ = row.show_in(ui, style);
                        });
                        let what = format!("{name} {st:?} {problem:?} {style:?} at {scale}");
                        assert_clean(&d, &what);
                        let min = if style == RowStyle::OneLine { 40.0 } else { 52.0 };
                        assert!(d.used.height() >= 2.0 * min - 0.5, "{what}: {:?}", d.used);
                        // One line means one line: a long name is cut with …
                        // (the tooltip has it all), never wrapped.
                        if style == RowStyle::OneLine {
                            let shown = d.texts.iter().find(|t| name.starts_with(t.text.trim_end_matches('…'))).expect("the name");
                            assert_eq!(shown.words_per_row.len(), 1, "{what}: the name wrapped");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn a_click_on_a_row_chooses_it() {
    let ctx = preview::context(fonts(), Mode::Light);
    let row = PlaceRow { menu: true, ..PlaceRow::new(Icon::Speaker, "Kitchen", "Sonos · about 0.2 s delay") };
    let at = Pos2::new(20.0 + 150.0, 20.0 + 20.0);
    let mut got = vec![];
    let frames = [
        vec![Event::PointerMoved(at)],
        vec![Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }],
        vec![Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }],
        vec![Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed: true, modifiers: Default::default() }],
        vec![Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed: false, modifiers: Default::default() }],
    ];
    for (i, events) in frames.into_iter().enumerate() {
        let mut input = preview::input(Vec2::new(PANEL_ROW + 40.0, 300.0), 1.0, i as f64 * 0.1);
        input.events = events;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none().inner_margin(20.0)).show(ctx, |ui| {
                ui.set_width(PANEL_ROW);
                if let Some(a) = row.show(ui) {
                    got.push(a);
                }
            });
        });
    }
    assert_eq!(got.first(), Some(&RowAction::Choose));
    assert!(matches!(got.last(), Some(RowAction::Menu(_))), "right-click asks for the menu: {got:?}");
}

#[test]
fn notices_and_bars_wrap_cleanly_in_the_panel_and_in_dialogs() {
    let longest = "Stream To Speaker is free. Donations pay for signing its sound driver.";
    let panel = 360.0 - 2.0;
    let lab = "They can stop a speaker from playing. While any of them differs from its default, the panel says \"Lab settings are on\".";
    for scale in SCALES {
        for text in [longest.to_string(), longer(longest)] {
            let d = draw(panel, scale, |ui| {
                let n = Notice { mark: Mark::Warning, lead: "Not looking for speakers:", text: &text, actions: &["Settings", "What's new"], dismissable: true };
                let _ = notice_line(ui, &n);
            });
            assert_clean(&d, "notice");
        }
        for width in [360.0, SETTINGS_CONTENT] {
            for text in [lab.to_string(), longer(lab)] {
                let d = draw(width, scale, |ui| {
                    let b = InfoBar { kind: BarKind::Warning, title: Some("Settings for testing speakers that aren't fully supported yet."), text: &text, action: Some("Reset everything in Lab") };
                    let _ = info_bar(ui, &b);
                });
                assert_clean(&d, &format!("bar at {width}"));
            }
        }
    }
}

#[test]
fn setting_rows_keep_their_control_and_wrap_their_words() {
    let title = "Remove or add delay from the keyboard, in any app";
    let desc = "Works during full-screen video. Each press shows a small confirmation by the clock.";
    for scale in SCALES {
        for (t, d) in [(title.to_string(), desc.to_string()), (longer(title), longer(desc)), (CHINESE.to_string(), CHINESE.repeat(3))] {
            let drawn = draw(SETTINGS_CONTENT, scale, |ui| {
                let _ = SettingRow::new(&t).icon(Icon::Keyboard).description(&d).show(ui, |ui| Switch::new(true, "shortcuts").show(ui));
                let _ = SettingRow::new(&t)
                    .icon(Icon::List)
                    .description(&d)
                    .show(ui, |ui| DropDown::new("language", "Language", &["Windows default (English)", "English", "Русский"], 0).show(ui));
                let _ = SettingRow::new(&t).description(&d).opens_page().show(ui, |_| ());
                ui.horizontal(|ui| {
                    let _ = Button::primary("Add a speaker by address").icon(Icon::Plus).show(ui);
                    let _ = Button::standard("Search again").icon(Icon::Refresh).show(ui);
                });
            });
            assert_clean(&drawn, &format!("setting rows {t:?} at {scale}"));
        }
    }
}

#[test]
fn menu_items_wrap_long_words() {
    for scale in SCALES {
        let d = draw(250.0 - 8.0, scale, |ui| {
            let _ = MenuItem::new("Quit (sound returns to this PC)").show(ui);
            let _ = MenuItem::new(&longer("Delay: about 0.15 s · 50 ms removed")).submenu(true).show(ui);
            let _ = MenuItem::new(CHINESE).checked(true).show(ui);
        });
        assert_clean(&d, "menu");
    }
}
