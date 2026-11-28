//! Pictures of the panel's views on the scripted app: every view in the
//! states that matter, framed as the panel frames them.

use std::sync::Arc;

use egui::{Id, Margin, Ui};

use crate::model::port::Port;
use crate::model::fake::ScriptedEngine;
use crate::model::Snapshot;
use crate::model::text::LocalTime;
use crate::shell::page::{show_view, ViewState};
use crate::shell::ViewContent;
use crate::ui::preview::{Scene, Stage};
use crate::ui::theme::Theme;

use super::*;
use crate::settings::port::*;
use crate::settings::scripted::{Names, ScriptedSettings, State};

const MARGIN: f32 = 20.0;
const WIDTH: f32 = crate::surface::panel::WIDTH;

/// Every scene of the views.
pub fn scenes() -> Vec<Scene> {
    let s = |name, h: f32, draw| Scene { name, size: [WIDTH + 2.0 * MARGIN, h], draw };
    vec![
        s("view-speaker-sonos-playing", 1180.0, speaker_kitchen),
        s("view-speaker-office", 1220.0, speaker_office),
        s("view-speaker-office-sonos", 1220.0, speaker_office_short),
        s("view-speaker-needs-code", 1080.0, speaker_needs_code),
        s("view-speaker-needs-password", 1220.0, speaker_needs_password),
        s("view-speaker-not-found", 1120.0, speaker_garage),
        s("view-speaker-homepod-pair", 1220.0, speaker_bedroom),
        s("view-speaker-fixed-delay", 1180.0, speaker_den),
        s("view-speaker-sendspin", 1100.0, speaker_desk),
        s("view-speaker-all-bands", 1700.0, speaker_bands),
        s("view-speaker-details", 1500.0, speaker_details),
        s("view-speaker-amount", 1180.0, speaker_exact_delay),
        s("view-speaker-busy-chime", 1320.0, speaker_busy),
        s("view-speaker-long-name", 1300.0, speaker_long),
        s("view-speaker-chinese-name", 1300.0, speaker_chinese),
        s("view-settings-all-off", 1040.0, settings_start),
        s("view-settings-all-on", 1590.0, settings_open),
        s("view-speaker-refused", 1380.0, speaker_kitchen),
        s("view-settings-update-ready", 1080.0, settings_update),
        s("view-settings-shortcut-taken", 1080.0, settings_taken),
        s("apps-settings", 0.0, apps_settings),
        s("apps-settings-rejected-keys", 0.0, apps_rejected_keys),
        s("apps-settings-shift", 0.0, apps_shift),
        s("apps-speaker-delay-key", 0.0, apps_speaker_delay_key),
        s("apps-settings-screen-reader", 0.0, apps_screen_reader),
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
        s("view-cant-hear-connecting", 640.0, cant_hear_connecting),
        s("view-cant-hear-trying", 640.0, cant_hear_trying),
        s("view-cant-hear-not-asking", 760.0, cant_hear_not_asking),
        s("view-cant-hear-windows-elsewhere", 640.0, cant_hear_windows),
        s("view-cant-hear-app-elsewhere", 640.0, cant_hear_app),
        s("view-cant-hear-app-elsewhere-playing", 900.0, cant_hear_app_while_playing),
        s("view-cant-hear-input-lost", 560.0, cant_hear_input_lost),
        s("view-cant-hear-music-assistant", 700.0, cant_hear_music_assistant),
        s("view-cant-hear-music-assistant-not-connected", 640.0, cant_hear_music_assistant_away),
        s("view-cant-hear-refused", 760.0, cant_hear_refused),
        s("view-cant-hear-paused", 760.0, cant_hear_paused),
        s("view-cant-hear-volume-zero", 760.0, cant_hear_zero),
        s("view-cant-hear-on-this-pc", 360.0, cant_hear_pc),
        s("view-music-assistant-off", 520.0, ma_off),
        s("view-music-assistant-setting-up", 1120.0, ma_setting_up),
        s("view-music-assistant-done", 1100.0, ma_done),
        s("view-music-assistant-not-connected", 1100.0, ma_not_connected),
        s("view-about", 880.0, about),
        s("view-about-up-to-date", 760.0, about_latest),
        s("view-lab", 1300.0, lab),
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

fn snap(s: &str) -> Arc<Snapshot> {
    ScriptedEngine::new(s).snapshot()
}

fn speaker_view(
    ui: &mut Ui,
    key: &str,
    scenario: &str,
    names: Names,
    id: &str,
    f: impl FnOnce(&mut State),
    tune: impl FnOnce(SpeakerOptions) -> SpeakerOptions,
) {
    let p = port(names, f);
    let mut scene = crate::model::samples::named(scenario);
    let page = p.speaker(id).unwrap();
    let sid = crate::model::SpeakerId::new(id);
    use crate::model::{samples::{seen, way}, situation::{Confirm, WayChoice}};
    if scene.speaker(&sid).is_none() { scene.speakers.push(seen(sid.clone(), &page.row.name)); }
    let speaker = scene.speakers.iter_mut().find(|s| s.id == sid).unwrap();
    speaker.name = page.row.name.clone();
    if id == "office" {
        speaker.ways = vec![way(WayChoice::Native, 150), way(WayChoice::AirPlaySteady, 2000)];
        speaker.way = if key == "office-sonos" { WayChoice::Native } else { WayChoice::AirPlaySteady };
    }
    match id {
        "desk" => {
            speaker.kind = crate::model::facts::SpeakerKind::Sendspin;
            speaker.ways = vec![way(WayChoice::Native, 500)];
        }
        "garage" => speaker.ways.clear(),
        "bedroom" | "den" => {
            use crate::model::facts::{Applies, SpeakerKind};
            speaker.kind = if id == "bedroom" { SpeakerKind::HomePodPair } else { SpeakerKind::AppleTv };
            let mut steady = way(WayChoice::AirPlaySteady, 2000);
            steady.applies = Applies::Fixed;
            let mut short = way(WayChoice::AirPlayShort, 200);
            short.applies = Applies::Reconnect;
            speaker.way = if id == "bedroom" { short.way } else { steady.way };
            speaker.ways = vec![short, steady];
        }
        _ => {}
    }
    let mut v = tune(SpeakerOptions::new(p, id));
    if key == "exact-delay" {
        let way = speaker.ways.iter_mut().find(|w| w.way == speaker.way).unwrap();
        way.delay.own_ms = 200;
        way.delay.change_ms = -50;
        v.open_exact_delay(way);
    }
    if key == "busy" {
        scene.situation.confirm_in_panel = Some(Confirm::ChimeBusy { speaker: sid, who: None });
    }
    let snapshot = crate::model::view::view(scene, None, false, 1);
    frame(ui, key, &mut v, &snapshot);
}

fn speaker_kitchen(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "kitchen", "kitchen-playing", Names::Usual, "kitchen", |_| (), |v| v);
}

fn speaker_office(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "office", "kitchen-playing", Names::Usual, "office", |s| s.general.shortcuts.as_mut().unwrap().on = false, |v| v);
}

