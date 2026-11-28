//! *Can't hear it?*: the chain from the apps to the speaker's volume,
//! checked live, stopping at the first broken link with its one action.

use std::sync::Arc;

use egui::Ui;

use crate::model::command::{Command, VolumeOwner};
use crate::model::panel::Sign;
use crate::model::Snapshot;
use crate::model::state::SpeakerId;
use crate::model::situation::Standing;
use crate::shell::{Ask, ViewBar, ViewContent};
use crate::ui::button::Button;
use crate::ui::text::{Para, Tone};
use crate::ui::theme::size;

use super::parts::{buttons, gap, sentence};
use crate::settings::port::*;
use crate::settings::widgets::{check_line, fitting, Found};
use crate::settings::words::w;
use crate::settings::SettingsPort;
use crate::settings::{Dialog, ReportDialog};

pub struct CantHearView {
    port: Arc<dyn SettingsPort>,
    target: Option<SpeakerId>,
}

impl CantHearView {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        Self::for_speaker(port, None)
    }

    pub fn for_speaker(port: Arc<dyn SettingsPort>, target: Option<SpeakerId>) -> Self {
        CantHearView { port, target }
    }
}

/// Whose level the place's is: a speaker's own, or the level this PC sends
/// to Music Assistant.
fn volume_owner(id: &SpeakerId) -> VolumeOwner {
    if id.is_music_assistant() {
        VolumeOwner::MusicAssistant
    } else {
        VolumeOwner::Speaker(id.clone())
    }
}

