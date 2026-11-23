//! The panel driven like a person would drive it, on the scripted engine:
//! every control sends exactly the command the snapshot gives it; the
//! keyboard reaches everything in the order Windows' flyouts use; and the
//! layout holds at 200 %, with long names and with Chinese names.

use egui::output::OutputEvent;
use egui::{Event, Key, Modifiers, Pos2, Rect, Shape, Vec2};
use stream_to_speaker::model::command::Command;
use stream_to_speaker::model::port::Port;
use stream_to_speaker::model::scripted::{Names, Scenario, ScriptedEngine};
use stream_to_speaker::model::snapshot::{Snapshot, Third};
use stream_to_speaker::panel::{show, PanelOutput, PanelState, WIDTH};
use stream_to_speaker::ui::preview;
use stream_to_speaker::ui::theme::Mode;

fn fonts() -> egui::FontDefinitions {
    stream_to_speaker::fonts::definitions(&[], &mut |_| None).0
}

fn snapshot(name: &str, names: Names) -> std::sync::Arc<Snapshot> {
    let scenario = Scenario::from_name(name).unwrap_or_else(|| panic!("no scenario {name}"));
    ScriptedEngine::with_names(scenario, names).snapshot()
}

/// A panel in a window of its own, frame after frame.
struct Harness {
    ctx: egui::Context,
    snap: std::sync::Arc<Snapshot>,
    state: PanelState,
    scale: f32,
    frame: u32,
}

/// What one frame produced.
struct Frame {
    out: PanelOutput,
    texts: Vec<(String, Rect)>,
    focused: Vec<String>,
}

impl Harness {
    fn new(snap: std::sync::Arc<Snapshot>, state: PanelState) -> Self {
        Harness { ctx: preview::context(fonts(), Mode::Light), snap, state, scale: 1.0, frame: 0 }
    }

    fn frame(&mut self, events: Vec<Event>) -> Frame {
        self.frame += 1;
        let mut input = preview::input(Vec2::new(WIDTH, 2000.0), self.scale, self.frame as f64 * 0.05);
        input.events = events;
        let mut out = PanelOutput::default();
        let snap = self.snap.clone();
        let state = &mut self.state;
        let full = self.ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
                let o = show(ui, &snap, 0.5, state);
                out.commands.extend(o.commands);
                out.close |= o.close;
                out.natural_height = o.natural_height;
            });
        });
        let mut texts = Vec::new();
        for cs in &full.shapes {
            collect(&cs.shape, &mut texts);
        }
        let focused = full
            .platform_output
            .events
            .iter()
            .filter_map(|e| match e {
                OutputEvent::FocusGained(info) => info.label.clone(),
                _ => None,
            })
            .collect();
        Frame { out, texts, focused }
    }

    /// A few quiet frames, for the layout to settle.
    fn settle(&mut self) -> Frame {
        let mut f = self.frame(vec![]);
        for _ in 0..3 {
            f = self.frame(vec![]);
        }
        f
    }

    /// Click the middle of the text `label`; the commands it sent.
    fn click(&mut self, label: &str) -> Vec<Command> {
        let f = self.settle();
        let rect = f.texts.iter().find(|(t, _)| t == label).map(|(_, r)| *r).unwrap_or_else(|| {
            let all: Vec<&str> = f.texts.iter().map(|(t, _)| t.as_str()).collect();
            panic!("no text {label:?} in {all:?}")
        });
        self.click_at(rect.center())
    }

    fn click_at(&mut self, at: Pos2) -> Vec<Command> {
        let press = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        let mut sent = Vec::new();
        for events in [vec![Event::PointerMoved(at)], vec![press(true)], vec![press(false)], vec![]] {
            sent.extend(self.frame(events).out.commands);
        }
        sent
    }

    /// Press and release `key`; what both frames produced.
    fn key(&mut self, key: Key, modifiers: Modifiers) -> Frame {
        let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
        let mut f = self.frame(vec![ev(true)]);
        let g = self.frame(vec![ev(false)]);
        f.out.commands.extend(g.out.commands);
        f.out.close |= g.out.close;
        f.focused.extend(g.focused);
        f
    }
}

fn collect(shape: &Shape, out: &mut Vec<(String, Rect)>) {
    match shape {
        Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
        Shape::Text(t) if !t.galley.text().trim().is_empty() => out.push((t.galley.text().to_string(), Rect::from_min_size(t.pos, t.galley.size()))),
        _ => {}
    }
}

