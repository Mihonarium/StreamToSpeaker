//! Pictures of the Settings window and its dialogs, drawn from the scripted
//! port: every page, every dialog and the states worth checking by eye.
//! Run them with the preview tool (`settings-*`, `dialog-*`).

use egui::Ui;

use crate::model::text::LocalTime;
use crate::ui::preview::{Scene, Stage};

use super::dialogs::{AddDialog, CodeDialog, Dialog, MaCodeDialog, PasswordDialog, ReportDialog, SendspinDialog, ToneDialog};
use super::port::*;
use super::scripted::{Names, ScriptedSettings, State};
use super::{Page, Window};

/// Every scene of the Settings window and its dialogs.
pub fn scenes() -> Vec<Scene> {
    vec![
        Scene { name: "settings-speakers", size: [880.0, 1000.0], draw: speakers },
        Scene { name: "settings-speakers-narrow", size: [640.0, 480.0], draw: speakers },
        Scene { name: "settings-speakers-long-names", size: [880.0, 1000.0], draw: speakers_long },
        Scene { name: "settings-speakers-empty", size: [880.0, 640.0], draw: speakers_empty },
        Scene { name: "settings-speaker-kitchen", size: [880.0, 1500.0], draw: kitchen },
        Scene { name: "settings-speaker-kitchen-chinese", size: [880.0, 1500.0], draw: kitchen_chinese },
        Scene { name: "settings-speaker-kitchen-typing", size: [880.0, 640.0], draw: kitchen_typing },
        Scene { name: "settings-speaker-kitchen-narrow", size: [640.0, 1700.0], draw: kitchen },
        Scene { name: "settings-speaker-office", size: [880.0, 1260.0], draw: office },
        Scene { name: "settings-speaker-office-reconnecting", size: [880.0, 900.0], draw: office_reconnecting },
        Scene { name: "settings-speaker-bedroom", size: [880.0, 1180.0], draw: bedroom },
        Scene { name: "settings-speaker-den-tv", size: [880.0, 980.0], draw: den },
        Scene { name: "settings-speaker-desk", size: [880.0, 900.0], draw: desk },
        Scene { name: "settings-speaker-garage", size: [880.0, 1000.0], draw: garage },
        Scene { name: "settings-general", size: [880.0, 1180.0], draw: general },
        Scene { name: "settings-music-assistant", size: [880.0, 1000.0], draw: music_assistant },
        Scene { name: "settings-music-assistant-done", size: [880.0, 1000.0], draw: music_assistant_done },
        Scene { name: "settings-privacy", size: [880.0, 880.0], draw: privacy },
        Scene { name: "settings-help", size: [880.0, 700.0], draw: help },
        Scene { name: "settings-help-guide", size: [880.0, 640.0], draw: guide_1 },
        Scene { name: "settings-help-guide-2", size: [880.0, 800.0], draw: guide_2 },
        Scene { name: "settings-cant-hear", size: [880.0, 820.0], draw: cant_hear_bedroom },
        Scene { name: "settings-cant-hear-muted", size: [880.0, 700.0], draw: cant_hear_muted },
        Scene { name: "settings-cant-hear-on-this-pc", size: [880.0, 500.0], draw: cant_hear_pc },
        Scene { name: "settings-cant-find", size: [880.0, 760.0], draw: cant_find },
        Scene { name: "settings-about", size: [880.0, 1000.0], draw: about },
        Scene { name: "settings-about-up-to-date", size: [880.0, 900.0], draw: about_latest },
        Scene { name: "settings-lab", size: [880.0, 1200.0], draw: lab },
        Scene { name: "dialog-pairing-code", size: [720.0, 400.0], draw: pairing_code },
        Scene { name: "dialog-pairing-code-wrong", size: [720.0, 420.0], draw: pairing_wrong },
        Scene { name: "dialog-pairing-code-timed-out", size: [720.0, 400.0], draw: pairing_timed_out },
        Scene { name: "dialog-password", size: [520.0, 320.0], draw: password },
        Scene { name: "dialog-password-wrong", size: [520.0, 360.0], draw: password_wrong },
        Scene { name: "dialog-sendspin-code", size: [560.0, 300.0], draw: sendspin },
        Scene { name: "dialog-sendspin-identity", size: [560.0, 360.0], draw: sendspin_identity },
        Scene { name: "dialog-music-assistant-code", size: [600.0, 340.0], draw: ma_code },
        Scene { name: "dialog-add-address", size: [680.0, 420.0], draw: add_found },
        Scene { name: "dialog-add-address-two-ways", size: [680.0, 500.0], draw: add_two_ways },
        Scene { name: "dialog-add-address-nothing", size: [680.0, 420.0], draw: add_nothing },
        Scene { name: "dialog-add-address-by-hand", size: [680.0, 480.0], draw: add_by_hand },
        Scene { name: "dialog-tone", size: [760.0, 470.0], draw: tone },
        Scene { name: "dialog-report", size: [840.0, 560.0], draw: report },
    ]
}

