//! Pictures of the panel's views on the scripted app: every view in the
//! states that matter, framed as the panel frames them.

use std::sync::Arc;

use egui::{Id, Margin, Ui};

use crate::model::port::Port;
use crate::model::scripted::{Scenario, ScriptedEngine};
use crate::model::snapshot::Snapshot;
use crate::model::state::{NowState, Secret, SpeakerId};
use crate::model::text::LocalTime;
use crate::panel::view::{show_view, ViewState};
use crate::shell::ViewContent;
use crate::ui::preview::{Scene, Stage};
use crate::ui::theme::Theme;

use super::*;
use crate::settings::port::*;
use crate::settings::scripted::{Names, ScriptedSettings, State};

const MARGIN: f32 = 20.0;
const WIDTH: f32 = crate::panel::WIDTH;

/// Every scene of the views.
pub fn scenes() -> Vec<Scene> {
    let s = |name, h: f32, draw| Scene { name, size: [WIDTH + 2.0 * MARGIN, h], draw };
    vec![
        s("view-speaker-sonos-playing", 1180.0, speaker_kitchen),
        s("view-speaker-airplay-steady", 1220.0, speaker_office),
        s("view-speaker-airplay-short", 1220.0, speaker_office_short),
        s("view-speaker-needs-code", 1080.0, speaker_needs_code),
        s("view-speaker-needs-password", 1220.0, speaker_needs_password),
        s("view-speaker-not-found", 1120.0, speaker_garage),
        s("view-speaker-homepod-pair", 1220.0, speaker_bedroom),
        s("view-speaker-fixed-on-steady", 1180.0, speaker_den),
        s("view-speaker-sendspin", 1100.0, speaker_desk),
        s("view-speaker-all-bands", 1700.0, speaker_bands),
        s("view-speaker-details", 1500.0, speaker_details),
        s("view-speaker-amount", 1180.0, speaker_amount),
        s("view-speaker-forget", 1320.0, speaker_forget),
        s("view-speaker-long-name", 1300.0, speaker_long),
        s("view-speaker-chinese-name", 1300.0, speaker_chinese),
        s("view-settings", 760.0, settings_start),
        s("view-settings-parts-open", 1260.0, settings_open),
        s("view-settings-refused", 880.0, settings_refused),
        s("view-settings-update-ready", 820.0, settings_update),
        s("view-settings-shortcut-taken", 980.0, settings_taken),
        s("view-missing", 860.0, missing_start),
        s("view-missing-searching", 760.0, missing_searching),
        s("view-missing-found", 800.0, missing_found),
        s("view-missing-two-ways", 900.0, missing_two_ways),
        s("view-missing-nothing", 800.0, missing_nothing),
        s("view-missing-by-hand", 900.0, missing_by_hand),
        s("view-missing-hidden", 980.0, missing_hidden),
        s("view-check-step-by-step", 860.0, check),
        s("view-cant-hear-all-good", 860.0, cant_hear_good),
        s("view-cant-hear-cant-tell", 900.0, cant_hear_cant_tell),
        s("view-cant-hear-muted", 760.0, cant_hear_muted),
        s("view-cant-hear-nothing-playing", 520.0, cant_hear_nothing),
        s("view-cant-hear-not-connected", 640.0, cant_hear_not_connected),
        s("view-cant-hear-not-asking", 760.0, cant_hear_not_asking),
        s("view-cant-hear-windows-elsewhere", 640.0, cant_hear_windows),
        s("view-cant-hear-on-this-pc", 360.0, cant_hear_pc),
        s("view-music-assistant-off", 520.0, ma_off),
        s("view-music-assistant-setting-up", 1120.0, ma_setting_up),
        s("view-music-assistant-done", 1100.0, ma_done),
        s("view-about", 880.0, about),
        s("view-about-up-to-date", 760.0, about_latest),
        s("view-lab", 1300.0, lab),
        s("view-guides", 1700.0, guides),
    ]
}

/// Draw `view` in the panel's frame, over `snap`.
fn frame(ui: &mut Ui, key: &str, view: &mut dyn ViewContent, snap: &Snapshot) {
    let t = Theme::of(ui.ctx());
    let id = Id::new(("settings-view-scene", key));
    let mut vs: ViewState = ui.ctx().data(|d| d.get_temp(id)).unwrap_or_default();
    ui.add_space(MARGIN);
    ui.horizontal(|ui| {
        ui.add_space(MARGIN);
        egui::Frame::none().fill(t.c.surface).stroke(t.card_stroke()).rounding(8.0).shadow(t.shadow()).inner_margin(Margin::ZERO).show(ui, |ui| {
            ui.set_width(WIDTH);
            let _ = show_view(ui, view, snap, &mut vs, 0);
        });
    });
    ui.add_space(MARGIN);
    ui.ctx().data_mut(|d| d.insert_temp(id, vs));
}

