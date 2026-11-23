//! Moving an earlier version's delay settings into this version's, so that
//! every speaker sounds the same after the update as before.
//!
//! Earlier versions kept delay for every speaker at once: one AirPlay
//! buffer (2000 ms unless changed), one HomePod delay (200 ms), a speaker's
//! own AirPlay buffer where one was set, and one extra delay for every
//! Sendspin speaker. This version keeps, per speaker, a delay choice (Short
//! delay or Steady, for AirPlay speakers and single HomePods) and what was
//! removed from or added to it.
//!
//! - Sendspin: the extra delay becomes what is added on every Sendspin
//!   speaker this PC has paired with. Speakers paired later start at their
//!   own delay.
//! - AirPlay: each AirPlay speaker this PC knew (it has settings, a
//!   password or a pairing, was played last, or was added by address)
//!   whose buffer wasn't the default gets the choice and change that make
//!   the same buffer `E`: Steady with `E − 2000` if `E` is at least
//!   1000 ms, else Short delay with `E − 300`; a HomePod on its short
//!   profile Short delay with `E − 200`. The every-speaker values go back
//!   to their defaults, so speakers found later start from the defaults.
//!   Which of those a speaker gets depends on whether it is a HomePod,
//!   which only discovery or a connect tells; so each is moved when it is
//!   next seen ([`UserConfig::settle_airplay_delay`]), from the values kept
//!   meanwhile in [`AirPlayDelayUpgrade`].
//!
//! A file this version wrote is never moved again: it says so
//! (`delay_per_speaker`). An earlier version reading it keeps working; it
//! plays AirPlay speakers at its own defaults, without what was moved.

use super::{DelayChoice, HOMEPOD_BASE_MS, SENDSPIN_ADD_MAX_MS, SHORT_BASE_MS, STEADY_BASE_MS, STEADY_MIN_MS};
use crate::user_config::{
    AirPlayDelayUpgrade, UserConfig, AIRPLAY_LATENCY_MS_DEFAULT, AIRPLAY_LATENCY_MS_MAX, AIRPLAY_LATENCY_MS_MIN,
    HOMEPOD_LATENCY_MS_DEFAULT, HOMEPOD_LATENCY_MS_MAX, HOMEPOD_LATENCY_MS_MIN,
};

/// The id prefix of a HomePod stereo pair.
const PAIR_PREFIX: &str = "airplay:pair:";

/// How an earlier version played an AirPlay speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EarlierRoute {
    /// The HomePod profile (pairs, and single HomePods by default).
    HomePodProfile,
    /// The usual AirPlay path, for a single HomePod (when single HomePods
    /// were set to the buffered stream).
    HomePodGeneral,
    /// The usual AirPlay path, for any other speaker.
    Plain,
}

/// The delay choice and change that ask a speaker for the buffer `ms` an
/// earlier version asked it for on `route`. `None` for the choice of a
/// HomePod pair, which has none.
pub fn from_buffer(ms: u32, route: EarlierRoute, pair: bool) -> (Option<DelayChoice>, i32) {
    let short_homepod = (Some(DelayChoice::Short), ms as i32 - HOMEPOD_BASE_MS as i32);
    match route {
        EarlierRoute::HomePodProfile if pair => (None, ms as i32 - HOMEPOD_BASE_MS as i32),
        EarlierRoute::HomePodProfile => short_homepod,
        EarlierRoute::HomePodGeneral | EarlierRoute::Plain if ms >= STEADY_MIN_MS => {
            (Some(DelayChoice::Steady), ms as i32 - STEADY_BASE_MS as i32)
        }
        EarlierRoute::HomePodGeneral => short_homepod,
        EarlierRoute::Plain => (Some(DelayChoice::Short), ms as i32 - SHORT_BASE_MS as i32),
    }
}

