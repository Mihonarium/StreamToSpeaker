//! A speaker's delay: how late it plays this PC's sound, and what the
//! person removed from it or added to it.
//!
//! What was removed or added belongs to the speaker, more exactly to the
//! way it is reached (a Sonos played over AirPlay has an id, and so a
//! change, of its own). It is kept with the speaker's other settings and
//! applied every time it plays. How a change reaches the speaker depends
//! on how it is reached:
//!
//! - Sonos and other UPnP speakers fetch the stream this PC serves. The
//!   stream drops single frames or repeats them, a few at a time, until
//!   the speaker's own buffer holds that much less or more: it applies
//!   while it plays, without a gap (see [`catch_up`]).
//! - AirPlay speakers and HomePods play each packet when this PC tells
//!   them to. The delay is what this PC asks for when it connects, so a
//!   change applies by connecting again (see [`reconnect`]).
//! - The AirPlay 2 buffered stream keeps a fixed delay of its own: nothing
//!   can change it yet.
//! - Sendspin speakers: the lead this PC gives the speaker, applied by
//!   connecting again.
//! - Music Assistant sets its own speakers' delay.
//!
//! An AirPlay speaker or HomePod whose delay nobody changed here plays
//! what earlier versions' settings give it (see [`upgrade`]).
//!
//! This module holds the rules (kinds, limits, what a press does); the
//! running parts are in [`engine`].

use serde::{Deserialize, Serialize};

pub mod catch_up;
pub mod engine;
pub mod reconnect;
pub mod upgrade;

pub use engine::{ChangeOutcome, DelayEngine, DelayStatus, Notice, Progress};

/// Short delay or Steady: the coarse choice of an AirPlay speaker or a
/// single HomePod. What is removed or added is kept for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelayChoice {
    /// About 0.3 s (0.2 s on a HomePod): better for video.
    Short,
    /// About 2 s: rides out Wi-Fi hiccups.
    Steady,
}

/// What kind of speaker it is, as far as its delay goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Sonos and other UPnP speakers, and Sonos groups.
    Upnp,
    /// An AirPlay speaker that isn't a HomePod. `buffered_on_steady`: on
    /// Steady it is reached over the AirPlay 2 buffered stream, as far as
    /// what it announces tells before it has played.
    AirPlay { buffered_on_steady: bool },
    /// A single HomePod (the same `buffered_on_steady`).
    HomePod { buffered_on_steady: bool },
    /// Two HomePods playing as a stereo pair: always the short profile.
    HomePodPair,
    /// A Sendspin speaker.
    Sendspin,
    /// Music Assistant, which sets its own speakers' delay.
    MusicAssistant,
}

/// What earlier versions kept for every AirPlay speaker at once, which
/// still gives an AirPlay speaker or HomePod its delay until it has one of
/// its own: the AirPlay buffer (`airplay_latency_ms`), the HomePod delay
/// (`airplay_homepod_latency_ms`), both clamped as earlier versions read
/// them, and whether single HomePods play the buffered stream
/// (`airplay_homepod_buffered`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Earlier {
    pub buffer_ms: u32,
    pub homepod_ms: u32,
    pub homepod_buffered: bool,
}

impl Default for Earlier {
    fn default() -> Self {
        Self { buffer_ms: STEADY_BASE_MS, homepod_ms: HOMEPOD_BASE_MS, homepod_buffered: false }
    }
}

