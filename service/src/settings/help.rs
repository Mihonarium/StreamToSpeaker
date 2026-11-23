//! Settings → Help: the two checks, the report, the logs, the offline
//! guide.

use egui::Ui;

use crate::ui::button::Button;
use crate::ui::card::SettingRow;
use crate::ui::icon::Icon;
use crate::ui::theme::{size, Theme};

use super::dialogs::{AddDialog, Dialog, ReportDialog};
use super::port::{Act, FindLine, PlaybackCheck, Taking, Volume, WindowsSends};
use super::speakers::place_words;
use super::widgets::{body, boxed, buttons, check_line, crumb, divider, link, note, page_title, strong, Found};
use super::words::w;
use super::{Cx, Page, Request};

pub(super) fn page(ui: &mut Ui, cx: &mut Cx) {
    let lang = cx.lang;
    page_title(ui, &w!(lang, SetNavHelp));
    let r = SettingRow::new(&w!(lang, CantHear)).icon(Icon::Volume).description(&w!(lang, SetCantHearLine)).show(ui, |ui| {
        Button::primary(&w!(lang, SetCheckNow)).accessible_name(w!(lang, SetCheckPlayback)).show(ui).clicked()
    });
    if r.inner {
        cx.go(Page::CantHear);
    }
    let r = SettingRow::new(&w!(lang, SetCantFindLink)).icon(Icon::Search).description(&w!(lang, SetCantFindLine)).show(ui, |ui| {
        Button::standard(&w!(lang, SetCheckNow)).accessible_name(w!(lang, SetCheckFinding)).show(ui).clicked()
    });
    if r.inner {
        cx.go(Page::CantFind);
    }
    let r = SettingRow::new(&w!(lang, SetReportProblem))
        .icon(Icon::Bell)
        .description(&w!(lang, SetReportLine))
        .show(ui, |ui| Button::standard(&w!(lang, SetWriteReport)).show(ui).clicked());
    if r.inner {
        cx.dialog(Dialog::Report(ReportDialog::default()));
    }
    let r = SettingRow::new(&w!(lang, SetLogs))
        .icon(Icon::List)
        .description(&w!(lang, SetLogsLine))
        .show(ui, |ui| Button::standard(&w!(lang, SetOpenLogs)).icon(Icon::External).show(ui).clicked());
    if r.inner {
        cx.act(Act::OpenLogFolder);
    }
    let r = SettingRow::new(&w!(lang, SetHowItWorks))
        .icon(Icon::Info)
        .description(&w!(lang, SetHowItWorksLine))
        .opens_page()
        .show(ui, |ui| Button::standard(&w!(lang, SetReadOnline)).icon(Icon::External).show(ui).clicked());
    if r.inner {
        cx.act(Act::ReadGuideOnline);
    } else if r.opened {
        cx.go(Page::Guide(1));
    }
    ui.add_space(10.0);
    if link(ui, &w!(lang, SetShowWelcome)) {
        cx.act(Act::ShowWelcomeAgain);
        cx.requests.push(Request::OpenPanel);
    }
}

