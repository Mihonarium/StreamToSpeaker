//! Drives the rules for where this PC's sound plays.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use log::{debug, info, warn};

use super::driver::{local_time, SurfaceOpener as Opener};
use super::place::{self, *};
use crate::app::App;
use crate::model::command::{Command, Effect, Refusal, Surface};
use crate::model::facts::SpeakerFacts;
use crate::model::situation::{Clock, WallTime, WayChoice};
use crate::model::state::{PcOutputId, Secret, SpeakerId};
use crate::platform::{Platform, SystemEvent};

mod carry;
mod commands;
mod connect;
mod pass;
pub(crate) mod run;
mod scene;
pub(crate) mod tap;
#[cfg(test)]
mod tests;
mod watch;
mod world;

/// Resolve the terminal and web API’s transport ID to the same speaker as the panel.
pub(crate) fn speaker_for_pick(app: &App, id: &str) -> SpeakerId {
    world::fold(&super::facts::speaker_list(app), |_| None).into_iter()
        .find(|(ids, _)| ids.has(id)).map_or_else(|| SpeakerId::new(id), |(ids, _)| ids.id)
}

pub(crate) enum PlaceInput {
    Command(Surface, Command, Option<run::Answer>),
    /// The first reply to this job. Its speaker keeps the same id whichever way it was reached.
    Answered(u64),
    Event(place::Event),
    /// The worker let the old session go to try another way to its speaker.
    WayFinished { gen: u64, speaker: SpeakerId, ended: Ended },
    System(SystemEvent),
    Stop,
}

pub(crate) struct PlaceDriver {
    app: Arc<App>,
    platform: Platform,
    core: Core,
    tx: Sender<PlaceInput>,
    events: Receiver<PlaceInput>,
    opener: Arc<Mutex<Option<Opener>>>,
    started: Instant,
    #[cfg(test)]
    offset: u64,
    panel_open: bool,
    list: Vec<SpeakerFacts>,
    list_at: Option<u64>,
    ids: Vec<world::Ids>,
    bound_ids: Option<world::Ids>,
    /// A Sonos connect can bind the group's coordinator under another id.
    bound_as: Option<(u64, String, SpeakerId)>,
    seen: Option<world::Observation>,
    session: watch::SessionWatch,
    buttons: Option<watch::Buttons>,
    jobs: HashMap<u64, Arc<connect::Attempt>>,
    parked: Option<Arc<carry::Parked>>,
    threads: Vec<JoinHandle<()>>,
    start_at_sign_in: fn(bool) -> Result<(), String>,
    passing: Option<PcOutputId>,
    played: Option<SpeakerId>,
    level: Option<(SpeakerId, u8, u64)>,
    ma_before_mute: Option<u8>,
    pass: Option<pass::Worker>,
}

fn clock_since(started: Instant) -> Clock {
    Clock {
        mono_ms: started.elapsed().as_millis() as u64,
        wall: WallTime(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64),
        local: local_time(),
    }
}

impl PlaceDriver {
    pub(crate) fn new(
        app: Arc<App>,
        platform: Platform,
        opener: Arc<Mutex<Option<Opener>>>,
        now: Instant,
    ) -> Self {
        let clock = clock_since(now);
        let list = super::facts::speaker_list(&app);
        let rows = world::fold(&list, |id| app.delay_status_of(id).choice);
        let kept = {
            let cfg = app.user_config.lock().unwrap();
            cfg.sound_place.clone().unwrap_or_else(|| {
                let named = cfg.last_speaker_id.as_deref().map(|id| {
                    rows.iter()
                        .find(|(ids, _)| ids.has(id))
                        .map_or_else(|| SpeakerId::new(id), |(ids, _)| ids.id.clone())
                });
                let mut kept = Kept {
                    place: named.clone().filter(|_| !cfg.chose_this_pc),
                    first_run_done: cfg.onboarding_dismissed || named.is_some(),
                    pc_output: cfg.pc_output.as_deref().map(PcOutputId::new),
                    named,
                    ..Default::default()
                };
                if let Some((ids, row)) =
                    rows.iter().find(|(s, _)| s.airplay.is_some() && s.airplay == cfg.last_speaker_id)
                {
                    if let Some(way) = row.ways.iter().find(|w| **w != WayChoice::Native) {
                        kept.ways.insert(ids.id.clone(), *way);
                    }
                }
                for (id, settings) in &cfg.speaker_settings {
                    if let (Some(level), Some(at)) = (settings.level, settings.level_at) {
                        let speaker = rows.iter().find(|(ids, _)| ids.has(id))
                            .map_or_else(|| SpeakerId::new(id), |(ids, _)| ids.id.clone());
                        let at = WallTime(i64::try_from(at).unwrap_or(i64::MAX).saturating_mul(1000));
                        if kept.levels.get(&speaker).is_none_or(|before| before.at < at) {
                            kept.levels.insert(speaker, KeptLevel { level, at });
                        }
                    }
                }
                kept
            })
        };
        let tap = app.capture_tap.get_or_init(tap::Tap::new);
        let pass = pass::Worker::start(tap.clone(), platform.pass.clone());
        let (tx, events) = crossbeam_channel::unbounded();
        Self {
            app,
            platform,
            tx,
            events,
            opener,
            core: Core::new(kept, clock),
            started: now,
            #[cfg(test)]
            offset: 0,
            list,
            list_at: Some(clock.mono_ms),
            ids: rows.into_iter().map(|(s, _)| s).collect(),
            bound_ids: None,
            bound_as: None,
            seen: None,
            panel_open: false,
            session: Default::default(),
            buttons: None,
            jobs: HashMap::new(),
            threads: Vec::new(),
            parked: None,
            passing: None,
            played: None,
            level: None,
            ma_before_mute: None,
            pass,
            start_at_sign_in: crate::autostart::set_enabled,
        }
    }