fn speaker_office_short(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "office-sonos", "kitchen-playing", Names::Usual, "office", |s| s.general.shortcuts.as_mut().unwrap().on = false, |v| v);
}

fn speaker_needs_code(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "den-code", "pc-den-needs-code", Names::Usual, "den", |_| (), |v| v);
}

fn speaker_needs_password(ui: &mut Ui, _: &mut Stage) {
    let engine = ScriptedEngine::new("pc-office-needs-password");
    let mut v = SpeakerOptions::new(port(Names::Usual, |_| ()), "office");
    frame(ui, "office-password", &mut v, &engine.snapshot());
}

fn speaker_garage(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "garage", "kitchen-playing", Names::Usual, "garage", |_| (), |v| v);
}

fn speaker_bedroom(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "bedroom", "kitchen-playing", Names::Usual, "bedroom", |_| (), |v| v);
}

fn speaker_den(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "den", "kitchen-playing", Names::Usual, "den", |_| (), |v| v);
}

fn speaker_desk(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "desk", "kitchen-playing", Names::Usual, "desk", |_| (), |v| v);
}

fn speaker_bands(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "bands", "kitchen-playing", Names::Usual, "kitchen", |_| (), |v| v.with_open("bands"));
}

fn speaker_details(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "details", "kitchen-playing", Names::Usual, "office", |_| (), |v| v.with_open("details"));
}

fn speaker_exact_delay(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "exact-delay", "kitchen-playing", Names::Usual, "kitchen", |_| (), |v| v);
}

fn speaker_busy(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "busy", "pc-kitchen-playing-something-else", Names::Usual, "kitchen", |_| (), |v| v);
}

fn speaker_long(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "long", "kitchen-playing", Names::Long, "kitchen", |_| (), |v| v);
}

fn speaker_chinese(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "chinese", "kitchen-playing", Names::Chinese, "kitchen", |_| (), |v| v);
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
    s.general.start_at_sign_in = Some(Ok(false));
    s.general.notes_by_clock = false;
    s.general.language = None;
}

fn settings_view(ui: &mut Ui, key: &str, f: impl FnOnce(&mut State)) {
    let mut v = SettingsView::new(port(Names::Usual, f));
    frame(ui, key, &mut v, &snap("kitchen-playing"));
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
        s.general.switch_back_after_quiet = true;
        s.general.play_calls = Some(true);
        s.general.language = None;
        s.privacy.only_playing = false;
        if let Some(a) = &mut s.privacy.api {
            a.on = true;
        }
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
            k.status = [crate::platform::KeyStatus::Registered, crate::platform::KeyStatus::Taken, crate::platform::KeyStatus::Registered];
        }
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
    });
}

