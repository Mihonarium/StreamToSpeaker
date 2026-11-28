//! Tests of the dialogs on the scripted port: each control makes exactly
//! the change it names; every dialog lays out at its size, at 200 %, with
//! words a third longer and a long name in Chinese. Every sentence of the
//! table's Settings block is said by a view or a dialog. On the real app,
//! each act changes the setting it names and the port reads it back.

use std::sync::{Arc, Mutex};

use egui::{Color32, Event, Pos2, Rect, Shape, Vec2};

use super::dialogs::{CodeDialog, Dialog, PasswordDialog, ReportDialog, SendspinDialog};
use super::port::*;
use super::real::{AppSettings, Desktop};
use super::scripted::{Names, ScriptedSettings, State, CHINESE_NAME};
use super::words::{is_settings_key, STRETCH};
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


/// A dialog on the scripted port; `None` once it closed.
struct Harness {
    ctx: egui::Context,
    port: Arc<ScriptedSettings>,
    dialog: Option<Dialog>,
    size: Vec2,
    scale: f32,
    pass: usize,
    texts: Vec<Text>,
    clipboard: String,
}

impl Harness {
    fn dialog(d: Dialog, port: ScriptedSettings) -> Self {
        let size = d.size();
        Self::with(d, port, Vec2::new(size[0], size[1] + 200.0), 1.0)
    }

    fn with(dialog: Dialog, port: ScriptedSettings, size: Vec2, scale: f32) -> Self {
        let mut h = Harness { ctx: preview::context(fonts(), Mode::Light), port: Arc::new(port), dialog: Some(dialog), size, scale, pass: 0, texts: Vec::new(), clipboard: String::new() };
        h.settle();
        h
    }

    /// One frame with `events`.
    fn run(&mut self, events: Vec<Event>) {
        let mut input = preview::input(self.size, self.scale, self.pass as f64 * 0.05);
        input.events = events;
        self.pass += 1;
        let port = self.port.clone();
        let d = &mut self.dialog;
        let out = self.ctx.run(input, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
                if let Some(dialog) = d {
                    if !dialog.show_in(ui, &*port) {
                        *d = None;
                    }
                }
            });
        });
        self.texts.clear();
        if !out.platform_output.copied_text.is_empty() {
            self.clipboard = out.platform_output.copied_text;
        }
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

    fn acts(&self) -> Vec<Act> {
        self.port.take_acts()
    }

    fn open(&self) -> bool {
        self.dialog.is_some()
    }
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
fn asking_for_a_new_code_after_no_answer_requests_one() {
    let port = ScriptedSettings::new();
    port.set(|s| s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage: CodeStage::NoAnswer, seconds_left: None })));
    let mut h = Harness::dialog(Dialog::Code(CodeDialog::new("den")), port);
    assert!(h.has("Den TV didn’t show a code."));
    h.click("Ask for a new code");
    assert_eq!(h.acts(), vec![Act::NewCode("den".into())]);
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
fn the_report_previews_exactly_what_is_copied() {
    let mut h = Harness::dialog(Dialog::Report(ReportDialog::default()), ScriptedSettings::new());
    let preview = |h: &Harness| h.texts.iter().find(|t| t.text.contains("Speakers: Kitchen")).unwrap().text.clone();
    let with_diagnostics = preview(&h);
    assert!(with_diagnostics.contains("Last message: Kitchen"));
    h.click("Copy report");
    assert_eq!(h.clipboard, with_diagnostics);
    h.click("Playback diagnostics");
    let without_diagnostics = preview(&h);
    assert!(!without_diagnostics.contains("Last message: Kitchen"));
    h.click("Open a GitHub issue");
    assert_eq!(h.clipboard, without_diagnostics);
    assert_eq!(h.acts(), vec![Act::OpenIssue(without_diagnostics)]);
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
        (Dialog::Report(ReportDialog::default()), |_| ()),
    ]
}