fn port(names: Names, f: impl FnOnce(&mut State)) -> Arc<dyn SettingsPort> {
    let p = ScriptedSettings::with_names(names);
    p.set(f);
    Arc::new(p)
}

fn snap(s: Scenario) -> Arc<Snapshot> {
    ScriptedEngine::new(s).snapshot()
}

fn speaker_view(
    ui: &mut Ui,
    key: &str,
    scenario: Scenario,
    names: Names,
    id: &str,
    f: impl FnOnce(&mut State),
    tune: impl FnOnce(SpeakerOptions) -> SpeakerOptions,
) {
    let mut v = tune(SpeakerOptions::new(port(names, f), id));
    frame(ui, key, &mut v, &snap(scenario));
}

fn speaker_kitchen(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "kitchen", Scenario::Playing, Names::Usual, "kitchen", |_| (), |v| v);
}

fn speaker_office(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "office", Scenario::Playing, Names::Usual, "office", |_| (), |v| v);
}

fn speaker_office_short(ui: &mut Ui, _: &mut Stage) {
    speaker_view(
        ui,
        "office-short",
        Scenario::Playing,
        Names::Usual,
        "office",
        |s| {
            if let Some(d) = s.pages.iter_mut().find(|p| p.row.id == "office").and_then(|p| p.delay.as_mut()) {
                d.status.choice = Some(DelayChoice::Short);
                d.status.own_delay_ms = 300;
                d.status.change_ms = 0;
                d.status.delay_ms = 300;
                d.status.limits = Some(Limits { remove_ms: 280, add_ms: 699 });
            }
        },
        |v| v,
    );
}

fn speaker_needs_code(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "den-code", Scenario::NeedsPairing, Names::Usual, "den", |_| (), |v| v);
}

fn speaker_needs_password(ui: &mut Ui, _: &mut Stage) {
    let engine = ScriptedEngine::new(Scenario::Playing);
    let mut f = engine.facts();
    f.state = NowState::NeedsSecret { speaker: SpeakerId::new("office"), secret: Secret::Password };
    engine.set_facts(f);
    let mut v = SpeakerOptions::new(port(Names::Usual, |_| ()), "office");
    frame(ui, "office-password", &mut v, &engine.snapshot());
}

fn speaker_garage(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "garage", Scenario::Playing, Names::Usual, "garage", |_| (), |v| v);
}

fn speaker_bedroom(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "bedroom", Scenario::Playing, Names::Usual, "bedroom", |_| (), |v| v);
}

fn speaker_den(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "den", Scenario::Playing, Names::Usual, "den", |_| (), |v| v);
}

fn speaker_desk(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "desk", Scenario::Playing, Names::Usual, "desk", |_| (), |v| v);
}

fn speaker_bands(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "bands", Scenario::Playing, Names::Usual, "kitchen", |_| (), |v| v.with_open("bands"));
}

fn speaker_details(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "details", Scenario::Playing, Names::Usual, "office", |_| (), |v| v.with_open("details"));
}

fn speaker_amount(ui: &mut Ui, st: &mut Stage) {
    speaker_view(ui, "amount", Scenario::Playing, Names::Usual, "kitchen", |_| (), |v| v.with_amount("150"));
    st.focus_named(ui.min_rect(), "Milliseconds to remove or add");
}

fn speaker_forget(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "forget", Scenario::Playing, Names::Usual, "kitchen", |_| (), |v| v.asking_to_forget());
}

fn speaker_long(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "long", Scenario::Playing, Names::Long, "kitchen", |_| (), |v| v);
}

fn speaker_chinese(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "chinese", Scenario::Playing, Names::Chinese, "kitchen", |_| (), |v| v);
}

fn starting(s: &mut State) {
    if let Some(k) = &mut s.general.shortcuts {
        k.on = false;
    }
    if let Some(p) = &mut s.privacy.phone {
        p.on = false;
        p.link = None;
    }
    s.privacy.refused = Some(Vec::new());
    s.ma.offer = Some(false);
}

