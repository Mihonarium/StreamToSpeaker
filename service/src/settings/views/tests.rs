//! Tests of the panel's views on the scripted app: each control makes
//! exactly the change it names, at once; the views lay out at the panel's
//! width at 100 % and 200 %, with words a third longer and long or Chinese
//! names; Tab goes through them in reading order; every sentence of the
//! table's Settings block is said somewhere.

use std::sync::Arc;

use egui::{Color32, Event, Pos2, Rect, Shape, Vec2};

use super::*;
use crate::model::command::{Command, DelayStep, Open, VolumeOwner};
use crate::model::port::Port;
use crate::model::fake::ScriptedEngine;
use crate::model::Snapshot;
use crate::model::state::{Place, SpeakerId};

use crate::shell::page::{show_view, ViewState};
use crate::settings::port::*;
use crate::settings::scripted::{Names, ScriptedSettings, State};
use crate::settings::words::STRETCH;
use crate::ui::preview;
use crate::ui::theme::Mode;

/// What a view asked of the panel, in a form tests compare.
#[derive(Debug, PartialEq)]
enum Asked {
    Command(Command),
    View(String),
    Dialog,
    Open(Open),
    OpenPanel,
    Back,
}

#[derive(Clone, Debug)]
struct Text {
    text: String,
    rect: Rect,
    words_per_row: Vec<usize>,
    galley: Arc<egui::Galley>,
    pos: Pos2,
}

fn collect(shape: &Shape, clip: Rect, out: &mut Vec<Text>) {
    match shape {
        Shape::Vec(v) => v.iter().for_each(|s| collect(s, clip, out)),
        Shape::Text(t) if !t.galley.text().trim().is_empty() && t.override_text_color != Some(Color32::TRANSPARENT) => {
            let words_per_row = t.galley.rows.iter().map(|r| r.glyphs.iter().map(|g| g.chr).collect::<String>().split_whitespace().count()).collect();
            let rect = Rect::from_min_size(t.pos, t.galley.size()).intersect(clip);
            out.push(Text { text: t.galley.text().to_string(), rect, words_per_row, galley: t.galley.clone(), pos: t.pos });
        }
        _ => {}
    }
}

struct H {
    ctx: egui::Context,
    port: Arc<ScriptedSettings>,
    view: Box<dyn crate::shell::ViewContent>,
    snap: Arc<Snapshot>,
    vs: ViewState,
    asked: Vec<Asked>,
    texts: Vec<Text>,
    width: f32,
    scale: f32,
    frame: u32,
}

impl H {
    fn new(port: Arc<ScriptedSettings>, view: Box<dyn crate::shell::ViewContent>, snap: Arc<Snapshot>) -> Self {
        Self::sized(port, view, snap, crate::surface::panel::WIDTH, 1.0)
    }

    fn sized(port: Arc<ScriptedSettings>, view: Box<dyn crate::shell::ViewContent>, snap: Arc<Snapshot>, width: f32, scale: f32) -> Self {
        let ctx = preview::context(crate::fonts::definitions(&[], &mut |_| None).0, Mode::Light);
        let mut h = H { ctx, port, view, snap, vs: ViewState::default(), asked: Vec::new(), texts: Vec::new(), width, scale, frame: 0 };
        h.settle();
        h
    }

    fn run(&mut self, events: Vec<Event>) {
        self.frame += 1;
        let mut input = preview::input(Vec2::new(self.width, 5000.0), self.scale, self.frame as f64 * 0.05);
        input.events = events;
        let (snap, vs, view) = (self.snap.clone(), &mut self.vs, &mut self.view);
        let mut asked = Vec::new();
        let out = self.ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
                let o = show_view(ui, view.as_mut(), &snap, vs, 0);
                for a in o.asks {
                    asked.push(match a {
                        crate::shell::Ask::Command(c) => Asked::Command(c),
                        crate::shell::Ask::View(v) => Asked::View(v.bar(&snap).title),
                        crate::shell::Ask::Dialog(_) => Asked::Dialog,
                        crate::shell::Ask::Open(o) => Asked::Open(o),
                        crate::shell::Ask::OpenPanel => Asked::OpenPanel,
                        crate::shell::Ask::Back => Asked::Back,
                    });
                }
                if o.back {
                    asked.push(Asked::Back);
                }
            });
        });
        self.asked.extend(asked);
        self.texts.clear();
        for cs in &out.shapes {
            collect(&cs.shape, cs.clip_rect, &mut self.texts);
        }
    }

    fn settle(&mut self) {
        for _ in 0..3 {
            self.run(vec![]);
        }
    }

    fn find(&self, text: &str) -> Option<Rect> {
        self.texts.iter().find(|t| t.text == text).map(|t| t.rect)
    }

    fn has(&self, text: &str) -> bool {
        self.find(text).is_some()
    }

    fn shows(&self, part: &str) -> bool {
        self.texts.iter().any(|t| t.text.contains(part))
    }

    fn click_at(&mut self, at: Pos2) {
        let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        self.run(vec![Event::PointerMoved(at)]);
        self.run(vec![button(true)]);
        self.run(vec![button(false)]);
        self.settle();
    }

    fn click(&mut self, text: &str) {
        let r = self.find(text).unwrap_or_else(|| panic!("no {text:?}; drawn: {:?}", self.texts.iter().map(|t| &t.text).collect::<Vec<_>>()));
        self.click_at(r.center());
    }

    /// Click the switch at the right end of the line that says `label`
    /// (in the middle of its words and the grey line under them).
    fn flip(&mut self, label: &str) {
        let r = self.find(label).unwrap_or_else(|| panic!("no {label:?}"));
        let under = self.texts.iter().find(|t| (t.rect.left() - r.left()).abs() < 1.0 && (t.rect.top() - r.bottom()).abs() < 4.0).map(|t| t.rect);
        let y = under.map_or(r.center().y, |u| (r.top() + u.bottom()) / 2.0);
        self.click_at(egui::pos2(self.width - 16.0 - 20.0, y));
    }

    /// Click the link `link` after the words of a sentence (the last thing
    /// in it, or followed by another link after a dot).
    fn click_link(&mut self, link: &str) {
        let glued = link.replace(' ', "\u{a0}");
        let at_end = |t: &Text| {
            let i = t.text.rfind(&glued)?;
            let rest = &t.text[i + glued.len()..];
            (i > 0 && (rest.is_empty() || rest.starts_with(" ·"))).then(|| t.text[..i].chars().count())
        };
        let (t, at) = self.texts.iter().find_map(|t| at_end(t).map(|at| (t.clone(), at))).unwrap_or_else(|| panic!("no link {link:?}"));
        let middle = at + link.chars().count() / 2;
        let r = t.galley.pos_from_cursor(&t.galley.from_ccursor(egui::text::CCursor::new(middle)));
        self.click_at(t.pos + r.center().to_vec2());
    }

    fn type_text(&mut self, text: &str) {
        self.run(vec![Event::Text(text.into())]);
        self.settle();
    }

    fn key(&mut self, key: egui::Key) {
        self.run(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
        self.settle();
    }

    fn acts(&self) -> Vec<Act> {
        self.port.take_acts()
    }

    fn asks(&mut self) -> Vec<Asked> {
        std::mem::take(&mut self.asked)
    }
}

fn scripted(f: impl FnOnce(&mut State)) -> Arc<ScriptedSettings> {
    let p = ScriptedSettings::new();
    p.set(f);
    Arc::new(p)
}

fn snap(s: &str) -> Arc<Snapshot> {
    ScriptedEngine::new(s).snapshot()
}

fn speaker(id: &str) -> H {
    speaker_in(id, "kitchen-playing", |_| ())
}

fn speaker_in(id: &str, scenario: &str, f: impl FnOnce(&mut State)) -> H {
    let port = scripted(f);
    let view = SpeakerOptions::new(port.clone(), id);
    H::new(port, Box::new(view), snap(scenario))
}

fn sid(id: &str) -> SpeakerId {
    SpeakerId::new(id)
}

// -------------------------------------------------------------------------
// A speaker's view
// -------------------------------------------------------------------------

fn draw_confirmation(remove: bool, confirm: bool) -> Vec<Command> {
    use crate::shell::WindowContent;
    let port = scripted(|_| ());
    let snap = snap("kitchen-playing");
    let mut dialog = super::speaker::ConfirmSettings { id: sid("kitchen"), name: "Kitchen".into(), remove, port: port.clone() };
    let ctx = preview::context(crate::fonts::definitions(&[], &mut |_| None).0, Mode::Light);
    let mut commands = Vec::new();
    let mut pos = Pos2::ZERO;
    for frame in 0..5 {
        let mut input = preview::input(dialog.sizes().0, 1.0, frame as f64 * 0.05);
        if frame >= 2 {
            input.events.push(Event::PointerMoved(pos));
            if frame > 2 { input.events.push(Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: frame == 3, modifiers: Default::default() }); }
        }
        let out = ctx.run(input, |ctx| {
            dialog.show(ctx, &snap, &mut |a| if let crate::shell::Ask::Command(c) = a { commands.push(c); });
        });
        if frame == 1 {
            let mut text = Vec::new(); for s in out.shapes { collect(&s.shape, s.clip_rect, &mut text); }
            if remove { assert!(text.iter().any(|t| t.text == "The sound moves to this PC.")); }
            let label = if !confirm { "Cancel" } else if remove { "Remove" } else { "Reset" };
            pos = text.iter().find(|t| t.text == label).unwrap().rect.center();
        }
    }
    assert_eq!(port.acts(), if confirm { vec![Act::ClearSpeakerCredentials("kitchen".into())] } else { Vec::new() });
    commands
}

#[test]
fn reset_and_remove_only_act_after_confirmation() {
    assert!(draw_confirmation(false, false).is_empty());
    assert!(draw_confirmation(true, false).is_empty());
    assert_eq!(draw_confirmation(false, true), vec![Command::ResetSpeaker(sid("kitchen"))]);
    assert_eq!(draw_confirmation(true, true), vec![Command::RemoveSpeaker(sid("kitchen"))]);
}

#[test]
fn unavailable_settings_and_their_shortcut_link_are_hidden() {
    let port = scripted(|s| { s.general.shortcuts = None; s.general.play_calls = None; s.general.what_plays = None; s.general.language = None; });
    let h = H::new(port.clone(), Box::new(SettingsView::new(port.clone())), snap("kitchen-playing"));
    for label in ["Keyboard shortcuts for the delay", "Play calls on the speaker too", "Only send some apps to the speaker", "Language"] {
        assert!(!h.shows(label));
    }
    let h = H::new(port.clone(), Box::new(SpeakerOptions::new(port, "kitchen")), snap("kitchen-playing"));
    assert!(!h.shows("Keyboard shortcuts") && !h.shows("From the keyboard"));
}

