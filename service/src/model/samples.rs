//! Named examples of the facts that drive the panel.

use super::command::Command;
use super::facts::*;
use super::scene::*;
use super::situation::*;
use super::state::{PcOutputId, Place, SpeakerId};
use super::text::{Lang, LocalTime};

pub fn id(name: &str) -> SpeakerId {
    SpeakerId::new(name)
}
pub fn kitchen() -> SpeakerId {
    id("kitchen")
}
pub fn way(choice: WayChoice, own_ms: u32) -> WaySeen {
    WaySeen {
        way: choice,
        delay: SpeakerDelay {
            own_ms,
            measured: false,
            change_ms: 0,
        },
        range: DelayRange {
            max_removed_ms: 100,
            max_added_ms: 200,
        },
        applies: Applies::Live,
    }
}
pub fn seen(id: SpeakerId, name: &str) -> SpeakerSeen {
    SpeakerSeen {
        airplay_id: None,
        id,
        name: name.into(),
        kind: SpeakerKind::Sonos,
        ways: vec![way(WayChoice::Native, 150)],
        way: WayChoice::Native,
        airplay: None,
        used_here: true,
        last_used: None,
        address: None,
    }
}
pub fn output(name: &str, kind: OutputKind) -> PcOutput {
    PcOutput {
        id: PcOutputId::new(name),
        friendly: format!("{name} (Realtek Audio)"),
        name: name.into(),
        device: "Realtek Audio".into(),
        kind,
    }
}
pub fn scene() -> Scene {
    let output = output("Speakers", OutputKind::Speakers);
    Scene {
        lang: Lang::English,
        start_at_sign_in: true,
        situation: Situation {
            only_some_apps: false,
            spot: Spot::Speaker(kitchen()),
            pc_output: Some(output.id.clone()),
            away: None,
            doing: Doing::Playing,
            amber: false,
            note: None,
            picking: None,
            pick_failed: None,
            standing: vec![],
            speakers: vec![SpeakerNow {
                can_chime: true,
                id: kitchen(),
                state: RowState::Plain,
                way: WayChoice::Native,
                is_place: true,
            }],
            confirm_in_panel: None,
            confirm_under_routing: None,
            popups: Popups::default(),
            first_run: None,
        },
        outputs: vec![output],
        outputs_open: false,
        speakers: vec![seen(kitchen(), "Kitchen")],
        volume: VolumeFacts {
            pc: Some(Level::at(62)),
            place: Some(Level::at(35)),
        },
        progress: Progress::Settled,
        steps: DelaySteps::default(),
        search: SearchFacts::default(),
        network: None,
        repair: RepairNow::NotAsked,
    }
}
pub fn on_pc(s: &mut Scene) {
    s.situation.spot = Spot::ThisPc(s.situation.pc_output.clone());
    for now in &mut s.situation.speakers {
        now.is_place = false;
    }
}
pub fn pc() -> Scene {
    let mut s = scene();
    on_pc(&mut s);
    s
}
pub fn note(cause: Cause) -> Note {
    Note {
        cause,
        at: Some(LocalTime::new(21, 14)),
        ahead: None,
        available_again: false,
    }
}
pub fn stopped() -> Cause {
    Cause::StoppedResponding {
        speaker: kitchen(),
        vpn: None,
    }
}
pub fn pick(speaker: SpeakerId) -> Picking {
    Picking {
        speaker,
        way: WayChoice::Native,
        by: SwitchBy::Switch,
        from: SwitchFrom::Window,
        can_cancel: true,
    }
}
pub fn shown<T>(what: T) -> Shown<T> {
    Shown {
        seq: 7,
        what,
        since_ms: 2000,
    }
}
pub fn switch_to(speaker: SpeakerId) -> Command {
    Command::Switch {
        place: Place::of_speaker(&speaker),
        way: None,
    }
}
pub fn to_pc() -> Command {
    Command::Switch {
        place: Place::ThisPc,
        way: None,
    }
}

/// A model scene and its stable name in the preview gallery.
pub struct Sample {
    pub name: &'static str,
    pub scene: Scene,
}

