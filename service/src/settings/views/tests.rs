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
use crate::model::scripted::{Scenario, ScriptedEngine};
use crate::model::snapshot::Snapshot;
use crate::model::state::{NowState, Place, Secret, SpeakerId};

use crate::panel::view::{show_view, ViewState};
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
        Self::sized(port, view, snap, crate::panel::WIDTH, 1.0)
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
                        crate::shell::Ask::Back | crate::shell::Ask::Window(_) => Asked::Back,
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

    /// Click the switch at the right end of the line that says `label`.
    fn flip(&mut self, label: &str) {
        let r = self.find(label).unwrap_or_else(|| panic!("no {label:?}"));
        self.click_at(egui::pos2(self.width - 16.0 - 20.0, r.top() + 9.0));
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

fn snap(s: Scenario) -> Arc<Snapshot> {
    ScriptedEngine::new(s).snapshot()
}

fn speaker(id: &str) -> H {
    speaker_in(id, Scenario::Playing, |_| ())
}

fn speaker_in(id: &str, scenario: Scenario, f: impl FnOnce(&mut State)) -> H {
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

#[test]
fn the_bar_tests_the_speaker_that_plays_and_plays_another_here() {
    let mut h = speaker("kitchen");
    h.click("Test sound");
    assert_eq!(h.asks(), vec![Asked::Command(Command::PlayTestSound(sid("kitchen")))]);
    // Another speaker: what a click on its row does; a test never moves
    // the sound.
    let mut h = speaker("office");
    assert!(!h.has("Test sound"));
    h.click("Play here");
    let row = h.snap.list.rows().find(|r| r.place == Place::Speaker(sid("office"))).and_then(|r| r.command.clone()).unwrap();
    assert_eq!(h.asks(), vec![Asked::Command(row)]);
    // One that isn't found: search again.
    let mut h = speaker("garage");
    h.click("Search again");
    assert_eq!(h.acts(), vec![Act::SearchAgain]);
    assert!(h.shows("not found right now"));
}

#[test]
fn the_delay_block_and_the_choice_send_the_engines_commands() {
    let mut h = speaker("kitchen");
    h.click("Remove 25 ms");
    assert_eq!(h.asks(), vec![Asked::Command(Command::ChangeDelay { speaker: sid("kitchen"), step: DelayStep::Remove(25) })]);
    assert!(h.has("Kitchen’s own delay is about 0.2 s."));
    assert!(h.shows("Sound behind the picture?"));
    // Another amount, typed, then Enter.
    h.click("Other amount…");
    h.type_text("40");
    h.key(egui::Key::Enter);
    assert_eq!(h.asks(), vec![Asked::Command(Command::ChangeDelay { speaker: sid("kitchen"), step: DelayStep::Remove(40) })]);
    let mut h = speaker("office");
    h.click("Short delay");
    assert_eq!(h.asks(), vec![Asked::Command(Command::UseDelayChoice { speaker: sid("office"), choice: crate::model::facts::DelayChoice::Short })]);
    assert!(h.has("About 0.3 s. Can break up on weak Wi-Fi."));
}

#[test]
fn tone_limit_titles_coming_back_and_the_name_change_at_once() {
    let mut h = speaker("kitchen");
    h.click("Reset to flat");
    assert_eq!(h.acts(), vec![Act::Tone("kitchen".into(), ToneAct::Flat)]);
    assert!(!h.has("Reset to flat"), "flat: nothing to reset");
    h.click("All bands…");
    assert!(h.has("31 Hz") && h.has("16k Hz") && h.has("Preamp"), "the bands open in place");
    h.click("Fewer bands");
    assert!(!h.has("31 Hz"));
    h.flip("Show song titles on Kitchen");
    assert_eq!(h.acts(), vec![Act::SongTitles("kitchen".into(), false)]);
    h.click("Never");
    assert_eq!(h.acts(), vec![Act::ComesBack("kitchen".into(), ComesBack::Never)]);
    assert!(h.has("Move sound to Kitchen automatically"));
    let field = h.find("Kitchen").unwrap();
    let _ = field;
    // The name: typed, then Enter.
    let name_label = h.find("Name in this app").unwrap();
    h.click_at(egui::pos2(name_label.left() + 40.0, name_label.bottom() + 16.0));
    h.run(vec![Event::Key { key: egui::Key::A, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND }]);
    h.type_text("Kitchen shelf");
    h.key(egui::Key::Enter);
    assert_eq!(h.acts(), vec![Act::ShowAs("kitchen".into(), "Kitchen shelf".into())]);
    // The limit: a click on its slider.
    let mut h = speaker("kitchen");
    let label = h.find("Never louder than").unwrap();
    h.click_at(egui::pos2(label.right() + 60.0, label.center().y));
    assert!(matches!(h.acts().as_slice(), [Act::VolumeLimit(id, _)] if id == "kitchen"));
}

#[test]
fn details_hiding_and_forgetting_are_at_the_bottom_and_forget_asks_first() {
    let mut h = speaker("office");
    h.click("Details");
    assert!(h.has("Model: AirPort Express\nAddress: 192.168.1.44\nConnection: AirPlay (RAOP)\nDelay: AirPlay buffer 1700 ms (Steady 2000 ms, 300 ms removed)\nLast problem: none\nId: airplay:00005E00539A"));
    h.click("Hide from the list");
    assert_eq!(h.acts(), vec![Act::HideSpeaker("office".into())]);
    h.click("Show in the list");
    assert_eq!(h.acts(), vec![Act::ShowSpeaker("office".into())]);
    h.click("Forget");
    assert!(h.acts().is_empty(), "it asks first");
    assert!(h.shows("Forget Office?"));
    h.click("Cancel");
    assert!(!h.shows("Forget Office?"));
    h.click("Forget");
    h.click("Forget Office");
    assert_eq!(h.acts(), vec![Act::Forget("office".into())]);
    // Added by address: Remove, which leaves the list, and the view goes
    // back.
    let mut h = speaker("garage");
    h.click("Remove");
    h.click("Remove Garage");
    assert_eq!(h.acts(), vec![Act::Remove("garage".into())]);
    assert!(h.asks().contains(&Asked::Back));
}

#[test]
fn a_code_or_a_password_is_asked_for_where_the_speaker_needs_one() {
    let mut h = speaker_in("den", Scenario::NeedsPairing, |_| ());
    h.click("Enter code…");
    assert_eq!(h.asks(), vec![Asked::Open(Open::Pair(sid("den")))]);
    let engine = ScriptedEngine::new(Scenario::Playing);
    let mut f = engine.facts();
    f.state = NowState::NeedsSecret { speaker: sid("office"), secret: Secret::Password };
    engine.set_facts(f);
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(SpeakerOptions::new(port, "office")), engine.snapshot());
    h.click("Enter password…");
    assert_eq!(h.asks(), vec![Asked::Open(Open::Pair(sid("office")))]);
    h.click_link("Change");
    assert_eq!(h.asks(), vec![Asked::Dialog]);
    h.click_link("Remove");
    assert_eq!(h.acts(), vec![Act::RemovePassword("office".into())]);
    let mut h = speaker("bedroom");
    h.click_link("Pair again");
    h.click_link("Forget pairing");
    assert_eq!(h.acts(), vec![Act::PairAgain("bedroom".into()), Act::ForgetPairing("bedroom".into())]);
    let mut h = speaker("bedroom");
    h.click_link("Report a problem");
    assert_eq!(h.asks(), vec![Asked::Dialog]);
}

// -------------------------------------------------------------------------
// Settings, Speaker missing?, Can't hear it?, Music Assistant, About, Lab
// -------------------------------------------------------------------------

#[test]
fn each_switch_of_settings_changes_its_setting() {
    let port = scripted(|s| {
        if let Some(k) = &mut s.general.shortcuts {
            k.on = false;
        }
    });
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap(Scenario::Playing));
    for (label, act) in [
        ("Remove or add delay from the keyboard, in any app", Act::Shortcuts(true)),
        ("Control Stream To Speaker from your phone", Act::Phone(false)),
        ("Play calls on the speaker too", Act::PlayCalls(true)),
        ("Only send some apps to the speaker", Act::WhatPlays(WhatPlays::OnlySentApps)),
        ("Start Stream To Speaker when I sign in", Act::StartAtSignIn(false)),
        ("…and go back to the speaker I was using", Act::GoBack(false)),
        ("Bring the sound back to this PC when a speaker stops responding", Act::WhenStops(WhenStops::StaySilent)),
        ("Tell me when sound leaves the place I chose", Act::Notifications(false)),
        ("Let scripts and home automation control this app", Act::Api(true)),
        ("Let any device on this network listen", Act::OnlyPlaying(false)),
    ] {
        h.flip(label);
        assert_eq!(h.acts(), vec![act], "{label}");
    }
    let s = h.port.state();
    assert_eq!(s.general.when_stops, Some(WhenStops::StaySilent));
    assert!(h.shows("Off: it keeps trying every 30 seconds"));
    assert!(h.shows("Anyone on this network who knows the address"));
    // Turning Music Assistant on opens its set-up.
    let port = scripted(|s| s.ma.offer = Some(false));
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap(Scenario::Playing));
    h.flip("Offer this PC to Music Assistant");
    assert_eq!(h.acts(), vec![Act::OfferToMa(true)]);
    assert_eq!(h.asks(), vec![Asked::Open(Open::MusicAssistantSettings)]);
}