#[test]
fn speaker_pages_keep_the_controls_in_reading_order() {
    for (id, scenario, first, closing) in [
        ("kitchen", "kitchen-playing", "Play a test chime", "Reset these settings…"),
        ("office", "kitchen-playing", "Switch to Office", "Reset these settings…"),
        ("kitchen", "pc-kitchen-playing-something-else", "Switch to Kitchen", "Reset these settings…"),
        ("garage", "kitchen-playing", "Search again", "Remove Garage…"),
        ("den", "pc-den-needs-code", "Enter code…", "Reset these settings…"),
    ] {
        let mut h = speaker_in(id, scenario, |_| ());
        let mut y = -1.0;
        for label in [first, "Delay", "Tone", "Volume limit", "Name in this app", closing, "Technical details"] {
            let r = h.find(label).unwrap_or_else(|| panic!("{id}: {label}"));
            assert!(r.top() > y, "{id}: {label}"); y = r.top();
        }
        if id == "garage" {
            assert!(!h.has("Reset these settings…") && !h.has("Play a test chime"));
            h.click("Search again"); assert_eq!(h.acts(), vec![Act::SearchAgain]);
        } else if id == "office" {
            h.click(first);
            let row = h.snap.panel.list.rows.iter().find(|r| r.place == Place::Speaker(sid(id))).unwrap();
            assert_eq!(h.asked, vec![Asked::Command(row.command.clone().unwrap())]);
        }
        h.click(closing); assert!(h.asks().contains(&Asked::Dialog));
    }
}

#[test]
fn a_busy_chime_asks_in_the_page_before_playing() {
    let mut h = speaker_in("kitchen", "pc-kitchen-playing-something-else", |_| ());
    h.click("Play a test chime");
    assert_eq!(h.asks(), vec![Asked::Command(Command::PlayChime { speaker: sid("kitchen"), confirmed: false })]);
    let mut scene = h.snap.scene.clone();
    scene.situation.confirm_in_panel = Some(crate::model::situation::Confirm::ChimeBusy { speaker: sid("kitchen"), who: None });
    h.snap = Arc::new(crate::model::view::view(scene, None, false, 2));
    h.settle();
    assert!(h.has("Kitchen is playing something else. A test chime stops it, and it may not start again by itself."));
    let yes = h.texts.iter().rfind(|t| t.text == "Play a test chime").unwrap().rect;
    h.click_at(yes.center());
    assert_eq!(h.asks(), vec![Asked::Command(Command::Confirm { yes: true })]);
}

#[test]
fn the_page_uses_the_panel_delay_commands_and_the_selected_ways_limits() {
    let mut h = speaker("kitchen");
    let crate::model::panel::About::Delay(d) = h.snap.panel.about.clone() else { panic!() };
    for (i,row) in d.rows.iter().enumerate() {
        for button in [&row.small, &row.large] {
            let r = h.texts.iter().filter(|t| t.text == button.label).nth(i).unwrap().rect;
            h.click_at(r.center()); assert_eq!(h.asks(), vec![Asked::Command(button.command.clone())]);
        }
    }
    let mut h = speaker_in("kitchen", "pc-plain", |_| ());
    h.click("AirPlay");
    assert_eq!(h.asks(), vec![Asked::Command(Command::ChooseWay { speaker: sid("kitchen"), way: crate::model::situation::WayChoice::AirPlaySteady })]);
}

#[test]
fn exact_delay_sets_the_displayed_total_and_the_reply_reports_the_limit() {
    use crate::engine::place_driver::PlaceDriver;
    use crate::model::{Effect, Surface};
    let app = crate::app::App::for_tests();
    let mut driver = PlaceDriver::new(app.clone(), crate::platform::fake::FakePlatform::new().platform(),
        Default::default(), std::time::Instant::now());
    app.set_delay_to("kitchen", 150).unwrap();
    let mut h = speaker("kitchen");
    let refresh = |h: &mut H| {
        let status = app.delay_status_of("kitchen");
        let mut scene = h.snap.scene.clone();
        let kitchen = scene.speakers.iter_mut().find(|s| s.id == sid("kitchen")).unwrap();
        let way = kitchen.ways.iter_mut().find(|w| w.way == kitchen.way).unwrap();
        way.delay.own_ms = status.own_delay_ms;
        way.delay.change_ms = status.change_ms;
        let limits = status.limits.unwrap();
        way.range = crate::model::facts::DelayRange { max_removed_ms: limits.remove_ms, max_added_ms: limits.add_ms };
        h.snap = Arc::new(crate::model::view::view(scene, None, false, h.snap.version + 1));
        h.settle();
    };
    refresh(&mut h);
    for (target, expected_step, enter) in [("175", -25, false), ("99999", 99799, true)] {
        h.click("Type an exact delay…");
        let before = app.delay_status_of("kitchen");
        assert!(h.has(&before.delay_ms.to_string()));
        assert!(!h.view.holds_unused_text());
        h.type_text(target);
        assert!(h.has(target) && h.view.holds_unused_text(), "typing replaces the selected total");
        if enter { h.key(egui::Key::Enter); } else { h.click("Set"); }
        let command = Command::ChangeDelay { speaker: sid("kitchen"), step: DelayStep::SetTotal(expected_step) };
        assert_eq!(h.asks(), vec![Asked::Command(command.clone())]);
        assert!(h.has("Type an exact delay…"));
        let Effect::Delay(reply) = driver.command(Surface::Panel, &command).unwrap() else { panic!("missing delay reply") };
        let upper = before.limits.unwrap().add_ms as i32;
        assert_eq!((reply.after_ms, reply.stopped_at_limit), (expected_step.min(upper), expected_step > upper));
        refresh(&mut h);
    }
    assert!(h.has("Kitchen can’t play with more delay than this."));
    h.click("Type an exact delay…");
    h.click("Use the default: 200 ms");
    let reset = Command::ChangeDelay { speaker: sid("kitchen"), step: DelayStep::Reset };
    assert_eq!(h.asks(), vec![Asked::Command(reset.clone())]);
    driver.command(Surface::Panel, &reset).unwrap();
    refresh(&mut h);
    assert!(h.has("Type an exact delay…") && !h.shows("Use the default"));
    h.click("Type an exact delay…");
    assert!(h.has("200") && !h.shows("Use the default"));
    app.request_shutdown();
}

#[test]
fn exact_delay_can_be_dismissed_and_is_hidden_for_a_fixed_way() {
    let mut h = speaker("kitchen");
    for escape in [true, false] {
        h.click("Type an exact delay…");
        h.type_text("25");
        if escape {
            h.key(egui::Key::Escape);
        } else {
            // Tab from the field to Set, then to the close button.
            h.key(egui::Key::Tab);
            h.key(egui::Key::Tab);
            h.key(egui::Key::Enter);
        }
        assert!(h.asks().is_empty() && h.has("Type an exact delay…"));
    }
    let h = speaker_in("kitchen", "kitchen-fixed-way", |_| ());
    assert!(!h.has("Type an exact delay…") && !h.has("Set"));
}

#[test]
fn tone_name_and_speaker_commands() {
    let mut h = speaker("kitchen");
    h.click("Reset tone");
    assert_eq!(h.acts(), vec![Act::Tone("kitchen".into(), ToneAct::Flat)]);
    h.click("More tone controls…"); assert!(h.has("31 Hz"));
    h.click("Allow");
    assert_eq!(h.acts(), vec![Act::Allow { address: "192.168.1.51".into(), speaker: "kitchen".into() }]);
    h.flip("Show song titles on Kitchen");
    assert_eq!(h.acts(), vec![Act::SongTitles("kitchen".into(), false)]);
    let label = h.find("Name in this app").unwrap();
    h.click_at(egui::pos2(label.left()+40.0,label.bottom()+20.0));
    h.type_text(" shelf"); h.key(egui::Key::Enter);
    assert!(matches!(h.acts().as_slice(), [Act::ShowAs(id, _)] if id == "kitchen"));
    assert!(h.has("Saved"));
    h.click("Technical details"); assert!(h.shows("Connection:") && h.has("Copy"));
    h.click("Hide from the list"); assert_eq!(h.acts(), vec![Act::HideSpeaker("kitchen".into())]);
    let mut h = speaker_in("den", "pc-den-needs-code", |_| ());
    h.click("Enter code…"); assert_eq!(h.asks(), vec![Asked::Open(Open::Pair(sid("den")))]);
}

#[test]
fn recording_the_delay_keys_uses_two_separate_presses() {
    use crate::platform::DelayKeys;
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
    assert!(h.has("Hold Shift with a shortcut to change the delay by 100 ms."));
    h.click("Change…");
    assert_eq!(h.acts(), [Act::RecordShortcuts(true)]);
    let key = |key, repeat| Event::Key {
        key, physical_key: None, pressed: true, repeat, modifiers: egui::Modifiers::CTRL,
    };
    h.run(vec![key(egui::Key::F9, false)]);
    assert!(h.acts().is_empty());
    h.key(egui::Key::Escape);
    assert_eq!(h.acts(), [Act::RecordShortcuts(false)]);
    assert!(h.asks().is_empty());
    assert_eq!(h.port.general().shortcuts.unwrap().remove, DelayKeys::default().0[0]);
    h.click("Change…");
    assert_eq!(h.acts(), [Act::RecordShortcuts(true)]);
    h.run(vec![key(egui::Key::ArrowDown, false)]);
    h.run(vec![key(egui::Key::ArrowDown, true)]);
    assert!(h.acts().is_empty());
    h.run(vec![Event::Key { key: egui::Key::ArrowDown, physical_key: None, pressed: false,
        repeat: false, modifiers: egui::Modifiers::CTRL }]);
    h.run(vec![key(egui::Key::ArrowDown, false)]);
    h.settle();
    assert!(h.has("Ctrl+Down is already the shortcut for Remove 25 ms. Press other keys for Add 25 ms… (Esc stops)"));
    assert!(h.acts().is_empty());
    h.run(vec![key(egui::Key::ArrowUp, false)]);
    assert_eq!(h.acts(), [
        Act::ShortcutKeys(DelayKeys(["Down", "Up"].map(|k| vec!["Ctrl".into(), k.into()]))),
        Act::RecordShortcuts(false),
    ]);
}

#[test]
fn the_remove_shortcut_has_no_mark_until_the_pair_is_saved() {
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
    h.click("Change…");
    let press = |key| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::CTRL };
    h.run(vec![press(egui::Key::ArrowDown)]);
    let line = "Remove 25 ms: Ctrl+Down";
    let plain_left = h.find("Press the new keys for Add 25 ms… (Esc stops)").unwrap().left();
    assert_eq!(h.find(line).unwrap().left(), plain_left, "the pending keys have no leading mark");
    h.run(vec![press(egui::Key::ArrowUp)]);
    h.settle();
    assert!(h.find(line).unwrap().left() > plain_left, "the saved keys have a leading mark");
}