fn home() -> Scene {
    let mut s = scene();
    s.outputs.push(output("Headphones", OutputKind::Headphones));
    s.volume.pc = Some(Level::at(40));
    s.volume.place = Some(Level::at(32));
    let mut kitchen = s.speakers.remove(0);
    kitchen.ways.push(way(WayChoice::AirPlaySteady, 2000));
    let mut bedroom = seen(id("bedroom"), "Bedroom");
    bedroom.kind = SpeakerKind::HomePodPair;
    let mut group = seen(id("living"), "Living Room + Dining Room");
    group.kind = SpeakerKind::SonosGroup;
    s.speakers = vec![
        bedroom,
        seen(id("garage"), "Garage"),
        kitchen,
        group,
        seen(id("office"), "Office"),
    ];
    s.situation.speakers = s
        .speakers
        .iter()
        .map(|sp| SpeakerNow {
            can_chime: true,
            id: sp.id.clone(),
            is_place: sp.id == self::kitchen(),
            way: sp.way,
            state: if sp.id == id("garage") {
                RowState::NotFound
            } else {
                RowState::Plain
            },
        })
        .collect();
    s.network = Some(ConnKind::WiFi);
    s
}

fn row_state(s: &mut Scene, id: &SpeakerId, state: RowState) {
    s.situation
        .speakers
        .iter_mut()
        .find(|r| r.id == *id)
        .unwrap()
        .state = state;
}

fn speaker_mut<'a>(s: &'a mut Scene, id: &SpeakerId) -> &'a mut SpeakerSeen {
    s.speakers.iter_mut().find(|sp| &sp.id == id).unwrap()
}
fn add_speaker(s: &mut Scene, sp: SpeakerSeen) {
    s.situation.speakers.push(SpeakerNow {
        can_chime: true,
        id: sp.id.clone(),
        state: RowState::Plain,
        way: sp.way,
        is_place: false,
    });
    s.speakers.push(sp);
}
fn on_speaker(s: &mut Scene, id: SpeakerId) {
    for r in &mut s.situation.speakers {
        r.is_place = r.id == id;
    }
    s.situation.spot = Spot::Speaker(id);
}