/// What AirPlay asks for on Steady, and what a Steady change starts from.
pub const STEADY_BASE_MS: u32 = crate::user_config::AIRPLAY_LATENCY_MS_DEFAULT;
/// What AirPlay asks for on Short delay.
pub const SHORT_BASE_MS: u32 = 300;
/// What a HomePod (single on Short delay, or a pair) is asked for.
pub const HOMEPOD_BASE_MS: u32 = crate::user_config::HOMEPOD_LATENCY_MS_DEFAULT;
/// The least AirPlay delay on Steady: below it an AirPlay 2 speaker is
/// driven over its realtime stream, which is what Short delay is.
pub const STEADY_MIN_MS: u32 = 1000;
/// The most on Steady.
pub const STEADY_MAX_MS: u32 = crate::user_config::AIRPLAY_LATENCY_MS_MAX;
/// The most on Short delay: one millisecond below Steady's least.
pub const SHORT_MAX_MS: u32 = STEADY_MIN_MS - 1;
/// The least delay any AirPlay speaker is asked for (the sender's own
/// pacing needs that much margin).
pub const AIRPLAY_MIN_MS: u32 = crate::user_config::AIRPLAY_LATENCY_MS_MIN;
/// The least and most a HomePod is asked for.
pub const HOMEPOD_MIN_MS: u32 = crate::user_config::HOMEPOD_LATENCY_MS_MIN;
pub const HOMEPOD_MAX_MS: u32 = crate::user_config::HOMEPOD_LATENCY_MS_MAX;
/// A Sonos's own delay, about: its prebuffer holds about 150-200 ms.
pub const UPNP_OWN_MS: u32 = 200;
/// The most that can be removed on a UPnP speaker: about its prebuffer.
pub const UPNP_REMOVE_MAX_MS: u32 = 200;
/// The most that can be added on a UPnP speaker (its listener's queue
/// holds three times that, so the stream never drops it).
pub const UPNP_ADD_MAX_MS: u32 = 1000;
/// A Sendspin speaker's delay before it has said what it needs: the
/// minimum buffer the stream assumes until the player reports its own.
pub const SENDSPIN_OWN_UNKNOWN_MS: u32 = 500;
/// The most that can be added on a Sendspin speaker, whatever its buffer.
pub const SENDSPIN_ADD_MAX_MS: u32 = 2000;
/// What the AirPlay 2 buffered stream holds, whatever is asked.
pub const BUFFERED_MS: u32 = 2000;

/// What a session tells about the speaker while it plays, beyond what its
/// settings and announcements say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Live {
    /// AirPlay: the least delay the receiver says it needs (its
    /// `Audio-Latency`), ms.
    pub receiver_min_ms: Option<u32>,
    /// AirPlay: whether the session runs the AirPlay 2 buffered stream.
    pub buffered: Option<bool>,
    /// Sendspin: the speaker's own delay (its minimum buffer and output
    /// delay), ms.
    pub sendspin_own_ms: Option<u32>,
    /// Sendspin: the most its buffer lets this PC add, ms.
    pub sendspin_add_max_ms: Option<u32>,
}

/// How far a speaker's delay can be changed from its own: up to
/// `remove_ms` removed, up to `add_ms` added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub remove_ms: u32,
    pub add_ms: u32,
}

impl Limits {
    /// `change_ms` brought within the limits.
    pub fn clamp(&self, change_ms: i64) -> i32 {
        change_ms.clamp(-(self.remove_ms as i64), self.add_ms as i64) as i32
    }
}

/// How a change reaches the speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    /// While it plays, without a gap.
    Live,
    /// By connecting again: a few seconds of silence.
    Reconnect,
}

/// Why a speaker's delay can't be changed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unchangeable {
    /// Sound plays on this PC: there is no speaker.
    NoSpeaker,
    /// Music Assistant sets its own speakers' delay.
    MusicAssistant,
    /// On Steady over AirPlay 2 the speaker keeps its delay itself; Short
    /// delay is the way to less.
    KeptBySpeakerOnSteady,
}

/// A speaker's delay as its settings (and its session, while it plays)
/// make it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting {
    /// Short delay or Steady, for an AirPlay speaker or a single HomePod.
    pub choice: Option<DelayChoice>,
    /// The speaker's own delay, with nothing removed or added, ms.
    pub own_ms: u32,
    /// What was removed (negative) or added (positive), ms, within the
    /// limits.
    pub change_ms: i32,
    /// How far it can go; `None` where nothing can change.
    pub limits: Option<Limits>,
    pub applies: Result<Applies, Unchangeable>,
    /// The least delay the session can get, whatever is asked (AirPlay:
    /// the receiver's own minimum), ms.
    pub floor_ms: u32,
}

impl Setting {
    /// The speaker's delay as far as the app knows it, change included.
    pub fn delay_ms(&self) -> u32 {
        ((self.own_ms as i64 + self.change_ms as i64).max(0) as u32).max(self.floor_ms)
    }

    /// What `request` makes of it: the change it ends at (stopped at a
    /// limit) and whether a limit stopped it. `None` where nothing can
    /// change.
    pub fn after(&self, request: Request) -> Option<(i32, bool)> {
        let limits = self.limits?;
        let wanted = match request {
            Request::Remove(ms) => self.change_ms as i64 - ms as i64,
            Request::Add(ms) => self.change_ms as i64 + ms as i64,
            Request::Set(ms) => ms as i64,
        };
        let to = limits.clamp(wanted);
        Some((to, to as i64 != wanted))
    }

