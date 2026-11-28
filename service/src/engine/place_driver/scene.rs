//! The rules and all the surfaces see the same outputs, speakers and clock.

use super::*;
use crate::delay::{self, DelayChoice, DelayStatus};
use crate::engine::facts::delay_range;
use crate::model::facts::{
    DelaySteps, Level, Progress, SearchFacts, SearchPhase, SpeakerDelay, SpeakerKind, VolumeFacts,
};
pub(super) const CANT_FIND_AFTER_MS: u64 = 10_000;
use crate::model::scene::{Scene, SpeakerSeen, WaySeen};
use crate::model::situation::{ConnKind, SpeakerNow};
use crate::model::text::t;
use world::{Ids, Observation};

impl PlaceDriver {
    #[cfg(test)]
    pub(crate) fn scene(&mut self) -> Scene {
        self.scene_and_output().0
    }

    pub(super) fn scene_and_output(&mut self) -> (Scene, Option<PcOutputId>) {
        let now = self.clock();
        let obs = self.observe(now, false);
        let w = &obs.world;
        let mut situation = self.core.situation(w, now);
        // The rules know an app by the name of its program's file; people
        // read the name Windows shows for it.
        for s in &mut situation.standing {
            if let crate::model::situation::Standing::AppElsewhere { app, .. } = s {
                if let Some(name) = self.app_names.get(app.as_str()) {
                    *app = name.clone();
                }
            }
        }
        let place = situation.place_speaker();
        let speakers = w
            .speakers
            .iter()
            .zip(&situation.speakers)
            .enumerate()
            .filter_map(|(i, (row, state))| {
                let keep = Some(&row.id) == place || Some(&row.id) == self.core.named();
                self.scene_row(self.ids.get(i), row, state, keep, &obs)
            })
            .collect();
        let pc = situation
            .pc_output
            .as_ref()
            .and_then(|id| self.platform.outputs.volume(id))
            .map(|(value, muted)| Level {
                value,
                muted,
                limit: None,
            });
        let place_level = place.and_then(|id| {
            if id.is_music_assistant() {
                let mut level = Level::at(self.app.ma_level());
                if let Some(before) = self.ma_before_mute.filter(|_| level.value == 0) {
                    level.value = before;
                    level.muted = true;
                }
                return Some(level);
            }
            let (bound, _) = self.session_of(id)?;
            Some(Level {
                value: self.app.current_volume()?.min(100) as u8,
                muted: self.app.vsync.is_muted(),
                limit: self
                    .app
                    .volume_limit(&bound)
                    .or_else(|| self.app.volume_limit(id.as_str())),
            })
        });
        let progress = place
            .and_then(|id| {
                let d = self
                    .app
                    .delay_status()
                    .filter(|d| self.speaker_of(&d.id) == *id)?;
                Some(match d.progress {
                    delay::Progress::Done => Progress::Settled,
                    delay::Progress::CatchingUp { ms_to_go } => Progress::Live { to_go_ms: ms_to_go },
                    delay::Progress::WillReconnect => Progress::ReconnectPending,
                    delay::Progress::Reconnecting { .. } => Progress::Reconnecting,
                })
            })
            .unwrap_or(Progress::Settled);
        let mut search = SearchFacts::default();
        if self.app.discovery.is_none() && self.app.airplay_discovery.is_none() {
            search.phase = SearchPhase::Off;
        } else if !w.speakers.iter().any(|s| s.found) {
            search.phase = if now.mono_ms < CANT_FIND_AFTER_MS {
                SearchPhase::Looking
            } else {
                SearchPhase::NoneFound
            };
        }
        let mut kinds = w
            .connections
            .iter()
            .map(|c| c.kind)
            .filter(|k| *k != ConnKind::Other);
        let network = kinds.next().filter(|_| kinds.next().is_none());
        let scene = Scene {
            lang: self.lang,
            start_at_sign_in: matches!(situation.first_run, Some(crate::model::situation::FirstRunStep::Three { .. })) && crate::autostart::is_enabled(),
            situation,
            speakers,
            outputs: obs.outputs,
            outputs_open: self.app.user_config.lock().unwrap().pc_outputs_open,
            volume: VolumeFacts {
                pc,
                place: place_level,
            },
            progress,
            steps: DelaySteps::default(),
            search,
            network,
            repair: self.core.repair_note(w),
        };
        (scene, obs.ours)
    }