fn settings_view(ui: &mut Ui, key: &str, f: impl FnOnce(&mut State)) {
    let mut v = SettingsView::new(port(Names::Usual, f));
    frame(ui, key, &mut v, &snap(Scenario::Playing));
}

fn settings_start(ui: &mut Ui, _: &mut Stage) {
    settings_view(ui, "settings", |s| {
        starting(s);
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
    });
}

fn settings_open(ui: &mut Ui, _: &mut Stage) {
    settings_view(ui, "settings-open", |s| {
        s.privacy.refused = Some(Vec::new());
        s.general.what_plays = Some(WhatPlays::OnlySentApps);
        s.general.when_stops = Some(WhenStops::StaySilent);
        s.privacy.only_playing = false;
        if let Some(a) = &mut s.privacy.api {
            a.on = true;
        }
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
    });
}

fn settings_refused(ui: &mut Ui, _: &mut Stage) {
    settings_view(ui, "settings-refused", |s| {
        let refused = s.privacy.refused.clone();
        starting(s);
        s.privacy.refused = refused;
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
    });
}

fn settings_update(ui: &mut Ui, _: &mut Stage) {
    settings_view(ui, "settings-update", starting);
}

fn settings_taken(ui: &mut Ui, _: &mut Stage) {
    settings_view(ui, "settings-taken", |s| {
        starting(s);
        if let Some(k) = &mut s.general.shortcuts {
            k.on = true;
            k.taken = [false, true, false];
        }
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
    });
}

fn missing_view(ui: &mut Ui, key: &str, f: impl FnOnce(&mut State), tune: impl FnOnce(MissingView) -> MissingView) {
    let scripted = Arc::new(ScriptedSettings::with_names(Names::Usual));
    let p: Arc<dyn SettingsPort> = scripted.clone();
    let mut v = tune(MissingView::new(p));
    // The view starts a new search for an address when it opens: the
    // state to show is set after.
    scripted.set(f);
    frame(ui, key, &mut v, &snap(Scenario::Playing));
}

fn no_hidden(s: &mut State) {
    s.speakers.hidden.clear();
}

fn missing_start(ui: &mut Ui, _: &mut Stage) {
    missing_view(ui, "missing", no_hidden, |v| v);
}

fn missing_searching(ui: &mut Ui, _: &mut Stage) {
    missing_view(
        ui,
        "missing-searching",
        |s| {
            no_hidden(s);
            s.probe = Probe::Checking { address: "192.168.20.40".into() };
        },
        |v| v.with_address("192.168.20.40"),
    );
}

fn garage(kind: SpeakerKind, way: Way, delay_ms: u32) -> Found {
    Found { name: "Garage".into(), kind, delay_ms, way }
}

fn missing_found(ui: &mut Ui, _: &mut Stage) {
    missing_view(
        ui,
        "missing-found",
        |s| {
            no_hidden(s);
            s.probe = Probe::Found { address: "192.168.20.40".into(), ways: vec![garage(SpeakerKind::AirPlay, Way::AirPlay, 2000)] };
        },
        |v| v.with_address("192.168.20.40"),
    );
}

fn missing_two_ways(ui: &mut Ui, _: &mut Stage) {
    missing_view(
        ui,
        "missing-two-ways",
        |s| {
            no_hidden(s);
            s.probe = Probe::Found {
                address: "192.168.20.40".into(),
                ways: vec![garage(SpeakerKind::Sonos, Way::Upnp, 200), garage(SpeakerKind::AirPlay, Way::AirPlay, 2000)],
            };
        },
        |v| v.with_address("192.168.20.40"),
    );
}

fn missing_nothing(ui: &mut Ui, _: &mut Stage) {
    missing_view(
        ui,
        "missing-nothing",
        |s| {
            no_hidden(s);
            s.probe = Probe::Nothing { address: "192.168.1.20".into() };
        },
        |v| v.with_address("192.168.1.20"),
    );
}

fn missing_by_hand(ui: &mut Ui, _: &mut Stage) {
    missing_view(ui, "missing-by-hand", no_hidden, |v| v.with_address("192.168.20.40").by_hand());
}

fn missing_hidden(ui: &mut Ui, _: &mut Stage) {
    missing_view(
        ui,
        "missing-hidden",
        |s| {
            let mut tv = s.speakers.hidden[0].clone();
            tv.id = "guest".into();
            tv.name = "Guest Room TV".into();
            tv.cant_play = Some(CantPlay::AirPlayOff);
            s.speakers.speakers.push(tv);
            s.speakers.sendspin = Some(true);
        },
        |v| v,
    );
}

