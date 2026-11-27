//! A Settings port that answers from memory: an example home
//! (Kitchen, a Sonos; Living Room + Dining Room; Bedroom, a
//! HomePod pair; Office, an AirPlay speaker; Garage, added by address; Den
//! TV; Desk Speaker), for pictures and tests. Every act is recorded, and
//! the ones that change a setting change it at once, as the real app does.

use std::sync::Mutex;

use crate::model::text::LocalTime;

use super::port::*;

/// The names the example speakers get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Names {
    Usual,
    /// Kitchen is called something long.
    Long,
    /// Kitchen has a long name in Chinese.
    Chinese,
}

pub const LONG_NAME: &str = "Living Room Sonos Arc and Sub by the Window";
pub const CHINESE_NAME: &str = "客厅里的索诺斯音箱和餐厅的音箱";

/// Everything the scripted app knows, all of it changeable.
#[derive(Clone, Debug)]
pub struct State {
    pub speakers: SpeakersView,
    pub pages: Vec<SpeakerView>,
    pub general: GeneralView,
    pub rejected_shortcut: Option<Vec<String>>,
    pub ma: MaView,
    pub privacy: PrivacyView,
    pub about: AboutView,
    pub lab: LabView,
    pub playback: PlaybackCheck,
    pub report_save: ReportSave,
    pub discovery: DiscoveryCheck,
    pub wanted: Option<Wanted>,
    pub probe: Probe,
    pub tones: Vec<(String, ToneView)>,
}

pub struct ScriptedSettings {
    state: Mutex<State>,
    acts: Mutex<Vec<Act>>,
}

impl ScriptedSettings {
    /// The usual example home, playing on Kitchen.
    pub fn new() -> Self {
        Self::with_names(Names::Usual)
    }

    pub fn with_names(names: Names) -> Self {
        let mut s = example();
        let kitchen = match names {
            Names::Usual => None,
            Names::Long => Some(LONG_NAME),
            Names::Chinese => Some(CHINESE_NAME),
        };
        if let Some(n) = kitchen {
            rename(&mut s, "kitchen", n);
        }
        ScriptedSettings { state: Mutex::new(s), acts: Mutex::new(Vec::new()) }
    }

    /// Change the state (a scene's or a test's set-up).
    pub fn set(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    /// Every act sent so far, oldest first.
    pub fn acts(&self) -> Vec<Act> {
        self.acts.lock().unwrap().clone()
    }

    pub fn take_acts(&self) -> Vec<Act> {
        std::mem::take(&mut self.acts.lock().unwrap())
    }
}

impl Default for ScriptedSettings {
    fn default() -> Self {
        Self::new()
    }
}

fn rename(s: &mut State, id: &str, name: &str) {
    for r in s.speakers.speakers.iter_mut().chain(s.speakers.hidden.iter_mut()) {
        if r.id == id {
            r.name = name.into();
        }
    }
    for p in &mut s.pages {
        if p.row.id == id {
            p.row.name = name.into();
            if p.show_as.is_some() {
                p.show_as = Some(name.into());
            }
        }
    }
    for (tid, t) in &mut s.tones {
        if tid == id {
            t.name = name.into();
        }
    }
    s.playback.speaker = Some(name.into());
}

impl SettingsPort for ScriptedSettings {
    fn accepts_shortcuts(&self, keys: &crate::platform::DelayKeys) -> bool {
        let s = self.state.lock().unwrap();
        keys.0[0] != keys.0[1] && s.rejected_shortcut.as_ref().is_none_or(|rejected| !keys.0.contains(rejected))
    }