    fn scene_row(
        &self,
        ids: Option<&Ids>,
        row: &SpeakerWorld,
        state: &SpeakerNow,
        keep: bool,
        obs: &Observation,
    ) -> Option<SpeakerSeen> {
        let id = &row.id;
        let lang = self.lang;
        let (name, kind, used_here, last_used, address) =
            if let Some(s) = self.list.iter().find(|s| s.id == *id) {
                if s.hidden && !keep {
                    return None;
                }
                (
                    s.name.clone(),
                    s.kind.clone(),
                    s.used_here,
                    s.last_used,
                    Some(s.address.clone()).filter(|a| !a.is_empty()),
                )
            } else {
                let saved = self.app.speaker_settings(id.as_str());
                let ma = id.is_music_assistant();
                let on_network = obs
                    .world
                    .connections
                    .iter()
                    .any(|c| Some(&c.net) == row.network.as_ref());
                // Music Assistant has a row before it has ever connected,
                // where its setup can say what is missing. Once it has, it
                // follows its network as a speaker does.
                let elsewhere = if ma { !row.found && row.network.is_some() && !on_network } else { !on_network };
                if !keep && (saved.hidden || elsewhere) {
                    return None;
                }
                let name = if ma {
                    t!(lang, MaName)
                } else {
                    self.app
                        .speaker_name(id.as_str())
                        .unwrap_or_else(|| id.as_str().to_string())
                };
                let kind = if ma {
                    SpeakerKind::Sendspin
                } else {
                    SpeakerKind::Other { maker: None }
                };
                (name, kind, true, saved.last_played, None)
            };
        let mut ways = Vec::new();
        let ap = ids
            .and_then(|s| s.airplay.as_ref())
            .and_then(|id| obs.airplay.get(id));
        for way in [
            WayChoice::Native,
            WayChoice::AirPlayShort,
            WayChoice::AirPlaySteady,
        ] {
            if !row.ways.contains(&way) {
                continue;
            }
            let status = match way {
                WayChoice::Native => {
                    let app_id = ids?.native.as_deref()?;
                    self.app.delay_status_of(app_id)
                }
                _ => self.airplay_preview(ap?, way),
            };
            // Music Assistant sets its own delay. Its row offers no delay controls.
            if let Some((range, applies)) = delay_range(&status) {
                ways.push(WaySeen {
                    way,
                    delay: SpeakerDelay {
                        own_ms: status.own_delay_ms,
                        measured: false,
                        change_ms: status.change_ms,
                    },
                    range,
                    applies,
                });
            }
        }
        let airplay = ap.and_then(|d| d.choice).map(|c| match c {
            DelayChoice::Short => WayChoice::AirPlayShort,
            DelayChoice::Steady => WayChoice::AirPlaySteady,
        });
        Some(SpeakerSeen {
            airplay_id: ids.and_then(|s| s.airplay.as_deref()).map(SpeakerId::new),
            id: id.clone(),
            way: state.way,
            ways,
            airplay,
            name,
            kind,
            used_here,
            last_used,
            address,
        })
    }

    /// The other AirPlay line previews a fresh connection with that
    /// choice. The active session's buffered flag belongs to its own way.
    fn airplay_preview(&self, current: &DelayStatus, way: WayChoice) -> DelayStatus {
        let choice = if way == WayChoice::AirPlayShort {
            DelayChoice::Short
        } else {
            DelayChoice::Steady
        };
        let mut status = current.clone();
        if current.choice.is_some_and(|c| c != choice) {
            let mut saved = self.app.speaker_settings(&current.id);
            saved.delay_choice = Some(choice);
            let setting = delay::setting(
                current.kind,
                &saved,
                &delay::Earlier::default(),
                &delay::Live::default(),
            );
            status.choice = setting.choice;
            status.own_delay_ms = setting.own_ms;
            status.delay_ms = setting.delay_ms();
            status.change_ms = setting.change_ms;
            status.limits = setting.limits;
            status.applies = setting.applies;
        }
        status
    }
}