#[test]
fn settings_lists_refused_devices_an_update_and_its_links() {
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port)), snap(Scenario::Playing));
    assert!(h.has("Version 0.2.1 is ready to install."), "an update is the first line");
    h.click("Update now");
    assert_eq!(h.acts(), vec![Act::UpdateNow]);
    let allow = h.texts.iter().filter(|t| t.text == "Allow").map(|t| t.rect).next().unwrap();
    h.click_at(allow.center());
    assert_eq!(h.acts(), vec![Act::Allow { address: "192.168.1.51".into(), speaker: "kitchen".into() }]);
    for (link, title) in [("Guides", "Guides"), ("Lab", "Lab"), ("About", "About")] {
        h.click(link);
        assert_eq!(h.asks(), vec![Asked::View(title.into())], "{link}");
    }
    h.click("Report a problem");
    assert_eq!(h.asks(), vec![Asked::Dialog]);
}

#[test]
fn speaker_missing_adds_by_address_in_place() {
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(MissingView::new(port.clone())), snap(Scenario::Playing));
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
    let mut h = H::new(port.clone(), Box::new(MissingView::new(port.clone()).with_address("192.168.20.40")), snap(Scenario::Playing));
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
    let mut h = H::new(port.clone(), Box::new(MissingView::new(port.clone()).with_address("192.168.1.20")), snap(Scenario::Playing));
    h.click("Add");
    port.set(|s| s.probe = Probe::Nothing { address: "192.168.1.20".into() });
    h.settle();
    assert!(h.shows("Nothing at 192.168.1.20 answered as a speaker."));
    assert!(h.view.holds_unused_text(), "an address typed and not added keeps the view");
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
    let mut h = H::new(port.clone(), Box::new(MissingView::new(port)), snap(Scenario::Playing));
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
fn cant_hear_offers_the_one_fix_of_the_first_broken_step() {
    let port = scripted(|s| s.playback.volume = Some(Err(Volume::Muted)));
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap(Scenario::Playing));
    h.click("Unmute");
    assert_eq!(h.asks(), vec![Asked::Command(Command::SetMuted { owner: VolumeOwner::Speaker(sid("kitchen")), muted: false })]);
    let port = scripted(|s| {
        s.playback.taking = Some(Err(Taking::NotAsking { two_ways: true }));
        s.playback.volume = None;
    });
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap(Scenario::Playing));
    h.click("Reconnect");
    assert_eq!(h.asks(), vec![Asked::Command(Command::Reconnect(sid("kitchen")))]);
    h.click("Try the AirPlay connection");
    assert_eq!(h.acts(), vec![Act::TryAirPlay]);
    assert!(!h.shows("volume"), "the check stops at the first broken step");
    let port = scripted(|_| ());
    let mut h = H::new(port.clone(), Box::new(CantHearView::new(port)), snap(Scenario::Playing));
    assert!(h.shows("Everything this PC can check looks right."));
    h.click("Test sound");
    assert_eq!(h.asks(), vec![Asked::Command(Command::PlayTestSound(sid("kitchen")))]);
}

