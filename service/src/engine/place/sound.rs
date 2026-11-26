//! Real sound and short sound, from the levels of each app on each output.
//!
//! Real sound is sound above −50 dBFS for more than about 3 seconds;
//! Windows' system sounds never count, and sound muted from the panel or on
//! Windows' output counts as long as its level is measured before the mute
//! (the engine feeds the levels so). A sound that ends within about 3
//! seconds and never became real is short: where the app would first have
//! to connect a speaker, it plays it on the PC's own output instead.
//!
//! The engine feeds a sample of every app's peak about four times a
//! second; the detector says when a run of sound began, became real and
//! ended. Time comes in with each call.

use std::collections::BTreeMap;

use super::io::{AppKey, OutputRef};
use super::numbers::{LEVEL_THRESHOLD, MAX_SAMPLE_MS, REAL_ENDS_AFTER_MS, REAL_SOUND_MS, SHORT_SOUND_MS, SOUND_GAP_MS};

/// One app's peak on one output at the sample's time.
#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    pub app: AppKey,
    pub output: OutputRef,
    /// The linear peak, 0 to 1, measured before any mute.
    pub peak: f32,
    /// Windows' system sounds (they never count).
    pub system: bool,
}

/// What changed with a sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SoundChange {
    /// Sound from `app` began on `output`.
    Began { app: AppKey, output: OutputRef },
    /// It has lasted long enough to be real sound.
    Real { app: AppKey, output: OutputRef },
    /// It ended; `short` when it lasted [`SHORT_SOUND_MS`] or less and
    /// never became real.
    Ended { app: AppKey, output: OutputRef, short: bool },
}

#[derive(Clone, Debug)]
struct Run {
    began_ms: u64,
    last_loud_ms: u64,
    /// Time above the threshold, each sample counting at most
    /// [`MAX_SAMPLE_MS`].
    loud_ms: u64,
    real: bool,
    ended: bool,
}

/// Follows every app's sound on every output.
#[derive(Clone, Debug, Default)]
pub struct Detector {
    runs: BTreeMap<(AppKey, OutputRef), Run>,
    last_sample_ms: Option<u64>,
    /// The last moment real sound was heard anywhere.
    last_real_ms: Option<u64>,
}

/// Ended runs are kept this long, for what was heard in a recent window.
const KEEP_ENDED_MS: u64 = 10_000;

impl Detector {
    pub fn new() -> Self {
        Self::default()
    }

    /// One sample of the levels at `now_ms`.
    pub fn sample(&mut self, now_ms: u64, levels: &[Level]) -> Vec<SoundChange> {
        let step = match self.last_sample_ms {
            Some(last) => now_ms.saturating_sub(last).min(MAX_SAMPLE_MS),
            None => 0,
        };
        self.last_sample_ms = Some(now_ms);
        // Runs whose sound stopped before this sample end first, so that a
        // sound after a long quiet is a new run.
        let mut changes = self.advance(now_ms);
        for level in levels.iter().filter(|l| !l.system && l.peak > LEVEL_THRESHOLD) {
            let key = (level.app.clone(), level.output.clone());
            let run = self.runs.get_mut(&key).filter(|r| !r.ended);
            match run {
                Some(run) => {
                    run.loud_ms += step;
                    run.last_loud_ms = now_ms;
                }
                None => {
                    // Sound that comes back soon after real sound ended
                    // goes on being real (the next song).
                    let real = self
                        .runs
                        .get(&key)
                        .is_some_and(|r| r.real && now_ms.saturating_sub(r.last_loud_ms) <= REAL_ENDS_AFTER_MS);
                    self.runs.insert(key, Run { began_ms: now_ms, last_loud_ms: now_ms, loud_ms: 0, real, ended: false });
                    changes.push(SoundChange::Began { app: level.app.clone(), output: level.output.clone() });
                }
            }
        }
        for ((app, output), run) in self.runs.iter_mut().filter(|(_, r)| !r.ended) {
            if !run.real && run.loud_ms > REAL_SOUND_MS {
                run.real = true;
                changes.push(SoundChange::Real { app: app.clone(), output: output.clone() });
            }
            if run.real && run.last_loud_ms == now_ms {
                self.last_real_ms = Some(now_ms);
            }
        }
        changes
    }