#[test]
fn a_row_plays_its_place_and_the_current_row_does_nothing() {
    let snap = snapshot("playing", Names::Usual);
    let office = snap.list.speakers.iter().find(|r| r.title == "Office").unwrap().clone();
    let mut h = Harness::new(snap.clone(), PanelState::opened());
    assert_eq!(h.click("Office"), vec![office.command.clone().unwrap()]);
    let current = snap.list.rows().find(|r| r.current).unwrap();
    assert_eq!(current.command, None, "the current row has no command");
    assert_eq!(h.click(&current.title), vec![]);
}

#[test]
fn the_tops_buttons_send_their_commands() {
    for (scenario, label) in [("fell-back", 0usize), ("fell-back", 1), ("cant-find", 0), ("speaker-back", 0)] {
        let snap = snapshot(scenario, Names::Usual);
        let b = snap.top.actions[label].clone();
        let mut h = Harness::new(snap, PanelState::opened());
        assert_eq!(h.click(&b.label), vec![b.command], "{scenario}");
    }
    let snap = snapshot("connecting", Names::Usual);
    let cancel = snap.top.corner.clone().expect("Cancel while connecting");
    let mut h = Harness::new(snap, PanelState::opened());
    assert_eq!(h.click(&cancel.label), vec![cancel.command]);
}

#[test]
fn the_delay_opens_and_its_buttons_and_reset_send_their_commands() {
    let snap = snapshot("delay-removing", Names::Usual);
    let Some(Third::Delay(d)) = snap.top.third.clone() else { panic!("a delay line") };
    let mut h = Harness::new(snap, PanelState::opened());
    assert_eq!(h.click(&d.value), vec![], "opening sends nothing");
    assert!(h.state.delay.is_open());
    let stream_to_speaker::model::delay::DelayBody::Buttons { remove_small, add_large, .. } = &d.body else { panic!("buttons") };
    assert_eq!(h.click(&remove_small.label), vec![remove_small.command.clone()]);
    assert_eq!(h.click(&add_large.label), vec![add_large.command.clone()]);
    if let Some(reset) = &d.reset {
        // The link sits at the end of the line under the buttons.
        let f = h.settle();
        let line = f.texts.iter().find(|(t, _)| t.ends_with(&reset.label)).expect("the line with Reset").1;
        let sent = h.click_at(Pos2::new(line.right() - 8.0, line.center().y));
        assert_eq!(sent, vec![reset.command.clone()]);
    }
}

#[test]
fn the_volume_and_mute_send_the_rows_commands() {
    let snap = snapshot("playing", Names::Usual);
    let v = snap.top.volume.clone().unwrap();
    let mut h = Harness::new(snap, PanelState::opened());
    let f = h.settle();
    let number = f.texts.iter().find(|(t, _)| *t == v.number).unwrap().1;
    // The slider's far right end sets 100; its mute button toggles.
    let sent = h.click_at(Pos2::new(number.left() - 40.0, number.center().y));
    assert!(matches!(sent.first(), Some(Command::SetVolume { owner, level }) if *owner == v.owner && *level >= 90), "{sent:?}");
    let sent = h.click_at(Pos2::new(8.0, number.center().y));
    assert_eq!(sent, vec![v.mute.command.clone()]);
}

#[test]
fn the_footer_and_notice_send_their_commands() {
    let snap = snapshot("notice-updated", Names::Usual);
    let notice = snap.notice.clone().unwrap();
    let mut h = Harness::new(snap.clone(), PanelState::opened());
    let action = notice.actions[0].clone();
    let f = h.settle();
    let line = f.texts.iter().find(|(t, _)| t.replace('\u{a0}', " ").contains(&action.label)).expect("the notice").1;
    // The link's words end the line before the ×.
    let sent = h.click_at(Pos2::new(line.right() - 30.0, line.center().y));
    assert_eq!(sent, vec![action.command.clone()]);
    let speaker_missing = snap.footer.speaker_missing.clone();
    assert_eq!(h.click(&speaker_missing.label), vec![speaker_missing.command]);
    // *Can't hear it?* shows while the place is a speaker.
    let snap = snapshot("playing", Names::Usual);
    let cant_hear = snap.footer.cant_hear.clone().unwrap();
    let mut h = Harness::new(snap, PanelState::opened());
    assert_eq!(h.click(&cant_hear.label), vec![cant_hear.command]);
}