/// Build one example. An unknown name is a programming error.
pub fn named(name: &str) -> Scene {
    if name.starts_with("apps-") {
        return apps(name);
    }
    let mut s = home();
    let k = kitchen();
    if name.starts_with("pc-") {
        on_pc(&mut s);
    }
    match name {
        "pc-plain" => {}
        "pc-welcome-pick" => s.situation.first_run = Some(FirstRunStep::One),
        "pc-welcome-looking" | "pc-welcome-none-found" => {
            s.situation.first_run = Some(FirstRunStep::One);
            s.situation.speakers.clear();
            s.speakers.clear();
            s.search.phase = if name.ends_with("looking") { SearchPhase::Looking } else { SearchPhase::NoneFound };
        }
        "pc-welcome-busy" => {
            s.situation.first_run = Some(FirstRunStep::One);
            row_state(&mut s, &k, RowState::PlayingElse);
            s.situation.confirm_in_panel = Some(Confirm::ChimeBusy { speaker: k, who: None });
        }
        "pc-welcome-chime" => s.situation.first_run = Some(FirstRunStep::Two { speaker: k, level: Some(25), limit: 100 }),
        "kitchen-welcome-done" => s.situation.first_run = Some(FirstRunStep::Three { speaker: k }),
        "kitchen-playing" => {
            s.situation.popups.popup = Some(shown(Popup::PlayingOn { speaker: k }));
        }
        "pc-den-needs-code" => {
            let den = id("den");
            add_speaker(&mut s, seen(den.clone(), "Den TV"));
            s.situation.note = Some(note(Cause::NeedsSecret { speaker: den, secret: super::state::Secret::Code }));
        }
        "pc-office-needs-password" => {
            s.situation.pick_failed = Some(PickFailed { speaker: id("office"), why: PickWhy::WrongSecret(super::state::Secret::Password), by: SwitchBy::Switch, from: SwitchFrom::Window });
        }
        "kitchen-nothing-playing" => s.situation.doing = Doing::NothingPlaying,
        "kitchen-not-responding-trying" => {
            s.situation.amber = true;
            s.situation.doing = Doing::Trying;
            row_state(&mut s, &k, RowState::NotResponding);
            s.situation.note = Some(Note {
                ahead: Some(Ahead::InAFewSeconds),
                ..note(stopped())
            });
        }
        "pc-kitchen-stopped-responding" | "pc-kitchen-available-again" => {
            let available_again = name == "pc-kitchen-available-again";
            if !available_again {
                row_state(&mut s, &k, RowState::NotResponding);
            }
            s.situation.amber = !available_again;
            s.situation.note = Some(Note {
                available_again,
                ..note(stopped())
            });
        }
        "pc-connecting-to-kitchen" => {
            s.situation.picking = Some(pick(k.clone()));
            row_state(&mut s, &k, RowState::Connecting);
        }
        "pc-kitchen-didnt-respond" | "pc-long-name-didnt-respond" => {
            let target = if name == "pc-long-name-didnt-respond" {
                id("living")
            } else {
                k
            };
            row_state(&mut s, &target, RowState::NotResponding);
            s.situation.note = Some(note(Cause::DidntGetReady {
                speaker: target,
                why: PickWhy::NotResponding,
            }));
        }
        "kitchen-connecting" => {
            s.situation.doing = Doing::Connecting;
            row_state(&mut s, &k, RowState::Connecting);
        }
        "pc-headphones-connected" => {
            s.situation.pc_output = Some(s.outputs[1].id.clone());
            on_pc(&mut s);
            s.situation.note = Some(note(Cause::HeadphonesConnected {
                before: k,
                name: "Headphones".into(),
            }));
        }
        "pc-headphones-disconnected" => {
            s.outputs[1] = output("Display audio", OutputKind::Display);
            s.situation.note = Some(note(Cause::HeadphonesGone {
                before: k,
                name: "Headphones".into(),
            }));
        }
        "pc-kitchen-playing-something-else" => {
            row_state(&mut s, &k, RowState::PlayingElse);
            s.situation.note = Some(note(Cause::PlayingElse {
                speaker: k,
                who: None,
            }));
        }
        "office-playing" => on_speaker(&mut s, id("office")),
        "living-room-playing" => on_speaker(&mut s, id("living")),
        "kitchen-changing-way" => {
            s.situation.picking = Some(Picking {
                way: WayChoice::AirPlaySteady,
                by: SwitchBy::Way,
                ..pick(k)
            });
        }
        "kitchen-removing" => s.progress = Progress::Live { to_go_ms: -75 },
        "kitchen-least-delay" => {
            let w = &mut speaker_mut(&mut s, &k).ways[0];
            w.delay.change_ms = -(w.range.max_removed_ms as i32);
        }
        "kitchen-fixed-way" => {
            let sp = speaker_mut(&mut s, &k);
            sp.way = WayChoice::AirPlaySteady;
            sp.ways[1].applies = Applies::Fixed;
            s.situation
                .speakers
                .iter_mut()
                .find(|r| r.id == k)
                .unwrap()
                .way = WayChoice::AirPlaySteady;
        }
        "homepod-steady-only" => {
            let id = id("homepod");
            let mut sp = seen(id.clone(), "HomePod");
            sp.kind = SpeakerKind::HomePod;
            sp.way = WayChoice::AirPlaySteady;
            sp.ways = vec![WaySeen {
                applies: Applies::Fixed,
                ..way(sp.way, 2000)
            }];
            add_speaker(&mut s, sp);
            on_speaker(&mut s, id);
        }
        "kitchen-change-stops-sound" => {
            speaker_mut(&mut s, &k).ways[0].applies = Applies::Reconnect
        }
        "kitchen-away" => {
            s.situation.away = Some(Away::NotResponding);
            s.situation.doing = Doing::NothingPlaying;
            row_state(&mut s, &k, RowState::NotResponding);
            s.situation.note = Some(Note {
                ahead: Some(Ahead::WhenSoundStarts),
                ..note(Cause::NotRespondingAtStart {
                    speaker: k,
                    why: Away::NotResponding,
                })
            });
        }
        "kitchen-except-an-app" => s.situation.standing.push(Standing::AppElsewhere {
            app: "Chrome".into(),
            output: s.outputs[0].id.clone(),
        }),
        "kitchen-only-chosen-apps" => {
            s.situation.only_some_apps = true;
            s.situation.doing = Doing::NothingPlaying;
            s.situation
                .standing
                .push(Standing::OnlySomeApps { none_playing: true });
        }
        "kitchen-muted" => s.volume.place.as_mut().unwrap().muted = true,
        "kitchen-full-volume" => s.volume.place = Some(Level::at(100)),
        "kitchen-volume-zero" => s.volume.place = Some(Level::at(0)),
        "kitchen-volume-limit" => {
            s.volume.place = Some(Level {
                value: 60,
                limit: Some(60),
                muted: false,
            })
        }
        "question" => {
            on_pc(&mut s);
            row_state(&mut s, &k, RowState::PlayingElse);
            s.situation.confirm_in_panel = Some(Confirm::Busy {
                speaker: k,
                way: WayChoice::Native,
                who: None,
            });
        }
        "repair-needed" | "repair-unavailable" | "restart-needed" | "repair-running" | "repair-not-approved"
        | "output-turned-off" | "output-turned-off-refused" | "pc-welcome-repair" | "pc-welcome-output-turned-off" => {
            s.repair = match name {
                "repair-unavailable" => RepairNow::Unavailable,
                "restart-needed" => RepairNow::DidntWork,
                "repair-running" => RepairNow::Running,
                "repair-not-approved" => RepairNow::NotApproved,
                "output-turned-off" | "pc-welcome-output-turned-off" => RepairNow::TurnedOff,
                "output-turned-off-refused" => RepairNow::TurnOnFailed,
                _ => RepairNow::NotAsked,
            };
            on_pc(&mut s);
            if name.starts_with("pc-welcome-") { s.situation.first_run = Some(FirstRunStep::One); }
            s.situation.standing.push(Standing::OutputMissing);
            for r in &mut s.situation.speakers {
                r.state = RowState::RepairFirst;
            }
        }
        "repair-worked" => {
            on_pc(&mut s);
            s.repair = RepairNow::Worked;
        }
        "pc-outputs-open" => {
            s.outputs.push(output(
                "Digital Audio (High Definition Audio Device on DisplayPort)",
                OutputKind::Display,
            ));
            s.outputs_open = true;
        }
        "pc-no-output" => {
            s.outputs.clear();
            s.situation.pc_output = None;
            on_pc(&mut s);
            s.situation.doing = Doing::NothingToPlayOn;
        }
        "fold-closed" | "fold-open" | "first-speakers" | "many-speakers" => {
            let (count, used) = match name {
                "first-speakers" => (6, 0),
                "many-speakers" => (14, 14),
                _ => (12, 7),
            };
            on_pc(&mut s);
            for index in s.speakers.len()..count {
                let room = format!("Room {}", index + 1);
                add_speaker(&mut s, seen(id(&room), &room));
            }
            for (i, sp) in s.speakers.iter_mut().enumerate() {
                sp.used_here = i < used;
            }
            if used == 0 {
                row_state(&mut s, &id("garage"), RowState::Plain);
            }
        }
        "looking" | "none-found" => {
            on_pc(&mut s);
            s.speakers.clear();
            s.situation.speakers.clear();
            s.search.phase = if name == "looking" {
                SearchPhase::Looking
            } else {
                SearchPhase::NoneFound
            };
        }
        "music-assistant-playing" => {
            let ma = SpeakerId::music_assistant();
            // As the app lists it: Music Assistant sets its own delay.
            let row = SpeakerSeen { kind: SpeakerKind::Sendspin, ways: Vec::new(), ..seen(ma.clone(), "Music Assistant") };
            add_speaker(&mut s, row);
            on_speaker(&mut s, ma);
        }
        "pc-music-assistant-not-connected" | "music-assistant-not-connected" => {
            let ma = SpeakerId::music_assistant();
            let row = SpeakerSeen { kind: SpeakerKind::Sendspin, ways: Vec::new(), ..seen(ma.clone(), "Music Assistant") };
            add_speaker(&mut s, row);
            // No server is connected: no chime can play there.
            let now = s.situation.speakers.last_mut().unwrap();
            now.can_chime = false;
            now.state = RowState::NotFound;
            if name == "music-assistant-not-connected" {
                // It is the place all the same: sound that plays isn't
                // heard until its server connects.
                on_speaker(&mut s, ma);
                s.situation.standing.push(Standing::MusicAssistantNotConnected);
            }
        }
        "twins" => {
            speaker_mut(&mut s, &k).address = Some("192.168.1.7".into());
            let mut twin = seen(id("second"), "Kitchen");
            twin.address = Some("192.168.1.8".into());
            add_speaker(&mut s, twin);
        }
        "long-names" => {
            on_pc(&mut s);
            // Two outputs with one name go by Windows' full names.
            s.outputs[0].friendly = "Speakers (Realtek High Definition Audio)".into();
            s.outputs[1].friendly = "Speakers (USB High Definition Audio Device)".into();
            s.outputs[1].name = "Speakers".into();
            s.outputs[1].kind = OutputKind::Speakers;
            for (sp, name) in s.speakers.iter_mut().zip([
                "Bedroom HomePod Stereo Pair by the Window",
                "Living Room + Dining Room + Kitchen",
                "客厅里的索诺斯音箱和餐厅的音箱",
                "Living Room + Dining Room + Upstairs Landing",
                "Office Speakers at the End of the Hall",
            ]) {
                sp.name = name.into();
            }
        }
        _ => panic!("unknown panel sample {name}"),
    }
    // The moments that raise a pop-up by the clock: a speaker becoming
    // available again and headphones being connected raise none.
    if matches!(name, "pc-kitchen-stopped-responding" | "pc-headphones-disconnected" | "pc-kitchen-didnt-respond" | "pc-long-name-didnt-respond") {
        s.situation.popups.popup = s.situation.note.clone().map(|n| shown(Popup::Note(n)));
    }
    if name == "question" {
        s.situation.popups.confirmation = s.situation.confirm_in_panel.clone().map(shown);
    }
    s
}