#[test]
fn a_held_alt_left_stays_in_the_shortcut_recorder() {
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
    h.click("Change…");
    let key = |key, repeat| Event::Key {
        key, physical_key: None, pressed: true, repeat, modifiers: egui::Modifiers::ALT,
    };
    h.run(vec![key(egui::Key::ArrowLeft, false), key(egui::Key::ArrowLeft, true)]);
    h.run(vec![key(egui::Key::ArrowLeft, true)]);
    assert!(h.asks().is_empty());
    assert_eq!(h.acts(), [Act::RecordShortcuts(true)]);
    h.run(vec![key(egui::Key::ArrowRight, false)]);
    assert_eq!(h.acts(), [
        Act::ShortcutKeys(crate::platform::DelayKeys(["Left", "Right"].map(|k| vec!["Alt".into(), k.into()]))),
        Act::RecordShortcuts(false),
    ]);
}

#[test]
fn choosing_apps_hides_the_call_switch_and_keeps_its_value() {
    let port = scripted(|s| s.general.play_calls = Some(true));
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
    let calls = "Play calls on the speaker too";
    let apps = "Only send some apps to the speaker";
    assert!(h.has(calls));
    h.flip(apps);
    assert!(!h.has(calls));
    assert!(h.has("The rest keep playing on this PC. In Windows, set the output of each app you choose to the one with “Stream To Speaker” in its name."));
    h.click("Choose apps…");
    assert_eq!(h.asks(), [Asked::Open(Open::VolumeMixer)]);
    h.flip(apps);
    assert!(h.has(calls));
    assert_eq!(h.port.general().play_calls, Some(true));
    assert!(!h.has("Choose apps…"));
}

#[test]
fn quitting_explains_the_silence_only_for_chosen_apps() {
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
    let note = "Your chosen apps are silent while Stream To Speaker isn’t running.";
    assert!(!h.has(note));
    h.flip("Only send some apps to the speaker");
    assert!(h.find(note).unwrap().top() >= h.find("Quit").unwrap().bottom());
    h.click("Quit");
    assert_eq!(h.asks(), [Asked::Command(Command::Quit)]);
    h.flip("Only send some apps to the speaker");
    assert!(!h.has(note));
}

#[test]
fn each_settings_switch_changes_its_own_field() {
    let port = scripted(|s| { s.general.shortcuts.as_mut().unwrap().on = false; s.ma.offer = Some(false); });
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
    for label in ["Guides", "Lab", "…and go back to the speaker I was using", "Bring the sound back to this PC when a speaker stops responding",
        "Tell me when sound leaves the place I chose", "Let any device on this network listen", "Start Stream To Speaker when I sign in", "Offer this PC to Music Assistant"] {
        assert!(!h.has(label));
    }
    for (label, act) in [
        ("Keyboard shortcuts for the delay", Act::Shortcuts(true)),
        ("Control from your phone", Act::Phone(false)),
        ("Play calls on the speaker too", Act::PlayCalls(true)),
        ("Only send some apps to the speaker", Act::WhatPlays(WhatPlays::OnlySentApps)),
        ("Start when I sign in to Windows", Act::StartAtSignIn(false)),
        ("Switch back to this PC after 30 minutes with nothing playing", Act::SwitchBackAfterQuiet(true)),
        ("Show pop-ups by the clock about where the sound is playing", Act::NotesByClock(false)),
        ("Let Music Assistant play this PC’s sound", Act::OfferToMa(true)),
        ("Let scripts and home automation control this app", Act::Api(true)),
    ] {
        let mut expected = h.port.state();
        match &act {
            Act::Shortcuts(v) => expected.general.shortcuts.as_mut().unwrap().on = *v,
            Act::Phone(v) => { let p = expected.privacy.phone.as_mut().unwrap(); p.on = *v; p.link = None; }
            Act::PlayCalls(v) => expected.general.play_calls = Some(*v),
            Act::WhatPlays(v) => expected.general.what_plays = Some(*v),
            Act::StartAtSignIn(v) => expected.general.start_at_sign_in = Some(Ok(*v)),
            Act::SwitchBackAfterQuiet(v) => expected.general.switch_back_after_quiet = *v,
            Act::NotesByClock(v) => expected.general.notes_by_clock = *v,
            Act::OfferToMa(v) => expected.ma.offer = Some(*v),
            Act::Api(v) => expected.privacy.api.as_mut().unwrap().on = *v,
            _ => unreachable!(),
        }
        h.flip(label); assert_eq!(h.acts(), vec![act], "{label}");
        let actual = h.port.state();
        assert_eq!((actual.general, actual.privacy, actual.ma), (expected.general, expected.privacy, expected.ma));
    }
    assert_eq!(h.asks(), vec![Asked::Open(Open::MusicAssistantSettings)]);
    // This app opens the page that has the update: the line doesn't call
    // it ready to install, and the button says where it leads.
    assert!(h.has("Version 0.2.1 is available.") && !h.shows("ready to install"));
    h.click("Open the download page"); assert_eq!(h.acts(), vec![Act::WhatsNew]);
    h.click("Report a problem"); assert_eq!(h.asks(), vec![Asked::Dialog]);
    h.click("About"); assert_eq!(h.asks(), vec![Asked::View("About".into())]);
    h.click("Quit"); assert_eq!(h.asks(), vec![Asked::Command(Command::Quit)]);
}

#[test]
fn speaker_missing_adds_by_address_in_place() {
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(MissingView::new(port.clone())), snap("kitchen-playing"));
    assert_eq!(h.acts(), vec![Act::ResetProbe], "a new search when it opens");
    let field = h.find("192.168.1.20 or kitchen.local").unwrap();
    h.click_at(field.center());
    h.type_text("192.168.20.40");
    h.click("Add");
    assert_eq!(h.acts(), vec![Act::Find { address: "192.168.20.40".into(), name: String::new() }]);
    assert!(h.shows("Checking 192.168.20.40…"));
    // Found one way: added at once.
    port.set(|s| {
        s.probe = Probe::Found {
            address: "192.168.20.40".into(),
            ways: vec![Found { name: "Garage".into(), kind: SpeakerKind::AirPlay, delay_ms: 2000, way: Way::AirPlay }],
        }
    });
    h.settle();
    assert_eq!(h.acts(), vec![Act::AddFound { way: Way::AirPlay, name: "Garage".into() }]);
    assert!(h.shows("Added Garage."));
    // Two ways: which one, then *Add*.
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(MissingView::new(port.clone()).with_address("192.168.20.40")), snap("kitchen-playing"));
    h.acts();
    port.set(|s| {
        s.probe = Probe::Found {
            address: "192.168.20.40".into(),
            ways: vec![
                Found { name: "Garage".into(), kind: SpeakerKind::Sonos, delay_ms: 200, way: Way::Upnp },
                Found { name: "Garage".into(), kind: SpeakerKind::AirPlay, delay_ms: 2000, way: Way::AirPlay },
            ],
        }
    });
    h.settle();
    assert!(h.acts().is_empty(), "nothing added before the person picks");
    h.click("AirPlay");
    h.click("Add Garage");
    assert_eq!(h.acts(), vec![Act::AddFound { way: Way::AirPlay, name: "Garage".into() }]);
    assert!(!h.view.holds_unused_text(), "added: nothing is left to lose");
    // Nothing answered: the address typed keeps the view open.
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(MissingView::new(port.clone()).with_address("192.168.1.20")), snap("kitchen-playing"));
    h.click("Add");
    port.set(|s| s.probe = Probe::Nothing { address: "192.168.1.20".into() });
    h.settle();
    assert!(h.shows("Nothing at 192.168.1.20 answered as a speaker."));
    assert!(h.view.holds_unused_text(), "an address typed and not added keeps the view");
}

#[test]
fn the_discovery_check_explains_why_search_is_off() {
    let port = scripted(|_| ());
    let mut scene = snap("kitchen-playing").scene.clone();
    scene.search.phase = crate::model::facts::SearchPhase::Off;
    let h = H::new(port.clone(), Box::new(CheckView::new(port)), Arc::new(crate::model::view::view(scene, None, false, 1)));
    assert!(h.has("Not looking for speakers: speaker search was turned off when the app was started."));
    assert!(!h.shows("Looking on") && !h.shows("network") && !h.shows("found") && !h.has("Search again"));
}

#[test]
fn the_discovery_check_cautions_about_a_public_network() {
    // A private network is said as found, and nothing is said of Windows
    // Firewall, which the app doesn't read.
    let port = scripted(|_| ());
    let h = H::new(port.clone(), Box::new(CheckView::new(port)), snap("kitchen-playing"));
    assert!(h.has("Windows treats “HomeNet” as a private network.") && !h.shows("Firewall"));
    // A public one is a caution, with the way to Windows' page for it.
    let port = scripted(|s| s.discovery.lines = vec![FindLine::Profile { network: "HomeNet".into(), private: false }]);
    let mut h = H::new(port.clone(), Box::new(CheckView::new(port)), snap("kitchen-playing"));
    assert!(h.has("Windows treats “HomeNet” as a public network."));
    assert!(h.shows("Windows Firewall may keep speakers from reaching this PC.") && h.shows("choose Private."));
    h.click("Open network settings");
    assert_eq!(h.asks(), vec![Asked::Open(Open::NetworkSettings)]);
    assert!(h.acts().is_empty());
}

#[test]
fn speaker_missing_searches_shows_hidden_speakers_and_opens_the_check() {
    let port = scripted(|s| {
        let mut tv = s.speakers.hidden[0].clone();
        tv.id = "guest".into();
        tv.name = "Guest Room TV".into();
        tv.cant_play = Some(CantPlay::AirPlayOff);
        s.speakers.speakers.push(tv);
    });
    let mut h = H::new(port.clone(), Box::new(MissingView::new(port)), snap("kitchen-playing"));
    h.acts();
    assert!(h.has("Looking on Wi-Fi “HomeNet” and Ethernet 2"), "no count of speakers");
    h.click("Search again");
    h.click("Show");
    h.flip("Look for Sendspin speakers");
    assert_eq!(h.acts(), vec![Act::SearchAgain, Act::ShowSpeaker("neighbour".into()), Act::SendspinSpeakers(true)]);
    assert!(h.has("Guest Room TV") && h.shows("Turn on AirPlay in the Apple TV’s settings."));
    h.click("All networks");
    h.click("Ethernet 2");
    assert_eq!(h.acts(), vec![Act::LookOn(Some("eth2".into()))]);
    h.click("Check step by step");
    assert_eq!(h.asks(), vec![Asked::View("Check step by step".into())]);
}

#[test]
fn the_playback_check_shows_only_chosen_apps_after_the_roles_return() {
    let port = scripted(|s| s.playback.playing = Some(Err(None)));
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("apps-panel-playing"));
    assert!(h.has("Only chosen apps play here"));
    let mut scene = h.snap.scene.clone();
    scene.situation.standing.retain(|s| !matches!(s, crate::model::situation::Standing::OnlySomeApps { .. }));
    h.snap = Arc::new(crate::model::view::view(scene, None, false, 2));
    h.settle();
    assert!(!h.has("Only chosen apps play here"));
}

