//! Tests of the Settings window: on the scripted port, each control makes
//! exactly the change it names, at once; every page and dialog lays out at
//! the window's smallest size, at 200 %, with words a third longer and with
//! a long name in Chinese; every sentence of the window's block of the
//! table is used. On the real app, each act changes the setting it names
//! and the views read it back.

use std::sync::{Arc, Mutex};

use egui::{Color32, Event, Pos2, Rect, Shape, Vec2};

use super::dialogs::{AddDialog, CodeDialog, Dialog, PasswordDialog, ReportDialog, SendspinDialog, ToneDialog};
use super::port::*;
use super::real::{AppSettings, Desktop};
use super::scripted::{Names, ScriptedSettings, State, CHINESE_NAME};
use super::words::{is_settings_key, STRETCH};
use super::{Page, Request, Window};
use crate::model::text::{Key, LocalTime, USED};
use crate::ui::preview;
use crate::ui::theme::Mode;

fn fonts() -> egui::FontDefinitions {
    crate::fonts::definitions(&[], &mut |_| None).0
}

/// A piece of text drawn, where.
#[derive(Clone, Debug)]
struct Text {
    text: String,
    rect: Rect,
    words_per_row: Vec<usize>,
    galley: Arc<egui::Galley>,
    /// Where the galley starts (its top left, before clipping).
    pos: Pos2,
}

/// The text shapes in `shape`, each with the part of it that shows (a
/// field's text scrolls inside the field's clip).
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

/// What is drawn: the window or a dialog, on the scripted port.
enum Surface {
    Window(Window),
    Dialog(Option<Dialog>),
}

struct Harness {
    ctx: egui::Context,
    port: Arc<ScriptedSettings>,
    surface: Surface,
    size: Vec2,
    scale: f32,
    pass: usize,
    requests: Vec<Request>,
    texts: Vec<Text>,
}

impl Harness {
    fn window(page: Page, port: ScriptedSettings) -> Self {
        let mut w = Window::new();
        w.open(page);
        Self::with(Surface::Window(w), port, Vec2::new(880.0, 2400.0), 1.0)
    }

    /// The window on `page` with the folds `open` open.
    fn window_open(page: Page, port: ScriptedSettings, open: &[&str]) -> Self {
        let mut w = Window::new();
        w.open(page);
        for f in open {
            w.st.open.insert(f.to_string());
        }
        Self::with(Surface::Window(w), port, Vec2::new(880.0, 2400.0), 1.0)
    }

    fn dialog(d: Dialog, port: ScriptedSettings) -> Self {
        let size = d.size();
        Self::with(Surface::Dialog(Some(d)), port, Vec2::new(size[0], size[1] + 200.0), 1.0)
    }

    fn with(surface: Surface, port: ScriptedSettings, size: Vec2, scale: f32) -> Self {
        let mut h = Harness {
            ctx: preview::context(fonts(), Mode::Light),
            port: Arc::new(port),
            surface,
            size,
            scale,
            pass: 0,
            requests: Vec::new(),
            texts: Vec::new(),
        };
        h.settle();
        h
    }

    /// One frame with `events`.
    fn run(&mut self, events: Vec<Event>) {
        let mut input = preview::input(self.size, self.scale, self.pass as f64 * 0.05);
        input.events = events;
        self.pass += 1;
        let port = self.port.clone();
        let surface = &mut self.surface;
        let mut requests = Vec::new();
        let out = self.ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| match surface {
                Surface::Window(w) => requests = w.show_in(ui, &*port),
                Surface::Dialog(d) => {
                    if let Some(dialog) = d {
                        if !dialog.show_in(ui, &*port) {
                            *d = None;
                        }
                    }
                }
            });
        });
        self.requests.extend(requests);
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

    fn click_at(&mut self, at: Pos2) {
        let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        self.run(vec![Event::PointerMoved(at)]);
        self.run(vec![button(true)]);
        self.run(vec![button(false)]);
        self.settle();
    }

    /// Click the text `text` (a button's words, a row's title).
    fn click(&mut self, text: &str) {
        let r = self.find(text).unwrap_or_else(|| panic!("no {text:?} drawn; drawn: {:?}", self.texts.iter().map(|t| &t.text).collect::<Vec<_>>()));
        self.click_at(r.center());
    }

    /// Whether some text drawn contains `part`.
    fn shows(&self, part: &str) -> bool {
        self.texts.iter().any(|t| t.text.contains(part))
    }

    /// Click the link `link` at the end of a line of words (its spaces
    /// are kept together, so they are no-break spaces there).
    fn click_link(&mut self, link: &str) {
        let glued = link.replace(' ', "\u{a0}");
        let (t, at) =
            self.texts.iter().find_map(|t| t.text.find(&glued).map(|i| (t.clone(), t.text[..i].chars().count()))).unwrap_or_else(|| panic!("no link {link:?}"));
        let middle = at + link.chars().count() / 2;
        let r = t.galley.pos_from_cursor(&t.galley.from_ccursor(egui::text::CCursor::new(middle)));
        self.click_at(t.pos + r.center().to_vec2());
    }

    /// Type `text` into the field that has the focus.
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

    fn page(&self) -> Page {
        match &self.surface {
            Surface::Window(w) => w.page(),
            Surface::Dialog(_) => panic!("a dialog"),
        }
    }

    fn open(&self) -> bool {
        matches!(&self.surface, Surface::Dialog(Some(_)))
    }

    fn state(&self) -> State {
        self.port.state()
    }
}

fn page_of(id: &str) -> Page {
    Page::Speaker(id.into())
}

fn speaker(s: &State, id: &str) -> SpeakerView {
    s.pages.iter().find(|p| p.row.id == id).unwrap().clone()
}

// -------------------------------------------------------------------------
// Each control makes exactly the change it names, at once
// -------------------------------------------------------------------------

/// Click `text` on `page` of the usual example: exactly `acts` are sent,
/// and `check` holds of the state right after.
fn clicking(page: Page, text: &str, acts: Vec<Act>, check: impl Fn(&State) -> bool) {
    let mut h = Harness::window(page.clone(), ScriptedSettings::new());
    if h.has(text) {
        h.click(text);
    } else {
        h.click_link(text);
    }
    assert_eq!(h.acts(), acts, "{page:?}: {text}");
    assert!(check(&h.state()), "{page:?}: {text}: the change isn't there");
}