/// The window on `page`, the scripted app changed by `f`.
fn window(ui: &mut Ui, names: Names, page: Page, f: impl FnOnce(&mut State)) {
    let port = ScriptedSettings::with_names(names);
    port.set(f);
    let mut w = Window::new();
    w.open(page);
    let _ = w.show_in(ui, &port);
}

fn on(ui: &mut Ui, page: Page) {
    window(ui, Names::Usual, page, |_| ());
}

fn speakers(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Speakers);
}

fn speakers_long(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Long, Page::Speakers, |s| {
        if let Some(r) = s.speakers.speakers.get_mut(2) {
            r.name = super::scripted::CHINESE_NAME.into();
        }
    });
}

fn speakers_empty(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Usual, Page::Speakers, |s| {
        s.speakers.speakers.clear();
        s.speakers.hidden.clear();
        s.header.status = "Sound is on this PC".into();
    });
}

fn kitchen(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Speaker("kitchen".into()));
}

fn kitchen_chinese(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Chinese, Page::Speaker("kitchen".into()), |_| ());
}

fn kitchen_typing(ui: &mut Ui, st: &mut Stage) {
    let port = ScriptedSettings::new();
    let mut w = Window::new();
    w.open(Page::Speaker("kitchen".into()));
    w.st.delay.insert("kitchen".into(), crate::ui::delay::DelayState::with_amount("150", false));
    let _ = w.show_in(ui, &port);
    st.focus_named(ui.min_rect(), "Milliseconds to remove or add");
}

fn office(ui: &mut Ui, _: &mut Stage) {
    let port = ScriptedSettings::new();
    port.set(|s| {
        s.header.status = "Playing on Office".into();
        if let Some(p) = s.pages.iter_mut().find(|p| p.row.id == "office") {
            p.row.presence = Presence::PlayingNow;
        }
    });
    let mut w = Window::new();
    w.open(Page::Speaker("office".into()));
    // After a Remove press on Steady, the offer of Short delay; the details
    // open.
    w.st.open.insert("steady-offer:office".into());
    w.st.open.insert("delay-details:office".into());
    w.st.open.insert("details:office".into());
    let _ = w.show_in(ui, &port);
}

fn office_reconnecting(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Usual, Page::Speaker("office".into()), |s| {
        s.header.status = "Playing on Office".into();
        if let Some(p) = s.pages.iter_mut().find(|p| p.row.id == "office") {
            p.row.presence = Presence::PlayingNow;
            if let Some(c) = &mut p.delay {
                c.status.change_ms = -400;
                c.status.delay_ms = 1600;
                c.status.progress = Progress::Reconnecting { from_ms: -300 };
            }
        }
    });
}

fn bedroom(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Speaker("bedroom".into()));
}

fn den(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Speaker("den".into()));
}

fn desk(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Speaker("desk".into()));
}

fn garage(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Speaker("garage".into()));
}

fn general(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::General);
}

fn music_assistant(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::MusicAssistant);
}

fn music_assistant_done(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Usual, Page::MusicAssistant, |s| {
        s.ma.code = None;
        s.ma.paired_with = vec!["Music Assistant (Home)".into()];
        s.ma.played = Some(Some(Date { day: 12, month: 10 }));
    });
}

fn privacy(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Privacy);
}

fn help(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Help);
}

fn guide_1(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Guide(1));
}

fn guide_2(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::Guide(2));
}

fn cant_hear_bedroom(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Usual, Page::CantHear, |s| {
        s.header.status = "Playing on Bedroom".into();
        s.playback = PlaybackCheck {
            speaker: Some("Bedroom".into()),
            playing: Some(Ok(Some("Edge".into()))),
            windows: Some(Ok(WindowsSends::ToUs)),
            connected: Some(Ok(())),
            taking: Some(Ok(Taking::CantTell)),
            volume: Some(Ok(Volume::Level(18))),
            untested: true,
            apple_tv: false,
            on_short: false,
        };
    });
}

