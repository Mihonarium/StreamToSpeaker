//! One cancellable worker for each connection the core asks for.

use super::*;
use crate::app::{TakenOff, CODE_WINDOW_CLOSED, SUPERSEDED};
use crate::delay::{DelayChoice, Progress};
use crate::engine::{cancel, driver};

pub(super) struct Attempt {
    pub binding: Arc<cancel::Binding>,
    pub speaker: SpeakerId,
    pub resumes: bool,
}

impl PlaceDriver {
    pub(super) fn supersede(&mut self, gen: u64) {
        if let Some(job) = self.jobs.remove(&gen) {
            job.binding.cancelled.store(true, Ordering::SeqCst);
            self.threads.extend(self.app.release_job(&job.binding));
        }
    }

    pub(super) fn connect(&mut self, job: Job) {
        let gen = job.gen;
        let id = self.app_id(&job.speaker, job.way);
        let app = self.app.clone();
        let tx = self.tx.clone();
        let platform = self.platform.clone();
        let own = self.session_of(&job.speaker);
        let chime_here = self.core.connected_speaker() == Some(&job.speaker) && own.is_some();
        let parked = self.parked.clone().filter(|p| p.speaker == job.speaker && job.purpose != Purpose::Chime);
        let answers = tx.clone();
        let binding = Arc::new(cancel::Binding::new(move || {
            let _ = answers.send(PlaceInput::Answered(gen));
        }));
        let attempt = Arc::new(Attempt { binding, speaker: job.speaker.clone(), resumes: parked.is_some() });
        self.jobs.insert(gen, attempt.clone());
        let speaker = job.speaker.clone();
        self.app.engine_connect_started(speaker.as_str(), gen);
        let worker = Worker { app, target: id.clone(), job, own, chime_here, parked, attempt };
        let spawned = std::thread::Builder::new().name("stream-to-speaker-connect".into()).spawn(move || {
            worker.run(&platform, tx);
        });
        match spawned {
            Ok(t) => self.threads.push(t),
            Err(e) => {
                warn!("couldn't start the connection to {id}: {e}");
                self.app.engine_connect_ended(speaker.as_str(), gen, Err(format!("No thread to connect to {id}.")));
                let _ = self.tx.send(PlaceInput::Event(Event::Connected { gen, ended: Ended::CouldntStart }));
            }
        }
    }
}

struct Worker {
    app: Arc<App>,
    target: String,
    job: Job,
    own: Option<(String, u64)>,
    chime_here: bool,
    parked: Option<Arc<super::carry::Parked>>,
    attempt: Arc<Attempt>,
}

impl Worker {
    fn run(&self, platform: &Platform, tx: Sender<PlaceInput>) {
        let _scope = self.attempt.binding.enter();
        let (app, target, gen) = (&self.app, self.target.as_str(), self.job.gen);
        let mut released_way = false;
        let (ended, result) = match self.connect(&mut released_way) {
            Ok(Ended::Ready) => (Ended::Ready, Ok(())),
            Ok(Ended::Busy { who }) => {
                let name = app.speaker_name(target).unwrap_or_else(|| target.to_string());
                let words = crate::model::view::busy_reply(crate::model::Lang::English, &name, who.as_deref());
                (Ended::Busy { who }, Err(words))
            }
            Ok(ended) => (ended, Err(format!("The speaker {target} didn't respond."))),
            Err(msg) => (ended_by(app, platform, target, self.job.in_window, &msg), Err(msg)),
        };
        let cancelled = cancel::cancelled();
        if cancelled || ended == Ended::NotResponding {
            // A late delay reconnect still belongs to this attempt.
            self.attempt.binding.cancelled.store(true, Ordering::SeqCst);
            if let Some(t) = app.release_job(&self.attempt.binding) {
                let _ = t.join();
            }
        }
        if cancelled {
            app.engine_connect_ended(self.job.speaker.as_str(), gen, Err(SUPERSEDED.into()));
        } else {
            app.engine_connect_ended(self.job.speaker.as_str(), gen, result);
        }
        if released_way {
            let _ = tx.send(PlaceInput::WayFinished { gen, speaker: self.job.speaker.clone(), ended });
        } else if !cancelled {
            let _ = tx.send(PlaceInput::Event(Event::Connected { gen, ended }));
        }
    }