#[test]
fn every_control_on_a_speakers_page_makes_its_change() {
    let k = || "kitchen".to_string();
    let table: Vec<(Page, &str, Vec<Act>, Box<dyn Fn(&State) -> bool>)> = vec![
        (
            page_of("kitchen"),
            "Remove 25 ms",
            vec![Act::Delay(k(), DelayAct::Remove(25))],
            Box::new(|s| speaker(s, "kitchen").delay.unwrap().status.change_ms == -75),
        ),
        (
            page_of("kitchen"),
            "Remove 100 ms",
            vec![Act::Delay(k(), DelayAct::Remove(100))],
            Box::new(|s| speaker(s, "kitchen").delay.unwrap().status.change_ms == -150),
        ),
        (page_of("kitchen"), "Add 25 ms", vec![Act::Delay(k(), DelayAct::Add(25))], Box::new(|s| speaker(s, "kitchen").delay.unwrap().status.change_ms == -25)),
        (
            page_of("kitchen"),
            "Add 100 ms",
            vec![Act::Delay(k(), DelayAct::Add(100))],
            Box::new(|s| speaker(s, "kitchen").delay.unwrap().status.change_ms == 50),
        ),
        (
            page_of("kitchen"),
            "AirPlay — about 2 s delay",
            vec![Act::ConnectWith { id: k(), own_way: false }],
            Box::new(|s| !speaker(s, "kitchen").connects_with.unwrap().own_way),
        ),
        (page_of("kitchen"), "Undo", vec![Act::Tone(k(), ToneAct::Undo)], Box::new(|_| true)),
        (page_of("kitchen"), "Reset to flat", vec![Act::Tone(k(), ToneAct::Flat)], Box::new(|s| speaker(s, "kitchen").tone == Some((0.0, 0.0)))),
        (page_of("kitchen"), "Forget Kitchen", vec![Act::Forget(k())], Box::new(|s| !s.pages.iter().any(|p| p.row.id == "kitchen"))),
        (page_of("kitchen"), "Hide from the list", vec![Act::HideSpeaker(k())], Box::new(|s| s.speakers.hidden.iter().any(|r| r.id == "kitchen"))),
        (
            page_of("office"),
            "Short delay — about 0.3 s",
            vec![Act::Delay("office".into(), DelayAct::Choose(DelayChoice::Short))],
            Box::new(|s| speaker(s, "office").delay.unwrap().status.choice == Some(DelayChoice::Short)),
        ),
        (page_of("office"), "Remove", vec![Act::RemovePassword("office".into())], Box::new(|s| speaker(s, "office").pairing.is_none())),
        (page_of("office"), "Play on Office", vec![Act::PlayOn("office".into())], Box::new(|_| true)),
        (page_of("bedroom"), "Pair again", vec![Act::PairAgain("bedroom".into())], Box::new(|_| true)),
        (page_of("bedroom"), "Forget pairing", vec![Act::ForgetPairing("bedroom".into())], Box::new(|s| speaker(s, "bedroom").pairing.is_none())),
        (page_of("den"), "Use Short delay", vec![Act::Delay("den".into(), DelayAct::Choose(DelayChoice::Short))], Box::new(|_| true)),
        (page_of("garage"), "Remove", vec![Act::Remove("garage".into())], Box::new(|s| !s.pages.iter().any(|p| p.row.id == "garage"))),
        (page_of("kitchen"), "Play a test sound", vec![Act::TestSound(k())], Box::new(|_| true)),
    ];
    for (page, text, acts, check) in table {
        clicking(page, text, acts, check);
    }
}

#[test]
fn switches_and_choices_change_their_setting() {
    // The switch's words (*On*/*Off*) are part of it: clicking them flips it.
    let mut h = Harness::window(page_of("kitchen"), ScriptedSettings::new());
    let on = h.texts.iter().filter(|t| t.text == "On").map(|t| t.rect).next().unwrap();
    h.click_at(on.center());
    assert_eq!(h.acts(), vec![Act::SongTitles("kitchen".into(), false)]);
    assert_eq!(speaker(&h.state(), "kitchen").song_titles, Some(false));

    let mut h = Harness::window(Page::General, ScriptedSettings::new());
    h.click("Only apps I send there");
    h.click("Keep trying, and stay silent meanwhile");
    assert_eq!(h.acts(), vec![Act::WhatPlays(WhatPlays::OnlySentApps), Act::WhenStops(WhenStops::StaySilent)]);
    let s = h.state().general;
    assert_eq!((s.what_plays, s.when_stops), (Some(WhatPlays::OnlySentApps), Some(WhenStops::StaySilent)));

    let mut h = Harness::window(Page::Privacy, ScriptedSettings::new());
    h.click("Any device on this network that asks");
    h.click("Make a new link");
    h.click("Allow for Kitchen");
    assert_eq!(h.acts(), vec![Act::OnlyPlaying(false), Act::NewLink, Act::Allow { address: "192.168.1.51".into(), speaker: "kitchen".into() }]);
    assert!(!h.state().privacy.only_playing);

    let mut h = Harness::window(Page::Speakers, ScriptedSettings::new());
    h.click("Show");
    h.click("Search again");
    let off = h.find("Off").unwrap();
    h.click_at(off.center());
    assert_eq!(h.acts(), vec![Act::ShowSpeaker("neighbour".into()), Act::SearchAgain, Act::SendspinSpeakers(true)]);
    assert!(h.state().speakers.hidden.is_empty());

    let mut h = Harness::window(Page::About, ScriptedSettings::new());
    h.click("Update now");
    h.click("Skip this version");
    h.click("Restart now…");
    let off = h.find("Off").unwrap();
    h.click_at(off.center());
    assert_eq!(h.acts(), vec![Act::UpdateNow, Act::SkipVersion, Act::RestartWindows, Act::ShowLab(true)]);
    assert!(h.state().header.lab);
}

#[test]
fn drop_downs_change_their_setting() {
    let mut h = Harness::window(Page::Speakers, ScriptedSettings::new());
    h.click("All networks");
    h.click("Ethernet 2");
    assert_eq!(h.acts(), vec![Act::LookOn(Some("eth2".into()))]);
    assert_eq!(h.state().speakers.networks.unwrap().chosen.as_deref(), Some("eth2"));

    let mut h = Harness::window(page_of("kitchen"), ScriptedSettings::new());
    h.click("When it was the last place you chose");
    h.click("Never");
    assert_eq!(h.acts(), vec![Act::ComesBack("kitchen".into(), ComesBack::Never)]);
    assert_eq!(speaker(&h.state(), "kitchen").comes_back, Some(ComesBack::Never));

    let port = ScriptedSettings::new();
    port.set(|s| s.header.lab = true);
    let mut h = Harness::window(Page::Lab, port);
    h.click("Gradually");
    let at_once = h.texts.iter().filter(|t| t.text == "At once").last().unwrap().rect;
    h.click_at(at_once.center());
    assert_eq!(h.acts(), vec![Act::Lab(LabAct::AtOnce(true))]);
    assert!(h.state().lab.differs(), "at once is a Lab setting that differs");
    h.click("Reset everything in Lab");
    assert_eq!(h.acts(), vec![Act::Lab(LabAct::ResetAll)]);
    assert!(!h.state().lab.differs());
}