#[test]
fn cant_hear_says_what_the_panel_says_while_the_app_connects_by_itself() {
    let not_connected = || {
        scripted(|s| {
            s.playback.connected = Some(Err(()));
            s.playback.taking = None;
            s.playback.volume = None;
        })
    };
    // Connecting because sound started: no fault, and nothing to press.
    let port = not_connected();
    let h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-connecting"));
    assert!(h.has("Connecting to Kitchen…") && !h.has("Not connected.") && !h.has("Connect"));
    // A step the app is still working on isn't a broken one.
    assert!(h.shows("This check updates by itself as things change.") && !h.shows("first broken step"));
    // Trying again after the speaker stopped responding: the panel's line
    // under it.
    let port = not_connected();
    let h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-not-responding-trying"));
    assert!(h.has("Connecting to Kitchen…") && h.shows("Not responding. Trying again…"));
    assert!(!h.has("Not connected.") && !h.has("Connect"));
    // Nothing under way: the step is broken, and Connect is its fix.
    let port = not_connected();
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port.clone())), snap("kitchen-playing"));
    assert!(h.has("Not connected."));
    h.click("Connect");
    assert_eq!(port.acts(), vec![Act::Reconnect]);
}

#[test]
fn cant_hear_offers_the_one_fix_of_the_first_broken_step() {
    let port = scripted(|s| s.playback.volume = Some(Err(Volume::Muted)));
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    h.click("Unmute");
    assert_eq!(h.asks(), vec![Asked::Command(Command::SetMuted { owner: VolumeOwner::Speaker(crate::model::samples::kitchen()), muted: false })]);
    // The level sent to Music Assistant is the app's own to set.
    let port = scripted(|s| s.playback.volume = Some(Err(Volume::Zero)));
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("music-assistant-playing"));
    h.click("Set to 20");
    assert_eq!(h.asks(), vec![Asked::Command(Command::SetVolume { owner: VolumeOwner::MusicAssistant, level: 20 })]);
    let port = scripted(|s| s.playback.volume = Some(Err(Volume::Muted)));
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("music-assistant-playing"));
    h.click("Unmute");
    assert_eq!(h.asks(), vec![Asked::Command(Command::SetMuted { owner: VolumeOwner::MusicAssistant, muted: false })]);
    // With every step good, a test chime is offered where the place can
    // play one: Music Assistant can while it plays this PC's sound.
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("music-assistant-playing"));
    assert!(h.shows("Everything this PC can check looks right.") && h.has("Write a report"));
    h.click("Play a test chime");
    assert_eq!(h.asks(), vec![Asked::Command(Command::PlayTestSound(SpeakerId::music_assistant()))]);
    // Nothing plays: a chime is the way to make a sound, where the place
    // can play one. Music Assistant with no server connected can't.
    let nothing = |s: &mut State| {
        s.playback.playing = Some(Err(None));
        s.playback.windows = None;
        s.playback.connected = None;
        s.playback.taking = None;
        s.playback.volume = None;
    };
    let port = scripted(nothing);
    let h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    assert!(h.has("Nothing on this PC is playing right now.") && h.has("Play a test chime"));
    let port = scripted(nothing);
    let h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("music-assistant-not-connected"));
    assert!(h.has("Nothing on this PC is playing right now.") && !h.has("Play a test chime"));
    // Music Assistant with no server connected, while sound is sent: said
    // in the panel's words, with its page in place of a Connect button.
    let port = scripted(|s| {
        s.playback.speaker = Some("Music Assistant".into());
        s.playback.connected = Some(Err(()));
        s.playback.server_away = true;
        s.playback.taking = None;
        s.playback.volume = None;
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("music-assistant-not-connected"));
    assert!(h.has("Music Assistant isn’t connected.") && h.has("This PC’s sound isn’t heard until it connects."));
    assert!(!h.has("Connect") && !h.shows("Connected to") && !h.has("Not connected."));
    assert!(h.shows("It stops at the first broken step."));
    h.click("How to connect it");
    assert_eq!(h.asks(), vec![Asked::Open(Open::MusicAssistantSettings)]);
    // The app can't read the sound Windows sends it: said, with the way
    // to hear the sound meanwhile, and the check ends there.
    let port = scripted(|s| {
        s.playback.windows = Some(Err(WindowsSends::InputLost));
        s.playback.connected = None;
        s.playback.taking = None;
        s.playback.volume = None;
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    assert!(h.has("Stream To Speaker can’t read this PC’s sound.") && h.has("It keeps trying. Kitchen is silent meanwhile."));
    assert!(!h.shows("Connected to") && !h.shows("Everything this PC can check looks right."));
    h.click("Switch to this PC");
    assert_eq!(h.asks(), vec![Asked::Command(Command::Switch { place: crate::model::state::Place::ThisPc, way: None })]);
    // An app that stayed on another output while the rest plays on the
    // speaker: a remark with the same ways out, and the check goes on.
    let port = scripted(|s| {
        s.playback.windows = Some(Ok(WindowsSends::AppElsewhere { app: "Chrome".into(), output: "Speakers".into() }));
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-except-an-app"));
    assert!(h.has("Chrome is still playing on this PC’s Speakers. The rest goes to Kitchen."));
    assert!(h.has("Connected to Kitchen.") && h.has("Kitchen is taking the sound.") && h.shows("32, not muted"));
    assert!(h.shows("This check updates by itself as things change.") && !h.shows("first broken step"), "a remark isn't a broken step");
    assert!(!h.shows("Everything this PC can check looks right."), "something was found");
    h.click("Set Chrome’s output in Windows…");
    assert_eq!(h.asks(), vec![Asked::Open(Open::VolumeMixer)]);
    // An app that stayed on one of this PC's own outputs: the line says
    // which one, and that it isn't the speaker.
    let port = scripted(|s| {
        s.playback.playing = Some(Ok(Some("Chrome".into())));
        s.playback.windows = Some(Err(WindowsSends::AppElsewhere { app: "Chrome".into(), output: "Speakers".into() }));
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    assert!(h.has("Chrome is still playing on this PC’s Speakers, not on Kitchen."));
    // The two ways out: a restart, named for the app, and Windows' page
    // of apps and their outputs.
    assert!(h.shows("Restart Chrome: some apps move to a new output only after a restart."));
    h.click("Set Chrome’s output in Windows…");
    assert_eq!(h.asks(), vec![Asked::Open(Open::VolumeMixer)]);
    let port = scripted(|s| {
        s.playback.taking = Some(Err(Taking::NotAsking { two_ways: true }));
        s.playback.volume = None;
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    h.click("Try Kitchen again");
    assert_eq!(h.asks(), vec![Asked::Command(Command::Reconnect(crate::model::samples::kitchen()))]);
    h.click("Switch to Kitchen by AirPlay");
    assert_eq!(h.asks(), vec![Asked::Command(Command::ChooseWay { speaker: crate::model::samples::kitchen(), way: crate::model::situation::WayChoice::AirPlaySteady })]);
    assert!(!h.shows("volume"), "the check stops at the first broken step");
    // Music Assistant that hasn't started this PC's sound: the panel's
    // note and its way out, in place of trying a speaker again.
    let port = scripted(|s| {
        s.playback.taking = Some(Err(Taking::NotAsking { two_ways: false }));
        s.playback.volume = None;
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("music-assistant-playing"));
    assert!(h.has("Music Assistant hasn’t started playing.") && !h.has("Try Music Assistant again"));
    h.click("How to start it");
    assert_eq!(h.asks(), vec![Asked::Open(Open::MusicAssistantHowTo)]);
    // A speaker paused with its own buttons while sound is sent: the
    // panel's note and its way out.
    let port = scripted(|s| {
        s.playback.taking = Some(Err(Taking::Paused));
        s.playback.volume = None;
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    assert!(h.has("Kitchen was paused.") && h.shows("It was paused with its own buttons or app, so this PC’s sound isn’t heard."));
    assert!(!h.shows("is taking the sound") && !h.shows("Everything this PC can check looks right."));
    assert!(h.shows("It stops at the first broken step.") && !h.shows("volume"));
    h.click("Switch to this PC");
    assert_eq!(h.asks(), vec![Asked::Command(Command::Switch { place: crate::model::state::Place::ThisPc, way: None })]);
    let port = scripted(|s| {
        s.playback.taking = Some(Ok(Taking::CantTell));
        s.playback.on_short = true;
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    h.click("Use “Steadier”");
    assert_eq!(h.asks(), vec![Asked::Command(Command::UseDelayChoice { speaker: sid("kitchen"), choice: crate::model::facts::DelayChoice::Steady })]);
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    assert!(h.shows("Everything this PC can check looks right."));
    h.click("Play a test chime");
    assert_eq!(h.asks(), vec![Asked::Command(Command::PlayTestSound(crate::model::samples::kitchen()))]);
}

#[test]
fn cant_hear_says_which_device_privacy_turned_away_and_allows_nothing() {
    let away = |device: Option<&str>| {
        let device = device.map(String::from);
        scripted(move |s| {
            s.playback.taking = Some(Err(Taking::NotAsking { two_ways: false }));
            s.playback.turned_away = Some(TurnedAway { device, address: "192.168.1.51".into() });
            s.playback.volume = None;
        })
    };
    let port = away(Some("Kitchen Sub"));
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    // The broken step stays the speaker's, with its ways out.
    assert!(h.has("Kitchen accepted the connection but isn’t asking for the sound.") && h.has("Try Kitchen again"));
    assert!(h.shows("It stops at the first broken step.") && !h.shows("is taking the sound") && !h.shows("volume"));
    // The device is named with its address, and the page allows nothing:
    // its button opens the speaker's page, which lists the refused ones.
    assert!(h.has("Kitchen Sub (192.168.1.51) asked for the sound and was refused."));
    assert!(h.shows("Only Kitchen is allowed to listen. If that device is yours and plays together with Kitchen, you can allow it in Kitchen’s settings."));
    assert!(!h.has("Allow") && !h.has("Allow it"));
    h.click("Open Kitchen’s settings");
    assert_eq!(h.asks(), vec![Asked::Open(Open::SpeakerSettings(sid("kitchen")))]);
    assert_eq!(h.acts(), vec![]);
    // A device the search doesn't know goes by its address.
    let port = away(None);
    let h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    assert!(h.has("A device at 192.168.1.51 asked for the sound and was refused."));
    // Without one, the step has nothing to say about privacy.
    let port = scripted(|s| {
        s.playback.taking = Some(Err(Taking::NotAsking { two_ways: false }));
        s.playback.volume = None;
    });
    let h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("kitchen-playing"));
    assert!(!h.shows("was refused") && !h.shows("allowed to listen"));
}

#[test]
fn cant_hear_says_why_the_sound_stayed_on_this_pc() {
    // Windows kept its own output when Kitchen was picked: the panel's
    // note with the ways out, where the page otherwise says to pick a
    // speaker.
    let kitchen = crate::model::samples::kitchen();
    let mut scene = snap("pc-plain").scene.clone();
    scene.situation.standing.push(crate::model::situation::Standing::WindowsDidntSwitch { speaker: kitchen.clone() });
    let port = scripted(|s| s.playback.speaker = None);
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), Arc::new(crate::model::view::view(scene, None, false, 1)));
    assert!(h.has("Your sound is on this PC.") && h.has("Windows didn’t switch to Kitchen."));
    assert!(h.shows("Try again, or choose this app’s output in Windows’ sound settings.") && !h.shows("Pick a speaker in the panel"));
    h.click("Try Kitchen again");
    assert_eq!(h.asks(), vec![Asked::Command(Command::Switch { place: crate::model::state::Place::Speaker(kitchen), way: None })]);
    h.click("Open Windows’ sound settings");
    assert_eq!(h.asks(), vec![Asked::Open(Open::SoundSettings)]);
    let port = scripted(|s| s.playback.speaker = None);
    let h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap("pc-plain"));
    assert!(h.has("Your sound is on this PC.") && h.shows("Pick a speaker in the panel") && !h.shows("Windows didn’t switch"));
}

#[test]
fn music_assistant_about_and_lab_act_at_once() {
    let port = scripted(|s| s.ma.paused = true);
    let mut h = H::new(port.clone(), Box::new(MusicAssistantView::new(port)), snap("kitchen-playing"));
    h.click_link("Allow pairing again");
    h.flip("Let Music Assistant play this PC’s sound");
    assert_eq!(h.acts(), vec![Act::AllowPairingAgain, Act::OfferToMa(false)]);
    let port = scripted(|s| s.ma.paired_with = vec!["Music Assistant (Home)".into()]);
    let mut h = H::new(port.clone(), Box::new(MusicAssistantView::new(port)), snap("kitchen-playing"));
    h.click_link("Unpair");
    assert_eq!(h.acts(), vec![Act::Unpair("Music Assistant (Home)".into())]);

    let port = scripted(|s| s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(crate::model::text::LocalTime::new(9, 12)) });
    let mut h = H::new(port.clone(), Box::new(super::AboutView::new(port)), snap("kitchen-playing"));
    h.click("Check now");
    assert!(!h.has("Lab"));
    h.flip("Show Lab");
    h.click("Lab");
    assert_eq!(h.asks(), vec![Asked::View("Lab".into())]);
    h.click("Licences and source");
    h.click("Show the welcome again");
    assert_eq!(h.acts(), vec![Act::CheckNow, Act::ShowLab(true), Act::LicenceAndSource]);
    assert_eq!(h.asks(), vec![Asked::Command(Command::FirstRunAction(crate::model::command::FirstRunAction::ShowAgain))]);

    // About says the same about an update as Settings does: the app opens
    // the page that has it. The line under the daily check says what the
    // check sends and that it installs nothing.
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(super::AboutView::new(port)), snap("kitchen-playing"));
    assert!(h.has("Version 0.2.1 is available.") && !h.shows("ready to install") && !h.has("Update now"));
    assert!(h.shows("Sends only the app’s version to GitHub. Nothing is downloaded or installed by itself."));
    h.click("Open the download page");
    assert_eq!(h.acts(), vec![Act::WhatsNew]);

    let port = scripted(|s| s.lab.mfi = Some(true));
    let mut h = H::new(port.clone(), Box::new(super::LabView::new(port)), snap("kitchen-playing"));
    assert!(h.shows("Settings for testing speakers"), "its warning first");
    h.click_link("Reset everything in Lab");
    assert_eq!(h.acts(), vec![Act::Lab(LabAct::ResetAll)]);
}

// -------------------------------------------------------------------------
// Layout, keyboard, words
// -------------------------------------------------------------------------

fn every_view(port: Arc<ScriptedSettings>) -> Vec<(String, Box<dyn crate::shell::ViewContent>)> {
    let p: Arc<dyn SettingsPort> = port;
    let mut v: Vec<(String, Box<dyn crate::shell::ViewContent>)> = vec![
        ("settings".into(), Box::new(SettingsView::new(p.clone()))),
        ("missing".into(), Box::new(MissingView::new(p.clone()).by_hand())),
        ("missing upnp".into(), Box::new(MissingView::new(p.clone()).by_hand_upnp())),
        ("check".into(), Box::new(CheckView::new(p.clone()))),
        ("cant hear".into(), Box::new(CantHearView::new(p.clone()))),
        ("music assistant".into(), Box::new(MusicAssistantView::new(p.clone()))),
        ("about".into(), Box::new(super::AboutView::new(p.clone()))),
        ("lab".into(), Box::new(super::LabView::new(p.clone()).with_counters())),
    ];
    for id in ["kitchen", "living", "bedroom", "office", "garage", "den", "desk"] {
        v.push((id.into(), Box::new(SpeakerOptions::new(p.clone(), id).with_open("bands").with_open("details"))));
    }
    v
}

fn assert_clean(texts: &[Text], right: f32, what: &str) {
    for (i, a) in texts.iter().enumerate() {
        assert!(a.rect.right() <= right + 0.5, "{what}: {:?} runs past the panel ({} > {right})", a.text, a.rect.right());
        for b in &texts[i + 1..] {
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
        assert!(
            !(a.words_per_row.len() >= 3 && words >= 3 && a.words_per_row.iter().all(|w| *w <= 1)),
            "{what}: {:?} is squeezed into a column of single words",
            a.text
        );
    }
}

#[test]
fn a_speaker_with_no_known_way_of_playing_has_no_delay_part() {
    // Nothing is known of how it plays, so there is nothing to set, and
    // no sentence about hearing a change.
    let mut scene = snap("kitchen-playing").scene.clone();
    scene.speakers.iter_mut().find(|s| s.id == sid("garage")).unwrap().ways.clear();
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(SpeakerOptions::new(port, "garage")), Arc::new(crate::model::view::view(scene, None, false, 1)));
    assert!(h.has("Tone") && h.has("Volume limit"));
    assert!(!h.has("Delay") && !h.shows("isn’t playing now"));
    // Why it isn't found is one click away, as on its row in the panel.
    h.click("Why?");
    assert_eq!(h.asks(), vec![Asked::Open(Open::SpeakerMissing)]);
    // A found speaker that isn't playing has both.
    let h = speaker("office");
    assert!(h.has("Delay") && h.shows("Office isn’t playing now"));
}

#[test]
fn office_inactive_sentence_keeps_its_last_two_words_at_both_scales() {
    let sentence = "Office isn’t playing now: you’ll hear a change the next time it plays.";
    for scale in [1.0, 2.0] {
        let mut h = speaker("office");
        h.scale = scale;
        h.settle();
        // Choose a page width where the original wrap ends in just “plays.”
        // even when the test machine uses fallback fonts.
        let office = h.texts.iter().find(|t| t.text == sentence).unwrap();
        assert!(office.words_per_row.last().unwrap() >= &2, "at panel width, scale {scale}");
        let mut job = (*office.galley.job).clone();
        job.wrap.max_width = f32::INFINITY;
        let unwrapped = h.ctx.fonts(|f| f.layout_job(job.clone()));
        let row = &unwrapped.rows[0];
        let width = row.x_offset(row.glyphs.len() - "plays.".len()).ceil() + 1.0;
        job.wrap.max_width = width;
        let original = h.ctx.fonts(|f| f.layout_job(job));
        assert_eq!(original.rows.last().unwrap().text(), "plays.");
        h.width = width + 32.0;
        h.settle();
        let wrapped = h.texts.iter().find(|t| t.text == sentence).unwrap();
        assert!(wrapped.galley.rows.last().unwrap().text().ends_with("it plays."));
        assert!((original.rows.len()..=original.rows.len()+1).contains(&wrapped.words_per_row.len()));
        assert!((width*0.75..width).contains(&wrapped.galley.job.wrap.max_width));
    }
}

#[test]
fn every_view_holds_at_the_panels_width_at_200_percent_with_longer_words_and_long_names() {
    STRETCH.with(|s| s.set(true));
    for scale in [1.0, 2.0] {
        for names in [Names::Long, Names::Chinese] {
            let port = Arc::new(ScriptedSettings::with_names(names));
            port.set(|s| {
                s.lab.carried_over = Some(CarriedOver {
                    airplay_ms: Some(500),
                    homepod_ms: Some(150),
                    homepod_buffered: true,
                    speakers: vec![crate::settings::scripted::CHINESE_NAME.into()],
                });
                s.probe = Probe::Found {
                    address: "192.168.20.40".into(),
                    ways: vec![
                        Found { name: crate::settings::scripted::CHINESE_NAME.into(), kind: SpeakerKind::Sonos, delay_ms: 200, way: Way::Upnp },
                        Found { name: "Garage".into(), kind: SpeakerKind::AirPlay, delay_ms: 2000, way: Way::AirPlay },
                    ],
                };
                s.general.what_plays = Some(WhatPlays::OnlySentApps);
                s.privacy.only_playing = false;
                if let Some(a) = &mut s.privacy.api {
                    a.on = true;
                }
                if let Some(k) = &mut s.general.shortcuts {
                    k.status = [crate::platform::KeyStatus::Taken, crate::platform::KeyStatus::Rejected, crate::platform::KeyStatus::Registered];
                }
                s.ma.paused = true;
            });
            for (what, view) in every_view(port.clone()) {
                // The add-address search starts again when the view opens:
                // its result is set again after.
                port.set(|s| {
                    if s.probe == Probe::Idle {
                        s.probe = Probe::Nothing { address: "192.168.1.20".into() };
                    }
                });
                let h = H::sized(port.clone(), view, snap("kitchen-playing"), crate::surface::panel::WIDTH, scale);
                assert_clean(&h.texts, crate::surface::panel::WIDTH, &format!("{what} {names:?} at {scale}"));
            }
        }
    }
    STRETCH.with(|s| s.set(false));
}

#[test]
fn tab_goes_through_a_speakers_view_in_reading_order() {
    let mut h = speaker("kitchen");
    let mut last = f32::MIN;
    let mut seen = 0;
    for _ in 0..40 {
        h.run(vec![Event::Key { key: egui::Key::Tab, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
        let Some(id) = h.ctx.memory(|m| m.focused()) else { continue };
        let Some(r) = h.ctx.read_response(id) else { continue };
        if r.rect.top() + 1.0 < last {
            break;
        }
        assert!(r.rect.top() + 1.0 >= last, "focus went up: {:?} after {last}", r.rect);
        last = r.rect.top();
        seen += 1;
    }
    assert!(seen >= 15, "Tab reached {seen} controls");
}

/// Every view in each of its states, for the test that every sentence of
/// the table's Settings block is said: what they drew.
pub(in crate::settings) fn say_everything() -> Vec<String> {
    let lang = crate::model::text::Lang::English;
    recording_keeps_shift_for_the_large_step();
    reset_and_remove_only_act_after_confirmation();
    a_busy_chime_asks_in_the_page_before_playing();
    the_discovery_check_explains_why_search_is_off();
    cant_hear_says_which_device_privacy_turned_away_and_allows_nothing();
    cant_hear_says_why_the_sound_stayed_on_this_pc();
    tone_name_and_speaker_commands();
    the_page_uses_the_panel_delay_commands_and_the_selected_ways_limits();
    // Draw the exact-delay row with a saved change so the reset link is covered too.
    let mut editor = speaker_in("kitchen", "kitchen-least-delay", |_| ());
    editor.click("Type an exact delay…");
    let mut drawn: Vec<String> = editor.texts.iter().map(|t| t.text.clone()).collect();
    // Every view in the states the example and these variants give.
    let variants: Vec<fn(&mut State)> = vec![
        |_| (),
        |s| {
            s.privacy.only_playing = false;
            s.general.what_plays = Some(WhatPlays::OnlySentApps);
            s.general.start_at_sign_in = Some(Err("access denied".into()));
            if let Some(a) = &mut s.privacy.api {
                a.on = true;
            }
            if let Some(k) = &mut s.general.shortcuts {
                k.status = [crate::platform::KeyStatus::Taken, crate::platform::KeyStatus::Rejected, crate::platform::KeyStatus::Registered];
            }
            if let Some(p) = &mut s.privacy.phone {
                p.problem = Some("port in use".into());
            }
            s.ma.paused = true;
            s.ma.failed = Some("port 8928 is in use".into());
            s.ma.found_by = None;
            s.ma.code = None;
            s.ma.paired_with = vec!["Music Assistant (Home)".into()];
            s.ma.played = Some(Some(Date { day: 12, month: 10 }));
            s.about.updates.state = UpdateState::Failed { why: "no network".into() };
            s.lab.carried_over = Some(CarriedOver { airplay_ms: Some(500), homepod_ms: Some(150), homepod_buffered: true, speakers: vec![] });
            s.playback.volume = Some(Err(Volume::Zero));
            s.playback.taking = Some(Err(Taking::NotAsking { two_ways: true }));
            s.speakers.networks.as_mut().unwrap().looking = Looking::AdapterOff { adapter: "Wi-Fi".into() };
            s.discovery.lines = vec![
                FindLine::Profile { network: "HomeNet".into(), private: false },
                FindLine::AirPlaySearchFailed,
                FindLine::AdapterOff { adapter: "Wi-Fi".into() },
                FindLine::Failed { adapter: "Wi-Fi".into(), why: "no answer".into() },
            ];
            let mut tv = s.speakers.hidden[0].clone();
            tv.id = "guest".into();
            tv.cant_play = Some(CantPlay::AirPlayOff);
            s.speakers.speakers.push(tv);
        },
        |s| {
            s.speakers.networks.as_mut().unwrap().looking = Looking::Nowhere;
            s.ma.offer = None;
            s.about.updates.state = UpdateState::NeverChecked;
            s.playback.taking = Some(Err(Taking::NotAsking { two_ways: false }));
            s.playback.turned_away = Some(TurnedAway { device: Some("Kitchen Sub".into()), address: "192.168.1.51".into() });
            s.playback.playing = Some(Ok(Some("Chrome".into())));
            s.playback.windows = Some(Err(WindowsSends::AppElsewhere { app: "Chrome".into(), output: "Speakers".into() }));
            s.general.shortcuts.as_mut().unwrap().on = true;
            for p in &mut s.pages {
                p.row.presence = Presence::Connecting;
            }
        },
        |s| {
            s.playback.windows = Some(Ok(WindowsSends::AppElsewhere { app: "Chrome".into(), output: "Speakers".into() }));
            s.speakers.networks.as_mut().unwrap().pinned = Some("192.168.1.23".into());
            s.playback.playing = Some(Err(None));
            s.playback.untested = true;
            s.playback.apple_tv = true;
            s.playback.on_short = true;
            s.ma.offer = Some(false);
            s.about.updates.state = UpdateState::Checking;
            for p in &mut s.pages {
                p.row.presence = Presence::Ready;
            }
        },
        |s| {
            s.playback.speaker = None;
            s.playback.windows = Some(Err(WindowsSends::InputLost));
            s.playback.connected = Some(Err(()));
            s.about.updates.state = UpdateState::Downloading;
            s.ma.found_by = Some(("Music Assistant (Home)".into(), None));
        },
        |s| {
            s.playback.taking = Some(Ok(Taking::CantTell));
            s.about.updates.state = UpdateState::ReadyToInstall { version: "0.2.1".into(), speaker: Some("Kitchen".into()) };
            s.ma.code = None;
            s.ma.paired_with = vec!["x".into()];
            s.ma.played = None;
        },
        |s| {
            s.playback.volume = Some(Err(Volume::Muted));
            s.about.updates.state = UpdateState::ReadyToInstall { version: "0.2.1".into(), speaker: None };
            s.ma.code = None;
            s.ma.paired_with = vec!["x".into()];
            s.ma.played = Some(None);
        },
        |s| {
            s.about.updates.state = UpdateState::NotSigned;
            s.general.shortcuts.as_mut().unwrap().on = false;
            s.ma.found_by = None;
            s.ma.code = None;
            s.ma.paired_with = vec!["x".into()];
        },
        |s| s.about.updates.state = UpdateState::DevBuild,
        |s| {
            s.about.updates.state = UpdateState::UpToDate { at: Seen::On(Date { day: 3, month: 1 }) };
            s.playback.windows = Some(Err(WindowsSends::InputLost));
            s.ma.found_by = None;
            s.ma.paired_with.clear();
            s.lab.carried_over = Some(CarriedOver { airplay_ms: Some(500), homepod_ms: None, homepod_buffered: false, speakers: vec!["Office".into()] });
        },
        |s| {
            s.about.updates.state = UpdateState::Available { version: "0.2.1".into(), at: None };
            s.about.updates.installs_here = true;
            s.playback.connected = Some(Err(()));
            s.ma.found_by = Some(("Music Assistant (Home)".into(), Some("192.168.1.10".into())));
            s.ma.code = Some(("482917".into(), Some(171)));
            s.ma.paired_with.clear();
        },
        |s| {
            s.playback.taking = Some(Ok(Taking::CantTell));
            s.playback.on_short = true;
            s.ma.code = None;
            s.ma.paired_with = vec!["x".into()];
            s.ma.played = Some(Some(Date { day: 12, month: 10 }));
        },
        |s| {
            s.playback.untested = true;
            s.playback.apple_tv = true;
        },
    ];
    for v in variants {
        let port = scripted(v);
        for (_, view) in every_view(port.clone()) {
            let h = H::new(port.clone(), view, snap("kitchen-playing"));
            drawn.extend(h.texts.iter().map(|t| t.text.clone()));
        }
        // Speakers in the states their views have.
        for id in ["kitchen", "office", "garage", "desk", "bedroom", "den"] {
            let h = H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), id)), snap("kitchen-playing"));
            drawn.extend(h.texts.iter().map(|t| t.text.clone()));
        }
        let h = H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), "den")), snap("pc-den-needs-code"));
        drawn.extend(h.texts.iter().map(|t| t.text.clone()));
    }
    // Speaker missing? with each result of an address.
    for probe in [
        Probe::Checking { address: "x".into() },
        Probe::Nothing { address: "x".into() },
        Probe::Invalid("That speaker is already in the list.".into()),
        Probe::Found { address: "x".into(), ways: vec![Found { name: "G".into(), kind: SpeakerKind::AirPlay, delay_ms: 2000, way: Way::AirPlay }] },
        Probe::Found {
            address: "x".into(),
            ways: vec![
                Found { name: "G".into(), kind: SpeakerKind::Sonos, delay_ms: 200, way: Way::Upnp },
                Found { name: "G".into(), kind: SpeakerKind::AirPlay, delay_ms: 2000, way: Way::AirPlay },
            ],
        },
    ] {
        let port = scripted(|_| ());
        let mut h = H::new(port.clone(), Box::new(MissingView::new(port.clone()).with_address("x")), snap("kitchen-playing"));
        h.click("Add");
        port.set(|s| s.probe = probe.clone());
        h.settle();
        drawn.extend(h.texts.iter().map(|t| t.text.clone()));
    }
    // A speaker that left the list, one hidden from it, one that wants its
    // password; the shortcut being recorded; Lab with its counters closed.
    let port = scripted(|_| ());
    let mut look = |h: &H| drawn.extend(h.texts.iter().map(|t| t.text.clone()));
    look(&H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), "gone")), snap("kitchen-playing")));
    let hidden = port.state().speakers.hidden[0].id.clone();
    look(&H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), &hidden)), snap("kitchen-playing")));
    let engine = ScriptedEngine::new("pc-office-needs-password");
    look(&H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), "office")), engine.snapshot()));
    look(&H::new(port.clone(), Box::new(super::LabView::new(port.clone())), snap("kitchen-playing")));
    // Can't hear it? while the app connects to the speaker.
    let connecting = scripted(|s| {
        s.playback.connected = Some(Err(()));
        s.playback.taking = None;
        s.playback.volume = None;
    });
    look(&H::new(connecting.clone(), Box::new(CantHearView::new(connecting)), snap("kitchen-connecting")));
    // Can't hear it? while Music Assistant hasn't started this PC's sound.
    let waiting = scripted(|s| {
        s.playback.taking = Some(Err(Taking::NotAsking { two_ways: false }));
        s.playback.volume = None;
    });
    look(&H::new(waiting.clone(), Box::new(CantHearView::new(waiting)), snap("music-assistant-playing")));
    // Can't hear it? for a speaker paused with its own buttons.
    let paused = scripted(|s| {
        s.playback.taking = Some(Err(Taking::Paused));
        s.playback.volume = None;
    });
    look(&H::new(paused.clone(), Box::new(CantHearView::new(paused)), snap("kitchen-playing")));
    // Can't hear it? while Music Assistant's server isn't connected, and
    // while the app can't read this PC's sound.
    let away = scripted(|s| {
        s.playback.connected = Some(Err(()));
        s.playback.server_away = true;
        s.playback.taking = None;
        s.playback.volume = None;
    });
    look(&H::new(away.clone(), Box::new(CantHearView::new(away)), snap("music-assistant-not-connected")));
    let lost = scripted(|s| s.playback.windows = Some(Err(WindowsSends::InputLost)));
    look(&H::new(lost.clone(), Box::new(CantHearView::new(lost)), snap("kitchen-playing")));
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port.clone())), snap("kitchen-playing"));
    h.click("Change…");
    look(&h);
    for _ in 0..2 {
        h.run(vec![Event::Key { key: egui::Key::Minus, physical_key: None, pressed: true,
            repeat: false, modifiers: egui::Modifiers::CTRL }]);
        h.run(vec![Event::Key { key: egui::Key::Minus, physical_key: None, pressed: false,
            repeat: false, modifiers: egui::Modifiers::CTRL }]);
    }
    look(&h);
    port.set(|s| s.rejected_shortcut = Some(vec!["Ctrl".into(), "F9".into()]));
    h.run(vec![Event::Key { key: egui::Key::F9, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::CTRL }]);
    assert!(h.has("Windows didn’t accept Ctrl+F9. Press other keys for Add 25 ms… (Esc stops)"));
    look(&h);
    for m in 1..=12 {
        let _ = crate::settings::words::date(lang, Date { day: 1, month: m });
    }
    let _ = crate::settings::words::seen(lang, Seen::Yesterday(crate::model::text::LocalTime::new(22, 10)));
    drawn
}