    /// The buffer an AirPlay or HomePod session asks for, ms.
    pub fn buffer_ms(&self) -> u32 {
        (self.own_ms as i64 + self.change_ms as i64).max(0) as u32
    }
}

/// A change someone asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Remove this many ms of delay.
    Remove(u32),
    /// Add this many.
    Add(u32),
    /// Make the change exactly this (negative: removed), for scripts.
    Set(i32),
}

/// Whether a speaker of `kind` has a delay choice.
pub fn has_choice(kind: Kind) -> bool {
    matches!(kind, Kind::AirPlay { .. } | Kind::HomePod { .. })
}

/// Whether a speaker of `kind` is played over AirPlay (and so may have no
/// delay of its own yet, see [`upgrade`]).
fn is_airplay(kind: Kind) -> bool {
    matches!(kind, Kind::AirPlay { .. } | Kind::HomePod { .. } | Kind::HomePodPair)
}

/// The delay choice and what was removed or added for a speaker of
/// `kind`: its own, or for an AirPlay speaker without one, what the
/// earlier settings give it.
pub fn choice_and_change(
    kind: Kind,
    stored: &crate::user_config::SpeakerSettings,
    earlier: &Earlier,
) -> (Option<DelayChoice>, i32) {
    if is_airplay(kind) && stored.delay_choice.is_none() {
        return upgrade::from_earlier(kind, stored.airplay_latency_ms, earlier);
    }
    let choice = stored.delay_choice.filter(|_| has_choice(kind));
    (choice, stored_change(stored, choice))
}

/// Store `change_ms` as what is removed from or added to a speaker of
/// `kind` on `choice`. An AirPlay speaker's delay is its own from then on:
/// its choice is kept (also for a HomePod pair, which has none, to say
/// so) and the earlier own-buffer setting is dropped, after what the
/// earlier settings gave it is kept for its current choice.
pub fn store(
    stored: &mut crate::user_config::SpeakerSettings,
    kind: Kind,
    earlier: &Earlier,
    choice: Option<DelayChoice>,
    change_ms: i32,
) {
    if is_airplay(kind) && stored.delay_choice.is_none() {
        let (was, was_change) = choice_and_change(kind, stored, earlier);
        store_change(stored, was, was_change);
        stored.delay_choice = Some(was.unwrap_or(DelayChoice::Short));
        stored.airplay_latency_ms = None;
    }
    if let Some(c) = choice.filter(|_| has_choice(kind)) {
        stored.delay_choice = Some(c);
    }
    store_change(stored, choice, change_ms);
}

/// The delay of a speaker of `kind` with `stored` settings: its choice,
/// its own delay, what was removed or added (see [`choice_and_change`]),
/// its limits and how a change reaches it.
pub fn setting(kind: Kind, stored: &crate::user_config::SpeakerSettings, earlier: &Earlier, live: &Live) -> Setting {
    let (choice, raw) = choice_and_change(kind, stored, earlier);
    let make = |own_ms: u32, limits: Limits, applies: Applies, floor_ms: u32| Setting {
        choice,
        own_ms,
        change_ms: limits.clamp(raw as i64),
        limits: Some(limits),
        applies: Ok(applies),
        floor_ms,
    };
    let fixed = |own_ms: u32, why: Unchangeable| Setting {
        choice,
        own_ms,
        change_ms: 0,
        limits: None,
        applies: Err(why),
        floor_ms: 0,
    };
    // A range of delays around `base` as the change's limits.
    let around = |base: u32, least: u32, most: u32| Limits {
        remove_ms: base.saturating_sub(least),
        add_ms: most.saturating_sub(base),
    };
    match kind {
        Kind::Upnp => make(
            UPNP_OWN_MS,
            Limits { remove_ms: UPNP_REMOVE_MAX_MS, add_ms: UPNP_ADD_MAX_MS },
            Applies::Live,
            0,
        ),
        Kind::AirPlay { buffered_on_steady } | Kind::HomePod { buffered_on_steady }
            if choice == Some(DelayChoice::Steady) =>
        {
            if live.buffered.unwrap_or(buffered_on_steady) {
                return fixed(BUFFERED_MS, Unchangeable::KeptBySpeakerOnSteady);
            }
            let floor = live.receiver_min_ms.unwrap_or(0);
            make(STEADY_BASE_MS, around(STEADY_BASE_MS, STEADY_MIN_MS, STEADY_MAX_MS), Applies::Reconnect, floor)
        }
        Kind::AirPlay { .. } => {
            let least = live.receiver_min_ms.unwrap_or(0).max(AIRPLAY_MIN_MS).min(SHORT_BASE_MS);
            let floor = live.receiver_min_ms.unwrap_or(0);
            make(SHORT_BASE_MS, around(SHORT_BASE_MS, least, SHORT_MAX_MS), Applies::Reconnect, floor)
        }
        Kind::HomePod { .. } | Kind::HomePodPair => {
            make(HOMEPOD_BASE_MS, around(HOMEPOD_BASE_MS, HOMEPOD_MIN_MS, HOMEPOD_MAX_MS), Applies::Reconnect, 0)
        }
        Kind::Sendspin => {
            let own = live.sendspin_own_ms.unwrap_or(SENDSPIN_OWN_UNKNOWN_MS);
            let add = live.sendspin_add_max_ms.unwrap_or(SENDSPIN_ADD_MAX_MS).min(SENDSPIN_ADD_MAX_MS);
            make(own, Limits { remove_ms: 0, add_ms: add }, Applies::Reconnect, 0)
        }
        Kind::MusicAssistant => fixed(0, Unchangeable::MusicAssistant),
    }
}