fn apps_settings(ui: &mut Ui, _: &mut Stage) {
    let mut view = SettingsView::new(port(Names::Usual, |s| {
        starting(s);
        s.general.what_plays = Some(WhatPlays::OnlySentApps);
        s.general.play_calls = Some(true);
        let keys = s.general.shortcuts.as_mut().unwrap();
        keys.on = true;
        keys.status = [crate::platform::KeyStatus::Registered, crate::platform::KeyStatus::Taken, crate::platform::KeyStatus::Registered];
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
    }));
    frame(ui, "apps-settings", &mut view, &snap("apps-panel-playing"));
}

fn apps_rejected_keys(ui: &mut Ui, _: &mut Stage) {
    let mut view = SettingsView::new(port(Names::Usual, |s| {
        starting(s);
        s.general.what_plays = Some(WhatPlays::OnlySentApps);
        s.general.shortcuts.as_mut().unwrap().on = true;
    }));
    view.start_recording();
    for _ in 0..2 { view.record_press(egui::Key::Minus, egui::Modifiers::CTRL); }
    frame(ui, "apps-settings-rejected-keys", &mut view, &snap("apps-panel-playing"));
}

fn apps_shift(ui: &mut Ui, _: &mut Stage) {
    let mut view = SettingsView::new(port(Names::Usual, |s| {
        starting(s);
        s.general.what_plays = Some(WhatPlays::OnlySentApps);
        s.general.shortcuts.as_mut().unwrap().on = true;
    }));
    view.start_recording();
    view.record_press(egui::Key::F9, egui::Modifiers::CTRL | egui::Modifiers::SHIFT);
    frame(ui, "apps-settings-shift", &mut view, &snap("apps-panel-playing"));
}

fn apps_speaker_delay_key(ui: &mut Ui, _: &mut Stage) {
    speaker_view(ui, "apps-speaker-delay-key", "apps-panel-playing", Names::Usual, "kitchen", |s| {
        s.general.shortcuts.as_mut().unwrap().status[1] = crate::platform::KeyStatus::Taken;
    }, |v| v);
}

fn apps_screen_reader(ui: &mut Ui, _: &mut Stage) {
    use crate::model::situation::{Confirm, WayChoice};
    let mut view = SettingsView::new(port(Names::Usual, starting));
    let mut scene = snap("kitchen-playing").scene.clone();
    scene.situation.confirm_in_panel = Some(Confirm::ScreenReader {
        speaker: crate::model::SpeakerId::new("kitchen"), way: WayChoice::Native,
    });
    scene.situation.confirm_under_routing = Some(crate::model::situation::RoutingQuestion::TakePlayback);
    frame(ui, "apps-settings-screen-reader", &mut view, &crate::model::view::view(scene, None, false, 1));
}

fn missing_view(ui: &mut Ui, key: &str, f: impl FnOnce(&mut State), tune: impl FnOnce(MissingView) -> MissingView) {
    let scripted = Arc::new(ScriptedSettings::with_names(Names::Usual));
    let p: Arc<dyn SettingsPort> = scripted.clone();
    let mut v = tune(MissingView::new(p));
    // The view starts a new search for an address when it opens: the
    // state to show is set after.
    scripted.set(f);
    frame(ui, key, &mut v, &snap("kitchen-playing"));
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
    frame(ui, "check", &mut v, &snap("kitchen-playing"));
}

fn cant_hear(ui: &mut Ui, key: &str, scenario: &str, f: impl FnOnce(&mut State)) {
    let mut v = CantHearView::new(port(Names::Usual, f));
    frame(ui, key, &mut v, &snap(scenario));
}

fn cant_hear_good(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-good", "kitchen-playing", |_| ());
}

fn cant_hear_cant_tell(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-cant-tell", "kitchen-playing", |s| {
        s.playback.speaker = Some("Bedroom".into());
        s.playback.on_short = true;
        s.playback.taking = Some(Ok(Taking::CantTell));
        s.playback.volume = Some(Ok(Volume::Level(18)));
        s.playback.untested = true;
    });
}

fn cant_hear_muted(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-muted", "kitchen-playing", |s| s.playback.volume = Some(Err(Volume::Muted)));
}

fn cant_hear_nothing(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-nothing", "kitchen-playing", |s| {
        s.playback.playing = Some(Err(None));
        s.playback.windows = None;
        s.playback.connected = None;
        s.playback.taking = None;
        s.playback.volume = None;
    });
}

fn cant_hear_not_connected(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-not-connected", "kitchen-playing", not_connected);
}

fn cant_hear_connecting(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-connecting", "kitchen-connecting", not_connected);
}