#[test]
fn links_and_buttons_open_pages_and_dialogs() {
    let mut h = Harness::window(Page::Speakers, ScriptedSettings::new());
    h.click("Living Room + Dining Room");
    assert_eq!(h.page(), page_of("living"));
    h.click("Speakers");
    assert_eq!(h.page(), Page::Speakers);
    h.click("Can’t find your speaker?");
    assert_eq!(h.page(), Page::CantFind);
    h.click("Add by address…");
    assert!(matches!(h.requests.pop(), Some(Request::Dialog(Dialog::AddAddress(_)))));
    // Alt+← goes back.
    h.run(vec![Event::Key { key: egui::Key::ArrowLeft, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::ALT }]);
    h.settle();
    assert_eq!(h.page(), Page::Speakers);

    let mut h = Harness::window(Page::Help, ScriptedSettings::new());
    h.click("Write a report");
    assert!(matches!(h.requests.pop(), Some(Request::Dialog(Dialog::Report(_)))));
    h.click("Open log folder");
    h.click("Show the welcome again");
    assert_eq!(h.acts(), vec![Act::OpenLogFolder, Act::ShowWelcomeAgain]);
    assert!(matches!(h.requests.pop(), Some(Request::OpenPanel)));
    h.click("How it works");
    assert_eq!(h.page(), Page::Guide(1));
    h.click("Next");
    assert_eq!(h.page(), Page::Guide(2));

    let mut h = Harness::window(page_of("kitchen"), ScriptedSettings::new());
    h.click("All bands…");
    assert!(matches!(h.requests.pop(), Some(Request::Dialog(Dialog::Tone(_)))));
    let mut h = Harness::window(page_of("office"), ScriptedSettings::new());
    h.click("Change");
    assert!(matches!(h.requests.pop(), Some(Request::Dialog(Dialog::Password(_)))));
}

#[test]
fn the_navigation_moves_between_pages_and_shows_lab_only_when_on() {
    let mut h = Harness::window(Page::Speakers, ScriptedSettings::new());
    for (name, page) in [
        ("General", Page::General),
        ("Music Assistant", Page::MusicAssistant),
        ("Privacy & remote", Page::Privacy),
        ("Help", Page::Help),
        ("About", Page::About),
    ] {
        let r = h.texts.iter().filter(|t| t.text == name).map(|t| t.rect).min_by(|a, b| a.left().total_cmp(&b.left())).unwrap();
        h.click_at(r.center());
        assert_eq!(h.page(), page);
    }
    assert!(!h.texts.iter().any(|t| t.text == "Lab"));
    h.port.set(|s| s.header.lab = true);
    h.settle();
    assert!(h.has("Lab"));
    // Lab turned off while it shows: About instead.
    let mut w = Harness::window(Page::Lab, ScriptedSettings::new());
    w.settle();
    assert_eq!(w.page(), Page::About);
}

#[test]
fn the_delay_control_sends_typed_amounts_and_back_to_own() {
    let mut h = Harness::window(page_of("kitchen"), ScriptedSettings::new());
    h.click("Other amount…");
    h.type_text("150");
    h.key(egui::Key::Enter);
    // 150 more than the 50 removed: the limit, 200 removed, stops it.
    assert_eq!(h.acts(), vec![Act::Delay("kitchen".into(), DelayAct::Remove(150))]);
    assert_eq!(speaker(&h.state(), "kitchen").delay.unwrap().status.change_ms, -200);
    assert!(h.has("Kitchen allows at most 200 ms removed."), "the limit is said under the buttons");
}

// -------------------------------------------------------------------------
// Dialogs
// -------------------------------------------------------------------------

#[test]
fn the_code_goes_in_by_itself_after_the_fourth_digit() {
    let port = ScriptedSettings::new();
    port.set(|s| s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage: CodeStage::Showing, seconds_left: Some(102) })));
    let mut h = Harness::dialog(Dialog::Code(CodeDialog::new("den")), port);
    assert!(h.shows("1:42 left · it goes in by itself after the fourth digit"));
    assert!(!h.texts.iter().any(|t| t.text.chars().all(|c| c.is_ascii_digit()) && t.text.len() == 4), "no example code is drawn");
    h.type_text("12");
    assert_eq!(h.acts(), vec![]);
    h.type_text("34");
    assert_eq!(h.acts(), vec![Act::SubmitCode { id: "den".into(), code: "1234".into() }]);
    // The scripted app paired: the dialog closes and the connection goes on.
    assert!(!h.open());
}

#[test]
fn a_wrong_code_clears_the_boxes_and_says_so() {
    let port = ScriptedSettings::new();
    port.set(|s| s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage: CodeStage::Wrong, seconds_left: None })));
    let mut h = Harness::dialog(Dialog::Code(CodeDialog::new("den")), port);
    assert!(h.has("That code didn’t work. Den TV is showing a new code."));
    h.click_link("Show a new code");
    h.click("Cancel");
    assert_eq!(h.acts(), vec![Act::NewCode("den".into()), Act::CancelPairing("den".into())]);
    assert!(!h.open());
}

#[test]
fn a_password_is_kept_or_used_to_connect() {
    let mut h = Harness::dialog(Dialog::Password(PasswordDialog::change("office", "Office")), ScriptedSettings::new());
    h.type_text("hunter2");
    h.click("Save");
    assert_eq!(h.acts(), vec![Act::SubmitPassword { id: "office".into(), password: "hunter2".into(), connect: false }]);
    assert!(!h.open());
    let mut h = Harness::dialog(Dialog::Password(PasswordDialog::to_connect("office", "Office")), ScriptedSettings::new());
    assert!(!h.texts.iter().any(|t| t.text.contains("hunter")), "the field starts empty");
    h.type_text("pw");
    h.click("Connect");
    assert_eq!(h.acts(), vec![Act::SubmitPassword { id: "office".into(), password: "pw".into(), connect: true }]);
}