impl UserConfig {
    /// Move an earlier version's delay settings into each speaker's own,
    /// once (see the module's description). Returns whether anything was
    /// moved, which the caller then saves.
    pub fn upgrade_delays(&mut self) -> bool {
        if self.delay_per_speaker {
            return false;
        }
        self.delay_per_speaker = true;
        let mut moved = false;

        let extra = self.sendspin.player_extra_latency_ms;
        if extra != 0 {
            let paired: Vec<String> = self
                .sendspin
                .player_pairings
                .values()
                .filter_map(|p| p.instance.as_deref())
                .map(|i| format!("{}{}", crate::sendspin::server::ID_PREFIX, i.to_ascii_lowercase()))
                .collect();
            for id in paired {
                let entry = self.speaker_settings.entry(id).or_default();
                if entry.delay_change_ms == 0 {
                    entry.delay_change_ms = extra.min(SENDSPIN_ADD_MAX_MS) as i32;
                }
            }
            self.sendspin.player_extra_latency_ms = 0;
            moved = true;
        }

        let buffer = self.effective_airplay_latency_ms();
        let homepod = self.effective_homepod_latency_ms();
        let changed = buffer != AIRPLAY_LATENCY_MS_DEFAULT || homepod != HOMEPOD_LATENCY_MS_DEFAULT;
        let pending: Vec<String> = self
            .known_airplay_ids()
            .into_iter()
            .filter(|id| changed || self.speaker_settings.get(id).is_some_and(|s| s.airplay_latency_ms.is_some()))
            .collect();
        if changed {
            self.airplay_latency_ms = AIRPLAY_LATENCY_MS_DEFAULT;
            self.airplay_homepod_latency_ms = HOMEPOD_LATENCY_MS_DEFAULT;
            moved = true;
        }
        if !pending.is_empty() && self.airplay_delay_upgrade.is_none() {
            let pairs: Vec<String> = pending.iter().filter(|id| id.starts_with(PAIR_PREFIX)).cloned().collect();
            self.airplay_delay_upgrade =
                Some(AirPlayDelayUpgrade { airplay_buffer_ms: buffer, homepod_delay_ms: homepod, speakers: pending });
            // A pair is known to be HomePods by its id alone.
            for id in pairs {
                self.settle_airplay_delay(&id, true);
            }
            moved = true;
        }
        moved
    }

    /// For an AirPlay speaker whose earlier buffer is still to be moved:
    /// move it into its delay choice and change, now that whether it is a
    /// HomePod (`homepod`) is known. Returns whether the settings changed.
    pub fn settle_airplay_delay(&mut self, id: &str, homepod: bool) -> bool {
        let Some(upgrade) = self.airplay_delay_upgrade.as_mut() else {
            return false;
        };
        let Some(at) = upgrade.speakers.iter().position(|s| s == id) else {
            return false;
        };
        upgrade.speakers.remove(at);
        let (every_buffer, every_homepod) = (upgrade.airplay_buffer_ms, upgrade.homepod_delay_ms);
        if upgrade.speakers.is_empty() {
            self.airplay_delay_upgrade = None;
        }
        let pair = id.starts_with(PAIR_PREFIX);
        let route = if pair || (homepod && !self.airplay_homepod_buffered) {
            EarlierRoute::HomePodProfile
        } else if homepod {
            EarlierRoute::HomePodGeneral
        } else {
            EarlierRoute::Plain
        };
        let entry = self.speaker_settings.entry(id.to_string()).or_default();
        let own = entry.airplay_latency_ms.take();
        let ms = match route {
            EarlierRoute::HomePodProfile => {
                own.unwrap_or(every_homepod).clamp(HOMEPOD_LATENCY_MS_MIN, HOMEPOD_LATENCY_MS_MAX)
            }
            _ => own.unwrap_or(every_buffer).clamp(AIRPLAY_LATENCY_MS_MIN, AIRPLAY_LATENCY_MS_MAX),
        };
        // A speaker given a delay of its own since then keeps it.
        let untouched = entry.delay_choice.is_none() && entry.delay_change_ms == 0 && entry.steady_delay_change_ms == 0;
        if untouched {
            let (choice, change) = from_buffer(ms, route, pair);
            entry.delay_choice = choice;
            super::store_change(entry, choice, change);
        }
        if entry.is_default() {
            self.speaker_settings.remove(id);
        }
        true
    }

    /// The AirPlay speakers these settings know: with settings, a password
    /// or a pairing of their own, played last, or added by address. Not a
    /// receiver that announces another speaker's identity: nothing stored
    /// goes to it.
    fn known_airplay_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .speaker_settings
            .keys()
            .chain(self.airplay_passwords.keys())
            .chain(self.airplay_pairings.keys())
            .cloned()
            .chain(self.last_speaker_id.clone())
            .chain(self.manual_speakers.iter().map(|m| m.id()))
            .filter(|id| id.starts_with("airplay:") && !crate::airplay::discovery::is_instance_id(id))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delay::{setting, Bases, Kind, Live};
    use crate::user_config::SpeakerSettings;

    /// The speakers of these tests: a plain AirPlay speaker, a single HomePod
    /// and a HomePod pair.
    const PLAIN: &str = "airplay:AA0000000001";
    const HOMEPOD: &str = "airplay:AA0000000002";
    const PAIR: &str = "airplay:pair:BB";

    /// The buffer an earlier version asked `id` for, from its settings and
    /// what it was (the earlier rules, as `start_airplay_one` applied them).
    fn earlier_buffer(c: &UserConfig, id: &str, homepod: bool) -> u32 {
        let profile = id.starts_with(PAIR_PREFIX) || (homepod && !c.airplay_homepod_buffered);
        if profile {
            c.homepod_latency_ms_for(id)
        } else {
            c.airplay_latency_ms_for(id)
        }
    }