#[test]
fn every_dialog_holds_at_its_size_at_200_percent_with_longer_words_and_chinese_names() {
    STRETCH.with(|s| s.set(true));
    for scale in [1.0, 2.0] {
        for (d, set) in every_dialog() {
            let port = ScriptedSettings::with_names(Names::Chinese);
            port.set(set);
            let size = d.size();
            let what = format!("{d:?}");
            let h = Harness::with(d, port, Vec2::new(size[0], size[1] + 400.0), scale);
            assert!(h.open(), "{what}: still open");
            assert_clean(&h.texts, size[0], &what);
        }
    }
    STRETCH.with(|s| s.set(false));
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
    Key::SetReportFolderClosed,
    Key::SetReportSaveFailed,
];

#[test]
fn every_sentence_of_the_settings_block_is_said_and_none_is_left_unfilled() {
    USED.with(|u| u.borrow_mut().clear());
    let mut drawn: Vec<String> = super::views::tests::say_everything();
    let mut look = |h: &Harness| drawn.extend(h.texts.iter().map(|t| t.text.clone()));
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
        // The report's buttons say what happened.
        for b in ["Copy report", "Save as a file", "Open a GitHub issue"] {
            if h.has(b) {
                h.click(b);
                look(&h);
            }
        }
    }
    for expected in ["Set", "Use the default: 150 ms", "Try Kitchen again", "Switch to Kitchen by AirPlay", "Use “Steadier”",
        "Ask for a new code", "Stream To Speaker closes while Setup runs, and your sound plays on this PC meanwhile. When Setup is done, it plays on Kitchen again."] {
        assert!(drawn.iter().any(|t| t == expected), "missing {expected:?}");
    }
    for t in &drawn {
        assert!(!t.contains('{') && !t.contains('}'), "an unfilled placeholder: {t:?}");
        assert!(!t.contains('\''), "a straight apostrophe: {t:?}");
    }
    let used = USED.with(|u| u.borrow().clone());
    let unused: Vec<&Key> = Key::ALL.iter().filter(|k| is_settings_key(**k) && !used.contains(k) && !REAL_ONLY.contains(k)).collect();
    assert!(unused.is_empty(), "sentences no view or dialog says: {unused:?}");
}

// -------------------------------------------------------------------------
// The real app
// -------------------------------------------------------------------------

#[derive(Default)]
struct FakeDesktop {
    opened: Mutex<Vec<String>>,
    refuse_folder: std::sync::atomic::AtomicBool,
}

impl Desktop for FakeDesktop {
    fn open_url(&self, url: &str) {
        // The PC's own desktop opens nothing that this check refuses.
        assert!(crate::is_openable_url(url), "the app would refuse to open {url}");
        self.opened.lock().unwrap().push(url.into());
    }
    fn open_folder(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.opened.lock().unwrap().push(dir.display().to_string());
        if self.refuse_folder.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::other("folder refused"));
        }
        Ok(())
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

    app.set_airplay_password(&id, "pw");
    port.act(Act::ClearSpeakerCredentials(id.clone()));
    assert!(app.airplay_password(&id).is_none());
    port.act(Act::HideSpeaker(id.clone()));
    assert!(port.speakers().hidden.iter().any(|r| r.id == id));
    assert!(app.speaker_settings(&id).hidden);
    port.act(Act::ShowSpeaker(id.clone()));
    assert!(!app.speaker_settings(&id).hidden);

    port.act(Act::SongTitles(id.clone(), true));
    assert_eq!(port.speaker(&id).unwrap().song_titles, Some(true));

    app.remove_manual_speaker(&id);
    assert!(port.speaker(&id).is_none(), "removed");
}

#[test]
fn the_real_app_keeps_tone_per_speaker() {
    let (app, port, _) = real();
    let id = with_speaker(&app);
    assert_eq!(port.speaker(&id).unwrap().tone, Some((0.0, 0.0)), "flat, as the app-wide equalizer");
    port.act(Act::Tone(id.clone(), ToneAct::Bass(-2.0)));
    port.act(Act::Tone(id.clone(), ToneAct::Treble(1.0)));
    assert_eq!(port.speaker(&id).unwrap().tone, Some((-2.0, 1.0)));
    assert_eq!(app.speaker_settings(&id).tone.unwrap().bands_db[0], -2.0);
    port.act(Act::Tone(id.clone(), ToneAct::Flat));
    assert_eq!(port.speaker(&id).unwrap().tone, Some((0.0, 0.0)));
    // The app-wide curve is untouched.
    assert_eq!(app.eq_saved(), crate::equalizer::EqSettings::default());
}