#[test]
fn a_sendspin_code_or_its_token() {
    let port = ScriptedSettings::new();
    port.set(|s| {
        s.wanted = Some(Wanted::SendspinCode(SendspinAsk {
            id: "desk".into(),
            name: "Desk Speaker".into(),
            live_code: true,
            seconds_left: Some(160),
            identity_changed: false,
            lost: false,
            wrong: false,
        }))
    });
    let mut h = Harness::dialog(Dialog::Sendspin(SendspinDialog::new("desk")), port);
    h.click("No code? Paste its pairing token (starts with SP:)");
    assert!(h.has("Paste Desk Speaker’s pairing token here. It starts with SP:"));
    h.type_text("SP:0abc");
    h.click("Pair");
    assert_eq!(h.acts(), vec![Act::SubmitSendspin { id: "desk".into(), code: "SP:0abc".into() }]);
}

#[test]
fn adding_by_address_finds_then_adds() {
    let mut h = Harness::dialog(Dialog::AddAddress(AddDialog::default()), ScriptedSettings::new());
    h.type_text("192.168.20.40");
    h.click("Find");
    assert_eq!(h.acts(), vec![Act::Find { address: "192.168.20.40".into(), name: String::new() }]);
    assert!(h.has("Checking 192.168.20.40…"));
    h.port.set(|s| {
        s.probe = Probe::Found {
            address: "192.168.20.40".into(),
            ways: vec![Found { name: "Garage".into(), kind: SpeakerKind::AirPlay, delay_ms: 2000, way: Way::AirPlay }],
        }
    });
    h.settle();
    assert!(h.has("AirPlay speaker at 192.168.20.40 · about 2 s delay"));
    h.click("Add Garage");
    // Added: the dialog closes, ready for the next time it opens.
    assert_eq!(h.acts(), vec![Act::AddFound { way: Way::AirPlay, name: "Garage".into() }, Act::ResetProbe]);
    assert!(!h.open());
}

#[test]
fn tone_bands_presets_undo_and_close() {
    let mut h = Harness::dialog(Dialog::Tone(ToneDialog::new("kitchen")), ScriptedSettings::new());
    h.click("Undo");
    h.click("Reset to flat");
    assert_eq!(h.acts(), vec![Act::Tone("kitchen".into(), ToneAct::Undo), Act::Tone("kitchen".into(), ToneAct::Flat)]);
    h.click("Flat");
    h.click("Bass boost");
    assert_eq!(h.acts(), vec![Act::Tone("kitchen".into(), ToneAct::Preset(1))]);
    h.click("Close");
    assert!(!h.open());
}

#[test]
fn the_report_previews_exactly_what_is_copied() {
    let mut h = Harness::dialog(Dialog::Report(ReportDialog::default()), ScriptedSettings::new());
    let preview = |h: &Harness| h.texts.iter().any(|t| t.text.contains("21:41:10 Delay on Kitchen"));
    assert!(preview(&h));
    h.click("What happened in the last hour");
    assert!(!preview(&h), "unticked, it leaves the preview");
    h.click("Open a GitHub issue");
    assert!(matches!(h.acts().as_slice(), [Act::OpenIssue(t)] if !t.contains("21:41:10")));
}

// -------------------------------------------------------------------------
// Layout
// -------------------------------------------------------------------------

/// The checks every page passes: no text overlaps other text, none runs
/// past the window, none is squeezed into a column of single words.
fn assert_clean(texts: &[Text], right: f32, what: &str) {
    for (i, a) in texts.iter().enumerate() {
        assert!(a.rect.right() <= right + 0.5, "{what}: {:?} runs past the window ({} > {right})", a.text, a.rect.right());
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

fn every_page() -> Vec<Page> {
    let mut pages = vec![
        Page::Speakers,
        Page::General,
        Page::MusicAssistant,
        Page::Privacy,
        Page::Help,
        Page::Guide(1),
        Page::Guide(2),
        Page::CantHear,
        Page::CantFind,
        Page::About,
        Page::Lab,
    ];
    for id in ["kitchen", "living", "bedroom", "office", "garage", "den", "desk"] {
        pages.push(page_of(id));
    }
    pages
}

fn every_dialog() -> Vec<(Dialog, fn(&mut State))> {
    vec![
        (Dialog::Code(CodeDialog::new("den")), |s| {
            s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: CHINESE_NAME.into(), stage: CodeStage::Wrong, seconds_left: Some(102) }))
        }),
        (Dialog::Password(PasswordDialog::to_connect("office", CHINESE_NAME)), |s| s.wanted = None),
        (Dialog::Sendspin(SendspinDialog::new("desk")), |s| {
            s.wanted = Some(Wanted::SendspinCode(SendspinAsk {
                id: "desk".into(),
                name: CHINESE_NAME.into(),
                live_code: true,
                seconds_left: Some(160),
                identity_changed: true,
                lost: false,
                wrong: true,
            }))
        }),
        (Dialog::MaCode(Default::default()), |s| {
            s.wanted = Some(Wanted::MaCode(MaCodeShow { code: "482917".into(), this_pc: CHINESE_NAME.into(), seconds_left: Some(161) }))
        }),
        (Dialog::AddAddress(AddDialog::by_hand_open()), |s| {
            s.probe = Probe::Found {
                address: "192.168.20.40".into(),
                ways: vec![
                    Found { name: CHINESE_NAME.into(), kind: SpeakerKind::Sonos, delay_ms: 200, way: Way::Upnp },
                    Found { name: CHINESE_NAME.into(), kind: SpeakerKind::AirPlay, delay_ms: 2000, way: Way::AirPlay },
                ],
            }
        }),
        (Dialog::Tone(ToneDialog::new("kitchen")), |_| ()),
        (Dialog::Report(ReportDialog::default()), |_| ()),
    ]
}

#[test]
fn every_page_holds_at_the_smallest_size_at_200_percent_with_longer_words_and_chinese_names() {
    STRETCH.with(|s| s.set(true));
    for scale in [1.0, 2.0] {
        for names in [Names::Long, Names::Chinese] {
            for page in every_page() {
                let port = ScriptedSettings::with_names(names);
                port.set(|s| {
                    s.header.lab = true;
                    s.lab.carried_over =
                        Some(CarriedOver { airplay_ms: Some(500), homepod_ms: Some(150), homepod_buffered: true, speakers: vec![CHINESE_NAME.into()] });
                });
                let mut w = Window::new();
                w.open(page.clone());
                let h = Harness::with(Surface::Window(w), port, Vec2::new(super::MIN_SIZE[0], 6000.0), scale);
                assert_eq!(h.page(), page, "the page shows");
                assert_clean(&h.texts, super::MIN_SIZE[0], &format!("{page:?} {names:?} at {scale}"));
            }
        }
        for (d, set) in every_dialog() {
            let port = ScriptedSettings::with_names(Names::Chinese);
            port.set(set);
            let size = d.size();
            let what = format!("{d:?}");
            let h = Harness::with(Surface::Dialog(Some(d)), port, Vec2::new(size[0], size[1] + 400.0), scale);
            assert!(h.open(), "{what}: still open");
            assert_clean(&h.texts, size[0], &what);
        }
    }
    STRETCH.with(|s| s.set(false));
}