fn check(ui: &mut Ui, _: &mut Stage) {
    let mut v = CheckView::new(port(Names::Usual, |s| {
        s.discovery.lines.push(FindLine::AirPlaySearchFailed);
    }));
    frame(ui, "check", &mut v, &snap(Scenario::Playing));
}

fn cant_hear(ui: &mut Ui, key: &str, scenario: Scenario, f: impl FnOnce(&mut State)) {
    let mut v = CantHearView::new(port(Names::Usual, f));
    frame(ui, key, &mut v, &snap(scenario));
}

fn cant_hear_good(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-good", Scenario::Playing, |_| ());
}

fn cant_hear_cant_tell(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-cant-tell", Scenario::Playing, |s| {
        s.playback.speaker = Some("Bedroom".into());
        s.playback.taking = Some(Ok(Taking::CantTell));
        s.playback.volume = Some(Ok(Volume::Level(18)));
        s.playback.untested = true;
    });
}

fn cant_hear_muted(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-muted", Scenario::Playing, |s| s.playback.volume = Some(Err(Volume::Muted)));
}

fn cant_hear_nothing(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-nothing", Scenario::Playing, |s| {
        s.playback.playing = Some(Err(None));
        s.playback.windows = None;
        s.playback.connected = None;
        s.playback.taking = None;
        s.playback.volume = None;
    });
}

fn cant_hear_not_connected(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-not-connected", Scenario::Playing, |s| {
        s.playback.connected = Some(Err(()));
        s.playback.taking = None;
        s.playback.volume = None;
    });
}

fn cant_hear_not_asking(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-not-asking", Scenario::Playing, |s| {
        s.playback.taking = Some(Err(Taking::NotAsking { two_ways: true }));
        s.playback.volume = None;
    });
}

fn cant_hear_windows(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-windows", Scenario::Playing, |s| {
        s.playback.windows = Some(Err(WindowsSends::Elsewhere { output: "Headphones (WH-1000XM4)".into() }));
        s.playback.connected = None;
        s.playback.taking = None;
        s.playback.volume = None;
    });
}

fn cant_hear_pc(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-pc", Scenario::OnThisPc, |s| s.playback.speaker = None);
}

fn ma(ui: &mut Ui, key: &str, f: impl FnOnce(&mut State)) {
    let mut v = MusicAssistantView::new(port(Names::Usual, f));
    frame(ui, key, &mut v, &snap(Scenario::Playing));
}

fn ma_off(ui: &mut Ui, _: &mut Stage) {
    ma(ui, "ma-off", |s| s.ma.offer = Some(false));
}

fn ma_setting_up(ui: &mut Ui, _: &mut Stage) {
    ma(ui, "ma-setting-up", |_| ());
}

fn ma_done(ui: &mut Ui, _: &mut Stage) {
    ma(ui, "ma-done", |s| {
        s.ma.code = None;
        s.ma.paired_with = vec!["Music Assistant (Home)".into()];
        s.ma.played = Some(Some(Date { day: 12, month: 10 }));
    });
}

fn about(ui: &mut Ui, _: &mut Stage) {
    let mut v = super::AboutView::new(port(Names::Usual, |_| ()));
    frame(ui, "about", &mut v, &snap(Scenario::Playing));
}

fn about_latest(ui: &mut Ui, _: &mut Stage) {
    let mut v = super::AboutView::new(port(Names::Usual, |s| {
        s.about.driver = Some(Driver::Running("1.1.2.215".into()));
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
    }));
    frame(ui, "about-latest", &mut v, &snap(Scenario::Playing));
}

fn lab(ui: &mut Ui, _: &mut Stage) {
    let mut v = super::LabView::new(port(Names::Usual, |s| {
        s.lab.catch_up_frames = Some(8);
        s.lab.carried_over =
            Some(CarriedOver { airplay_ms: Some(500), homepod_ms: None, homepod_buffered: false, speakers: vec!["Office".into(), "Garage".into()] });
    }))
    .with_counters();
    frame(ui, "lab", &mut v, &snap(Scenario::Playing));
}

fn guides(ui: &mut Ui, _: &mut Stage) {
    let mut v = GuidesView::new(port(Names::Usual, |_| ()));
    frame(ui, "guides", &mut v, &snap(Scenario::Playing));
}