/// *How it works*: two screens of the guide.
pub(super) fn guide(ui: &mut Ui, cx: &mut Cx, screen: u8) {
    let lang = cx.lang;
    if crumb(ui, &w!(lang, SetNavHelp), &w!(lang, SetHowItWorks)) {
        cx.go(Page::Help);
    }
    let sections: Vec<(String, String)> = if screen <= 1 {
        vec![(w!(lang, SetGuideWhereTitle), w!(lang, SetGuideWhere)), (w!(lang, SetGuideDelayTitle), w!(lang, SetGuideDelay))]
    } else {
        vec![
            (w!(lang, SetGuideOffTitle), w!(lang, SetGuideOff)),
            (w!(lang, SetGuideRoomsTitle), w!(lang, SetGuideRooms)),
            (w!(lang, SetGuideBackTitle), w!(lang, SetGuideBack)),
            (w!(lang, SetGuideAirPlayTitle), w!(lang, SetGuideAirPlay)),
            (w!(lang, SetGuideMaTitle), w!(lang, SetGuideMa)),
        ]
    };
    let t = Theme::of(ui.ctx());
    for (title, text) in &sections {
        boxed(ui, |ui| {
            strong(ui, title, size::BODY + 1.0);
            ui.add(egui::Label::new(egui::RichText::new(text).font(t.font(size::BODY)).color(t.c.text)).wrap());
        });
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(w!(lang, SetGuidePage, n = screen.clamp(1, 2), of = 2u8)).font(t.font(size::CAPTION)).color(t.c.text_secondary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if screen <= 1 {
                if Button::standard(&w!(lang, SetNext)).show(ui).clicked() {
                    cx.go(Page::Guide(2));
                }
            } else if Button::standard(&w!(lang, SetBackButton)).show(ui).clicked() {
                cx.go(Page::Guide(1));
            }
        });
    });
}