    fn speakers(&self) -> SpeakersView {
        self.state.lock().unwrap().speakers.clone()
    }
    fn speaker(&self, id: &str) -> Option<SpeakerView> {
        self.state.lock().unwrap().pages.iter().find(|p| p.row.id == id).cloned()
    }
    fn general(&self) -> GeneralView {
        self.state.lock().unwrap().general.clone()
    }
    fn music_assistant(&self) -> MaView {
        self.state.lock().unwrap().ma.clone()
    }
    fn privacy(&self) -> PrivacyView {
        self.state.lock().unwrap().privacy.clone()
    }
    fn about(&self) -> AboutView {
        self.state.lock().unwrap().about.clone()
    }
    fn lab(&self) -> LabView {
        self.state.lock().unwrap().lab.clone()
    }
    fn playback_check(&self) -> PlaybackCheck {
        self.state.lock().unwrap().playback.clone()
    }
    fn discovery_check(&self) -> DiscoveryCheck {
        self.state.lock().unwrap().discovery.clone()
    }
    fn wanted_dialog(&self) -> Option<Wanted> {
        self.state.lock().unwrap().wanted.clone()
    }
    fn probe(&self) -> Probe {
        self.state.lock().unwrap().probe.clone()
    }
    fn tone(&self, id: &str) -> Option<ToneView> {
        let s = self.state.lock().unwrap();
        if let Some((_, v)) = s.tones.iter().find(|(t, _)| t == id) {
            return Some(v.clone());
        }
        // Any other speaker: flat.
        let name = s.pages.iter().find(|p| p.row.id == id)?.row.name.clone();
        Some(ToneView { name, eq: EqSettings::default(), headroom_db: None })
    }
    fn report(&self, parts: ReportParts) -> String {
        let mut lines = Vec::new();
        if parts.versions {
            lines.push("Stream To Speaker 0.2.0 · Windows".to_string());
        }
        if parts.diagnostics {
            lines.push("Windows audio: playing".into());
            if parts.speakers {
                lines.push("Speaker: Kitchen".into());
                lines.push("Last message: Kitchen stopped responding".into());
            }
        }
        if parts.speakers {
            lines.push("Speakers: Kitchen (Sonos, 192.168.1.20)".into());
            lines.push("Bedroom (192.168.1.33, .34)".into());
        }
        if parts.log && parts.speakers {
            lines.push("[log file attached]".into());
        }
        lines.join("\n")
    }

    fn save_report(&self, _: &str) -> ReportSave {
        self.state.lock().unwrap().report_save
    }