fn apps(name: &str) -> Scene {
    let mut s = match name {
        "apps-first-pick" => named("pc-welcome-pick"),
        "apps-first-hear" => named("pc-welcome-chime"),
        "apps-first-done" => named("kitchen-welcome-done"),
        "apps-first-pick-no-chime" | "apps-first-switch-no-chime" | "apps-first-done-no-chime" => {
            let mut s = pc();
            let desk = id("desk");
            s.speakers[0] = seen(desk.clone(), "Desk");
            s.speakers[0].kind = SpeakerKind::Sendspin;
            s.situation.speakers[0].id = desk.clone();
            s.situation.speakers[0].can_chime = false;
            s.situation.first_run = Some(match name {
                "apps-first-pick-no-chime" => FirstRunStep::One,
                "apps-first-switch-no-chime" => FirstRunStep::Two { speaker: desk, level: None, limit: 100 },
                _ => {
                    on_speaker(&mut s, desk.clone());
                    FirstRunStep::Three { speaker: desk }
                }
            });
            s
        }
        "apps-panel-playing" => home(),
        "apps-move-soon" => named("kitchen-not-responding-trying"),
        "apps-when-starting" => named("kitchen-away"),
        "apps-moved" => named("pc-kitchen-stopped-responding"),
        "apps-connecting" => named("pc-connecting-to-kitchen"),
        _ => panic!("unknown apps sample {name}"),
    };
    s.situation.only_some_apps = true;
    if s.situation.place_speaker().is_some() {
        s.situation.standing.push(Standing::OnlySomeApps {
            none_playing: s.situation.doing == Doing::NothingPlaying,
        });
    }
    s
}

