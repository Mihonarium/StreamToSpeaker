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
}

impl Harness {
    fn dialog(d: Dialog, port: ScriptedSettings) -> Self {
        let size = d.size();
        Self::with(d, port, Vec2::new(size[0], size[1] + 200.0), 1.0)
    }

    fn with(dialog: Dialog, port: ScriptedSettings, size: Vec2, scale: f32) -> Self {
        let mut h = Harness { ctx: preview::context(fonts(), Mode::Light), port: Arc::new(port), dialog: Some(dialog), size, scale, pass: 0, texts: Vec::new() };
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
    Key::SetNothingChanged,
    Key::SetChoiceSteady,
    Key::SetChoiceShort,
    Key::SetDetailStream,
    Key::SetDetailBuffer,
    Key::SetDetailLead,
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