impl ViewContent for CantHearView {
    fn bar(&self, _snap: &Snapshot) -> ViewBar {
        ViewBar { title: w!(self.port.lang(), CantHear), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 8.0;
        if let Some(crate::model::situation::FirstRunStep::Two { speaker, level: Some(level), limit }) = &snap.scene.situation.first_run {
            if self.target.as_ref() == Some(speaker) {
                // The test session reached this speaker. The live playback chain
                // belongs to the current place and cannot diagnose this chime.
                let name = snap.scene.speaker_name(speaker);
                check_line(ui, Found::Good, &w!(lang, FirstChimeSent, speaker = &name), None, |_| ());
                super::first_run::chime_volume(ui, snap, &name, *level, *limit, ask);
                sentence(ui, &w!(lang, SetCheckCantHearRoom, speaker = &name));
                if Button::standard(&w!(lang, PlayTestChime)).show(ui).clicked() {
                    ask(Ask::Command(Command::FirstRunAction(crate::model::command::FirstRunAction::ChimeAgain)));
                }
                return;
            }
        }
        let c = self.port.playback_check();
        // The speaker the sound is on, for the actions that go to it.
        let id: Option<SpeakerId> = snap.scene.situation.place_speaker().cloned();
        // A test chime is offered where the place can play one.
        let chimes = id.as_ref().filter(|id| snap.scene.situation.speaker(id).is_some_and(|s| s.can_chime));
        let Some(speaker) = c.speaker.clone() else {
            check_line(ui, Found::Info, &w!(lang, SetCheckOnPc), Some(&w!(lang, SetCheckOnPcLineView)), |_| ());
            return;
        };
        sentence(ui, &w!(lang, SetCheckLead));
        let mut all_good = true;
        let send = |ask: &mut dyn FnMut(Ask), c: Command| ask(Ask::Command(c));
        // 1. Something plays.
        if let Some(r) = &c.playing {
            match r {
                Ok(app) => {
                    let detail = app.as_ref().map(|a| w!(lang, SetCheckAppPlays, app = a));
                    check_line(ui, Found::Good, &w!(lang, SetCheckPlaying), detail.as_deref(), |_| ());
                }
                Err(_) => {
                    all_good = false;
                    let quiet = if snap.scene.situation.only_some_apps { w!(lang, NothingPlaying) } else { w!(lang, ReadyLine) };
                    let detail = snap.scene.situation.standing.iter().any(|s| matches!(s, Standing::OnlySomeApps { .. }))
                        .then(|| w!(lang, OnlyChosenApps));
                    check_line(ui, Found::Bad, &quiet, detail.as_deref(), |ui| {
                        if let Some(id) = chimes {
                            if Button::standard(&w!(lang, PlayTestChime)).show(ui).clicked() {
                                send(ask, Command::PlayTestSound(id.clone()));
                            }
                        }
                    });
                }
            }
        }
        // 2. Windows sends it here.
        if let Some(r) = &c.windows {
            match r {
                // An app on another output. While the rest goes to the
                // speaker it is a remark, and the check goes on.
                Ok(WindowsSends::AppElsewhere { app, output }) | Err(WindowsSends::AppElsewhere { app, output }) => {
                    all_good = false;
                    // The remark says where the rest goes, so the speaker
                    // isn't taken for silent.
                    let (found, title) = if r.is_ok() {
                        (Found::Warning, w!(lang, SetCheckAppElsewhereRest, app = app, output = output, speaker = &speaker))
                    } else {
                        (Found::Bad, w!(lang, SetCheckAppElsewhere, app = app, output = output, speaker = &speaker))
                    };
                    check_line(ui, found, &title, Some(&w!(lang, SetCheckRestartApp, app = app)), |ui| {
                        // Windows' page of apps and their outputs: an app
                        // that was given an output of its own is set back
                        // there.
                        if Button::standard(&w!(lang, SetCheckSetOutput, app = app)).max_width(ui.max_rect().width()).show(ui).clicked() {
                            ask(Ask::Open(crate::model::command::Open::VolumeMixer));
                        }
                    });
                }
                Ok(_) => check_line(ui, Found::Good, &w!(lang, SetCheckWindows), None, |_| ()),
                Err(WindowsSends::InputLost) => {
                    all_good = false;
                    // The way to hear the sound meanwhile: what plays on
                    // this PC's own output doesn't pass through the app.
                    check_line(ui, Found::Bad, &w!(lang, SetCheckInputLost), Some(&w!(lang, SetCheckInputLostLine, speaker = &speaker)), |ui| {
                        if Button::standard(&w!(lang, SwitchToPc)).show(ui).clicked() {
                            send(ask, Command::Switch { place: crate::model::state::Place::ThisPc, way: None });
                        }
                    });
                }
                Err(WindowsSends::Elsewhere { output }) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckWindowsElsewhere, output = output), None, |ui| {
                        let long = w!(lang, SetSendTo, speaker = &speaker);
                        let short = w!(lang, SetSendToThis);
                        if Button::standard(fitting(ui, &long, &short)).accessible_name(long.clone()).show(ui).clicked() {
                            self.port.act(Act::SendToUs);
                        }
                    });
                }
                Err(WindowsSends::ToUs) => {}
            }
        }
        // 3. Connected.
        if let Some(r) = &c.connected {
            match r {
                Ok(()) => check_line(ui, Found::Good, &w!(lang, SetCheckConnected, speaker = &speaker), None, |_| ()),
                Err(()) => {
                    all_good = false;
                    // While the app connects or tries again by itself, the
                    // step says so, with the panel's line about a try that
                    // follows trouble, and offers nothing: the app is at it
                    // already.
                    let state = &snap.panel.place.state;
                    match state.sign {
                        Sign::Spinner => check_line(ui, Found::Info, &w!(lang, SetCheckConnecting, speaker = &speaker), None, |_| ()),
                        Sign::SpinnerWarn => check_line(ui, Found::Warning, &w!(lang, SetCheckConnecting, speaker = &speaker), Some(&state.text), |_| ()),
                        // Music Assistant's server connects to the app:
                        // a Connect button would do nothing, and its page
                        // says how it connects.
                        _ if c.server_away => check_line(ui, Found::Bad, &w!(lang, SetCheckMaNotConnected), Some(&w!(lang, MaNotConnectedLine)), |ui| {
                            if Button::standard(&w!(lang, HowToConnect)).show(ui).clicked() {
                                ask(Ask::Open(crate::model::command::Open::MusicAssistantSettings));
                            }
                        }),
                        _ => check_line(ui, Found::Bad, &w!(lang, SetCheckNotConnected), None, |ui| {
                            if Button::standard(&w!(lang, SetConnect)).show(ui).clicked() {
                                self.port.act(Act::Reconnect);
                            }
                        }),
                    }
                }
            }
        }
        // 4. Taking the sound.
        if let Some(r) = &c.taking {
            let reconnect = |ui: &mut Ui, ask: &mut dyn FnMut(Ask)| {
                if Button::standard(&w!(lang, TrySpeakerAgain, speaker = &speaker)).max_width(ui.max_rect().width()).show(ui).clicked() {
                    match &id {
                        Some(id) => ask(Ask::Command(Command::Reconnect(id.clone()))),
                        None => self.port.act(Act::Reconnect),
                    }
                }
            };
            match r {
                Ok(Taking::CantTell) | Err(Taking::CantTell) => {
                    check_line(ui, Found::Unknown, &w!(lang, SetCheckCantTell, speaker = &speaker), Some(&w!(lang, SetCheckCantTellLine)), |ui| {
                        buttons(ui, |ui| {
                            reconnect(ui, ask);
                            if c.on_short && Button::standard(&w!(lang, SetTrySteady)).show(ui).clicked() {
                                if let Some(id) = &id {
                                    send(ask, Command::UseDelayChoice { speaker: id.clone(), choice: crate::model::facts::DelayChoice::Steady });
                                }
                            }
                        });
                    });
                }
                Ok(_) => check_line(ui, Found::Good, &w!(lang, SetCheckTaking, speaker = &speaker), None, |_| ()),
                Err(Taking::NotAsking { .. }) if id.as_ref().is_some_and(SpeakerId::is_music_assistant) => {
                    // Music Assistant's server is connected and hasn't
                    // started this PC's sound: the panel's note, and its
                    // way out.
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckMaNotStarted), Some(&w!(lang, MaStartPcSound)), |ui| {
                        if Button::standard(&w!(lang, HowToStart)).show(ui).clicked() {
                            ask(Ask::Open(crate::model::command::Open::MusicAssistantHowTo));
                        }
                    });
                }
                Err(Taking::NotAsking { two_ways }) => {
                    all_good = false;
                    // The same speaker by AirPlay: the choice the panel's
                    // own ways make.
                    let airplay = id.as_ref().and_then(|id| Some((id, snap.scene.speaker(id)?.airplay_way()?)));
                    check_line(ui, Found::Bad, &w!(lang, SetCheckNotAsking, speaker = &speaker), None, |ui| {
                        buttons(ui, |ui| {
                            reconnect(ui, ask);
                            if *two_ways && Button::standard(&w!(lang, SwitchByWay, speaker = &speaker, way = &w!(lang, KindAirPlay)))
                                .max_width(ui.max_rect().width()).show(ui).clicked() {
                                match airplay {
                                    Some((id, way)) => send(ask, Command::ChooseWay { speaker: id.clone(), way }),
                                    None => self.port.act(Act::TryAirPlay),
                                }
                            }
                        });
                    });
                }
                Err(Taking::Refused { device, address }) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckRefused, device = device, speaker = &speaker), None, |ui| {
                        if Button::standard(&w!(lang, SetAllowIt)).show(ui).clicked() {
                            self.port.act(Act::Allow { address: address.clone(), speaker: speaker.clone() });
                        }
                    });
                }
                Err(Taking::Yes) => {}
            }
        }
        // 5. Its volume.
        if let Some(r) = &c.volume {
            match r {
                Ok(Volume::Level(n)) => {
                    check_line(ui, Found::Good, &w!(lang, SetCheckVolume, speaker = &speaker), Some(&w!(lang, SetCheckVolumeLevel, level = *n)), |_| ())
                }
                Ok(_) => check_line(ui, Found::Good, &w!(lang, SetCheckVolume, speaker = &speaker), None, |_| ()),
                Err(Volume::Muted) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckMuted, speaker = &speaker), None, |ui| {
                        if Button::standard(&w!(lang, Unmute)).show(ui).clicked() {
                            match &id {
                                Some(id) => send(ask, Command::SetMuted { owner: volume_owner(id), muted: false }),
                                None => self.port.act(Act::Unmute),
                            }
                        }
                    });
                }
                Err(_) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckZero, speaker = &speaker), None, |ui| {
                        if Button::standard(&w!(lang, SetSetTo, level = 20u8)).show(ui).clicked() {
                            match &id {
                                Some(id) => send(ask, Command::SetVolume { owner: volume_owner(id), level: 20 }),
                                None => self.port.act(Act::SetVolume(20)),
                            }
                        }
                    });
                }
            }
        }
        if all_good && c.volume.is_some() {
            gap(ui);
            let mut text = w!(lang, SetCheckCantHearRoom, speaker = &speaker);
            if c.untested {
                text = format!("{text} {}", w!(lang, SetCheckUntested));
            }
            if c.apple_tv {
                text = format!("{text} {}", w!(lang, SetCheckAppleTv));
            }
            let _ = Para::new(&text).lead(&w!(lang, SetCheckAllGood)).size(size::SECONDARY).tone(Tone::Primary).show(ui);
            buttons(ui, |ui| {
                if let Some(id) = chimes {
                    if Button::standard(&w!(lang, PlayTestChime)).show(ui).clicked() {
                        send(ask, Command::PlayTestSound(id.clone()));
                    }
                }
                if Button::standard(&w!(lang, SetWriteReport)).show(ui).clicked() {
                    super::open_dialog(&self.port, Dialog::Report(ReportDialog::default()), ask);
                }
            });
        }
        gap(ui);
        // That the list stops at a broken step is said only when it did:
        // not at a step the app is still working on, nor after a remark.
        let connecting = matches!(snap.panel.place.state.sign, Sign::Spinner | Sign::SpinnerWarn);
        let broken = matches!(c.playing, Some(Err(_)))
            || matches!(c.windows, Some(Err(WindowsSends::AppElsewhere { .. } | WindowsSends::InputLost | WindowsSends::Elsewhere { .. })))
            || (matches!(c.connected, Some(Err(()))) && !connecting)
            || matches!(c.taking, Some(Err(Taking::NotAsking { .. } | Taking::Refused { .. })))
            || matches!(c.volume, Some(Err(_)));
        let live = w!(lang, SetCheckLive);
        sentence(ui, &if broken { format!("{live} {}", w!(lang, SetCheckStops)) } else { live });
    }
}