#[test]
fn the_real_app_keeps_general_privacy_about_and_lab() {
    let (app, port, desk) = real();
    port.act(Act::SwitchBackAfterQuiet(true));
    port.act(Act::NotesByClock(false));
    let g = port.general();
    // The delay's keyboard shortcuts are off until a person turns them on.
    assert!(!g.shortcuts.unwrap().on && g.language.is_none());
    assert_eq!(g.play_calls, Some(false));
    assert_eq!(g.what_plays, Some(WhatPlays::Everything));
    port.act(Act::Shortcuts(true));
    assert!(port.general().shortcuts.unwrap().on && app.user_config.lock().unwrap().delay_shortcuts);
    port.act(Act::WhatPlays(WhatPlays::OnlySentApps));
    port.act(Act::PlayCalls(true));
    port.act(Act::Shortcuts(false));
    port.act(Act::ShortcutKeys(crate::platform::DelayKeys(["F8", "F9"].map(|k| vec!["Alt".into(), k.into()]))));
    let g = port.general();
    assert_eq!(g.what_plays, Some(WhatPlays::OnlySentApps));
    assert_eq!(g.play_calls, Some(true));
    let keys = g.shortcuts.unwrap();
    assert!(!keys.on);
    assert_eq!(keys.add, ["Alt", "F9"]);
    port.act(Act::ShowLab(true));
    assert!(port.general().switch_back_after_quiet && !port.general().notes_by_clock && port.about().show_lab);
    {
        let cfg = app.user_config.lock().unwrap();
        assert!(cfg.switch_back_after_quiet && !cfg.notes_by_clock);
    }
    port.act(Act::OnlyPlaying(true));
    assert!(port.privacy().only_playing && app.is_privacy_mode());
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
fn the_check_finds_a_muted_speaker_before_it_reads_the_level() {
    use super::real::volume_found;
    assert_eq!(volume_found(32, true), Err(Volume::Muted));
    assert_eq!(volume_found(0, true), Err(Volume::Muted));
    assert_eq!(volume_found(0, false), Err(Volume::Zero));
    assert_eq!(volume_found(32, false), Ok(Volume::Level(32)));
    assert_eq!(volume_found(250, false), Ok(Volume::Level(100)));
}

#[test]
fn every_link_the_app_opens_is_one_it_can_open() {
    let (_, port, desk) = real();
    for act in [Act::OpenApiHelp, Act::WhatsNew, Act::UpdateNow, Act::Donate, Act::LicenceAndSource, Act::OpenIssue(String::new())] {
        let before = desk.opened.lock().unwrap().len();
        port.act(act.clone());
        // The test desktop fails on an address the real one would refuse to open.
        assert_eq!(desk.opened.lock().unwrap().len(), before + 1, "{act:?} opened nothing");
    }
    // The API's link leads to a heading that the document has.
    let opened = desk.opened.lock().unwrap();
    let (page, place) = opened[0].split_once('#').unwrap();
    assert!(page.ends_with("/TECHNICAL.md"), "{page}");
    let heading = include_str!("../../../TECHNICAL.md").lines().filter_map(|l| l.strip_prefix("## ")).find(|h| h.to_lowercase().replace(' ', "-") == place);
    assert!(heading.is_some(), "TECHNICAL.md has no heading for #{place}");
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
    app.change_delay(Some(&id), crate::delay::Request::Remove(100)).unwrap();
    assert!(port.lab().carried_over.unwrap().speakers.is_empty());
    port.act(Act::Lab(LabAct::ResetCarriedOver));
    assert!(port.lab().carried_over.is_none());
    assert_eq!(app.user_config.lock().unwrap().airplay_latency_ms, 2000);
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
fn the_real_app_keeps_a_limit_and_a_local_name() {
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
    let page = port.speaker(&id).unwrap();
    assert_eq!(page.show_as.as_deref(), Some("Garage shelf"));
    assert_eq!(page.row.name, "Garage shelf");
    port.act(Act::ShowAs(id.clone(), "  ".into()));
    assert_eq!(app.speaker_settings(&id).show_as, None, "an empty name gives it its own back");
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

#[test]
fn the_playback_check_names_the_place_before_its_session_starts() {
    use crate::model::{samples, view::view, PortCore};
    let (app, settings, _) = real();
    let port = Arc::new(PortCore::new(view(samples::named("kitchen-nothing-playing"), None, false, 1)));
    app.engine_port.set(Arc::downgrade(&port)).unwrap();
    assert!(!app.is_speaker_bound());
    assert_eq!(settings.playback_check().speaker.as_deref(), Some("Kitchen"));
    port.publish(view(samples::named("pc-plain"), None, false, 2));
    assert_eq!(settings.playback_check().speaker, None);
}

#[test]
fn the_playback_check_reads_its_first_steps_from_what_the_panel_shows() {
    use crate::model::{samples, view::view, PortCore};
    let (app, settings, _) = real();
    let port = Arc::new(PortCore::new(view(samples::named("kitchen-nothing-playing"), None, false, 1)));
    app.engine_port.set(Arc::downgrade(&port)).unwrap();
    // Nothing plays: the check stops at its first step, as the panel says.
    let c = settings.playback_check();
    assert_eq!((c.playing, c.windows, c.connected), (Some(Err(None)), None, None));
    // Sound goes to the place: Windows sends it here, and the connection
    // is looked at next.
    port.publish(view(samples::named("kitchen-playing"), None, false, 2));
    assert!(!app.is_speaker_bound());
    let c = settings.playback_check();
    assert_eq!((c.playing, c.windows, c.connected), (Some(Ok(None)), Some(Ok(WindowsSends::ToUs)), Some(Err(()))));
    // An app stayed on another output while the rest goes to the place:
    // it is named, and the links after it are still looked at, since the
    // speaker may be silent for a reason of its own.
    // The output has Windows' full name for it, as on the panel's rows.
    let except = || {
        let mut scene = samples::named("kitchen-except-an-app");
        for o in &mut scene.outputs {
            o.friendly = "Speakers (Realtek Audio)".into();
            o.name = "Speakers".into();
        }
        scene
    };
    port.publish(view(except(), None, false, 3));
    let c = settings.playback_check();
    assert_eq!(c.playing, Some(Ok(None)));
    assert!(matches!(&c.windows, Some(Ok(WindowsSends::AppElsewhere { app, output })) if app == "Chrome" && output == "Speakers (Realtek Audio)"), "{:?}", c.windows);
    assert_eq!(c.connected, Some(Err(())));
    // Nothing else plays: the app is what plays, and the check ends there.
    let mut scene = except();
    scene.situation.doing = crate::model::situation::Doing::NothingPlaying;
    port.publish(view(scene, None, false, 4));
    let c = settings.playback_check();
    assert_eq!(c.playing, Some(Ok(Some("Chrome".into()))));
    assert!(matches!(&c.windows, Some(Err(WindowsSends::AppElsewhere { app, output })) if app == "Chrome" && output == "Speakers (Realtek Audio)"), "{:?}", c.windows);
    assert_eq!((c.connected, c.taking, c.volume), (None, None, None));
    // The app can't read the sound Windows sends it: the check ends there
    // whatever plays, and goes on once the sound is read again.
    port.publish(view(samples::named("kitchen-playing"), None, false, 5));
    app.note_sound_input_lost("the device went away");
    let c = settings.playback_check();
    assert_eq!((c.playing, c.windows, c.connected), (Some(Ok(None)), Some(Err(WindowsSends::InputLost)), None));
    app.note_sound_input_back();
    let c = settings.playback_check();
    assert_eq!((c.windows, c.connected), (Some(Ok(WindowsSends::ToUs)), Some(Err(()))));
}

#[test]
fn the_playback_check_says_what_the_speaker_page_knows_about_the_place() {
    use crate::model::{samples, view::view, PortCore};
    let (app, settings, _) = real();
    let port = Arc::new(PortCore::new(view(samples::named("kitchen-playing"), None, false, 1)));
    app.engine_port.set(Arc::downgrade(&port)).unwrap();
    assert!(!settings.playback_check().apple_tv);
    // An Apple TV may not pass the sound on to its HomePods: the check's
    // last lines say so.
    let mut scene = samples::named("kitchen-playing");
    scene.speakers.iter_mut().find(|s| s.id == samples::kitchen()).unwrap().kind = SpeakerKind::AppleTv;
    port.publish(view(scene, None, false, 2));
    assert!(settings.playback_check().apple_tv);
}

#[test]
fn the_playback_check_follows_music_assistants_server() {
    use crate::model::{samples, situation::Standing, view::view, PortCore};
    use crate::sendspin::source::SourceState;
    let (app, settings, _) = real();
    app.set_sendspin_source_enabled(true).unwrap();
    app.streaming_enabled.store(true, std::sync::atomic::Ordering::Release);
    app.connect_again(crate::sendspin_app::MA_OUTPUT_ID).unwrap();
    assert!(app.is_speaker_bound(), "choosing Music Assistant binds a session with no server there");
    let port = Arc::new(PortCore::new(view(samples::named("music-assistant-playing"), None, false, 1)));
    app.engine_port.set(Arc::downgrade(&port)).unwrap();
    // No server is connected: the check says so and ends there.
    let c = settings.playback_check();
    assert!(c.server_away);
    assert_eq!((c.connected, c.taking, c.volume), (Some(Err(())), None, None));
    // A server that only asks to pair isn't one that plays.
    app.set_sendspin_source_state(SourceState::NeedsPairing { server: "Server".into() });
    let c = settings.playback_check();
    assert!(c.server_away && c.connected == Some(Err(())));
    // Connected, and it hasn't started this PC's sound: the panel's note.
    app.set_sendspin_source_state(SourceState::Connected { server: "Server".into(), streaming: false });
    let mut scene = samples::named("music-assistant-playing");
    scene.situation.standing.push(Standing::MusicAssistantNotStarted);
    port.publish(view(scene, None, false, 2));
    let c = settings.playback_check();
    assert!(!c.server_away);
    assert_eq!((c.connected, c.taking, c.volume), (Some(Ok(())), Some(Err(Taking::NotAsking { two_ways: false })), None));
    // It plays this PC's sound.
    app.set_sendspin_source_state(SourceState::Connected { server: "Server".into(), streaming: true });
    port.publish(view(samples::named("music-assistant-playing"), None, false, 3));
    let c = settings.playback_check();
    assert_eq!((c.connected, c.taking), (Some(Ok(())), Some(Ok(Taking::Yes))));
    assert!(matches!(c.volume, Some(Ok(_))), "{:?}", c.volume);
    app.set_sendspin_source_enabled(false).unwrap();
}

#[test]
fn the_playback_check_reads_the_level_and_mute_the_panel_shows() {
    use crate::model::{facts::Level, samples, view::view, PortCore};
    let (app, settings, _) = real();
    // Music Assistant as the output, playing this PC's sound: it is bound
    // with no speaker to reach.
    app.set_sendspin_source_enabled(true).unwrap();
    app.set_sendspin_source_state(crate::sendspin::source::SourceState::Connected { server: "Server".into(), streaming: true });
    app.streaming_enabled.store(true, std::sync::atomic::Ordering::Release);
    app.connect_again(crate::sendspin_app::MA_OUTPUT_ID).unwrap();
    assert!(app.is_speaker_bound());
    let mut scene = samples::named("music-assistant-playing");
    scene.volume.place = Some(Level::at(32));
    let port = Arc::new(PortCore::new(view(scene.clone(), None, false, 1)));
    app.engine_port.set(Arc::downgrade(&port)).unwrap();
    assert_eq!(settings.playback_check().volume, Some(Ok(Volume::Level(32))));
    // A muted place is silent at any level.
    scene.volume.place.as_mut().unwrap().muted = true;
    port.publish(view(scene.clone(), None, false, 2));
    assert_eq!(settings.playback_check().volume, Some(Err(Volume::Muted)));
    scene.volume.place = Some(Level::at(0));
    port.publish(view(scene, None, false, 3));
    assert_eq!(settings.playback_check().volume, Some(Err(Volume::Zero)));
    app.set_sendspin_source_enabled(false).unwrap();
}

#[test]
fn a_report_without_speaker_details_omits_identifying_text_from_every_part() {
    let (app, port, _) = real();
    let id = with_speaker(&app);
    let message = format!("Garage AirPlay {id} 192.0.2.40");
    *app.connecting.lock().unwrap() = Some(message.clone());
    app.record_error(&message);
    app.record_fault(&message);
    let report = port.report(ReportParts { speakers: false, log: true, ..ReportParts::default() });
    assert!(report.contains("[now]") && report.contains("[statistics just before it]"));
    for detail in ["Garage", "AirPlay", &id, "192.0.2.40"] {
        assert!(!report.contains(detail), "{report}");
    }

    let mut h = Harness::dialog(Dialog::Report(ReportDialog::with_text("Bedroom is silent")), ScriptedSettings::new());
    h.click("The log file");
    h.click("Speaker names, kinds and addresses");
    h.click("Open a GitHub issue");
    // The person's own words go as written; the app adds no speaker details.
    assert!(matches!(h.acts().as_slice(), [Act::OpenIssue(text)] if text.contains("Bedroom is silent")
        && ["Kitchen", "Sonos", "192.168", "[log file attached]"].iter().all(|detail| !text.contains(detail))));
}

#[test]
fn saving_a_report_reports_the_file_and_folder_results() {
    use std::sync::atomic::Ordering;
    let (_, settings, desk) = real();
    let dir = std::env::temp_dir().join(format!("sts-report-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(settings.save_report_in(None, "report"), ReportSave::Failed);
    assert_eq!(settings.save_report_in(Some(&dir.join("missing")), "report"), ReportSave::Failed);
    assert!(desk.opened.lock().unwrap().is_empty());
    desk.refuse_folder.store(true, Ordering::Relaxed);
    assert_eq!(settings.save_report_in(Some(&dir), "report"), ReportSave::SavedOnly);
    let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    assert_eq!(std::fs::read_to_string(file).unwrap(), "report");
    desk.refuse_folder.store(false, Ordering::Relaxed);
    assert_eq!(settings.save_report_in(Some(&dir), "report"), ReportSave::Opened);
    std::fs::remove_dir_all(&dir).unwrap();

    for (result, words) in [
        (ReportSave::Failed, "Couldn’t save the report in your log folder."),
        (ReportSave::SavedOnly, "Saved in your log folder. The folder couldn’t be opened."),
        (ReportSave::Opened, "Saved in your log folder, which is open now."),
    ] {
        let port = ScriptedSettings::new();
        port.set(|s| s.report_save = result);
        let mut h = Harness::dialog(Dialog::Report(ReportDialog::default()), port);
        h.click("Save as a file");
        assert!(h.shows(words));
    }
}

#[test]
fn diagnostics_follows_the_report_versions_checkbox() {
    let (_, port, _) = real();
    let report = |versions| port.report(ReportParts { versions, diagnostics: true, speakers: false, log: false });
    let with = report(true);
    assert!(with.contains(&format!("{} v{} diagnostics", crate::PRODUCT_NAME, crate::display_version())));
    let without = report(false);
    assert_eq!(without.lines().next().unwrap(), format!("{} diagnostics", crate::PRODUCT_NAME));
    assert!(!without.contains(crate::display_version()));
    assert!(without.contains("[now]"));
}