/// What is stored as removed or added for `choice` (Steady keeps its own).
pub fn stored_change(stored: &crate::user_config::SpeakerSettings, choice: Option<DelayChoice>) -> i32 {
    match choice {
        Some(DelayChoice::Steady) => stored.steady_delay_change_ms,
        _ => stored.delay_change_ms,
    }
}

/// Store `change_ms` as what is removed or added for `choice`.
pub fn store_change(stored: &mut crate::user_config::SpeakerSettings, choice: Option<DelayChoice>, change_ms: i32) {
    match choice {
        Some(DelayChoice::Steady) => stored.steady_delay_change_ms = change_ms,
        _ => stored.delay_change_ms = change_ms,
    }
}

/// Milliseconds as frames at the wire rate.
pub fn ms_to_frames(ms: i64) -> i64 {
    ms * crate::WIRE_SAMPLE_RATE as i64 / 1000
}

/// Frames as whole milliseconds at the wire rate, rounded to the nearest.
pub fn frames_to_ms(frames: i64) -> i64 {
    let rate = crate::WIRE_SAMPLE_RATE as i64;
    (frames * 1000 + frames.signum() * rate / 2) / rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::SpeakerSettings;

    /// A speaker's own delay: `change` on Short delay (or with no choice),
    /// `steady` on Steady, and its choice (Short for a speaker without
    /// one, which says the delay is its own).
    fn with(change: i32, steady: i32, choice: Option<DelayChoice>) -> SpeakerSettings {
        SpeakerSettings {
            delay_change_ms: change,
            steady_delay_change_ms: steady,
            delay_choice: Some(choice.unwrap_or(DelayChoice::Short)),
            ..Default::default()
        }
    }

    const PLAIN: Kind = Kind::AirPlay { buffered_on_steady: false };
    const HOMEPOD: Kind = Kind::HomePod { buffered_on_steady: false };

    /// The ranges of each kind of speaker: (kind, choice, own delay,
    /// removed at most, added at most, how a change applies).
    #[test]
    fn each_kind_of_speaker_has_its_range() {
        use DelayChoice::*;
        let b = Earlier::default();
        let none = Live::default();
        let table: &[(Kind, Option<DelayChoice>, u32, u32, u32, Applies)] = &[
            (Kind::Upnp, None, 200, 200, 1000, Applies::Live),
            (PLAIN, Some(Steady), 2000, 1000, 1000, Applies::Reconnect),
            (PLAIN, Some(Short), 300, 280, 699, Applies::Reconnect),
            (HOMEPOD, Some(Short), 200, 180, 1800, Applies::Reconnect),
            (HOMEPOD, Some(Steady), 2000, 1000, 1000, Applies::Reconnect),
            (Kind::HomePodPair, None, 200, 180, 1800, Applies::Reconnect),
            (Kind::Sendspin, None, 500, 0, 2000, Applies::Reconnect),
        ];
        for &(kind, choice, own, remove, add, applies) in table {
            let s = setting(kind, &with(0, 0, choice), &b, &none);
            assert_eq!(s.choice, choice, "{kind:?}");
            assert_eq!(s.own_ms, own, "{kind:?} {choice:?}");
            assert_eq!(s.limits, Some(Limits { remove_ms: remove, add_ms: add }), "{kind:?} {choice:?}");
            assert_eq!(s.applies, Ok(applies), "{kind:?} {choice:?}");
        }
    }

    #[test]
    fn defaults_are_steady_for_airplay_and_short_for_homepods() {
        let b = Earlier::default();
        let none = Live::default();
        let d = SpeakerSettings::default();
        assert_eq!(setting(PLAIN, &d, &b, &none).choice, Some(DelayChoice::Steady));
        assert_eq!(setting(HOMEPOD, &d, &b, &none).choice, Some(DelayChoice::Short));
        assert_eq!(setting(Kind::HomePodPair, &d, &b, &none).choice, None);
        assert_eq!(setting(Kind::Upnp, &d, &b, &none).choice, None);
        // The earlier "buffered stream for single HomePods" makes Steady
        // their default.
        let buffered = Earlier { homepod_buffered: true, ..b };
        assert_eq!(setting(HOMEPOD, &d, &buffered, &none).choice, Some(DelayChoice::Steady));
        // A choice stored for a speaker that has none (a pair) is ignored.
        let s = setting(Kind::HomePodPair, &with(-50, 0, Some(DelayChoice::Steady)), &b, &none);
        assert_eq!((s.choice, s.change_ms), (None, -50));
    }

    #[test]
    fn each_choice_keeps_its_own_change() {
        let b = Earlier::default();
        let none = Live::default();
        let s = with(100, -300, Some(DelayChoice::Steady));
        assert_eq!(setting(PLAIN, &s, &b, &none).change_ms, -300);
        assert_eq!(setting(PLAIN, &s, &b, &none).buffer_ms(), 1700);
        let s = with(100, -300, Some(DelayChoice::Short));
        assert_eq!(setting(PLAIN, &s, &b, &none).change_ms, 100);
        assert_eq!(setting(PLAIN, &s, &b, &none).buffer_ms(), 400);
    }

    #[test]
    fn a_press_stops_at_the_limit_and_says_so() {
        let b = Earlier::default();
        let none = Live::default();
        // Remove 100 ms at 150 ms removed on a Sonos: 200 removed, stopped.
        let s = setting(Kind::Upnp, &with(-150, 0, None), &b, &none);
        assert_eq!(s.after(Request::Remove(100)), Some((-200, true)));
        assert_eq!(s.after(Request::Remove(25)), Some((-175, false)));
        assert_eq!(s.after(Request::Add(1500)), Some((1000, true)));
        assert_eq!(s.after(Request::Set(-150)), Some((-150, false)));
        assert_eq!(s.after(Request::Set(-5000)), Some((-200, true)));
        // From 50 removed, Add 25 twice is back at the speaker's own delay.
        let s = setting(Kind::Upnp, &with(-50, 0, None), &b, &none);
        let (once, _) = s.after(Request::Add(25)).unwrap();
        let s = setting(Kind::Upnp, &with(once, 0, None), &b, &none);
        assert_eq!(s.after(Request::Add(25)), Some((0, false)));
        // Nothing can be removed from a Sendspin speaker.
        let s = setting(Kind::Sendspin, &SpeakerSettings::default(), &b, &none);
        assert_eq!(s.after(Request::Remove(25)), Some((0, true)));
    }

    #[test]
    fn a_stored_change_beyond_the_limits_counts_as_the_limit() {
        let b = Earlier::default();
        // A Sendspin speaker whose buffer holds less than what was stored.
        let live = Live { sendspin_own_ms: Some(300), sendspin_add_max_ms: Some(800), ..Default::default() };
        let s = setting(Kind::Sendspin, &with(1000, 0, None), &b, &live);
        assert_eq!((s.own_ms, s.change_ms, s.delay_ms()), (300, 800, 1100));
        // A press works from what applies, not from what was stored.
        assert_eq!(s.after(Request::Remove(25)), Some((775, false)));
    }

    #[test]
    fn the_receivers_own_minimum_limits_short_delay() {
        let b = Earlier::default();
        let live = Live { receiver_min_ms: Some(100), ..Default::default() };
        let s = setting(PLAIN, &with(0, 0, Some(DelayChoice::Short)), &b, &live);
        assert_eq!(s.limits, Some(Limits { remove_ms: 200, add_ms: 699 }));
        // And the delay never reads below it.
        let s = setting(PLAIN, &with(0, -1000, Some(DelayChoice::Steady)), &b, &Live { receiver_min_ms: Some(1500), ..Default::default() });
        assert_eq!(s.delay_ms(), 1500);
    }

    #[test]
    fn the_buffered_stream_on_steady_cant_be_changed() {
        let b = Earlier::default();
        let buffered = Kind::AirPlay { buffered_on_steady: true };
        let s = setting(buffered, &with(0, -300, Some(DelayChoice::Steady)), &b, &Live::default());
        assert_eq!((s.applies, s.limits, s.delay_ms()), (Err(Unchangeable::KeptBySpeakerOnSteady), None, 2000));
        assert_eq!(s.after(Request::Remove(25)), None);
        // Short delay on the same speaker can.
        let s = setting(buffered, &with(0, 0, Some(DelayChoice::Short)), &b, &Live::default());
        assert_eq!(s.applies, Ok(Applies::Reconnect));
        // What the session runs decides once it plays.
        let d = SpeakerSettings::default();
        let s = setting(buffered, &d, &b, &Live { buffered: Some(false), ..Default::default() });
        assert_eq!(s.applies, Ok(Applies::Reconnect));
        let s = setting(PLAIN, &d, &b, &Live { buffered: Some(true), ..Default::default() });
        assert_eq!(s.applies, Err(Unchangeable::KeptBySpeakerOnSteady));
        let s = setting(Kind::MusicAssistant, &d, &b, &Live::default());
        assert_eq!(s.applies, Err(Unchangeable::MusicAssistant));
    }

    #[test]
    fn storing_a_change_makes_the_delay_the_speakers_own() {
        let b = Earlier { buffer_ms: 1700, ..Earlier::default() };
        let none = Live::default();
        // Nothing stored: the earlier every-speaker buffer gives Steady with
        // 300 ms removed.
        let mut s = SpeakerSettings::default();
        assert_eq!(choice_and_change(PLAIN, &s, &b), (Some(DelayChoice::Steady), -300));
        // Removing 25 ms more keeps it there, as its own.
        store(&mut s, PLAIN, &b, Some(DelayChoice::Steady), -325);
        assert_eq!((s.delay_choice, s.steady_delay_change_ms), (Some(DelayChoice::Steady), -325));
        // Whatever the earlier setting becomes.
        let other = Earlier { buffer_ms: 500, ..b };
        assert_eq!(setting(PLAIN, &s, &other, &none).buffer_ms(), 1675);
        // A switch of choice keeps what the earlier settings gave the other.
        let mut s = SpeakerSettings { airplay_latency_ms: Some(250), ..Default::default() };
        store(&mut s, PLAIN, &b, Some(DelayChoice::Steady), 0);
        assert_eq!((s.delay_change_ms, s.steady_delay_change_ms, s.airplay_latency_ms), (-50, 0, None));
        // A pair has no choice, but its delay is its own too.
        let mut s = SpeakerSettings::default();
        store(&mut s, Kind::HomePodPair, &b, None, -20);
        assert_eq!((s.delay_choice.is_some(), s.delay_change_ms), (true, -20));
        assert_eq!(setting(Kind::HomePodPair, &s, &Earlier { homepod_ms: 900, ..b }, &none).buffer_ms(), 180);
        // Other speakers keep a plain change.
        let mut s = SpeakerSettings::default();
        store(&mut s, Kind::Upnp, &b, None, -50);
        assert_eq!((s.delay_choice, s.delay_change_ms), (None, -50));
    }

    #[test]
    fn milliseconds_and_frames() {
        assert_eq!(ms_to_frames(25), 1102);
        assert_eq!(ms_to_frames(-100), -4410);
        assert_eq!(frames_to_ms(1102), 25);
        assert_eq!(frames_to_ms(-4410), -100);
        assert_eq!(frames_to_ms(22), 0);
        assert_eq!(frames_to_ms(23), 1);
    }
}
