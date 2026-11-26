//! Drives the rules for where this PC's sound plays.

use std::collections::{HashMap, VecDeque};
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
    WindowsNotTaken { decision: u64, at_ms: u64 },
    /// A connect's work let this speaker's session go.
    Released(SpeakerId),
    /// How a connect ended and which sessions it let go.
    Connected { gen: u64, ended: Ended, released: Vec<(String, u64)> },
    /// A change of way or delay let its speaker's session go and then failed.
    WayFinished { speaker: SpeakerId, ended: Ended },
    System(SystemEvent),
    Stop,
}

pub(crate) struct PlaceDriver {
    app: Arc<App>,
    platform: Platform,
    windows_decision: Arc<Mutex<u64>>,
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
    /// A release can arrive after another session has taken the coordinator's id.
    recent_bindings: VecDeque<(u64, SpeakerId)>,
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
    /// A `--player` request waits while a change of Windows' output is told
    /// apart: a person who is choosing an output goes first.
    held: VecDeque<(Surface, Command, Option<run::Answer>)>,
    /// The answer to a held request, once it was carried out.
    held_answers: Vec<(run::Answer, Result<Effect, Refusal>)>,
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
            windows_decision: Arc::default(),
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
            recent_bindings: VecDeque::new(),
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
            held: VecDeque::new(),
            held_answers: Vec::new(),
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
        let mut lost_binding = None;
        let event = match input {
            PlaceInput::Command(from, cmd, reply) => {
                if matches!(cmd, Command::PlayWhenListed { .. } | Command::PlayAfterPairing { .. }) && self.core.deciding_output() {
                    self.held.push_back((from, cmd, reply));
                    return None;
                }
                let result = self.command(from, &cmd);
                return reply.map(|answer| (answer, result));
            }
            PlaceInput::Answered(gen) => {
                let job = self.jobs.get(&gen)?;
                Event::Speaker { id: job.speaker.clone(), signal: SpeakerSignal::Responding }
            }
            PlaceInput::WindowsNotTaken { decision, at_ms } => {
                if decision != *self.windows_decision.lock().unwrap() { return None; }
                Event::Output(OutputEvent::NotTaken { at_ms })
            }
            PlaceInput::Event(e) => {
                if let Event::Connected { gen, ended } = &e {
                    let job = self.jobs.remove(gen)?;
                    let number = job.binding.number.load(Ordering::Acquire);
                    if *ended == Ended::Ready && number != 0 {
                        self.recent_bindings.push_front((number, job.speaker.clone()));
                        self.recent_bindings.truncate(32);
                        if let Some((id, n)) = self.bound().filter(|(_, n)| *n == number) {
                            self.bound_as = Some((n, id, job.speaker.clone()));
                        } else {
                            lost_binding = Some(job.speaker.clone());
                        }
                    }
                    if *ended == Ended::Ready && job.resumes {
                        self.parked = None;
                        self.session = Default::default();
                    }
                }
                e
            }
            PlaceInput::Released(id) => {
                if self.session_of(&id).is_some() { return None; }
                Event::PlaceReleased(id)
            }
            PlaceInput::Connected { gen, ended, released } => {
                self.finished(gen, ended, released);
                return None;
            }
            PlaceInput::WayFinished { speaker, ended } => {
                if self.session_of(&speaker).is_some() { return None; }
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
                    self.cancel_job(gen);
                }
                return None;
            }
        };
        let now = self.clock();
        let obs = self.observe(now, false);
        self.event(event, &obs.world, now);
        if let Some(speaker) = lost_binding {
            let _ = self.tx.send(PlaceInput::Released(speaker));
        }
        self.remember_playback(&obs.world, now);
        None
    }

    fn event(&mut self, e: Event, world: &World, now: Clock) {
        let follows_output = matches!(e, Event::Started(_) | Event::Output(OutputEvent::DefaultChanged { .. }));
        let mut actions = self.core.event(e, world, now);
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
        if !self.core.deciding_output() {
            while let Some((from, cmd, reply)) = self.held.pop_front() {
                let result = self.command(from, &cmd);
                if let Some(answer) = reply {
                    self.held_answers.push((answer, result));
                }
            }
        }
    }
}

impl Do {
    fn starts_pick(&self) -> bool {
        matches!(self, Do::Connect(Job { purpose: Purpose::Switch | Purpose::Way | Purpose::TryAgain, .. }))
    }
}