#[test]
fn music_assistant_about_and_lab_act_at_once() {
    let port = scripted(|s| s.ma.paused = true);
    let mut h = H::new(port.clone(), Box::new(MusicAssistantView::new(port)), snap(Scenario::Playing));
    h.click_link("Allow pairing again");
    h.flip("Offer this PC to Music Assistant");
    assert_eq!(h.acts(), vec![Act::AllowPairingAgain, Act::OfferToMa(false)]);
    let port = scripted(|s| s.ma.paired_with = vec!["Music Assistant (Home)".into()]);
    let mut h = H::new(port.clone(), Box::new(MusicAssistantView::new(port)), snap(Scenario::Playing));
    h.click_link("Unpair");
    assert_eq!(h.acts(), vec![Act::Unpair("Music Assistant (Home)".into())]);

    let port = scripted(|s| s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(crate::model::text::LocalTime::new(9, 12)) });
    let mut h = H::new(port.clone(), Box::new(super::AboutView::new(port)), snap(Scenario::Playing));
    h.click("Check now");
    h.click_link("Donate");
    h.click("Licences and source");
    h.click("Show the welcome again");
    assert_eq!(h.acts(), vec![Act::CheckNow, Act::Donate, Act::LicenceAndSource, Act::ShowWelcomeAgain]);
    assert_eq!(h.asks(), vec![Asked::OpenPanel]);

    let port = scripted(|s| s.lab.mfi = Some(true));
    let mut h = H::new(port.clone(), Box::new(super::LabView::new(port)), snap(Scenario::Playing));
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
        ("check".into(), Box::new(CheckView::new(p.clone()))),
        ("cant hear".into(), Box::new(CantHearView::new(p.clone()))),
        ("music assistant".into(), Box::new(MusicAssistantView::new(p.clone()))),
        ("about".into(), Box::new(super::AboutView::new(p.clone()))),
        ("lab".into(), Box::new(super::LabView::new(p.clone()).with_counters())),
        ("guides".into(), Box::new(GuidesView::new(p.clone()))),
    ];
    for id in ["kitchen", "living", "bedroom", "office", "garage", "den", "desk"] {
        v.push((id.into(), Box::new(SpeakerOptions::new(p.clone(), id).with_open("bands").with_open("details").asking_to_forget())));
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
                s.general.when_stops = Some(WhenStops::StaySilent);
                s.privacy.only_playing = false;
                if let Some(a) = &mut s.privacy.api {
                    a.on = true;
                }
                if let Some(k) = &mut s.general.shortcuts {
                    k.taken = [true, true, false];
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
                let h = H::sized(port.clone(), view, snap(Scenario::Playing), crate::panel::WIDTH, scale);
                assert_clean(&h.texts, crate::panel::WIDTH, &format!("{what} {names:?} at {scale}"));
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
    // Every view in the states the example and these variants give.
    let variants: Vec<fn(&mut State)> = vec![
        |_| (),
        |s| {
            s.privacy.only_playing = false;
            s.general.what_plays = Some(WhatPlays::OnlySentApps);
            s.general.when_stops = Some(WhenStops::StaySilent);
            s.general.start_at_sign_in = Some(Err("access denied".into()));
            if let Some(a) = &mut s.privacy.api {
                a.on = true;
            }
            if let Some(k) = &mut s.general.shortcuts {
                k.taken = [true, false, false];
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
            s.about.driver = Some(Driver::Running("1.1.2.215".into()));
            s.lab.carried_over = Some(CarriedOver { airplay_ms: Some(500), homepod_ms: Some(150), homepod_buffered: true, speakers: vec![] });
            s.playback.volume = Some(Err(Volume::Zero));
            s.playback.taking = Some(Err(Taking::NotAsking { two_ways: true }));
            s.speakers.networks.as_mut().unwrap().looking = Looking::AdapterOff { adapter: "Wi-Fi".into() };
            s.discovery.lines = vec![
                FindLine::Profile { network: "HomeNet".into(), private: false },
                FindLine::Firewall { open: false },
                FindLine::AirPlaySearchFailed,
                FindLine::AdapterOff { adapter: "Wi-Fi".into() },
                FindLine::Failed { adapter: "Wi-Fi".into(), why: "no answer".into() },
            ];
            let mut tv = s.speakers.hidden[0].clone();
            tv.id = "guest".into();
            tv.cant_play = Some(CantPlay::AirPlayOff);
            s.speakers.speakers.push(tv);
            for p in &mut s.pages {
                p.comes_back = Some(ComesBack::WhenItAppears);
                p.pairing = Some(Pairing::Paired { on: Some(Date { day: 3, month: 10 }) });
                if let Some(d) = &mut p.delay {
                    d.at_once = true;
                }
            }
        },
        |s| {
            s.speakers.networks.as_mut().unwrap().looking = Looking::Nowhere;
            s.ma.offer = None;
            s.about.updates.state = UpdateState::NeverChecked;
            s.about.updates.installs_here = false;
            s.playback.taking = Some(Err(Taking::Refused { device: "Kitchen Sub".into(), address: "192.168.1.51".into() }));
            s.playback.windows = Some(Err(WindowsSends::AppElsewhere { app: "Chrome".into(), output: "Speakers".into() }));
            s.general.shortcuts.as_mut().unwrap().on = true;
            for p in &mut s.pages {
                p.comes_back = Some(ComesBack::Never);
                p.row.presence = Presence::Connecting;
                p.group = Some(Group::AppleTv { members: vec!["A".into(), "B".into()] });
            }
        },
        |s| {
            s.speakers.networks.as_mut().unwrap().pinned = Some("192.168.1.23".into());
            s.playback.playing = Some(Err(None));
            s.playback.untested = true;
            s.playback.apple_tv = true;
            s.playback.on_short = true;
            s.ma.offer = Some(false);
            s.about.updates.state = UpdateState::Checking;
            for p in &mut s.pages {
                p.row.presence = Presence::Ready;
                if let Some(d) = &mut p.delay {
                    d.status.choice = Some(DelayChoice::Short);
                }
            }
        },
        |s| {
            s.playback.speaker = None;
            s.playback.windows = Some(Err(WindowsSends::Elsewhere { output: "Headphones".into() }));
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
        |s| s.about.updates.state = UpdateState::NotSigned,
        |s| s.about.updates.state = UpdateState::DevBuild,
    ];
    let mut drawn: Vec<String> = Vec::new();
    for v in variants {
        let port = scripted(v);
        for (_, view) in every_view(port.clone()) {
            let h = H::new(port.clone(), view, snap(Scenario::Playing));
            drawn.extend(h.texts.iter().map(|t| t.text.clone()));
        }
        // Speakers in the states their views have.
        for id in ["kitchen", "office", "garage", "desk", "bedroom", "den"] {
            let h = H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), id)), snap(Scenario::Playing));
            drawn.extend(h.texts.iter().map(|t| t.text.clone()));
        }
        let h = H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), "den")), snap(Scenario::NeedsPairing));
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
        let mut h = H::new(port.clone(), Box::new(MissingView::new(port.clone()).with_address("x")), snap(Scenario::Playing));
        h.click("Add");
        port.set(|s| s.probe = probe.clone());
        h.settle();
        drawn.extend(h.texts.iter().map(|t| t.text.clone()));
    }
    // A speaker that left the list, one hidden from it, one that wants its
    // password; the shortcut being recorded; Lab with its counters closed.
    let port = scripted(|_| ());
    let mut look = |h: &H| drawn.extend(h.texts.iter().map(|t| t.text.clone()));
    look(&H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), "gone")), snap(Scenario::Playing)));
    let hidden = port.state().speakers.hidden[0].id.clone();
    look(&H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), &hidden)), snap(Scenario::Playing)));
    let engine = ScriptedEngine::new(Scenario::Playing);
    let mut f = engine.facts();
    f.state = NowState::NeedsSecret { speaker: sid("office"), secret: Secret::Password };
    engine.set_facts(f);
    look(&H::new(port.clone(), Box::new(SpeakerOptions::new(port.clone(), "office")), engine.snapshot()));
    look(&H::new(port.clone(), Box::new(super::LabView::new(port.clone())), snap(Scenario::Playing)));
    let mut h = H::new(port.clone(), Box::new(SettingsView::new(port.clone())), snap(Scenario::Playing));
    h.click("Change…");
    look(&h);
    for m in 1..=12 {
        let _ = crate::settings::words::date(lang, Date { day: 1, month: m });
    }
    let _ = crate::settings::words::seen(lang, Seen::Yesterday(crate::model::text::LocalTime::new(22, 10)));
    drawn
}