fn welcome_page(snap: Arc<Snapshot>) -> H {
    H::new(scripted(|_| ()), Box::new(FirstRunPage::new(snap.scene.situation.first_run.clone().unwrap())), snap)
}

#[test]
fn first_run_shows_the_output_note_and_its_action_above_the_speakers() {
    for (sample, title, command) in [
        ("pc-welcome-repair", "The app’s sound output is missing", Command::RepairOutput),
        ("pc-welcome-output-turned-off", "Stream To Speaker is turned off in Windows", Command::TurnOnOutput),
    ] {
        let mut page = welcome_page(snap(sample));
        let heading = page.texts.iter().find(|text| text.text == title).unwrap();
        let theme = crate::ui::theme::Theme::of(&page.ctx);
        assert_eq!(heading.galley.job.sections[0].format.font_id, theme.strong(crate::surface::grid::BODY));
        assert!(page.has("Repair first"));
        assert!(page.find("Repair").unwrap().bottom() <= page.find("Kitchen").unwrap().top());
        page.click("Repair");
        assert_eq!(page.asks(), [Asked::Command(command)]);
    }
}

#[test]
fn first_run_explains_routing_for_chosen_apps() {
    for (sample, chimed, title) in [
        ("pc-welcome-pick", true, "Where should the apps you choose play?"),
        ("pc-welcome-chime", false, "Play the apps you choose on Kitchen?"),
        ("pc-welcome-chime", true, "Did you hear the chime on Kitchen?"),
        ("kitchen-welcome-done", true, "Kitchen is ready for the apps you choose"),
    ] {
        let mut scene = snap(sample).scene.clone();
        scene.situation.only_some_apps = true;
        if !chimed {
            if let Some(crate::model::situation::FirstRunStep::Two { level, .. }) = &mut scene.situation.first_run {
                *level = None;
            }
        }
        let mut page = welcome_page(Arc::new(crate::model::view::view(scene, None, false, 1)));
        assert!(page.has(title), "{title}");
        assert!(!page.shows("this PC’s sound"));
        if sample == "kitchen-welcome-done" {
            assert!(page.has("In Windows, set the output of each app you choose to the one with “Stream To Speaker” in its name."));
            page.click("Choose apps…");
            assert_eq!(page.asks(), [Asked::Open(Open::VolumeMixer)]);
        } else if sample == "pc-welcome-chime" {
            assert!(page.has(if chimed {
                "If you did, the apps you send to Stream To Speaker will play on Kitchen."
            } else {
                "Kitchen can’t play a test chime first. Switch to it, then play something in an app you send to Stream To Speaker."
            }));
        }
    }
}

