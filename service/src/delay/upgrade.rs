//! Earlier versions' delay settings, so that every speaker sounds the same
//! after the update as before.
//!
//! Earlier versions kept delay for every speaker at once: one AirPlay
//! buffer (2000 ms unless changed), one HomePod delay (200 ms), whether
//! single HomePods play the buffered stream, a speaker's own AirPlay buffer
//! where one was set, and one extra delay for every Sendspin speaker. This
//! version keeps, per speaker, a delay choice (Short delay or Steady, for
//! AirPlay speakers and single HomePods) and what was removed from or
//! added to it.
//!
//! - AirPlay: a speaker whose delay hasn't been changed here plays the
//!   buffer `E` the earlier settings give it, as the choice and change
//!   that make the same buffer: Steady with `E − 2000` if `E` is at least
//!   1000 ms, else Short delay with `E − 300`; a HomePod on its short
//!   profile Short delay with `E − 200` ([`from_buffer`]). That is worked
//!   out each time it is needed, because whether a speaker is a HomePod is
//!   known only from discovery or a connect, and so that a speaker found
//!   after the update plays as the earlier settings would have played it.
//!   The first change made here stores the choice and change as the
//!   speaker's own (see [`super::store`]).
//! - Sendspin: the extra delay becomes what is added on every Sendspin
//!   speaker this PC has paired with ([`UserConfig::upgrade_delays`]).
//!   Speakers paired later start at their own delay.
//!
//! The earlier settings stay where earlier versions read them, so an
//! earlier version reading the file plays every speaker as before (without
//! the delays changed here since).

use super::{DelayChoice, Earlier, Kind, HOMEPOD_BASE_MS, SENDSPIN_ADD_MAX_MS, SHORT_BASE_MS, STEADY_BASE_MS, STEADY_MIN_MS};
use crate::user_config::{
    UserConfig, AIRPLAY_LATENCY_MS_MAX, AIRPLAY_LATENCY_MS_MIN, HOMEPOD_LATENCY_MS_MAX, HOMEPOD_LATENCY_MS_MIN,
};

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

/// The choice and change the earlier settings give an AirPlay speaker of
/// `kind` with its own earlier buffer `own` (if it had one).
pub fn from_earlier(kind: Kind, own: Option<u32>, earlier: &Earlier) -> (Option<DelayChoice>, i32) {
    let pair = kind == Kind::HomePodPair;
    let route = match kind {
        Kind::HomePodPair => EarlierRoute::HomePodProfile,
        Kind::HomePod { .. } if !earlier.homepod_buffered => EarlierRoute::HomePodProfile,
        Kind::HomePod { .. } => EarlierRoute::HomePodGeneral,
        _ => EarlierRoute::Plain,
    };
    let ms = match route {
        EarlierRoute::HomePodProfile => own.unwrap_or(earlier.homepod_ms).clamp(HOMEPOD_LATENCY_MS_MIN, HOMEPOD_LATENCY_MS_MAX),
        _ => own.unwrap_or(earlier.buffer_ms).clamp(AIRPLAY_LATENCY_MS_MIN, AIRPLAY_LATENCY_MS_MAX),
    };
    from_buffer(ms, route, pair)
}

impl UserConfig {
    /// The earlier every-speaker AirPlay settings, as earlier versions
    /// read them.
    pub fn earlier_delays(&self) -> Earlier {
        Earlier {
            buffer_ms: self.effective_airplay_latency_ms(),
            homepod_ms: self.effective_homepod_latency_ms(),
            homepod_buffered: self.airplay_homepod_buffered,
        }
    }