fn cant_hear_muted(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Usual, Page::CantHear, |s| {
        s.playback.volume = Some(Err(Volume::Muted));
    });
}

fn cant_hear_pc(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Usual, Page::CantHear, |s| {
        s.header.status = "Sound is on this PC".into();
        s.playback.speaker = None;
    });
}

fn cant_find(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::CantFind);
}

fn about(ui: &mut Ui, _: &mut Stage) {
    on(ui, Page::About);
}

fn about_latest(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Usual, Page::About, |s| {
        s.about.driver = Some(Driver::Running("1.1.2.215".into()));
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
        s.about.lab = true;
        s.header.lab = true;
    });
}

fn lab(ui: &mut Ui, _: &mut Stage) {
    window(ui, Names::Usual, Page::Lab, |s| {
        s.header.lab = true;
        s.about.lab = true;
        s.lab.catch_up_frames = Some(8);
        s.lab.carried_over = Some(CarriedOver { airplay_ms: Some(500), homepod_ms: None, homepod_buffered: false, speakers: vec!["Office".into(), "Garage".into()] });
    });
}

/// `d` drawn with the scripted app changed by `f`.
fn dialog(ui: &mut Ui, mut d: Dialog, f: impl FnOnce(&mut State)) {
    let port = ScriptedSettings::new();
    port.set(f);
    let _ = d.show_in(ui, &port);
}

fn code_ask(stage: CodeStage) -> impl FnOnce(&mut State) {
    move |s: &mut State| s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage, seconds_left: Some(102) }))
}

fn pairing_code(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Code(CodeDialog::new("den")), code_ask(CodeStage::Showing));
}

fn pairing_wrong(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Code(CodeDialog::new("den")), code_ask(CodeStage::Wrong));
}

fn pairing_timed_out(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Code(CodeDialog::new("den")), code_ask(CodeStage::TimedOut));
}

fn password(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Password(PasswordDialog::change("office", "Office")), |_| ());
}

fn password_wrong(ui: &mut Ui, _: &mut Stage) {
    let mut d = PasswordDialog::change("office", "Office");
    d.id = "office".into();
    dialog(ui, Dialog::Password(d), |s| {
        s.wanted = Some(Wanted::Password(PasswordAsk { id: "office".into(), name: "Office".into(), wrong: true, once: false }));
    });
}

fn sendspin_ask(identity_changed: bool) -> impl FnOnce(&mut State) {
    move |s: &mut State| {
        s.wanted = Some(Wanted::SendspinCode(SendspinAsk {
            id: "desk".into(),
            name: "Desk Speaker".into(),
            live_code: true,
            seconds_left: Some(160),
            identity_changed,
            lost: false,
            wrong: false,
        }))
    }
}

fn sendspin(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Sendspin(SendspinDialog::new("desk")), sendspin_ask(false));
}

fn sendspin_identity(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Sendspin(SendspinDialog::new("desk")), sendspin_ask(true));
}

fn ma_code(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::MaCode(MaCodeDialog::default()), |s| {
        s.wanted = Some(Wanted::MaCode(MaCodeShow { code: "482917".into(), this_pc: "DESKTOP-7Q".into(), seconds_left: Some(161) }));
    });
}

fn garage_found(kind: SpeakerKind, way: Way, delay: u32) -> Found {
    Found { name: "Garage".into(), kind, delay_ms: delay, way }
}

fn add_found(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::AddAddress(AddDialog::default()), |s| {
        s.probe = Probe::Found { address: "192.168.20.40".into(), ways: vec![garage_found(SpeakerKind::AirPlay, Way::AirPlay, 2000)] };
    });
}

fn add_two_ways(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::AddAddress(AddDialog::default()), |s| {
        s.probe = Probe::Found {
            address: "192.168.20.40".into(),
            ways: vec![garage_found(SpeakerKind::Sonos, Way::Upnp, 200), garage_found(SpeakerKind::AirPlay, Way::AirPlay, 2000)],
        };
    });
}

fn add_nothing(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::AddAddress(AddDialog::default()), |s| s.probe = Probe::Nothing { address: "192.168.1.20".into() });
}

fn add_by_hand(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::AddAddress(AddDialog::by_hand_open()), |_| ());
}

fn tone(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Tone(ToneDialog::new("kitchen")), |_| ());
}

fn report(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Report(ReportDialog::with_text("Bedroom plays for about two seconds, then stops. Only the left one plays.")), |_| ());
}