pub(crate) fn draw_welcome_words() {
    first_run_explains_routing_for_chosen_apps();
    welcome_without_chime();
    for sample in ["pc-welcome-pick", "pc-welcome-busy", "pc-welcome-looking", "pc-welcome-none-found", "pc-welcome-chime", "kitchen-welcome-done"] {
        welcome_page(snap(sample));
    }
    let snap = snap("pc-welcome-chime");
    let port = scripted(|_| ());
    let view = CantHearView::for_speaker(port.clone(), Some(sid("kitchen")));
    H::new(port, Box::new(view), snap);
}

#[test]
fn welcome_pages_and_their_controls() {
    use crate::model::command::FirstRunAction as A;
    let command = |a| Asked::Command(Command::FirstRunAction(a));
    let page = |name| welcome_page(snap(name));
    let ordered = |h: &H, words: &[&str]| {
        let mut texts = h.texts.iter();
        for word in words {
            assert!(texts.any(|t| t.text == *word), "missing or out of order: {word} in {:?}", h.texts.iter().map(|t| &t.text).collect::<Vec<_>>());
        }
        assert_clean(&h.texts, h.width, words[0]);
    };
    let engine = ScriptedEngine::new("pc-welcome-pick");
    let mut scene = engine.snapshot().scene.clone();
    for speaker in &mut scene.speakers { speaker.used_here = false; }
    engine.publish(scene);
    assert!(engine.snapshot().panel.list.rows.is_empty());
    let mut h = welcome_page(engine.snapshot());
    ordered(&h, &["Where should this PC’s sound play?", "Click a speaker to hear a short chime on it.", "Bedroom", "Kitchen", "Office", "Looking for more speakers…", "Speaker missing?", "Settings"]);
    assert!(!h.view.has_back() && !h.has("This PC") && !h.has("Can’t hear it?"));
    assert_eq!(h.find("Kitchen").unwrap().left(), 46.0);
    h.click("Kitchen");
    assert_eq!(h.asks(), [command(A::Chime(sid("kitchen")))]);
    h.click_at(egui::pos2(358.0, h.find("Kitchen").unwrap().center().y));
    assert_eq!(h.asks(), [command(A::Chime(sid("kitchen")))], "the gear slot belongs to the row");
    h.click("Settings");
    assert_eq!(h.asks(), [Asked::Open(Open::Settings)]);
    assert!(page("pc-welcome-looking").has("Looking for speakers…"));
    let mut h = page("pc-welcome-none-found");
    assert!(!h.has("Click a speaker to hear a short chime on it.") && !h.has("Click a speaker to play this PC’s sound on it."));
    ordered(&h, &["No speakers found on the network.", "Why?", "Speaker missing?", "Settings"]);
    h.click("Why?");
    assert_eq!(h.asks(), [Asked::Open(Open::SpeakerMissing)]);
    let mut h = page("pc-welcome-busy");
    ordered(&h, &["Kitchen", "Kitchen is playing something else. A test chime stops it, and it may not start again by itself.", "Play a test chime", "Cancel", "Living Room + Dining Room"]);
    for (label, yes) in [("Cancel", false), ("Play a test chime", true)] {
        h.click(label);
        assert_eq!(h.asks(), [Asked::Command(Command::Confirm { yes })]);
    }

    let mut h = page("pc-welcome-chime");
    ordered(&h, &["Did you hear the chime on Kitchen?", "If you did, this PC’s sound will play on Kitchen.", "I heard it", "I didn’t hear it", "Volume on Kitchen", "25", "Play it again", "Use Kitchen without checking", "Choose another speaker"]);
    assert!(h.view.has_back() && !h.has("Bedroom"));
    h.click_at(egui::pos2(170.0, h.find("25").unwrap().center().y));
    let levels = h.asks();
    assert!(matches!(levels.last(), Some(Asked::Command(Command::FirstRunAction(A::ChimeVolume(v)))) if *v > 25));
    for (a, b) in [("I heard it", "I didn’t hear it"), ("Play it again", "Use Kitchen without checking")] {
        assert_eq!(h.find(a).unwrap().center().y, h.find(b).unwrap().center().y);
    }
    for (word, act) in [("Choose another speaker", A::ChooseAnother), ("I heard it", A::Heard), ("Play it again", A::ChimeAgain), ("Use Kitchen without checking", A::UseWithoutChecking)] {
        h.click(word);
        assert_eq!(h.asks(), [command(act)]);
    }
    h.click("I didn’t hear it");
    let dest = Open::CantHear(Some(sid("kitchen")));
    assert_eq!(h.asks(), [Asked::Open(dest.clone())]);
    let views = SettingsViews::new(h.port.clone());
    let help = H::new(h.port.clone(), views.view(&dest, &h.snap).unwrap(), h.snap.clone());
    assert!(help.has("This PC sent the chime to Kitchen."));
    let mut h = page("kitchen-welcome-done");
    ordered(&h, &["Kitchen now plays this PC’s sound", "It stays on Kitchen until you switch it.", "The app’s icon is by the clock, or under the ^ arrow there. A click on it opens the same controls as this window.", "Start when I sign in to Windows", "Done"]);
    assert!(!h.view.has_back());
    h.flip("Start when I sign in to Windows");
    assert_eq!(h.asks(), [command(A::StartAtSignIn(false))]);
    h.click("Done");
    assert_eq!(h.asks(), [command(A::Done)]);
}