    fn clock(&self) -> Clock {
        let clock = clock_since(self.started);
        #[cfg(test)]
        let clock = clock.later(self.offset);
        clock
    }

    #[cfg(test)]
    fn skip(&mut self, ms: u64) {
        self.offset += ms;
    }

    pub(crate) fn next_wake(&self) -> Duration {
        Duration::from_millis(self.core.next_due().map_or(250, |at| at.saturating_sub(self.clock().mono_ms).min(250)))
    }

    pub(crate) fn start(&mut self, kind: StartKind) {
        let now = self.clock();
        let obs = self.observe(now, true);
        self.event(Event::Started(kind), &obs.world, now);
        self.seen = Some(obs);
    }

    #[cfg(test)]
    pub(crate) fn situation(&mut self) -> crate::model::situation::Situation {
        let now = self.clock();
        let obs = self.observe(now, false);
        self.core.situation(&obs.world, now)
    }

    pub(crate) fn input(&mut self, input: PlaceInput) -> Option<(run::Answer, Result<Effect, Refusal>)> {
        let event = match input {
            PlaceInput::Command(from, cmd, reply) => {
                let result = self.command(from, &cmd);
                return reply.map(|answer| (answer, result));
            }
            PlaceInput::Answered(gen) => {
                let job = self.jobs.get(&gen)?;
                Event::Speaker { id: job.speaker.clone(), signal: SpeakerSignal::Responding }
            }
            PlaceInput::Event(e) => {
                if let Event::Connected { gen, ended } = &e {
                    let job = self.jobs.remove(gen)?;
                    if *ended == Ended::Ready {
                        let number = job.binding.number.load(Ordering::Acquire);
                        if let Some((id, n)) = self.bound().filter(|(_, n)| *n == number) {
                            self.bound_as = Some((n, id, job.speaker.clone()));
                        }
                    }
                    if *ended == Ended::Ready && job.resumes {
                        self.parked = None;
                        self.session = Default::default();
                    }
                }
                e
            }
            PlaceInput::WayFinished { gen, speaker, ended } => {
                // Finish a current switch before reconciling its binding. Even
                // a superseded worker can have already released the old way.
                self.input(PlaceInput::Event(Event::Connected { gen, ended: ended.clone() }));
                if self.core.connected_speaker() != Some(&speaker) || self.session_of(&speaker).is_some() {
                    return None;
                }
                Event::WayLost { speaker, ended }
            }
            PlaceInput::System(e) => {
                self.compare(Some(e));
                return None;
            }
            PlaceInput::Stop => {
                if let Some(worker) = self.pass.take() {
                    worker.stop();
                }
                for gen in self.jobs.keys().copied().collect::<Vec<_>>() {
                    self.supersede(gen);
                }
                return None;
            }
        };
        let now = self.clock();
        let obs = self.observe(now, false);
        self.event(event, &obs.world, now);
        self.remember_playback(&obs.world, now);
        None
    }

    fn event(&mut self, e: Event, world: &World, now: Clock) {
        let follows_output = matches!(e, Event::Started(_) | Event::Output(OutputEvent::DefaultChanged { .. }));
        let mut actions = self.core.event(e, world, now);
        if actions.iter().any(Do::starts_pick) {
            self.app.note_user_pick();
        }
        // Windows can choose an output without changing the place (including at launch).
        if follows_output && !actions.contains(&Do::Save) {
            let kept = self.core.kept();
            let cfg = self.app.user_config.lock().unwrap();
            let saved = cfg.sound_place.as_ref().map_or(&[][..], |k| k.last_outputs.as_slice());
            if kept.last_outputs != saved {
                actions.push(Do::Save);
            }
        }
        self.execute(actions);
    }

    pub(crate) fn look(&mut self) {
        self.compare(None);
    }
}

impl Do {
    fn starts_pick(&self) -> bool {
        matches!(self, Do::Connect(Job { purpose: Purpose::Switch | Purpose::Way | Purpose::TryAgain, .. }))
    }
}