#[test]
fn below_800_points_the_navigation_shows_only_its_icons() {
    let h = Harness::with(Surface::Window(Window::new()), ScriptedSettings::new(), Vec2::new(640.0, 480.0), 1.0);
    assert!(!h.has("Music Assistant"), "no page names in the navigation");
    assert!(!h.has("Stream To Speaker"));
    let h = Harness::with(Surface::Window(Window::new()), ScriptedSettings::new(), Vec2::new(880.0, 640.0), 1.0);
    assert!(h.has("Music Assistant") && h.has("Stream To Speaker"));
}

// -------------------------------------------------------------------------
// Words
// -------------------------------------------------------------------------

/// The sentences only the real app says, in situations the scripted one
/// doesn't have (a thread that couldn't start, a pairing that had ended,
/// the report's version line, a speaker's raw delay values).
const REAL_ONLY: &[Key] = &[
    Key::SetProbeNoThread,
    Key::SetPairingEnded,
    Key::SetReportVersionLine,
    Key::SetNothingChanged,
    Key::SetChoiceSteady,
    Key::SetChoiceShort,
    Key::SetDetailStream,
    Key::SetDetailBuffer,
    Key::SetDetailLead,
];

#[test]
fn every_sentence_of_the_window_is_used_and_none_is_left_unfilled() {
    USED.with(|u| u.borrow_mut().clear());
    let mut drawn: Vec<String> = Vec::new();
    let mut look = |h: &Harness| drawn.extend(h.texts.iter().map(|t| t.text.clone()));
    // Every page, in the states the example and these variants give.
    let variants: Vec<fn(&mut State)> = vec![
        |_| (),
        |s| {
            s.header.lab = true;
            s.lab.carried_over = Some(CarriedOver { airplay_ms: Some(500), homepod_ms: Some(150), homepod_buffered: true, speakers: vec![] });
            s.speakers.speakers.clear();
            s.speakers.networks.as_mut().unwrap().looking = Looking::AdapterOff { adapter: "Wi-Fi".into() };
            s.general.start_at_sign_in = Some(Err("access denied".into()));
            s.ma.code = None;
            s.ma.found_by = Some(("Music Assistant (Home)".into(), None));
            s.ma.paired_with = vec!["Music Assistant (Home)".into()];
            s.ma.played = Some(None);
            s.ma.paused = true;
            s.ma.failed = Some("port 8928 is in use".into());
            s.about.updates.state = UpdateState::Failed { why: "no network".into() };
            s.about.driver = Some(Driver::Running("1.1.2.215".into()));
            s.playback.volume = Some(Err(Volume::Zero));
            s.playback.taking = Some(Err(Taking::NotAsking { two_ways: true }));
            s.discovery.lines = vec![
                FindLine::Profile { network: "HomeNet".into(), private: false },
                FindLine::Firewall { open: false },
                FindLine::AirPlaySearchFailed,
                FindLine::AdapterOff { adapter: "Wi-Fi".into() },
                FindLine::Failed { adapter: "Wi-Fi".into(), why: "no answer".into() },
            ];
            s.privacy.api.as_mut().unwrap().on = true;
            for p in &mut s.pages {
                p.comes_back = Some(ComesBack::WhenItAppears);
                p.pairing = Some(Pairing::Paired { on: None });
                if let Some(d) = &mut p.delay {
                    d.measured = Some((210, Date { day: 12, month: 10 }));
                    d.at_once = true;
                }
            }
        },
        |s| {
            s.header.lab = true;
            s.speakers.networks.as_mut().unwrap().looking = Looking::Nowhere;
            s.speakers.networks.as_mut().unwrap().pinned = Some("192.168.1.23".into());
            s.ma.offer = None;
            s.about.updates.state = UpdateState::NeverChecked;
            s.about.updates.installs_here = false;
            s.playback.taking = Some(Err(Taking::Refused { device: "Kitchen Sub".into(), address: "192.168.1.51".into() }));
            s.playback.windows = Some(Err(WindowsSends::AppElsewhere { app: "Chrome".into(), output: "Speakers (Realtek Audio)".into() }));
            s.general.shortcuts.as_mut().unwrap().taken = [true, false, false];
            for p in &mut s.pages {
                p.comes_back = Some(ComesBack::Never);
                p.row.cant_play = Some(CantPlay::AirPlayOff);
                p.row.presence = Presence::Connecting;
            }
        },
        |s| {
            s.playback.playing = Some(Err(None));
            s.playback.windows = Some(Err(WindowsSends::Elsewhere { output: "Headphones".into() }));
            s.playback.connected = Some(Err(()));
            s.playback.untested = true;
            s.playback.apple_tv = true;
            s.playback.on_short = true;
            s.about.updates.state = UpdateState::Checking;
            s.general.shortcuts.as_mut().unwrap().on = false;
        },
        |s| {
            s.playback.speaker = None;
            s.about.updates.state = UpdateState::Downloading;
        },
        |s| s.about.updates.state = UpdateState::ReadyToInstall { version: "0.2.1".into(), speaker: Some("Kitchen".into()) },
        |s| s.about.updates.state = UpdateState::ReadyToInstall { version: "0.2.1".into(), speaker: None },
        |s| s.about.updates.state = UpdateState::NotSigned,
        |s| s.about.updates.state = UpdateState::DevBuild,
        |s| s.about.updates.state = UpdateState::UpToDate { at: Seen::On(Date { day: 3, month: 1 }) },
        |s| {
            s.speakers.networks.as_mut().unwrap().looking = Looking::Nowhere;
            s.about.updates.installs_here = false;
            s.ma.found_by = None;
            s.ma.paired_with.clear();
            s.playback.taking = Some(Ok(Taking::CantTell));
            s.playback.on_short = true;
            s.lab.carried_over = Some(CarriedOver { airplay_ms: Some(500), homepod_ms: None, homepod_buffered: false, speakers: vec!["Office".into()] });
            s.header.lab = true;
            let presences = [Presence::Ready, Presence::Connecting, Presence::NotFound { last_seen: None }, Presence::Available];
            for (r, p) in s.speakers.speakers.iter_mut().zip(presences.iter().cycle()) {
                r.presence = p.clone();
                r.added_by_address = true;
            }
            s.speakers.speakers[0].cant_play = Some(CantPlay::AirPlayOff);
            for (pg, p) in s.pages.iter_mut().zip(presences.iter().cycle()) {
                pg.row.presence = p.clone();
            }
        },
        |s| {
            s.ma.found_by = None;
            s.ma.paired_with = vec!["Music Assistant (Home)".into()];
            s.ma.played = Some(Some(Date { day: 12, month: 10 }));
            s.playback.volume = Some(Err(Volume::Muted));
        },
        |s| {
            s.ma.code = None;
            s.ma.paired_with = vec!["Music Assistant (Home)".into()];
            s.ma.played = Some(None);
            s.playback.untested = true;
            s.playback.apple_tv = true;
        },
    ];
    for (i, v) in variants.iter().enumerate() {
        for page in every_page() {
            let port = ScriptedSettings::new();
            port.set(v);
            if i == 0 {
                port.set(|s| s.header.lab = true);
            }
            let h = Harness::window(page.clone(), port);
            look(&h);
        }
    }
    // Speakers' delay notes and pages in other states.
    let delay_states: Vec<fn(&mut DelayStatusView)> =
        vec![|d| d.progress = Progress::CatchingUp { ms_to_go: -25 }, |d| d.progress = Progress::WillReconnect, |d| {
            d.notice = Some(DelayNotice::ChoiceSwitched { to: DelayChoice::Short, now_ms: 0, other_ms: -300 })
        }];
    for f in delay_states {
        let port = ScriptedSettings::new();
        port.set(|s| s.pages.iter_mut().filter_map(|p| p.delay.as_mut()).for_each(|d| f(&mut d.status)));
        for id in ["kitchen", "office", "desk"] {
            look(&Harness::window(page_of(id), ScriptedSettings::new()));
            let _ = id;
        }
        look(&Harness::window(page_of("office"), port));
    }
    // The folds open, on every speaker.
    for id in ["kitchen", "living", "bedroom", "office", "garage", "den", "desk"] {
        let folds = [format!("details:{id}"), format!("delay-details:{id}")];
        let refs: Vec<&str> = folds.iter().map(String::as_str).collect();
        for at_once in [false, true] {
            let port = ScriptedSettings::new();
            port.set(|s| s.pages.iter_mut().filter_map(|p| p.delay.as_mut()).for_each(|d| d.at_once = at_once));
            look(&Harness::window_open(page_of(id), port, &refs));
        }
    }
    // Months, days and the rest of the moments.
    for m in 1..=12 {
        let _ = super::words::date(crate::model::text::Lang::English, Date { day: 1, month: m });
    }
    let _ = super::words::seen(crate::model::text::Lang::English, Seen::Today(LocalTime::new(9, 12)));
    let open = |h: &mut Harness, link: &str| {
        if h.has(link) {
            h.click(link);
        }
    };
    let mut h = Harness::window(page_of("kitchen"), ScriptedSettings::new());
    open(&mut h, "Details");
    look(&h);
    let mut h = Harness::window(page_of("office"), ScriptedSettings::new());
    open(&mut h, "Details");
    h.click("Remove 25 ms");
    look(&h);
    let port = ScriptedSettings::new();
    port.set(|s| s.general.shortcuts.as_mut().unwrap().on = false);
    let mut h = Harness::window(page_of("kitchen"), port);
    open(&mut h, "Details");
    look(&h);
    let mut h = Harness::window(page_of("desk"), ScriptedSettings::new());
    open(&mut h, "Details");
    look(&h);
    let mut h = Harness::window(page_of("bedroom"), ScriptedSettings::new());
    open(&mut h, "Details");
    look(&h);
    let port = ScriptedSettings::new();
    port.set(|s| {
        for p in &mut s.pages {
            if let Some(d) = &mut p.delay {
                d.status.choice = Some(DelayChoice::Short);
            }
        }
    });
    let mut h = Harness::window(page_of("office"), port);
    open(&mut h, "Details");
    look(&h);
    let mut h = Harness::window(Page::MusicAssistant, ScriptedSettings::new());
    open(&mut h, "Pairing with another Sendspin server?");
    look(&h);
    let mut h = Harness::window(Page::General, ScriptedSettings::new());
    h.click("Change…");
    look(&h);
    look(&Harness::window(page_of("living"), ScriptedSettings::new()));
    let port = ScriptedSettings::new();
    port.set(|s| {
        for p in &mut s.pages {
            p.group = Some(Group::AppleTv { members: vec!["A".into(), "B".into(), "C".into()] });
            p.show_as = Some("x".into());
            p.row.kind = SpeakerKind::AirPlay;
        }
    });
    look(&Harness::window(page_of("den"), port));
    // Every dialog, in each of its states.
    let mut dialogs = every_dialog();
    dialogs.push((Dialog::Code(CodeDialog::new("den")), |s| {
        s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage: CodeStage::Starting, seconds_left: None }))
    }));
    dialogs.push((Dialog::Code(CodeDialog::new("den")), |s| {
        s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage: CodeStage::Checking, seconds_left: None }))
    }));
    dialogs.push((Dialog::Code(CodeDialog::new("den")), |s| {
        s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage: CodeStage::TimedOut, seconds_left: None }))
    }));
    dialogs.push((Dialog::Code(CodeDialog::new("den")), |s| {
        s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage: CodeStage::NoAnswer, seconds_left: None }))
    }));
    dialogs.push((Dialog::Code(CodeDialog::new("den")), |s| {
        s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage: CodeStage::Showing, seconds_left: None }))
    }));
    dialogs.push((Dialog::Password(PasswordDialog::to_connect("office", "Office")), |s| s.wanted = None));
    dialogs.push((Dialog::Password(PasswordDialog::change("office", "Office")), |s| s.wanted = None));
    dialogs.push((Dialog::Sendspin(SendspinDialog::new("desk")), |s| {
        s.wanted = Some(Wanted::SendspinCode(SendspinAsk {
            id: "desk".into(),
            name: "Desk".into(),
            live_code: false,
            seconds_left: None,
            identity_changed: false,
            lost: true,
            wrong: false,
        }))
    }));
    dialogs.push((Dialog::MaCode(Default::default()), |s| {
        s.wanted = Some(Wanted::MaCode(MaCodeShow { code: "482917".into(), this_pc: "PC".into(), seconds_left: None }))
    }));
    dialogs.push((Dialog::AddAddress(AddDialog::default()), |s| s.probe = Probe::Nothing { address: "192.168.1.20".into() }));
    dialogs.push((Dialog::AddAddress(AddDialog::default()), |s| s.probe = Probe::Invalid("That speaker is already in the list.".into())));
    dialogs.push((Dialog::AddAddress(AddDialog::default()), |s| s.probe = Probe::Checking { address: "x".into() }));
    dialogs.push((Dialog::AddAddress(AddDialog::default()), |s| {
        s.probe = Probe::Found { address: "x".into(), ways: vec![Found { name: "G".into(), kind: SpeakerKind::AirPlay, delay_ms: 2000, way: Way::AirPlay }] }
    }));
    let pw_wrong = |s: &mut State| s.wanted = Some(Wanted::Password(PasswordAsk { id: "office".into(), name: "Office".into(), wrong: true, once: true }));
    let mut pw = PasswordDialog::to_connect("office", "Office");
    pw.id = "office".into();
    dialogs.push((Dialog::Password(pw), pw_wrong));
    for (d, set) in dialogs {
        let port = ScriptedSettings::new();
        port.set(set);
        let mut h = Harness::dialog(d, port);
        look(&h);
        for link in ["Show", "No code? Paste its pairing token (starts with SP:)", "Type a code instead"] {
            if h.has(link) {
                h.click(link);
                look(&h);
            }
        }
        // Details by hand, for a UPnP speaker.
        if h.has("AirPlay") && h.has("Port") {
            let kind = h.texts.iter().filter(|t| t.text == "AirPlay").last().unwrap().rect;
            h.click_at(kind.center());
            h.click("UPnP");
            look(&h);
        }
        // The report's buttons say what happened.
        for b in ["Copy report", "Save as a file", "Open a GitHub issue"] {
            if h.has(b) {
                h.click(b);
                look(&h);
            }
        }
    }
    // The window's title.
    let _ = super::title(crate::model::text::Lang::English);
    drawn.extend(super::views::tests::say_everything());
    for t in &drawn {
        assert!(!t.contains('{') && !t.contains('}'), "an unfilled placeholder: {t:?}");
        assert!(!t.contains('\''), "a straight apostrophe: {t:?}");
    }
    let used = USED.with(|u| u.borrow().clone());
    let unused: Vec<&Key> = Key::ALL.iter().filter(|k| is_settings_key(**k) && !used.contains(k) && !REAL_ONLY.contains(k)).collect();
    assert!(unused.is_empty(), "sentences the window never says: {unused:?}");
}