    /// Time passes without a sample: runs whose sound has stopped end.
    pub fn advance(&mut self, now_ms: u64) -> Vec<SoundChange> {
        let mut changes = Vec::new();
        for ((app, output), run) in self.runs.iter_mut().filter(|(_, r)| !r.ended) {
            if now_ms.saturating_sub(run.last_loud_ms) > SOUND_GAP_MS {
                run.ended = true;
                let short = !run.real && run.last_loud_ms.saturating_sub(run.began_ms) <= SHORT_SOUND_MS;
                changes.push(SoundChange::Ended { app: app.clone(), output: output.clone(), short });
            }
        }
        self.runs.retain(|_, r| {
            !r.ended || now_ms.saturating_sub(r.last_loud_ms) <= KEEP_ENDED_MS.max(REAL_ENDS_AFTER_MS)
        });
        changes
    }

    /// Sound is going on on `output` now (a run that hasn't ended).
    pub fn audible_on(&self, output: &OutputRef) -> bool {
        self.runs.iter().any(|((_, o), r)| o == output && !r.ended)
    }

    /// Sound was heard on `output` in the sample taken at `now_ms`. A run in
    /// the quiet before its end isn't sounding, and between two samples
    /// nothing is: the answer counts when a sample has just been taken.
    pub fn sounding_on(&self, output: &OutputRef, now_ms: u64) -> bool {
        self.runs.iter().any(|((_, o), r)| o == output && !r.ended && r.last_loud_ms == now_ms)
    }

    /// Sound is going on now on any output that `pick` accepts.
    pub fn audible_where(&self, pick: impl Fn(&OutputRef) -> bool) -> bool {
        self.runs.iter().any(|((_, o), r)| pick(o) && !r.ended)
    }

    /// Real sound is playing on `output`: a real run whose sound was heard
    /// within the last [`REAL_ENDS_AFTER_MS`].
    pub fn real_on(&self, output: &OutputRef, now_ms: u64) -> bool {
        self.runs
            .iter()
            .any(|((_, o), r)| o == output && r.real && now_ms.saturating_sub(r.last_loud_ms) <= REAL_ENDS_AFTER_MS)
    }

    /// Real sound is playing on any output that `pick` accepts.
    pub fn real_where(&self, now_ms: u64, pick: impl Fn(&OutputRef) -> bool) -> bool {
        self.runs
            .iter()
            .any(|((_, o), r)| pick(o) && r.real && now_ms.saturating_sub(r.last_loud_ms) <= REAL_ENDS_AFTER_MS)
    }

    /// The apps sounding on `output` now: a run going on, or real sound
    /// heard within the last [`REAL_ENDS_AFTER_MS`].
    pub fn apps_on(&self, output: &OutputRef, now_ms: u64) -> Vec<AppKey> {
        let mut apps: Vec<AppKey> = self
            .runs
            .iter()
            .filter(|((_, o), r)| {
                o == output && (!r.ended || (r.real && now_ms.saturating_sub(r.last_loud_ms) <= REAL_ENDS_AFTER_MS))
            })
            .map(|((a, _), _)| a.clone())
            .collect();
        apps.dedup();
        apps
    }

    /// The apps heard above the threshold since `since_ms` on any output
    /// that `pick` accepts, with the output each was heard on.
    pub fn heard_since(&self, since_ms: u64, pick: impl Fn(&OutputRef) -> bool) -> Vec<(AppKey, OutputRef)> {
        self.runs
            .iter()
            .filter(|((_, o), r)| pick(o) && r.last_loud_ms >= since_ms)
            .map(|((a, o), _)| (a.clone(), o.clone()))
            .collect()
    }

    /// The last moment real sound was heard on any output.
    pub fn last_real_ms(&self) -> Option<u64> {
        self.last_real_ms
    }