#[test]
fn first_runs_controls_send_their_commands() {
    let snap = snapshot("first-run-hear", Names::Usual);
    let fr = snap.first_run.clone().unwrap();
    let mut h = Harness::new(snap, PanelState::opened());
    for b in &fr.actions {
        assert_eq!(h.click(&b.label), vec![b.command.clone()], "{}", b.label);
    }
    let link = fr.link.clone().unwrap();
    assert_eq!(h.click(&link.label), vec![link.command]);
    let snap = snapshot("first-run-pick", Names::Usual);
    let fr = snap.first_run.clone().unwrap();
    let mut h = Harness::new(snap, PanelState::opened());
    let not_now = fr.not_now.clone().unwrap();
    assert_eq!(h.click(&not_now.label), vec![not_now.command]);
}

#[test]
fn a_busy_speakers_question_and_a_failed_click_send_their_commands() {
    let snap = snapshot("busy-question", Names::Usual);
    let q = snap.list.rows().find_map(|r| r.question.clone()).expect("a question");
    let mut h = Harness::new(snap, PanelState::opened());
    assert_eq!(h.click(&q.confirm.label), vec![q.confirm.command.clone()]);
    assert_eq!(h.click(&q.cancel.label), vec![q.cancel.command.clone()]);
    let snap = snapshot("row-catalogue", Names::Usual);
    let e = snap.list.rows().find_map(|r| r.error.clone()).expect("a failed row");
    let mut h = Harness::new(snap, PanelState::opened());
    assert_eq!(h.click(&e.retry.label), vec![e.retry.command.clone()]);
}

#[test]
fn a_rows_menu_opens_on_right_click_and_sends_its_item() {
    let snap = snapshot("playing", Names::Usual);
    let office = snap.list.speakers.iter().find(|r| r.title == "Office").unwrap().clone();
    let item = office.menu.last().unwrap().clone();
    let mut h = Harness::new(snap, PanelState::opened());
    let f = h.settle();
    let at = f.texts.iter().find(|(t, _)| *t == "Office").unwrap().1.center();
    let right = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![Event::PointerMoved(at)], vec![right(true)], vec![right(false)], vec![], vec![]] {
        assert!(h.frame(events).out.commands.is_empty());
    }
    assert_eq!(h.click(&item.label), vec![item.command]);
}

#[test]
fn the_keyboard_starts_on_the_current_row_and_tabs_through_the_top_then_the_footer() {
    let snap = snapshot("playing", Names::Usual);
    let current = snap.list.rows().find(|r| r.current).unwrap().spoken.clone();
    let Some(Third::Delay(d)) = snap.top.third.clone() else { panic!() };
    let mut h = Harness::new(snap.clone(), PanelState::opened());
    let mut order: Vec<String> = Vec::new();
    for _ in 0..4 {
        order.extend(h.frame(vec![]).focused);
    }
    for _ in 0..8 {
        order.extend(h.key(Key::Tab, Modifiers::NONE).focused);
        order.extend(h.frame(vec![]).focused);
    }
    let v = snap.top.volume.clone().unwrap();
    // The panel opens with the keyboard on the current row (no event says
    // so: the focus was given, not moved), so the first Tab goes on to the
    // top.
    let expected = [
        v.mute.spoken.clone().unwrap_or(v.mute.label.clone()),
        v.spoken.clone(),
        d.spoken.clone(),
        snap.footer.cant_hear.clone().unwrap().label,
        snap.footer.speaker_missing.label.clone(),
        snap.footer.settings.spoken.clone().unwrap_or(snap.footer.settings.label.clone()),
        current.clone(),
        v.mute.spoken.clone().unwrap_or(v.mute.label.clone()),
    ];
    let order: Vec<String> = order.into_iter().fold(Vec::new(), |mut acc, x| {
        if acc.last() != Some(&x) {
            acc.push(x);
        }
        acc
    });
    assert_eq!(order[..expected.len().min(order.len())], expected[..expected.len().min(order.len())], "focus went {order:#?}");
    assert!(order.len() >= expected.len(), "focus went {order:#?}");
}