// -------------------------------------------------------------------------
// The real app
// -------------------------------------------------------------------------

#[derive(Default)]
struct FakeDesktop {
    opened: Mutex<Vec<String>>,
}

impl Desktop for FakeDesktop {
    fn open_url(&self, url: &str) {
        self.opened.lock().unwrap().push(url.into());
    }
    fn open_folder(&self, dir: &std::path::Path) {
        self.opened.lock().unwrap().push(dir.display().to_string());
    }
    fn utc_offset_secs(&self) -> i64 {
        3600
    }
}

fn real() -> (Arc<crate::app::App>, AppSettings, Arc<FakeDesktop>) {
    let app = crate::app::App::for_tests();
    let desk = Arc::new(FakeDesktop::default());
    let port = AppSettings::with_desktop(app.clone(), desk.clone());
    (app, port, desk)
}

/// A speaker the test app lists: added by address.
fn with_speaker(app: &crate::app::App) -> String {
    app.add_manual_speaker(crate::manual_speakers::ManualKind::AirPlay, "Garage", "192.0.2.40", "7000").unwrap()
}

#[test]
fn the_real_app_lists_its_speakers_and_keeps_each_change() {
    let (app, port, _) = real();
    let id = with_speaker(&app);
    let v = port.speakers();
    let row = v.speakers.iter().find(|r| r.id == id).expect("the speaker is listed");
    assert_eq!((row.name.as_str(), row.added_by_address), ("Garage", true));
    assert_eq!(row.kind, SpeakerKind::AirPlay);

    port.act(Act::HideSpeaker(id.clone()));
    assert!(port.speakers().hidden.iter().any(|r| r.id == id));
    assert!(app.speaker_settings(&id).hidden);
    port.act(Act::ShowSpeaker(id.clone()));
    assert!(!app.speaker_settings(&id).hidden);

    port.act(Act::ComesBack(id.clone(), ComesBack::Never));
    assert_eq!(port.speaker(&id).unwrap().comes_back, Some(ComesBack::Never));
    assert_eq!(app.speaker_settings(&id).auto_connect, crate::user_config::AutoConnect::Never);

    port.act(Act::SongTitles(id.clone(), true));
    assert_eq!(port.speaker(&id).unwrap().song_titles, Some(true));

    port.act(Act::Delay(id.clone(), DelayAct::Remove(100)));
    let d = port.speaker(&id).unwrap().delay.unwrap().status;
    assert_eq!(d.change_ms, -100, "the delay engine kept it");
    port.act(Act::Delay(id.clone(), DelayAct::BackToOwn));
    assert_eq!(port.speaker(&id).unwrap().delay.unwrap().status.change_ms, 0);

    port.act(Act::Remove(id.clone()));
    assert!(port.speaker(&id).is_none(), "removed");
}