fn cant_hear_trying(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-trying", "kitchen-not-responding-trying", not_connected);
}

/// The check as the app reports it while no speaker is connected.
fn not_connected(s: &mut State) {
    s.playback.connected = Some(Err(()));
    s.playback.taking = None;
    s.playback.volume = None;
}

fn cant_hear_not_asking(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-not-asking", "kitchen-playing", |s| {
        s.playback.taking = Some(Err(Taking::NotAsking { two_ways: true }));
        s.playback.volume = None;
    });
}

fn cant_hear_app(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-app", "kitchen-playing", |s| {
        s.playback.playing = Some(Ok(Some("Chrome".into())));
        s.playback.windows = Some(Err(WindowsSends::AppElsewhere { app: "Chrome".into(), output: "Speakers (Realtek Audio)".into() }));
        s.playback.connected = None;
        s.playback.taking = None;
        s.playback.volume = None;
    });
}

fn cant_hear_app_while_playing(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-app-playing", "kitchen-except-an-app", |s| {
        s.playback.windows = Some(Ok(WindowsSends::AppElsewhere { app: "Chrome".into(), output: "Speakers (Realtek Audio)".into() }));
    });
}

fn cant_hear_input_lost(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-input-lost", "kitchen-playing", |s| {
        s.playback.windows = Some(Err(WindowsSends::InputLost));
        s.playback.connected = None;
        s.playback.taking = None;
        s.playback.volume = None;
    });
}

fn cant_hear_music_assistant_away(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-music-assistant-away", "music-assistant-not-connected", |s| {
        s.playback.speaker = Some("Music Assistant".into());
        s.playback.server_away = true;
        not_connected(s);
    });
}

fn cant_hear_music_assistant(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-music-assistant", "music-assistant-playing", |s| {
        s.playback.speaker = Some("Music Assistant".into());
        s.playback.taking = Some(Err(Taking::NotAsking { two_ways: false }));
        s.playback.volume = None;
    });
}

fn cant_hear_refused(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-refused", "kitchen-playing", |s| {
        s.playback.taking = Some(Err(Taking::Refused { device: "Kitchen Sub".into(), address: "192.168.1.51".into() }));
        s.playback.volume = None;
    });
}

fn cant_hear_paused(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-paused", "kitchen-playing", |s| {
        s.playback.taking = Some(Err(Taking::Paused));
        s.playback.volume = None;
    });
}

fn cant_hear_zero(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-zero", "kitchen-playing", |s| s.playback.volume = Some(Err(Volume::Zero)));
}

fn cant_hear_windows(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-windows", "kitchen-playing", |s| {
        s.playback.windows = Some(Err(WindowsSends::Elsewhere { output: "Headphones (WH-1000XM4)".into() }));
        s.playback.connected = None;
        s.playback.taking = None;
        s.playback.volume = None;
    });
}

fn cant_hear_pc(ui: &mut Ui, _: &mut Stage) {
    cant_hear(ui, "cant-hear-pc", "pc-plain", |s| s.playback.speaker = None);
}

fn ma(ui: &mut Ui, key: &str, f: impl FnOnce(&mut State)) {
    let mut v = MusicAssistantView::new(port(Names::Usual, f));
    frame(ui, key, &mut v, &snap("kitchen-playing"));
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
        s.ma.played = Some(None);
    });
}

/// Paired earlier; Music Assistant is off, or this PC is on another network.
fn ma_not_connected(ui: &mut Ui, _: &mut Stage) {
    ma(ui, "ma-not-connected", |s| {
        s.ma.found_by = None;
        s.ma.code = None;
        s.ma.paired_with = vec!["Music Assistant (Home)".into()];
    });
}

fn about(ui: &mut Ui, _: &mut Stage) {
    let mut v = super::AboutView::new(port(Names::Usual, |_| ()));
    frame(ui, "about", &mut v, &snap("kitchen-playing"));
}

fn about_latest(ui: &mut Ui, _: &mut Stage) {
    let mut v = super::AboutView::new(port(Names::Usual, |s| {
        s.about.show_lab = true;
        s.about.updates.state = UpdateState::UpToDate { at: Seen::Today(LocalTime::new(9, 12)) };
    }));
    frame(ui, "about-latest", &mut v, &snap("kitchen-playing"));
}

fn lab(ui: &mut Ui, _: &mut Stage) {
    let mut v = super::LabView::new(port(Names::Usual, |s| {
        s.lab.catch_up_frames = Some(8);
        s.lab.carried_over =
            Some(CarriedOver { airplay_ms: Some(500), homepod_ms: None, homepod_buffered: false, speakers: vec!["Office".into(), "Garage".into()] });
    }))
    .with_counters();
    frame(ui, "lab", &mut v, &snap("kitchen-playing"));
}