    fn kind_of(id: &str, homepod: bool) -> Kind {
        match (id.starts_with(PAIR_PREFIX), homepod) {
            (true, _) => Kind::HomePodPair,
            (false, true) => Kind::HomePod { buffered_on_steady: false },
            (false, false) => Kind::AirPlay { buffered_on_steady: false },
        }
    }

    /// The buffer this version asks `id` for.
    fn buffer_now(c: &UserConfig, id: &str, homepod: bool) -> u32 {
        let bases = Bases {
            steady_ms: c.effective_airplay_latency_ms(),
            homepod_ms: c.effective_homepod_latency_ms(),
            homepod_steady_by_default: c.airplay_homepod_buffered,
        };
        let stored = c.speaker_settings.get(id).cloned().unwrap_or_default();
        setting(kind_of(id, homepod), &stored, &bases, &Live::default()).buffer_ms()
    }

    /// Every combination of an earlier version's AirPlay settings, for each
    /// kind of speaker: after the move (and the speaker being seen), the
    /// speaker is asked for the same buffer as before.
    #[test]
    fn every_earlier_airplay_setting_keeps_its_delay() {
        let mut cases = 0;
        for every_buffer in [2000, 2500, 1000, 999, 500, 20, 3000] {
            for every_homepod in [200, 150, 20, 900, 2000] {
                for own in [None, Some(1500), Some(250), Some(2800), Some(999), Some(20)] {
                    for homepod_buffered in [false, true] {
                        for (id, homepod) in [(PLAIN, false), (HOMEPOD, true), (PAIR, true)] {
                            let mut c = UserConfig::default();
                            c.airplay_latency_ms = every_buffer;
                            c.airplay_homepod_latency_ms = every_homepod;
                            c.airplay_homepod_buffered = homepod_buffered;
                            c.last_speaker_id = Some(id.into());
                            if own.is_some() {
                                c.speaker_settings
                                    .insert(id.into(), SpeakerSettings { airplay_latency_ms: own, ..Default::default() });
                            }
                            let before = earlier_buffer(&c, id, homepod);
                            c.upgrade_delays();
                            c.settle_airplay_delay(id, homepod);
                            let case = format!(
                                "{id} buffer {every_buffer} homepod {every_homepod} own {own:?} homepod buffered {homepod_buffered}"
                            );
                            assert_eq!(buffer_now(&c, id, homepod), before, "{case}");
                            // The every-speaker values are back at their defaults.
                            assert_eq!(c.airplay_latency_ms, AIRPLAY_LATENCY_MS_DEFAULT, "{case}");
                            assert_eq!(c.airplay_homepod_latency_ms, HOMEPOD_LATENCY_MS_DEFAULT, "{case}");
                            assert!(c.airplay_delay_upgrade.is_none(), "{case}");
                            assert!(c.speaker_settings.get(id).is_none_or(|s| s.airplay_latency_ms.is_none()), "{case}");
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 7 * 5 * 6 * 2 * 3);
    }

    /// The choices used as examples.
    #[test]
    fn buffers_become_a_choice_and_a_change() {
        use DelayChoice::*;
        use EarlierRoute::*;
        assert_eq!(from_buffer(2000, Plain, false), (Some(Steady), 0));
        assert_eq!(from_buffer(1700, Plain, false), (Some(Steady), -300));
        assert_eq!(from_buffer(1000, Plain, false), (Some(Steady), -1000));
        assert_eq!(from_buffer(999, Plain, false), (Some(Short), 699));
        assert_eq!(from_buffer(250, Plain, false), (Some(Short), -50));
        assert_eq!(from_buffer(200, HomePodProfile, false), (Some(Short), 0));
        assert_eq!(from_buffer(150, HomePodProfile, false), (Some(Short), -50));
        assert_eq!(from_buffer(150, HomePodProfile, true), (None, -50));
        assert_eq!(from_buffer(2000, HomePodGeneral, false), (Some(Steady), 0));
        assert_eq!(from_buffer(500, HomePodGeneral, false), (Some(Short), 300));
    }

    #[test]
    fn speakers_found_later_start_from_the_defaults() {
        let mut c = UserConfig::default();
        c.airplay_latency_ms = 500;
        c.last_speaker_id = Some(PLAIN.into());
        assert!(c.upgrade_delays());
        // A speaker this PC didn't know plays the defaults.
        let later = "airplay:CC0000000003";
        assert!(!c.settle_airplay_delay(later, false));
        assert_eq!(buffer_now(&c, later, false), 2000);
        // The one it knew keeps 500 ms, once seen.
        assert!(c.settle_airplay_delay(PLAIN, false));
        assert_eq!(buffer_now(&c, PLAIN, false), 500);
        assert_eq!(c.speaker_settings[PLAIN].delay_choice, Some(DelayChoice::Short));
        // Only once.
        assert!(!c.settle_airplay_delay(PLAIN, false));
        assert!(!c.upgrade_delays());
    }

    #[test]
    fn every_speaker_the_settings_know_is_moved_and_no_other() {
        use crate::manual_speakers::{ManualKind, ManualSpeaker};
        let mut c = UserConfig::default();
        c.airplay_latency_ms = 1500;
        c.airplay_passwords.insert("airplay:A1".into(), "pw".into());
        c.speaker_settings.insert("airplay:A2".into(), SpeakerSettings { hidden: true, ..Default::default() });
        c.speaker_settings.insert("uuid:RINCON_1".into(), SpeakerSettings { hidden: true, ..Default::default() });
        c.speaker_settings
            .insert("airplay:A3/a3@other._raop._tcp.local.".into(), SpeakerSettings { hidden: true, ..Default::default() });
        c.last_speaker_id = Some("airplay:A4".into());
        let manual = ManualSpeaker { kind: ManualKind::AirPlay, name: "Den".into(), host: "10.0.0.5".into(), port: 7000 };
        c.manual_speakers.push(manual.clone());
        c.upgrade_delays();
        let pending = &c.airplay_delay_upgrade.as_ref().unwrap().speakers;
        let mut expected = vec!["airplay:A1".to_string(), "airplay:A2".into(), "airplay:A4".into(), manual.id()];
        expected.sort();
        assert_eq!(pending, &expected);
    }

    #[test]
    fn nothing_to_move_moves_nothing() {
        let mut c = UserConfig::default();
        c.last_speaker_id = Some(PLAIN.into());
        assert!(!c.upgrade_delays());
        assert!(c.delay_per_speaker);
        assert!(c.airplay_delay_upgrade.is_none());
        assert!(c.speaker_settings.is_empty());
    }

    #[test]
    fn a_speakers_own_buffer_is_moved_even_with_the_defaults() {
        let mut c = UserConfig::default();
        c.speaker_settings.insert(PLAIN.into(), SpeakerSettings { airplay_latency_ms: Some(1700), ..Default::default() });
        assert!(c.upgrade_delays());
        c.settle_airplay_delay(PLAIN, false);
        let s = &c.speaker_settings[PLAIN];
        assert_eq!((s.delay_choice, s.steady_delay_change_ms, s.airplay_latency_ms), (Some(DelayChoice::Steady), -300, None));
    }

    #[test]
    fn a_pair_is_moved_at_once() {
        let mut c = UserConfig::default();
        c.airplay_homepod_latency_ms = 120;
        c.last_speaker_id = Some(PAIR.into());
        c.upgrade_delays();
        assert!(c.airplay_delay_upgrade.is_none());
        let s = &c.speaker_settings[PAIR];
        assert_eq!((s.delay_choice, s.delay_change_ms), (None, -80));
    }

    #[test]
    fn the_sendspin_delay_goes_to_every_paired_speaker() {
        use crate::sendspin::store::PlayerPairing;
        let mut c = UserConfig::default();
        c.sendspin.player_extra_latency_ms = 150;
        let pairing = |instance: Option<&str>| PlayerPairing {
            psk: "00".repeat(32),
            name: None,
            instance: instance.map(str::to_string),
        };
        c.sendspin.player_pairings.insert("client-a".into(), pairing(Some("Desk Speaker")));
        c.sendspin.player_pairings.insert("client-b".into(), pairing(None));
        assert!(c.upgrade_delays());
        assert_eq!(c.sendspin.player_extra_latency_ms, 0);
        assert_eq!(c.speaker_settings["sendspin:desk speaker"].delay_change_ms, 150);
        assert_eq!(c.speaker_settings.len(), 1);
    }

    /// A file this version wrote, read by an earlier version (which knows
    /// none of the new keys), still loads with every setting it knows.
    #[test]
    fn an_earlier_version_reads_the_new_file() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct EarlierSpeaker {
            #[serde(default)]
            hidden: bool,
            #[serde(default)]
            airplay_latency_ms: Option<u32>,
        }
        #[derive(serde::Deserialize)]
        struct Earlier {
            #[serde(default)]
            airplay_latency_ms: u32,
            #[serde(default)]
            speaker_settings: std::collections::HashMap<String, EarlierSpeaker>,
        }
        let mut c = UserConfig::default();
        c.airplay_latency_ms = 700;
        c.last_speaker_id = Some(PLAIN.into());
        c.upgrade_delays();
        c.settle_airplay_delay(PLAIN, false);
        let text = serde_json::to_string(&c).unwrap();
        let earlier: Earlier = serde_json::from_str(&text).unwrap();
        assert_eq!(earlier.airplay_latency_ms, 2000);
        assert!(earlier.speaker_settings.contains_key(PLAIN));
    }
}