/// *Can't hear it?*: the chain from the apps to the speaker's volume,
/// stopping at the first broken link with its one action.
pub(super) fn cant_hear(ui: &mut Ui, cx: &mut Cx) {
    let lang = cx.lang;
    if crumb(ui, &w!(lang, SetNavHelp), &w!(lang, CantHear)) {
        cx.go(Page::Help);
    }
    let c = cx.port.playback_check();
    strong(ui, &w!(lang, SetCheckLead), size::TITLE);
    ui.add_space(4.0);
    let Some(speaker) = c.speaker.clone() else {
        boxed(ui, |ui| {
            check_line(ui, Found::Info, &w!(lang, SetCheckOnPc), Some(&w!(lang, SetCheckOnPcLine)), |ui| {
                if Button::standard(&w!(lang, SetOpenPanel)).show(ui).clicked() {
                    cx.requests.push(Request::OpenPanel);
                }
            });
        });
        return;
    };
    let mut all_good = true;
    boxed(ui, |ui| {
        let mut first = true;
        let mut sep = |ui: &mut Ui| {
            if !first {
                divider(ui);
            }
            first = false;
        };
        // 1. Something plays.
        if let Some(r) = &c.playing {
            sep(ui);
            match r {
                Ok(app) => {
                    let detail = app.as_ref().map(|a| w!(lang, SetCheckAppPlays, app = a));
                    check_line(ui, Found::Good, &w!(lang, SetCheckPlaying), detail.as_deref(), |_| ());
                }
                Err(_) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, ReadyLine), None, |ui| {
                        if Button::standard(&w!(lang, SetTestSound)).show(ui).clicked() {
                            cx.act(Act::TestSound(String::new()));
                        }
                    });
                }
            }
        }
        // 2. Windows sends it here.
        if let Some(r) = &c.windows {
            sep(ui);
            match r {
                Ok(_) => check_line(ui, Found::Good, &w!(lang, SetCheckWindows), None, |_| ()),
                Err(WindowsSends::Elsewhere { output }) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckWindowsElsewhere, output = output), None, |ui| {
                        let long = w!(lang, SetSendTo, speaker = &speaker);
                        let short = w!(lang, SetSendToThis);
                        if Button::standard(super::widgets::fitting(ui, &long, &short)).show(ui).clicked() {
                            cx.act(Act::SendToUs);
                        }
                    });
                }
                Err(WindowsSends::AppElsewhere { app, output }) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckAppElsewhere, app = app, output = output), Some(&w!(lang, SetCheckRestartApp)), |_| ());
                }
                Err(WindowsSends::ToUs) => {}
            }
        }
        // 3. Connected.
        if let Some(r) = &c.connected {
            sep(ui);
            match r {
                Ok(()) => check_line(ui, Found::Good, &w!(lang, SetCheckConnected, speaker = &speaker), None, |_| ()),
                Err(()) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckNotConnected), None, |ui| {
                        if Button::standard(&w!(lang, SetConnect)).show(ui).clicked() {
                            cx.act(Act::Reconnect);
                        }
                    });
                }
            }
        }
        // 4. Taking the sound.
        if let Some(r) = &c.taking {
            sep(ui);
            match r {
                Ok(Taking::CantTell) | Err(Taking::CantTell) => {
                    check_line(ui, Found::Unknown, &w!(lang, SetCheckCantTell, speaker = &speaker), Some(&w!(lang, SetCheckCantTellLine)), |ui| {
                        buttons(ui, |ui| {
                            if Button::standard(&w!(lang, SetReconnect)).show(ui).clicked() {
                                cx.act(Act::Reconnect);
                            }
                            if c.on_short && Button::standard(&w!(lang, SetTrySteady)).show(ui).clicked() {
                                cx.act(Act::TrySteady);
                            }
                        });
                    });
                }
                Ok(_) => check_line(ui, Found::Good, &w!(lang, SetCheckTaking, speaker = &speaker), None, |_| ()),
                Err(Taking::NotAsking { two_ways }) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckNotAsking, speaker = &speaker), None, |ui| {
                        buttons(ui, |ui| {
                            if Button::standard(&w!(lang, SetReconnect)).show(ui).clicked() {
                                cx.act(Act::Reconnect);
                            }
                            if *two_ways && Button::standard(&w!(lang, SetTryAirPlay)).show(ui).clicked() {
                                cx.act(Act::TryAirPlay);
                            }
                        });
                    });
                }
                Err(Taking::Refused { device, address }) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckRefused, device = device, speaker = &speaker), None, |ui| {
                        if Button::standard(&w!(lang, SetAllowIt)).show(ui).clicked() {
                            cx.act(Act::Allow { address: address.clone(), speaker: speaker.clone() });
                        }
                    });
                }
                Err(Taking::Yes) => {}
            }
        }
        // 5. Its volume.
        if let Some(r) = &c.volume {
            sep(ui);
            match r {
                Ok(Volume::Level(n)) => check_line(ui, Found::Good, &w!(lang, SetCheckVolume, speaker = &speaker), Some(&w!(lang, SetCheckVolumeLevel, level = *n)), |_| ()),
                Ok(_) => check_line(ui, Found::Good, &w!(lang, SetCheckVolume, speaker = &speaker), None, |_| ()),
                Err(Volume::Muted) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckMuted, speaker = &speaker), None, |ui| {
                        if Button::standard(&w!(lang, Unmute)).show(ui).clicked() {
                            cx.act(Act::Unmute);
                        }
                    });
                }
                Err(_) => {
                    all_good = false;
                    check_line(ui, Found::Bad, &w!(lang, SetCheckZero, speaker = &speaker), None, |ui| {
                        if Button::standard(&w!(lang, SetSetTo, level = 20u8)).show(ui).clicked() {
                            cx.act(Act::SetVolume(20));
                        }
                    });
                }
            }
        }
        if all_good && c.volume.is_some() {
            divider(ui);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let mut text = w!(lang, SetCheckCantHearRoom, speaker = &speaker);
                if c.untested {
                    text = format!("{text} {}", w!(lang, SetCheckUntested));
                }
                if c.apple_tv {
                    text = format!("{text} {}", w!(lang, SetCheckAppleTv));
                }
                let _ = crate::ui::text::Para::new(&text).lead(&w!(lang, SetCheckAllGood)).size(size::SECONDARY).tone(crate::ui::text::Tone::Primary).show(ui);
            });
            buttons(ui, |ui| {
                let long = w!(lang, SetTestSoundOn, speaker = &speaker);
                let short = w!(lang, SetTestSound);
                if Button::standard(super::widgets::fitting(ui, &long, &short)).show(ui).clicked() {
                    cx.act(Act::TestSound(String::new()));
                }
                if Button::primary(&w!(lang, SetWriteReport)).show(ui).clicked() {
                    cx.dialog(Dialog::Report(ReportDialog::default()));
                }
            });
        }
    });
    ui.add_space(6.0);
    note(ui, &w!(lang, SetCheckLive));
    let _ = check_links(&c);
}