    /// Forgets what was heard on `output` (it went away).
    pub fn forget_output(&mut self, output: &OutputRef) {
        self.runs.retain(|(_, o), _| o != output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PcOutputId;

    const LOUD: f32 = 0.01;
    const QUIET: f32 = 0.001;

    fn level(app: &str, output: &OutputRef, peak: f32) -> Level {
        Level { app: AppKey::new(app), output: output.clone(), peak, system: false }
    }

    /// Feeds `ms` of one app's sound at `peak`, a sample every 250 ms, from
    /// `t`; returns every change and the time after.
    fn feed(d: &mut Detector, t: u64, ms: u64, app: &str, output: &OutputRef, peak: f32) -> (Vec<SoundChange>, u64) {
        let mut changes = Vec::new();
        let mut t = t;
        let end = t + ms;
        while t < end {
            t += 250;
            changes.extend(d.sample(t, &[level(app, output, peak)]));
        }
        (changes, t)
    }

    #[test]
    fn real_sound_is_more_than_three_seconds() {
        let ours = OutputRef::Ours;
        let mut d = Detector::new();
        let (changes, t) = feed(&mut d, 0, 3_000, "Spotify", &ours, LOUD);
        assert!(changes.contains(&SoundChange::Began { app: AppKey::new("Spotify"), output: ours.clone() }));
        assert!(!changes.iter().any(|c| matches!(c, SoundChange::Real { .. })), "3 s isn't more than 3 s");
        assert!(d.audible_on(&ours));
        assert!(!d.real_on(&ours, t));
        let (changes, t) = feed(&mut d, t, 500, "Spotify", &ours, LOUD);
        assert_eq!(changes, vec![SoundChange::Real { app: AppKey::new("Spotify"), output: ours.clone() }]);
        assert!(d.real_on(&ours, t));
        assert_eq!(d.last_real_ms(), Some(t));
        // The gap between two songs ends the run but not real sound, and
        // the next song is real at once…
        let (changes, t2) = feed(&mut d, t, 2_000, "Spotify", &ours, QUIET);
        assert!(changes.contains(&SoundChange::Ended { app: AppKey::new("Spotify"), output: ours.clone(), short: false }));
        assert!(d.real_on(&ours, t2), "two seconds between songs is still playing");
        let (changes, t3) = feed(&mut d, t2, 250, "Spotify", &ours, LOUD);
        assert_eq!(changes, vec![SoundChange::Began { app: AppKey::new("Spotify"), output: ours.clone() }]);
        assert!(d.real_on(&ours, t3 + 4_900), "the new song carries on the real sound");
        // …a longer quiet ends it.
        let (_, t4) = feed(&mut d, t3, 6_000, "Spotify", &ours, QUIET);
        assert!(!d.real_on(&ours, t4));
    }

    #[test]
    fn a_sound_that_ends_within_three_seconds_is_short() {
        let ours = OutputRef::Ours;
        let mut d = Detector::new();
        let (_, t) = feed(&mut d, 0, 1_000, "Discord", &ours, LOUD);
        let (changes, _) = feed(&mut d, t, 2_000, "Discord", &ours, QUIET);
        assert!(changes.contains(&SoundChange::Ended { app: AppKey::new("Discord"), output: ours, short: true }));
    }

    #[test]
    fn system_sounds_never_count_and_a_late_sample_is_not_seconds_of_sound() {
        let ours = OutputRef::Ours;
        let mut d = Detector::new();
        let system = Level { app: AppKey::new("System"), output: ours.clone(), peak: 0.5, system: true };
        assert!(d.sample(0, &[system.clone()]).is_empty());
        assert!(d.sample(5_000, &[system]).is_empty());
        assert!(!d.audible_on(&ours));
        // Two loud samples 4 s apart: a new run each time, never real.
        let mut d = Detector::new();
        d.sample(0, &[level("Game", &ours, LOUD)]);
        d.sample(800, &[level("Game", &ours, LOUD)]);
        assert!(!d.real_on(&ours, 800));
        d.sample(4_000, &[level("Game", &ours, LOUD)]);
        assert!(!d.real_on(&ours, 4_000));
    }

    #[test]
    fn sound_is_followed_per_app_and_per_output() {
        let ours = OutputRef::Ours;
        let speakers = OutputRef::Pc(PcOutputId::new("speakers"));
        let mut d = Detector::new();
        let mut t = 0;
        for _ in 0..16 {
            t += 250;
            d.sample(t, &[level("Spotify", &ours, LOUD), level("Teams", &speakers, LOUD)]);
        }
        assert_eq!(d.apps_on(&ours, t), vec![AppKey::new("Spotify")]);
        assert_eq!(d.apps_on(&speakers, t), vec![AppKey::new("Teams")]);
        assert!(d.real_where(t, |o| matches!(o, OutputRef::Pc(_))));
        assert_eq!(d.heard_since(0, |o| matches!(o, OutputRef::Pc(_))), vec![(AppKey::new("Teams"), speakers.clone())]);
        d.forget_output(&speakers);
        assert!(d.apps_on(&speakers, t).is_empty());
        // Without samples, time ends the runs.
        let changes = d.advance(t + 2_000);
        assert_eq!(changes, vec![SoundChange::Ended { app: AppKey::new("Spotify"), output: ours, short: false }]);
    }
}
