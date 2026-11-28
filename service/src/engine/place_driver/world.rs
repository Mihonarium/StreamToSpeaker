//! Windows' observations and the app's protocol ids for each speaker.

use super::*;
use crate::delay::DelayChoice;
use crate::engine::facts;
use crate::model::facts::{Presence, SpeakerFacts, SpeakerKind};
use crate::model::situation::ConnKind;
use crate::platform::{Player, PlayerStatus as Status};
use crate::sendspin_app::MA_OUTPUT_ID;

#[derive(Clone)]
pub(super) struct Ids {
    pub id: SpeakerId,
    pub native: Option<String>,
    pub airplay: Option<String>,
}

impl Ids {
    pub fn has(&self, app_id: &str) -> bool {
        self.native.as_deref() == Some(app_id) || self.airplay.as_deref() == Some(app_id)
    }

    fn for_way(&self, way: WayChoice) -> &str {
        let preferred = match way {
            WayChoice::Native => self.native.as_ref(),
            _ => self.airplay.as_ref(),
        };
        preferred.or(self.native.as_ref()).or(self.airplay.as_ref()).map_or(self.id.as_str(), String::as_str)
    }
}

fn airplay(s: &SpeakerFacts) -> bool {
    matches!(s.kind, SpeakerKind::HomePod | SpeakerKind::HomePodPair | SpeakerKind::AppleTv | SpeakerKind::AirPlay)
}

fn upnp(s: &SpeakerFacts) -> bool {
    matches!(s.kind, SpeakerKind::Sonos | SpeakerKind::SonosGroup | SpeakerKind::Other { .. })
}

/// A pair keeps the UPnP row's place in the list, even when its AirPlay
/// announcement came first.
pub(super) fn fold(list: &[SpeakerFacts], mut choice: impl FnMut(&str) -> Option<DelayChoice>) -> Vec<(Ids, SpeakerWorld)> {
    let mut rows = Vec::new();
    for s in list {
        let twin = s.twin.as_ref().and_then(|id| list.iter().find(|t| t.id == *id));
        if airplay(s) && twin.is_some_and(upnp) {
            continue;
        }
        let ap = if airplay(s) { Some(s) } else { twin.filter(|t| upnp(s) && airplay(t)) };
        let mut ways = Vec::new();
        if !airplay(s) {
            ways.push(WayChoice::Native);
        }
        if let Some(t) = ap {
            use WayChoice::{AirPlayShort, AirPlaySteady};
            match choice(t.id.as_str()) {
                Some(DelayChoice::Short) => ways.extend([AirPlayShort, AirPlaySteady]),
                Some(DelayChoice::Steady) => ways.extend([AirPlaySteady, AirPlayShort]),
                None => ways.push(AirPlaySteady),
            }
        }
        rows.push((
            Ids {
                id: s.id.clone(),
                native: (!airplay(s)).then(|| s.id.as_str().into()),
                airplay: ap.map(|t| t.id.as_str().into()),
            },
            SpeakerWorld {
                found: s.presence == Presence::Found || ap.is_some_and(|t| t.presence == Presence::Found),
                ways,
                shows_doing: upnp(s),
                can_chime: (upnp(s) && s.presence == Presence::Found) || ap.is_some_and(|t| t.presence == Presence::Found),
                level_unreadable: airplay(s),
                ..SpeakerWorld::found(s.id.as_str())
            },
        ));
    }
    rows
}

pub(super) struct Observation {
    pub world: World,
    pub players: Vec<Player>,
    pub ours: Option<PcOutputId>,
    pub outputs: Vec<crate::model::facts::PcOutput>,
    pub airplay: HashMap<String, crate::delay::DelayStatus>,
    /// This PC's connections with what Windows says of each network.
    pub networks: Vec<crate::model::facts::NetworkSeen>,
}

impl PlaceDriver {
    pub(super) fn app_id(&self, speaker: &SpeakerId, way: WayChoice) -> String {
        if speaker.is_music_assistant() {
            return MA_OUTPUT_ID.into();
        }
        self.ids.iter().find(|s| s.id == *speaker).map_or(speaker.as_str(), |s| s.for_way(way)).into()
    }

    pub(super) fn speaker_of(&self, id: &str) -> SpeakerId {
        if let Some((_, _, speaker)) = self.bound_as.as_ref().filter(|(_, bound, _)| bound == id) {
            return speaker.clone();
        }
        if id == MA_OUTPUT_ID {
            return SpeakerId::music_assistant();
        }
        self.ids
            .iter()
            .find(|s| s.has(id))
            .or_else(|| self.bound_ids.as_ref().filter(|s| s.has(id)))
            .map_or_else(|| SpeakerId::new(id), |s| s.id.clone())
    }