// Preview scenes need literal names and non-capturing draw functions.
macro_rules! with_sample_names {
    (apps_first_run, $f:ident) => {
        $f!("apps-first-pick", "apps-first-hear", "apps-first-done",
            "apps-first-pick-no-chime", "apps-first-switch-no-chime", "apps-first-done-no-chime")
    };
    (apps_panel, $f:ident) => {
        $f!("apps-panel-playing", "apps-move-soon", "apps-when-starting", "apps-moved", "apps-connecting")
    };
    ($f:ident) => {
        $f!(
            "kitchen-playing",
            "kitchen-nothing-playing",
            "kitchen-not-responding-trying",
            "pc-kitchen-stopped-responding",
            "pc-kitchen-available-again",
            "pc-plain",
            "pc-welcome-pick",
            "pc-welcome-busy",
            "pc-welcome-looking",
            "pc-welcome-none-found",
            "pc-welcome-chime",
            "kitchen-welcome-done",
            "pc-den-needs-code",
            "pc-office-needs-password",
            "pc-connecting-to-kitchen",
            "pc-kitchen-didnt-respond",
            "kitchen-connecting",
            "pc-headphones-connected",
            "pc-headphones-disconnected",
            "pc-kitchen-playing-something-else",
            "pc-long-name-didnt-respond",
            "office-playing",
            "living-room-playing",
            "kitchen-changing-way",
            "kitchen-removing",
            "kitchen-least-delay",
            "kitchen-fixed-way",
            "homepod-steady-only",
            "kitchen-change-stops-sound",
            "kitchen-away",
            "kitchen-except-an-app",
            "kitchen-only-chosen-apps",
            "kitchen-muted",
            "kitchen-full-volume",
            "kitchen-volume-zero",
            "kitchen-volume-limit",
            "question",
            "repair-needed",
            "repair-unavailable",
            "repair-running",
            "repair-not-approved",
            "output-turned-off",
            "output-turned-off-refused",
            "restart-needed",
            "repair-worked",
            "pc-outputs-open",
            "pc-no-output",
            "fold-closed",
            "fold-open",
            "first-speakers",
            "many-speakers",
            "looking",
            "none-found",
            "music-assistant-playing",
            "music-assistant-not-connected",
            "pc-music-assistant-not-connected",
            "twins",
            "long-names"
        )
    };
}
pub(crate) use with_sample_names;

/// Every sample, in gallery order.
pub fn all() -> Vec<Sample> {
    macro_rules! collect {
        ($($name:literal),* $(,)?) => { vec![$( Sample { name: $name, scene: named($name) } ),*] }
    }
    let mut samples = with_sample_names!(collect);
    samples.extend(with_sample_names!(apps_first_run, collect));
    samples.extend(with_sample_names!(apps_panel, collect));
    samples
}