    fn connect(&self, released_way: &mut bool) -> Result<Ended, String> {
        let (app, target, job, own) = (&self.app, self.target.as_str(), &self.job, &self.own);
        if cancel::cancelled() {
            return Err(SUPERSEDED.into());
        }
        if !job.in_window && app.is_airplay_newcomer(target) {
            info!("not connecting to {} by itself: it announces another speaker's identity", target);
            let name = app.speaker_name(target).unwrap_or_else(|| target.to_string());
            return Err(format!("To play to “{name}”, pick it in the Stream To Speaker window."));
        }
        if job.purpose == Purpose::Chime && self.chime_here {
            if let Some(level) = job.chime_level {
                if job.speaker.is_music_assistant() { app.set_ma_level(level); }
                else { app.set_speaker_volume(u32::from(level)); }
            }
            app.play_test_sound();
            return Ok(Ended::Ready);
        }
        if job.look_first {
            if let Some(who) = driver::busy(app, target) {
                return Ok(Ended::Busy { who });
            }
        }
        if cancel::cancelled() {
            return Err(SUPERSEDED.into());
        }
        if job.purpose == Purpose::Chime {
            app.chime_beside(target, !job.look_first, job.chime_level)?;
            return Ok(Ended::Ready);
        }
        let stopped = if let Some(parked) = &self.parked {
            // A sound can start while Stop is still on its way.
            while parked.done.recv_timeout(Duration::from_millis(100)).is_err_and(|e| e.is_timeout()) {
                if cancel::cancelled() {
                    return Err(SUPERSEDED.into());
                }
            }
            if cancel::cancelled() {
                return Err(SUPERSEDED.into());
            }
            (parked.how.get() == Some(&Some(TakenOff::Stopped))).then_some(parked.number)
        } else {
            None
        };
        if job.way != WayChoice::Native {
            let choice = if job.way == WayChoice::AirPlayShort { DelayChoice::Short } else { DelayChoice::Steady };
            let status = app.delay_status_of(target);
            if let Some(before) = status.choice {
                app.set_delay_choice(target, choice).map_err(|why| {
                    if let Some(view) = app.engine_view() {
                        return crate::model::view::refusal_text(view.lang, &driver::refusal(why, &job.speaker), &view);
                    }
                    let name = app.speaker_name(target).unwrap_or_else(|| target.to_owned());
                    format!("Can't change {name}'s delay now.")
                })?;
                if before != choice && own.as_ref().is_some_and(|(id, _)| id == target) {
                    return Ok(wait_for_delay(app, target, &self.attempt.binding));
                }
            }
        }
        if let Some((_, number)) = own.as_ref().filter(|(id, _)| id != target) {
            *released_way = true;
            if let Some(t) = app.release_number_then(Some(*number), true, Box::new(|| {})) {
                let _ = t.join();
            }
        }
        if cancel::cancelled() {
            return Err(SUPERSEDED.into());
        }
        if let Some(number) = stopped {
            if app
                .session
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.stable_id() == target && app.session_number() == number)
            {
                app.restart_upnp_stream_number(Some(number))?;
                return Ok(Ended::Ready);
            }
        }
        app.engine_connect(target, job.in_window)?;
        Ok(if self.attempt.binding.number.load(Ordering::Acquire) != 0 { Ended::Ready } else { Ended::NotResponding })
    }
}

fn wait_for_delay(app: &App, id: &str, job: &cancel::Binding) -> Ended {
    let end = Instant::now() + Duration::from_secs(20);
    while !cancel::cancelled() && Instant::now() < end {
        if !matches!(app.delay_status_of(id).progress, Progress::WillReconnect | Progress::Reconnecting { .. }) {
            if app.session.lock().unwrap().as_ref().is_some_and(|s| {
                s.stable_id() == id && !s.is_dead() && app.session_number() == job.number.load(Ordering::Acquire)
            }) {
                return Ended::Ready;
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ended::NotResponding
}

fn ended_by(app: &App, platform: &Platform, id: &str, window: bool, msg: &str) -> Ended {
    if platform.network.connections().is_empty() {
        return Ended::NoNetwork;
    }
    if msg == CODE_WINDOW_CLOSED {
        return Ended::NeedsSecret(Secret::Code);
    }
    if app.asked_for_password(id) {
        return if window { Ended::WrongSecret(Secret::Password) } else { Ended::NeedsSecret(Secret::Password) };
    }
    if !window && crate::app::needs_the_person(msg) {
        return Ended::NeedsSecret(Secret::Code);
    }
    let lower = msg.to_lowercase();
    if lower.contains("busy") || lower.contains("is playing something else") {
        return Ended::Busy { who: None };
    }
    if lower.contains("no thread") {
        return Ended::CouldntStart;
    }
    if id != crate::sendspin_app::MA_OUTPUT_ID
        && !crate::engine::facts::speaker_list(app)
            .iter()
            .any(|s| s.id.as_str() == id && s.presence == crate::model::facts::Presence::Found)
    {
        return Ended::NotFound;
    }
    if ["timed out", "too slow", "refused", "unreachable", "no route", "not found", "10060", "10061"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return Ended::NotResponding;
    }
    Ended::Refused
}