    /// Move the extra delay earlier versions gave every Sendspin speaker
    /// to each speaker this PC has paired with, once. Returns whether
    /// anything moved, which the caller then saves.
    pub fn upgrade_delays(&mut self) -> bool {
        let extra = self.sendspin.player_extra_latency_ms;
        if extra == 0 {
            return false;
        }
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
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delay::{setting, Live};
    use crate::user_config::{SpeakerSettings, AIRPLAY_LATENCY_MS_DEFAULT, HOMEPOD_LATENCY_MS_DEFAULT};

    /// The speakers of these tests: a plain AirPlay speaker, a single HomePod
    /// and a HomePod pair.
    const PLAIN: &str = "airplay:AA0000000001";
    const HOMEPOD: &str = "airplay:AA0000000002";
    const PAIR: &str = "airplay:pair:BB";

    fn kind_of(id: &str) -> Kind {
        match id {
            PAIR => Kind::HomePodPair,
            HOMEPOD => Kind::HomePod { buffered_on_steady: false },
            _ => Kind::AirPlay { buffered_on_steady: false },
        }
    }

    /// The buffer an earlier version asked `id` for, from its settings and
    /// what it was (the earlier rules, as `start_airplay_one` applied them).
    fn earlier_buffer(c: &UserConfig, id: &str) -> u32 {
        let profile = id == PAIR || (id == HOMEPOD && !c.airplay_homepod_buffered);
        if profile {
            c.homepod_latency_ms_for(id)
        } else {
            c.airplay_latency_ms_for(id)
        }
    }

    /// The buffer this version asks `id` for.
    fn buffer_now(c: &UserConfig, id: &str) -> u32 {
        let stored = c.speaker_settings.get(id).cloned().unwrap_or_default();
        setting(kind_of(id), &stored, &c.earlier_delays(), &Live::default()).buffer_ms()
    }

    /// Every combination of an earlier version's AirPlay settings, for each
    /// kind of speaker: this version asks the speaker for the same buffer.
    #[test]
    fn every_earlier_airplay_setting_keeps_its_delay() {
        let mut cases = 0;
        for every_buffer in [2000, 2500, 1000, 999, 500, 20, 3000, 0, 9999] {
            for every_homepod in [200, 150, 20, 900, 2000, 0] {
                for own in [None, Some(1500), Some(250), Some(2800), Some(999), Some(20), Some(5)] {
                    for homepod_buffered in [false, true] {
                        for id in [PLAIN, HOMEPOD, PAIR] {
                            let mut c = UserConfig::default();
                            c.airplay_latency_ms = every_buffer;
                            c.airplay_homepod_latency_ms = every_homepod;
                            c.airplay_homepod_buffered = homepod_buffered;
                            if own.is_some() {
                                c.speaker_settings
                                    .insert(id.into(), SpeakerSettings { airplay_latency_ms: own, ..Default::default() });
                            }
                            let case = format!(
                                "{id} buffer {every_buffer} homepod {every_homepod} own {own:?} homepod buffered {homepod_buffered}"
                            );
                            assert_eq!(buffer_now(&c, id), earlier_buffer(&c, id), "{case}");
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 9 * 6 * 7 * 2 * 3);
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

    /// A speaker found after the update plays as the earlier settings
    /// would have played it, and a change here makes its delay its own.
    #[test]
    fn a_speaker_found_later_plays_as_before_until_changed_here() {
        let mut c = UserConfig::default();
        c.airplay_latency_ms = 500;
        let later = "airplay:CC0000000003";
        assert_eq!(buffer_now(&c, later), 500);
        let kind = kind_of(later);
        let earlier = c.earlier_delays();
        let entry = c.speaker_settings.entry(later.into()).or_default();
        crate::delay::store(entry, kind, &earlier, Some(DelayChoice::Short), 175);
        c.airplay_latency_ms = AIRPLAY_LATENCY_MS_DEFAULT;
        assert_eq!(buffer_now(&c, later), 475);
        assert_eq!(c.airplay_homepod_latency_ms, HOMEPOD_LATENCY_MS_DEFAULT);
    }

    #[test]
    fn the_sendspin_delay_goes_to_every_paired_speaker() {
        use crate::sendspin::store::PlayerPairing;
        let mut c = UserConfig::default();
        assert!(!c.upgrade_delays(), "nothing to move");
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
        // Once: nothing left to move.
        assert!(!c.upgrade_delays());
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
        struct EarlierFile {
            #[serde(default)]
            airplay_latency_ms: u32,
            #[serde(default)]
            speaker_settings: std::collections::HashMap<String, EarlierSpeaker>,
        }
        let mut c = UserConfig::default();
        c.airplay_latency_ms = 700;
        let earlier = c.earlier_delays();
        let entry = c.speaker_settings.entry(PLAIN.into()).or_default();
        crate::delay::store(entry, kind_of(PLAIN), &earlier, Some(DelayChoice::Short), 100);
        let text = serde_json::to_string(&c).unwrap();
        let read: EarlierFile = serde_json::from_str(&text).unwrap();
        assert_eq!(read.airplay_latency_ms, 700);
        assert!(read.speaker_settings.contains_key(PLAIN));
    }
}