fn welcome_without_chime() -> H {
    let mut scene = snap("pc-welcome-chime").scene.clone();
    if let Some(crate::model::situation::FirstRunStep::Two { level, .. }) = &mut scene.situation.first_run {
        *level = None;
    }
    welcome_page(Arc::new(crate::model::view::view(scene, None, false, 1)))
}

#[test]
fn a_speaker_without_a_chime_offers_the_switch_without_a_volume() {
    let mut page = speaker_in("office", "pc-welcome-pick", |_| ());
    let mut scene = page.snap.scene.clone();
    scene.situation.speakers.iter_mut().find(|s| s.id == sid("office")).unwrap().can_chime = false;
    page.snap = Arc::new(crate::model::view::view(scene, None, false, 2));
    page.settle();
    assert!(!page.has("Play a test chime"));
    assert!(page.has("Switch to Office"));
    let mut h = welcome_without_chime();
    for text in ["Play this PC’s sound on Kitchen?", "Kitchen can’t play a test chime first. Switch to it, then play something on this PC to check.",
        "Switch to Kitchen", "Choose another speaker"] {
        assert!(h.has(text), "{text}");
    }
    for text in ["I heard it", "I didn’t hear it", "Play it again", "Use Kitchen without checking", "Volume on Kitchen"] { assert!(!h.has(text), "{text}"); }
    h.click("Switch to Kitchen");
    assert_eq!(h.asks(), [Asked::Command(Command::FirstRunAction(crate::model::command::FirstRunAction::UseWithoutChecking))]);
}

#[test]
fn a_first_run_failure_outlives_its_row_and_shows_once_above_the_list() {
    use crate::model::situation::{PickFailed, PickWhy, SwitchBy, SwitchFrom};
    let mut scene = snap("pc-welcome-pick").scene.clone();
    let speaker = sid("kitchen");
    scene.situation.pick_failed = Some(PickFailed { speaker: speaker.clone(), why: PickWhy::NotResponding, by: SwitchBy::FirstRun, from: SwitchFrom::Window });
    scene.speakers.retain(|s| s.id != speaker);
    let (title, sentence) = crate::model::notes::didnt_get_ready(&scene, &speaker, PickWhy::NotResponding);
    let mut h = welcome_page(Arc::new(crate::model::view::view(scene.clone(), None, false, 1)));
    assert_eq!(h.texts.iter().filter(|t| t.text == title).count(), 1);
    assert!(h.find(&title).unwrap().bottom() <= h.find("Bedroom").unwrap().top());
    assert!(!h.has(&sentence));
    scene.situation.pick_failed = None;
    h.snap = Arc::new(crate::model::view::view(scene, None, false, 2));
    h.settle();
    assert!(!h.has(&title));
}