/// How many links of the chain are known (for tests).
pub(super) fn check_links(c: &PlaybackCheck) -> usize {
    [c.playing.is_some(), c.windows.is_some(), c.connected.is_some(), c.taking.is_some(), c.volume.is_some()].iter().filter(|b| **b).count()
}

/// *Can't find your speaker?*: each line says only what was checked.
pub(super) fn cant_find(ui: &mut Ui, cx: &mut Cx) {
    let lang = cx.lang;
    if crumb(ui, &w!(lang, SetNavHelp), &w!(lang, SetCantFindLink)) {
        cx.go(Page::Help);
    }
    let c = cx.port.discovery_check();
    boxed(ui, |ui| {
        for (i, line) in c.lines.iter().enumerate() {
            if i > 0 {
                divider(ui);
            }
            match line {
                FindLine::LookingOn { place, found } => {
                    check_line(ui, Found::Good, &w!(lang, SetFindLooking, network = &place_words(lang, place)), Some(&w!(lang, SetFindFound, count = *found)), |_| ())
                }
                FindLine::Vpn { vpn, only } => check_line(ui, Found::Warning, &w!(lang, SetFindVpn, network = &place_words(lang, vpn)), Some(&w!(lang, SetFindVpnLine)), |ui| {
                    if Button::primary(&w!(lang, SetLookOnlyOn, adapter = &only.name)).show(ui).clicked() {
                        cx.act(Act::LookOnlyOn(only.key.clone()));
                    }
                }),
                FindLine::Profile { network, private: true } => check_line(ui, Found::Good, &w!(lang, SetFindPrivate, network = network), None, |_| ()),
                FindLine::Profile { network, private: false } => check_line(ui, Found::Bad, &w!(lang, SetFindPublic, network = network), None, |ui| {
                    if Button::standard(&w!(lang, SetOpenNetworkSettings)).icon(Icon::External).show(ui).clicked() {
                        cx.act(Act::OpenNetworkSettings);
                    }
                }),
                FindLine::Firewall { open: true } => check_line(ui, Found::Good, &w!(lang, SetFindFirewallOpen), None, |_| ()),
                FindLine::Firewall { open: false } => check_line(ui, Found::Bad, &w!(lang, SetFindFirewallBlocks), None, |ui| {
                    if Button::standard(&w!(lang, SetAllowPrivate)).show(ui).clicked() {
                        cx.act(Act::AllowThroughFirewall);
                    }
                }),
                FindLine::AirPlaySearchFailed => check_line(ui, Found::Warning, &w!(lang, SetFindAirPlayFailed), None, |_| ()),
                FindLine::AdapterOff { adapter } => check_line(ui, Found::Bad, &w!(lang, SetNotLookingOff, adapter = adapter), None, |ui| {
                    if Button::standard(&w!(lang, SetLookEverywhere)).show(ui).clicked() {
                        cx.act(Act::LookOn(None));
                    }
                }),
                FindLine::Failed { adapter, why } => check_line(ui, Found::Bad, &w!(lang, SetFindFailed, adapter = adapter), Some(why), |_| ()),
            }
        }
        if !c.lines.is_empty() {
            divider(ui);
        }
        check_line(ui, Found::Info, &w!(lang, SetFindSameNetwork), Some(&w!(lang, SetFindSameNetworkLine)), |_| ());
    });
    ui.add_space(8.0);
    buttons(ui, |ui| {
        if Button::standard(&w!(lang, SetSearchAgain)).icon(Icon::Refresh).show(ui).clicked() {
            cx.act(Act::SearchAgain);
        }
        if Button::standard(&w!(lang, SetAddByAddressShort)).icon(Icon::Plus).show(ui).clicked() {
            cx.dialog(Dialog::AddAddress(AddDialog::default()));
        }
    });
    let _ = body;
}