    pub(super) fn bound(&self) -> Option<(String, u64)> {
        self.app.session.lock().unwrap().as_ref().map(|s| (s.stable_id(), self.app.session_number()))
    }

    pub(super) fn session_of(&self, speaker: &SpeakerId) -> Option<(String, u64)> {
        self.bound().filter(|(id, _)| self.speaker_of(id) == *speaker)
    }

    pub(super) fn observe(&mut self, clock: Clock, remember_network: bool) -> Observation {
        let app = &self.app;
        if self.list_at.is_none_or(|at| clock.mono_ms.saturating_sub(at) >= 1000) {
            self.list = facts::speaker_list(app);
            self.list_at = Some(clock.mono_ms);
        }
        let binding = app.session.lock().unwrap().as_ref().map(|session| {
            (session.stable_id(), app.session_number(), matches!(session, crate::app::ActiveSession::Sendspin(s) if !s.accepts_volume()))
        });
        if self.bound_as.as_ref().is_some_and(|(n, _, _)| binding.as_ref().is_none_or(|(_, current, _)| current != n)) {
            self.bound_as = None;
        }
        let local_level = binding.as_ref().is_some_and(|(_, _, local)| *local);
        let bound = binding.map(|(id, _, _)| id);
        let bound_place = self.bound_as.as_ref().map(|(_, _, speaker)| speaker);
        let seen = self.platform.network.connections();
        let networks = seen
            .iter()
            .map(|c| crate::model::facts::NetworkSeen { adapter: c.adapter.clone(), name: c.name.clone(), private: c.private, kind: c.kind })
            .collect();
        let connections: Vec<_> = seen.into_iter().map(|c| Connection { net: NetId(c.id), name: c.name, kind: c.kind }).collect();
        let mut airplay = HashMap::new();
        let mut rows = fold(&self.list, |id| {
            let status = app.delay_status_of(id);
            let choice = status.choice;
            airplay.insert(id.to_owned(), status);
            choice
        });
        let (ma_found, ma_playing) = match app.sendspin_source_state() {
            Some(crate::sendspin::source::SourceState::Connected { streaming, .. }) => (true, streaming),
            _ => (false, false),
        };
        if app.is_sendspin_source_enabled() {
            rows.push((
                Ids { id: SpeakerId::music_assistant(), native: Some(MA_OUTPUT_ID.into()), airplay: None },
                SpeakerWorld { shows_doing: false, can_chime: false, found: ma_found, ..SpeakerWorld::found(MA_OUTPUT_ID) },
            ));
        }
        let kept = self.core.kept();
        self.ids.clear();
        let mut speakers = Vec::new();
        for (ids, mut s) in rows {
            s.only_from_window = ids.airplay.as_deref().is_some_and(|id| app.is_airplay_newcomer(id));
            let plays_here =
                bound_place.map_or_else(|| bound.as_deref().is_some_and(|id| ids.has(id)), |k| *k == ids.id);
            if let Some(id) = bound.as_deref().filter(|_| plays_here) {
                // Music Assistant can play a chime only while it plays
                // this PC's sound.
                s.can_chime |= self.core.connected_speaker() == Some(&ids.id) && (!s.id.is_music_assistant() || ma_playing);
                // A fixed player starts at full scale on its first session.
                // Later sessions use the level kept for this speaker.
                s.level_unreadable |= local_level && kept.levels.contains_key(&s.id);
                self.bound_ids = Some(ids.clone());
                s.level = if s.id.is_music_assistant() {
                    Some(app.ma_level())
                } else {
                    app.current_volume().map(|v| v.min(100) as u8)
                };
                if ids.airplay.as_deref() == Some(id) {
                    s.shows_doing = false;
                    s.level_unreadable = true;
                }
            }
            self.ids.push(ids);
            speakers.push(s);
        }
        if bound.is_none() {
            self.bound_ids = None;
        }
        let bound_speaker = bound.as_deref().map(|id| self.speaker_of(id));
        let net = connections.iter().find(|c| c.kind != ConnKind::Other).map(|c| &c.net);
        // Music Assistant's network is the one this PC was on when its
        // server connected, the same server connecting again included. A
        // connection can outlast a change of network by some seconds, so
        // being connected doesn't say it is on this network.
        let ma_now = ma_found.then(|| app.sendspin_source_arrivals());
        let ma_arrived = remember_network && ma_found && self.ma_connected != ma_now;
        if remember_network && (net.is_some() || !ma_found) {
            self.ma_connected = ma_now;
        }
        let settings = {
            let mut cfg = app.user_config.lock().unwrap();
            let mut changed = false;
            for (s, ids) in speakers.iter_mut().zip(&self.ids) {
                let key = s.id.as_str();
                // A discovery announcement alone doesn't make this a previously used speaker.
                let used = cfg.speaker_settings.contains_key(key) || cfg.last_speaker_id.as_deref() == Some(key);
                let here = if s.id.is_music_assistant() { ma_arrived } else { remember_network && s.found && used };
                if let Some(net) = net.filter(|_| here) {
                    let entry = cfg.speaker_settings.entry(key.into()).or_default();
                    if entry.network.as_ref() != Some(net) {
                        entry.network = Some(net.clone());
                        changed = true;
                    }
                }
                s.network = cfg.speaker_settings.get(key).and_then(|v| v.network.clone());
                let active = bound.as_deref().filter(|id| ids.has(id)).unwrap_or(key);
                s.max_level = cfg
                    .speaker_settings
                    .get(active)
                    .and_then(|v| v.volume_limit)
                    .or_else(|| cfg.speaker_settings.get(key).and_then(|v| v.volume_limit));
            }
            // A speaker absent on this network still needs its last network
            // in the world for the core's minute after a change of network.
            let mut absent: Vec<_> = cfg.speaker_settings.iter().filter(|(_, s)| s.network.is_some())
                .map(|(k, _)| k.as_str())
                .chain(kept.place.iter().chain(kept.named.iter()).map(SpeakerId::as_str))
                .filter(|id| !self.ids.iter().any(|s| s.has(id)) && *id != MA_OUTPUT_ID)
                .collect();
            absent.sort_unstable();
            absent.dedup();
            for id in absent {
                let saved = cfg.speaker_settings.get(id);
                speakers.push(SpeakerWorld {
                    found: false,
                    can_chime: self.core.connected_speaker().is_some_and(|k| k.as_str() == id && bound_speaker.as_ref() == Some(k)),
                    level_unreadable: local_level && bound_speaker.as_ref().is_some_and(|k| k.as_str() == id && kept.levels.contains_key(k)),
                    ways: Vec::new(),
                    network: saved.and_then(|s| s.network.clone()),
                    max_level: saved.and_then(|s| s.volume_limit),
                    ..SpeakerWorld::found(id)
                });
            }
            if changed {
                cfg.save();
            }
            SettingsWorld {
                switch_back_after_quiet: cfg.switch_back_after_quiet,
                notes: cfg.notes_by_clock,
                only_some_apps: cfg.only_some_apps,
            }
        };
        let outputs = &self.platform.outputs;
        let ours = outputs.our_output();
        let output_turned_off = ours.is_none() && self.platform.device.turned_off();
        let players = self.platform.players.list();
        let enabled: Vec<_> = outputs.list().iter().filter(|o| Some(&o.id) != ours.as_ref()).map(|o| o.to_model()).collect();
        let world = World {
            speakers,
            outputs: enabled.iter()
                .map(|o| OutputWorld { id: o.id.clone(), name: o.name.clone(), kind: o.kind })
                .collect(),
            default_output: outputs.default_output().map(|o| output_ref(o, &ours)),
            multimedia_output: outputs.default_for(crate::platform::Role::Multimedia).map(|o| output_ref(o, &ours)),
            our_device: ours.is_some(),
            can_repair: ours.is_none() && !output_turned_off && self.platform.device.can_repair(),
            output_turned_off,
            players: players
                .iter()
                .map(|p| PlayerWorld {
                    id: PlayerId::new(&p.id),
                    app: AppKey::new(p.app.as_deref().unwrap_or(&p.id)),
                    name: p.name.clone(),
                    status: match p.status {
                        Status::Playing => PlayerStatus::Playing,
                        Status::Paused => PlayerStatus::Paused,
                        Status::Stopped => PlayerStatus::Stopped,
                        Status::Other => PlayerStatus::Other,
                    },
                })
                .collect(),
            connections,
            settings,
            screen_reader: self.platform.desk.screen_reader(),
            panel_open: self.panel_open,
            music_assistant_playing: ma_playing,
            sound_unreadable: app.sound_input_lost(),
        };
        Observation { world, players, ours, outputs: enabled, airplay, networks }
    }
}

pub(super) fn output_ref(id: PcOutputId, ours: &Option<PcOutputId>) -> OutputRef {
    if Some(&id) == ours.as_ref() {
        OutputRef::Ours
    } else {
        OutputRef::Pc(id)
    }
}