#[test]
fn the_real_app_keeps_tone_per_speaker_with_undo() {
    let (app, port, _) = real();
    let id = with_speaker(&app);
    assert_eq!(port.speaker(&id).unwrap().tone, Some((0.0, 0.0)), "flat, as the app-wide equalizer");
    port.act(Act::Tone(id.clone(), ToneAct::Bass(-2.0)));
    port.act(Act::Tone(id.clone(), ToneAct::Treble(1.0)));
    assert_eq!(port.speaker(&id).unwrap().tone, Some((-2.0, 1.0)));
    assert_eq!(app.speaker_settings(&id).tone.unwrap().bands_db[0], -2.0);
    assert!(port.tone(&id).unwrap().can_undo);
    port.act(Act::Tone(id.clone(), ToneAct::Undo));
    assert_eq!(port.speaker(&id).unwrap().tone, Some((-2.0, 0.0)));
    port.act(Act::Tone(id.clone(), ToneAct::Flat));
    assert_eq!(port.speaker(&id).unwrap().tone, Some((0.0, 0.0)));
    // The app-wide curve is untouched.
    assert_eq!(app.eq_saved(), crate::equalizer::EqSettings::default());
}

#[test]
fn the_real_app_keeps_general_privacy_about_and_lab() {
    let (app, port, desk) = real();
    port.act(Act::GoBack(false));
    assert_eq!(port.general().go_back, Some(false));
    assert!(!app.is_auto_reconnect_on_launch());
    port.act(Act::OnlyPlaying(true));
    assert!(port.privacy().only_playing && app.is_privacy_mode());
    port.act(Act::ShowLab(true));
    assert!(port.header().lab && port.about().lab);
    port.act(Act::Lab(LabAct::AtOnce(true)));
    port.act(Act::Lab(LabAct::Mfi(true)));
    port.act(Act::Lab(LabAct::Realtime(true)));
    port.act(Act::Lab(LabAct::UncompressedAlac(true)));
    port.act(Act::Lab(LabAct::CatchUpFrames(8)));
    let lab = port.lab();
    assert_eq!((lab.at_once, lab.mfi, lab.realtime, lab.uncompressed_alac, lab.catch_up_frames), (Some(true), Some(true), Some(true), Some(true), Some(8)));
    assert!(lab.differs());
    {
        let uc = app.user_config.lock().unwrap();
        assert!(uc.delay_at_once && uc.airplay_mfi_encryption && uc.prefer_realtime_airplay && uc.airplay_uncompressed_alac);
    }
    port.act(Act::Lab(LabAct::ResetAll));
    assert!(!port.lab().differs());
    port.act(Act::Donate);
    port.act(Act::LicenceAndSource);
    assert_eq!(desk.opened.lock().unwrap().len(), 2);
    assert!(desk.opened.lock().unwrap().iter().all(|u| crate::is_openable_url(u)));
}