#[test]
fn arrows_move_between_rows_typing_jumps_and_esc_closes() {
    let snap = snapshot("playing", Names::Usual);
    let rows: Vec<String> = snap.list.rows().filter(|r| !r.dimmed || true).map(|r| r.spoken.clone()).collect();
    let current = snap.list.rows().position(|r| r.current).unwrap();
    let mut h = Harness::new(snap.clone(), PanelState::opened());
    h.settle();
    let f = h.key(Key::ArrowDown, Modifiers::NONE);
    let next: Vec<String> = f.focused.into_iter().chain(h.frame(vec![]).focused).collect();
    assert_eq!(next, vec![rows[current + 1].clone()]);
    let f = h.key(Key::ArrowUp, Modifiers::NONE);
    let back: Vec<String> = f.focused.into_iter().chain(h.frame(vec![]).focused).collect();
    assert_eq!(back, vec![rows[current].clone()]);
    // Typing "o" jumps to Office.
    let office = snap.list.rows().find(|r| r.title == "Office").unwrap().spoken.clone();
    let f = h.frame(vec![Event::Text("o".into())]);
    let jumped: Vec<String> = f.focused.into_iter().chain(h.frame(vec![]).focused).collect();
    assert_eq!(jumped, vec![office]);
    // Enter plays it.
    let enter = h.key(Key::Enter, Modifiers::NONE).out.commands;
    let office_row = snap.list.rows().find(|r| r.title == "Office").unwrap();
    assert_eq!(enter, vec![office_row.command.clone().unwrap()]);
    assert!(h.key(Key::Escape, Modifiers::NONE).out.close);
    let settings = h.key(Key::Comma, Modifiers::COMMAND).out.commands;
    assert_eq!(settings, vec![snap.footer.settings.command.clone()]);
}

#[test]
fn the_arrow_keys_change_the_delay_on_the_closed_line() {
    let snap = snapshot("playing", Names::Usual);
    let Some(Third::Delay(d)) = snap.top.third.clone() else { panic!() };
    let stream_to_speaker::model::delay::DelayBody::Buttons { remove_small, add_large, .. } = &d.body else { panic!() };
    let mut h = Harness::new(snap, PanelState::opened());
    h.settle();
    // Tab twice from the row to the slider, once more to the delay line.
    for _ in 0..3 {
        h.key(Key::Tab, Modifiers::NONE);
    }
    assert_eq!(h.key(Key::ArrowLeft, Modifiers::NONE).out.commands, vec![remove_small.command.clone()]);
    assert_eq!(h.key(Key::ArrowRight, Modifiers::SHIFT).out.commands, vec![add_large.command.clone()]);
    assert!(!h.state.delay.is_open(), "the keys work without opening it");
}

/// The panel at its natural height, then the checks every layout must
/// pass: no two pieces of text overlap and none runs past the panel.
fn assert_clean_layout(name: &str, names: Names, scale: f32, open_delay: bool) {
    let snap = snapshot(name, names);
    let mut state = PanelState::opened();
    state.delay.set_open(open_delay);
    let mut h = Harness::new(snap, state);
    h.scale = scale;
    let f = h.settle();
    for (i, (a, ra)) in f.texts.iter().enumerate() {
        assert!(ra.left() >= -0.5 && ra.right() <= WIDTH + 0.5, "{name} {names:?} at {scale}: {a:?} runs past the panel ({ra:?})");
        for (b, rb) in &f.texts[i + 1..] {
            let x = ra.intersect(*rb);
            assert!(!(ra.intersects(*rb) && x.width() > 1.0 && x.height() > 1.0), "{name} {names:?} at {scale}: {a:?} overlaps {b:?}");
        }
    }
}

#[test]
fn every_scenario_lays_out_cleanly_with_long_and_chinese_names_at_200_percent() {
    for scenario in Scenario::ALL {
        for names in [Names::Usual, Names::Long, Names::Chinese] {
            for scale in [1.0, 2.0] {
                assert_clean_layout(scenario.name(), names, scale, scenario.name().contains("delay"));
            }
        }
    }
}

#[test]
fn the_top_keeps_its_height_in_every_everyday_state() {
    // Choosing a place never moves the list: the top's height is the same
    // on This PC, while connecting, playing, ready and on Music Assistant.
    let mut tops = Vec::new();
    for name in ["on-this-pc", "connecting", "playing", "ready", "music-assistant-playing"] {
        let snap = snapshot(name, Names::Usual);
        let mut h = Harness::new(snap.clone(), PanelState::opened());
        let f = h.settle();
        let first_row = f.texts.iter().find(|(t, _)| t == "This PC").unwrap().1.top();
        tops.push((name, first_row));
    }
    let first = tops[0].1;
    assert!(tops.iter().all(|(_, y)| (y - first).abs() < 0.5), "{tops:?}");
}