#[test]
fn first_run_shows_only_the_failure_title_under_its_speaker() {
    use crate::model::situation::{PickFailed, PickWhy, SwitchBy, SwitchFrom};
    let mut scene = snap("pc-welcome-pick").scene.clone();
    for why in [PickWhy::Refused, PickWhy::CouldntStart, PickWhy::NoNetwork, PickWhy::NotResponding,
        PickWhy::NotFound, PickWhy::WrongSecret(crate::model::state::Secret::Code), PickWhy::NeedsSecret(crate::model::state::Secret::Code),
        PickWhy::WrongSecret(crate::model::state::Secret::Password), PickWhy::NeedsSecret(crate::model::state::Secret::Password)] {
        scene.situation.pick_failed = Some(PickFailed { speaker: sid("kitchen"), why, by: SwitchBy::FirstRun, from: SwitchFrom::Window });
        let (title, sentence) = crate::model::notes::didnt_get_ready(&scene, &sid("kitchen"), why);
        let h = welcome_page(Arc::new(crate::model::view::view(scene.clone(), None, false, 1)));
        let line = h.find(&title).unwrap();
        assert!(h.find("Kitchen").unwrap().bottom() <= line.top() && line.bottom() <= h.find("Office").unwrap().top());
        assert!(!h.has(&sentence));
    }
}

#[test]
fn welcome_done_follows_the_place_and_its_note() {
    use crate::model::situation::{FirstRunStep, Standing};
    for sample in ["pc-plain", "pc-kitchen-stopped-responding", "pc-headphones-connected"] {
        let mut scene = snap(sample).scene.clone();
        scene.situation.first_run = Some(FirstRunStep::Three { speaker: sid("kitchen") });
        if sample == "pc-plain" { scene.situation.standing.push(Standing::WindowsDidntSwitch { speaker: sid("kitchen") }); }
        let snap = Arc::new(crate::model::view::view(scene, None, false, 1));
        let crate::model::panel::About::Note(note) = &snap.panel.about else { panic!("{sample}") };
        let mut h = welcome_page(snap.clone());
        assert!(h.has(&snap.panel.place.name) && h.has(&note.title) && h.has(&note.sentence));
        assert!(!h.shows("now plays") && !h.shows("until you switch it.") && !h.has("Switch to this PC"));
        if let Some(button) = &note.button {
            h.click(&button.label);
            let expected = match &button.command { Command::Open(o) => Asked::Open(o.clone()), c => Asked::Command(c.clone()) };
            assert_eq!(h.asks(), [expected]);
        }
    }
}

#[test]
fn welcome_done_without_tray_omits_the_icon_sentence() {
    let snap = snap("kitchen-welcome-done");
    let mut page = FirstRunPage::new(snap.scene.situation.first_run.clone().unwrap());
    page.tray = false;
    let h = H::new(scripted(|_| ()), Box::new(page), snap);
    assert!(h.has("Kitchen now plays this PC’s sound") && h.has("Switch to this PC"));
    assert!(!h.shows("by the clock"));
}

#[test]
fn welcome_with_search_off_never_shows_a_search_in_progress() {
    use crate::model::facts::SearchPhase;
    for empty in [false, true] {
        let mut scene = snap("pc-welcome-pick").scene.clone();
        if empty { scene.speakers.clear(); }
        scene.search.phase = SearchPhase::Off;
        let search = crate::model::panel::first_run_search(&scene);
        assert_eq!(search.as_ref().map(|s| s.spinner), empty.then_some(false));
        let h = welcome_page(Arc::new(crate::model::view::view(scene, None, false, 1)));
        assert_eq!(h.has("No speakers found on the network."), empty);
        assert!(!h.shows("Looking for") && !h.shows("Searching"));
    }
}

#[test]
fn step_two_shows_only_its_speakers_failure_title() {
    use crate::model::situation::{FirstRunStep, PickFailed, PickWhy, SwitchBy, SwitchFrom};
    let mut scene = snap("pc-welcome-chime").scene.clone();
    for chimed in [false, true] {
        if let Some(FirstRunStep::Two { level, .. }) = &mut scene.situation.first_run { *level = chimed.then_some(25); }
        for speaker in [sid("kitchen"), sid("office")] {
            let why = PickWhy::Refused;
            scene.situation.pick_failed = Some(PickFailed { speaker: speaker.clone(), why, by: SwitchBy::Switch, from: SwitchFrom::Tray });
            let (title, sentence) = crate::model::notes::didnt_get_ready(&scene, &speaker, why);
            let h = welcome_page(Arc::new(crate::model::view::view(scene.clone(), None, false, 1)));
            assert_eq!(h.has(&title), speaker == sid("kitchen"));
            assert!(!h.has(&sentence));
        }
    }
}

#[test]
fn step_one_offers_a_chime_only_if_a_listed_speaker_can_play_one() {
    let mut scene = snap("pc-welcome-pick").scene.clone();
    scene.speakers.retain(|s| s.id == sid("kitchen"));
    for available in [false, true] {
        scene.situation.speakers.iter_mut().find(|s| s.id == sid("kitchen")).unwrap().can_chime = available;
        let h = welcome_page(Arc::new(crate::model::view::view(scene.clone(), None, false, 1)));
        assert_eq!(h.has("Click a speaker to hear a short chime on it."), available);
        assert_eq!(h.has("Click a speaker to play this PC’s sound on it."), !available);
    }
}

#[test]
fn step_three_shows_how_to_start_the_music_assistant_input() {
    use crate::model::situation::{FirstRunStep, Standing};
    let mut scene = snap("music-assistant-playing").scene.clone();
    scene.situation.first_run = Some(FirstRunStep::Three { speaker: SpeakerId::music_assistant() });
    scene.situation.standing.push(Standing::MusicAssistantNotStarted);
    let mut h = welcome_page(Arc::new(crate::model::view::view(scene, None, false, 1)));
    let stays = h.find("It stays on Music Assistant until you switch it.").unwrap();
    assert!(h.has("Music Assistant") && !h.shows("now plays"));
    let title = h.find("Music Assistant hasn’t started playing").unwrap();
    assert!(stays.bottom() <= title.top());
    assert!(h.has("Start this PC’s sound in Music Assistant, or set Music Assistant to start it by itself."));
    h.click("How to start it");
    assert_eq!(h.asks(), [Asked::Open(Open::MusicAssistantHowTo)]);
}

#[test]
fn the_speaker_page_links_to_delay_shortcuts_in_settings() {
    let label = "Keyboard shortcuts for the delay…";
    for (scenario, visible) in [
        ("office-playing", true),
        ("kitchen-least-delay", true),
        ("kitchen-playing", true),
        ("kitchen-fixed-way", false),
        ("kitchen-changing-way", false),
    ] {
        for on in [false, true] {
            let mut h = speaker_in("kitchen", scenario, |s| s.general.shortcuts.as_mut().unwrap().on = on);
            assert_eq!(h.has(label), visible, "{scenario}, shortcuts on = {on}");
            assert!(!h.shows("From the keyboard"), "{scenario}");
            for key in ["Ctrl+Win+Comma", "Ctrl+Win+Period"] {
                assert!(!h.shows(key), "{scenario} shows {key}");
            }
            if visible {
                h.click(label);
                assert_eq!(h.asks(), [Asked::Open(Open::Settings)]);
            }
        }
    }
}

#[test]
fn recording_keeps_shift_for_the_large_step() {
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
    h.click("Change…");
    h.acts();
    let press = |key, modifiers| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
    for (what, key) in [("Remove", egui::Key::ArrowDown), ("Add", egui::Key::ArrowUp)] {
        let modifiers = egui::Modifiers::CTRL | egui::Modifiers::SHIFT;
        h.run(vec![press(egui::Key::F9, modifiers)]);
        h.run(vec![Event::Key { key: egui::Key::F9, physical_key: None, pressed: false, repeat: false, modifiers }]);
        assert!(h.has(&format!("Shift is kept for the 100 ms step. Press other keys for {what} 25 ms… (Esc stops)")), "{what}");
        assert!(h.acts().is_empty());
        h.run(vec![press(key, egui::Modifiers::CTRL)]);
    }
    assert_eq!(h.acts(), [
        Act::ShortcutKeys(crate::platform::DelayKeys(["Down", "Up"].map(|key| vec!["Ctrl".into(), key.into()]))),
        Act::RecordShortcuts(false),
    ]);
}

#[test]
fn shortcut_lines_distinguish_windows_errors() {
    use crate::platform::KeyStatus::*;
    let port = scripted(|s| {
        let keys = s.general.shortcuts.as_mut().unwrap();
        keys.status = [Rejected, Taken, Rejected];
        keys.remove = vec!["Ctrl".into(), "Win".into(), ",".into()];
        keys.add = vec!["Ctrl".into(), "Win".into(), ".".into()];
    });
    let h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
    assert!(h.shows("Ctrl+Win+Comma. Windows didn’t accept this shortcut."));
    assert!(h.shows("Ctrl+Win+Period. This shortcut is already in use. Choose another."));
}

#[test]
fn shortcut_names_spell_out_punctuation() {
    let punctuation = [",", ".", ";", "/", "\\", "-", "=", "'", "`", "[", "]", "+"].map(String::from);
    assert_eq!(crate::settings::words::shortcut_name(&punctuation),
        "Comma+Period+Semicolon+Slash+Backslash+Minus+Equals+Quote+Backtick+Left Bracket+Right Bracket+Plus");
}

#[test]
fn the_routing_question_can_be_answered_under_the_setting() {
    for yes in [false, true] {
        let port = scripted(|s| s.general.what_plays = Some(WhatPlays::OnlySentApps));
        let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap("kitchen-playing"));
        let apps = "Only send some apps to the speaker";
        h.flip(apps);
        let mut scene = h.snap.scene.clone();
        let question = crate::model::situation::Confirm::ScreenReader {
            speaker: sid("kitchen"), way: crate::model::situation::WayChoice::Native,
        };
        let words = crate::model::notes::question(&scene, &question);
        scene.situation.confirm_in_panel = Some(question);
        scene.situation.confirm_under_routing = Some(crate::model::situation::RoutingQuestion::TakePlayback);
        h.snap = Arc::new(crate::model::view::view(scene, None, false, 2));
        h.settle();
        let title = h.texts.iter().find(|t| t.text == words.title).unwrap();
        assert!(title.rect.top() >= h.find(apps).unwrap().bottom());
        let format = &title.galley.job.sections[0].format;
        let theme = crate::ui::theme::Theme::of(&h.ctx);
        assert_eq!(format.font_id, theme.strong(crate::ui::theme::size::BODY));
        assert_eq!(format.color, theme.c.text);
        assert!(h.has(&words.sentence));
        assert!(h.has("Play this PC’s sound on Kitchen"));
        h.click(if yes { "Play this PC’s sound on Kitchen" } else { "Cancel" });
        assert_eq!(h.asks(), [Asked::Command(Command::Confirm { yes })]);
    }
}