    fn act(&self, act: Act) {
        apply(&mut self.state.lock().unwrap(), &act);
        self.acts.lock().unwrap().push(act);
    }
}

/// What an act changes in the scripted app.
fn apply(s: &mut State, act: &Act) {
    match act {
        Act::LookOn(k) => {
            if let Some(n) = &mut s.speakers.networks {
                n.chosen = k.clone();
            }
        }
        Act::ShowSpeaker(id) => {
            if let Some(i) = s.speakers.hidden.iter().position(|r| &r.id == id) {
                let r = s.speakers.hidden.remove(i);
                s.speakers.speakers.push(r);
            }
        }
        Act::HideSpeaker(id) => {
            if let Some(i) = s.speakers.speakers.iter().position(|r| &r.id == id) {
                let r = s.speakers.speakers.remove(i);
                s.speakers.hidden.push(r);
            }
        }
        Act::SendspinSpeakers(on) => s.speakers.sendspin = Some(*on),
        Act::VolumeLimit(id, v) => {
            if let Some(p) = s.pages.iter_mut().find(|p| &p.row.id == id) {
                p.volume_limit = Some(*v);
            }
        }
        Act::Tone(id, t) => {
            if !s.tones.iter().any(|(tid, _)| tid == id) {
                if let Some(name) = s.pages.iter().find(|p| &p.row.id == id).map(|p| p.row.name.clone()) {
                    s.tones.push((id.clone(), ToneView { name, eq: EqSettings::default(), headroom_db: None }));
                }
            }
            if let Some((_, tv)) = s.tones.iter_mut().find(|(tid, _)| tid == id) {
                match t {
                    ToneAct::Band(i, db) => tv.eq.bands_db[*i] = *db,
                    ToneAct::Preamp(db) => tv.eq.preamp_db = *db,
                    ToneAct::Flat => tv.eq = EqSettings::default(),
                    ToneAct::Bass(b) => {
                        tv.eq.bands_db[0] = *b;
                        tv.eq.bands_db[1] = *b;
                        tv.eq.bands_db[2] = (b / 2.0 * 2.0).round() / 2.0;
                    }
                    ToneAct::Treble(tr) => {
                        tv.eq.bands_db[7] = *tr;
                        tv.eq.bands_db[8] = *tr;
                        tv.eq.bands_db[9] = *tr;
                    }
                    ToneAct::Preset(i) => tv.eq.bands_db = crate::equalizer::EQ_PRESETS[*i].bands_db,
                }
                let (b, tr) = (tv.eq.bands_db[0], tv.eq.bands_db[9]);
                if let Some(p) = s.pages.iter_mut().find(|p| &p.row.id == id) {
                    p.tone = Some((b, tr));
                }
            }
        }
        Act::SongTitles(id, on) => {
            if let Some(p) = s.pages.iter_mut().find(|p| &p.row.id == id) {
                p.song_titles = Some(*on);
            }
        }
        Act::ShowAs(id, name) => {
            if let Some(p) = s.pages.iter_mut().find(|p| &p.row.id == id) {
                p.show_as = Some(name.clone());
                p.row.name = name.clone();
            }
            if let Some(r) = s.speakers.speakers.iter_mut().find(|r| &r.id == id) {
                r.name = name.clone();
            }
        }
        Act::WhatPlays(m) => s.general.what_plays = Some(*m),
        Act::PlayCalls(on) => s.general.play_calls = Some(*on),
        Act::StartAtSignIn(on) => s.general.start_at_sign_in = Some(Ok(*on)),
        Act::SwitchBackAfterQuiet(on) => s.general.switch_back_after_quiet = *on,
        Act::NotesByClock(on) => s.general.notes_by_clock = *on,
        Act::ShowLab(on) => s.about.show_lab = *on,
        Act::Shortcuts(on) => {
            if let Some(k) = &mut s.general.shortcuts {
                k.on = *on;
            }
        }
        Act::ShortcutKeys(keys) => {
            if let Some(k) = &mut s.general.shortcuts {
                [k.remove, k.add] = keys.0.clone();
            }
        }
        Act::Language(l) => s.general.language = Some(l.clone()),
        Act::OfferToMa(on) => s.ma.offer = Some(*on),
        Act::Unpair(server) => s.ma.paired_with.retain(|p| p != server),
        Act::AllowPairingAgain => s.ma.paused = false,
        Act::AppearsAs(n) => s.ma.appears_as = n.clone(),
        Act::OnlyPlaying(on) => s.privacy.only_playing = *on,
        Act::Allow { address, .. } => {
            if let Some(r) = &mut s.privacy.refused {
                r.retain(|x| &x.address != address);
            }
        }
        Act::Phone(on) => {
            if let Some(p) = &mut s.privacy.phone {
                p.on = *on;
                p.link = on.then(|| "http://192.168.1.23:5901/r/2f7a9c41d0b85e63a1c4f09b7d2e8a56/".into());
            }
        }
        Act::Api(on) => {
            if let Some(a) = &mut s.privacy.api {
                a.on = *on;
            }
        }
        Act::DailyUpdateCheck(on) => s.about.updates.daily = Some(*on),
        Act::Lab(l) => match l {
            LabAct::AtOnce(v) => s.lab.at_once = Some(*v),
            LabAct::CatchUpFrames(v) => s.lab.catch_up_frames = Some(*v),
            LabAct::DriftPpm(v) => s.lab.drift_ppm = Some(*v),
            LabAct::SilencePace(v) => s.lab.silence_pace_ms = Some(*v),
            LabAct::Realtime(v) => s.lab.realtime = Some(*v),
            LabAct::Mfi(v) => s.lab.mfi = Some(*v),
            LabAct::UncompressedAlac(v) => s.lab.uncompressed_alac = Some(*v),
            LabAct::ResetAll => {
                let carried = s.lab.carried_over.take();
                s.lab = example().lab;
                s.lab.carried_over = carried;
            }
            LabAct::ResetCarriedOver => s.lab.carried_over = None,
        },
        Act::Find { address, .. } => s.probe = Probe::Checking { address: address.clone() },
        Act::AddFound { .. } | Act::AddByHand { .. } => s.probe = Probe::Added,
        Act::ResetProbe => s.probe = Probe::Idle,
        Act::SubmitCode { .. } | Act::CancelPairing(_) | Act::CancelPassword(_) | Act::CancelSendspin(_) | Act::CancelMaPairing => s.wanted = None,
        _ => {}
    }
}

fn row(id: &str, name: &str, kind: SpeakerKind, presence: Presence) -> SpeakerRow {
    SpeakerRow {
        id: id.into(),
        name: name.into(),
        kind,
        presence,
        added_by_address: false,
        cant_play: None,
    }
}

fn page(row: SpeakerRow, model: Option<&str>) -> SpeakerView {
    let name = row.name.clone();
    SpeakerView {
        row,
        volume_limit: Some(100),
        tone: Some((0.0, 0.0)),
        song_titles: None,
        show_as: Some(name),
        details: Details { model: model.map(Into::into), ..Default::default() },
    }
}

/// The example home.
pub fn example() -> State {
    let kitchen_row = row("kitchen", "Kitchen", SpeakerKind::Sonos, Presence::PlayingNow);
    let living = row("living", "Living Room + Dining Room", SpeakerKind::SonosGroup, Presence::Available);
    let bedroom = row("bedroom", "Bedroom", SpeakerKind::HomePodPair, Presence::Available);
    let office = row("office", "Office", SpeakerKind::Sonos, Presence::Available);
    let mut garage = row("garage", "Garage", SpeakerKind::AirPlay, Presence::NotFound { last_seen: Some(Seen::Yesterday(LocalTime::new(22, 10))) });
    garage.added_by_address = true;
    let den = row("den", "Den TV", SpeakerKind::AppleTv, Presence::Available);
    let desk = row("desk", "Desk Speaker", SpeakerKind::Sendspin, Presence::Available);
    let neighbour = row("neighbour", "Neighbour’s TV", SpeakerKind::AppleTv, Presence::Available);

    let mut kitchen = page(kitchen_row.clone(), Some("Sonos One"));
    kitchen.volume_limit = Some(70);
    kitchen.tone = Some((-2.0, 1.0));
    kitchen.song_titles = Some(true);
    kitchen.show_as = Some("Kitchen".into());
    kitchen.details = Details {
        model: Some("Sonos One".into()),
        address: Some("192.168.1.40".into()),
        connection: Some("UPnP, Sonos".into()),
    };

    let mut living_p = page(living.clone(), None);
    living_p.song_titles = Some(true);
    living_p.show_as = Some("Living Room + Dining Room".into());

    let mut bedroom_p = page(bedroom.clone(), None);
    bedroom_p.show_as = Some("Bedroom".into());
    bedroom_p.details.connection = Some("AirPlay 2, realtime".into());

    let mut office_p = page(office.clone(), None);
    office_p.song_titles = Some(true);
    office_p.details = Details {
        model: Some("Sonos One".into()),
        address: Some("192.168.1.44".into()),
        connection: Some("Sonos / AirPlay".into()),
    };

    let garage_p = page(garage.clone(), None);

    let den_p = page(den.clone(), None);

    let desk_p = page(desk.clone(), None);

    let neighbour_p = page(neighbour.clone(), None);

    State {
        speakers: SpeakersView {
            networks: Some(Networks {
                adapters: vec![Adapter { key: "wifi".into(), name: "Wi-Fi".into() }, Adapter { key: "eth2".into(), name: "Ethernet 2".into() }],
                chosen: None,
                looking: Looking::On(vec![
                    NetworkPlace { adapter: "Wi-Fi".into(), network: Some("HomeNet".into()), address: "192.168.1.23".into() },
                    NetworkPlace { adapter: "Ethernet 2".into(), network: None, address: "10.8.0.4".into() },
                ]),
                pinned: None,
            }),
            speakers: vec![kitchen_row, living, bedroom, office, garage, den, desk],
            hidden: vec![neighbour],
            sendspin: Some(false),
        },
        pages: vec![kitchen, living_p, bedroom_p, office_p, garage_p, den_p, desk_p, neighbour_p],
        rejected_shortcut: None,
        general: GeneralView {
            what_plays: Some(WhatPlays::Everything),
            play_calls: Some(false),
            start_at_sign_in: Some(Ok(true)),
            switch_back_after_quiet: false,
            notes_by_clock: true,
            shortcuts: Some(Shortcuts {
                on: true,
                remove: vec!["Ctrl".into(), "Win".into(), ",".into()],
                add: vec!["Ctrl".into(), "Win".into(), ".".into()],
                larger: "Shift".into(),
                status: [crate::platform::KeyStatus::Registered; 3],
            }),
            language: Some(None),
        },
        ma: MaView {
            offer: Some(true),
            failed: None,
            appears_as: "DESKTOP-7Q".into(),
            found_by: Some(("Music Assistant (Home)".into(), Some("homeassistant.local".into()))),
            code: Some(("482917".into(), Some(171))),
            paired_with: vec![],
            played: None,
            paused: false,
            token: Some("SP:0f3a9c2e7b5d41a8c6e0b9d2f1a47e3c".into()),
        },
        privacy: PrivacyView {
            only_playing: true,
            refused: Some(vec![
                Refused {
                    name: Some("Kitchen Sub".into()),
                    address: "192.168.1.51".into(),
                    at: LocalTime::new(20, 40),
                    group_of: Some(("kitchen".into(), "Kitchen".into())),
                },
                Refused { name: None, address: "192.168.1.77".into(), at: LocalTime::new(21, 12), group_of: None },
            ]),
            phone: Some(Phone { on: true, link: Some("http://192.168.1.23:5901/r/2f7a9c41d0b85e63a1c4f09b7d2e8a56/".into()), problem: None }),
            api: Some(Api { on: false, key: Some("2f7a9c41d0b85e63a1c4f09b7d2e8a56".into()) }),
        },
        about: AboutView {
            version: "0.2.0".into(),
            show_lab: false,
            updates: Updates {
                daily: Some(true),
                state: UpdateState::Available { version: "0.2.1".into(), at: Some(Seen::Today(LocalTime::new(9, 12))) },
                installs_here: true,
            },
        },
        lab: LabView {
            at_once: Some(false),
            catch_up_frames: Some(4),
            drift_ppm: Some(0),
            silence_pace_ms: Some(10),
            realtime: Some(false),
            mfi: Some(false),
            uncompressed_alac: Some(false),
            counters: Some(vec![
                ("Packets sent".into(), "1 284 220".into()),
                ("Listeners".into(), "1".into()),
                ("Resent".into(), "12".into()),
                ("Up for".into(), "2 h 14 min".into()),
                ("Last problem".into(), "none".into()),
            ]),
            carried_over: None,
        },
        report_save: ReportSave::Opened,
        playback: PlaybackCheck {
            speaker: Some("Kitchen".into()),
            playing: Some(Ok(Some("Edge".into()))),
            windows: Some(Ok(WindowsSends::ToUs)),
            connected: Some(Ok(())),
            taking: Some(Ok(Taking::Yes)),
            volume: Some(Ok(Volume::Level(32))),
            untested: false,
            apple_tv: false,
            on_short: false,
        },
        discovery: DiscoveryCheck {
            lines: vec![
                FindLine::LookingOn {
                    place: NetworkPlace { adapter: "Wi-Fi".into(), network: Some("HomeNet".into()), address: "192.168.1.x".into() },
                    found: 4,
                },
                FindLine::Vpn {
                    vpn: NetworkPlace { adapter: "Corp VPN".into(), network: None, address: "10.8.0.x".into() },
                    only: Adapter { key: "wifi".into(), name: "Wi-Fi".into() },
                },
                FindLine::Profile { network: "HomeNet".into(), private: true },
                FindLine::Firewall { open: true },
            ],
        },
        wanted: None,
        probe: Probe::Idle,
        tones: vec![(
            "kitchen".into(),
            ToneView {
                name: "Kitchen".into(),
                eq: EqSettings { enabled: true, preamp_db: 0.0, bands_db: [-2.0, -2.0, -1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0] },
                headroom_db: Some(-3.0),
            },
        )],
    }
}