#[test]
fn values_from_an_earlier_version_are_carried_over_not_lab_settings() {
    let (app, port, _) = real();
    let id = with_speaker(&app);
    assert!(port.lab().carried_over.is_none());
    app.user_config.lock().unwrap().airplay_latency_ms = 500;
    let lab = port.lab();
    let c = lab.carried_over.clone().expect("shown as carried over");
    assert_eq!(c.airplay_ms, Some(500));
    assert!(c.speakers.contains(&"Garage".to_string()), "the AirPlay speaker whose sound changes");
    assert!(!lab.differs(), "it doesn't turn on the line that Lab settings are on");
    // A speaker with a delay of its own isn't named.
    port.act(Act::Delay(id.clone(), DelayAct::Remove(100)));
    assert!(port.lab().carried_over.unwrap().speakers.is_empty());
    port.act(Act::Lab(LabAct::ResetCarriedOver));
    assert!(port.lab().carried_over.is_none());
    assert_eq!(app.user_config.lock().unwrap().airplay_latency_ms, 2000);
}

#[test]
fn the_real_app_forgets_a_speaker_entirely() {
    let (app, port, _) = real();
    let id = with_speaker(&app);
    app.set_airplay_password(&id, "pw");
    port.act(Act::Delay(id.clone(), DelayAct::Add(100)));
    port.act(Act::Tone(id.clone(), ToneAct::Bass(3.0)));
    assert!(matches!(port.speaker(&id).unwrap().pairing, Some(Pairing::PasswordSaved)));
    port.act(Act::Forget(id.clone()));
    assert!(app.airplay_password(&id).is_none());
    assert!(app.speaker_settings(&id).is_default());
    assert_eq!(port.speaker(&id).unwrap().delay.unwrap().status.change_ms, 0);
}

#[test]
fn a_moment_is_said_as_today_yesterday_or_a_date() {
    use super::real::seen_at;
    // 2026-11-23 12:00 UTC, an hour east of it.
    let now = 1_795_435_200;
    assert_eq!(seen_at(now - 3 * 3600, now, 3600), Seen::Today(LocalTime::new(10, 0)));
    assert_eq!(seen_at(now - 20 * 3600, now, 3600), Seen::Yesterday(LocalTime::new(17, 0)));
    assert_eq!(seen_at(now - 50 * 86_400, now, 3600), Seen::On(Date { day: 4, month: 10 }));
    for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2026, 11, 23), (2024, 12, 31)] {
        let days = super::real::days_from_civil(y, m, d);
        assert_eq!(super::real::civil_from_days(days), (y, m as u8, d as u8));
    }
    assert_eq!(super::real::days_from_civil(2026, 11, 23) * 86_400 + 12 * 3600, now as i64);
}

#[test]
fn the_real_app_keeps_a_limit_a_local_name_and_what_happens_when_a_speaker_stops() {
    let (app, port, _) = real();
    let id = with_speaker(&app);
    assert_eq!(port.speaker(&id).unwrap().volume_limit, Some(100), "no limit");
    port.act(Act::VolumeLimit(id.clone(), 70));
    assert_eq!(port.speaker(&id).unwrap().volume_limit, Some(70));
    assert_eq!(app.volume_limit(&id), Some(70));
    port.act(Act::VolumeLimit(id.clone(), 100));
    assert_eq!(app.volume_limit(&id), None, "100 is no limit");

    port.act(Act::ShowAs(id.clone(), "Garage shelf".into()));
    assert_eq!(app.speaker_settings(&id).show_as.as_deref(), Some("Garage shelf"));
    assert_eq!(port.speaker(&id).unwrap().show_as.as_deref(), Some("Garage shelf"));
    port.act(Act::ShowAs(id.clone(), "  ".into()));
    assert_eq!(app.speaker_settings(&id).show_as, None, "an empty name gives it its own back");

    assert_eq!(port.general().when_stops, Some(WhenStops::BringBack));
    port.act(Act::WhenStops(WhenStops::StaySilent));
    assert!(!app.is_auto_reconnect_on_drop());
    assert_eq!(port.general().when_stops, Some(WhenStops::StaySilent));
}

#[test]
fn the_real_app_lists_refused_devices_and_allows_one() {
    let (app, port, _) = real();
    app.set_privacy_mode(true);
    let peer: std::net::IpAddr = "192.0.2.77".parse().unwrap();
    assert!(app.check_stream_client(peer).is_err());
    let refused = port.privacy().refused.unwrap();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].address, "192.0.2.77");
    assert_eq!(refused[0].name, None, "a device discovery doesn't know has no name");
    port.act(Act::Allow { address: "192.0.2.77".into(), speaker: String::new() });
    assert!(port.privacy().refused.unwrap().is_empty());
    assert!(app.check_stream_client(peer).is_ok(), "allowed for this run");
}
