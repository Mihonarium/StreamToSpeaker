//! One cancellable worker for each connection the core asks for.

use super::*;
use crate::app::{TakenOff, CODE_WINDOW_CLOSED, SUPERSEDED};
use crate::delay::{DelayChoice, Progress};
use crate::engine::{cancel, driver};

pub(super) struct Attempt {
    pub binding: Arc<cancel::Binding>,
    pub speaker: SpeakerId,
    pub resumes: bool,
    switching: bool,
    mixed_chime: Mutex<Option<u64>>,
}

impl PlaceDriver {
    pub(super) fn supersede(&mut self, gen: u64) {
        if let Some(speaker) = self.cancel_job(gen) {
            if self.core.connected_speaker() == Some(&speaker) {
                let _ = self.tx.send(PlaceInput::Released(speaker));
            }
        }
    }

    pub(super) fn cancel_job(&mut self, gen: u64) -> Option<SpeakerId> {
        let job = self.jobs.remove(&gen)?;
        let (released, thread) = self.app.release_job(&job.binding);
        self.threads.extend(thread);
        if let Some(number) = job.mixed_chime.lock().unwrap().take() {
            self.app.stop_test_sound_on(number);
        }
        released.then(|| job.speaker.clone())
    }

    fn released_speaker(&self, id: &str, number: u64) -> SpeakerId {
        self.recent_bindings.iter().find(|(n, _)| *n == number)
            .map_or_else(|| self.speaker_of(id), |(_, speaker)| speaker.clone())
    }

    pub(super) fn finished(&mut self, gen: u64, ended: Ended, released: Vec<(String, u64)>) {
        let job = self.jobs.get(&gen);
        let failed_switch = ended != Ended::Ready && job.is_some_and(|job| job.switching);
        let target = job.map(|job| job.speaker.clone());
        let released: Vec<_> = released.into_iter().map(|(id, n)| self.released_speaker(&id, n)).collect();
        if target.is_some() {
            self.input(PlaceInput::Event(Event::Connected { gen, ended: ended.clone() }));
        }
        for speaker in released {
            if Some(&speaker) != target.as_ref() {
                self.input(PlaceInput::Released(speaker));
            } else if failed_switch {
                self.input(PlaceInput::WayFinished { speaker, ended: ended.clone() });
            }
        }
    }

    pub(super) fn connect(&mut self, job: Job) {
        let gen = job.gen;
        let id = self.app_id(&job.speaker, job.way);
        let app = self.app.clone();
        let tx = self.tx.clone();
        let platform = self.platform.clone();
        let own = self.session_of(&job.speaker);
        let chime_here = own.as_ref().filter(|_| self.core.connected_speaker() == Some(&job.speaker)).map(|(_, n)| *n);
        let parked = self.parked.clone().filter(|p| p.speaker == job.speaker && job.purpose != Purpose::Chime);
        let answers = tx.clone();
        let binding = Arc::new(cancel::Binding::new(job.speaker.clone(), job.welcome_level, app.user_pick_count(), move || {
            let _ = answers.send(PlaceInput::Answered(gen));
        }));
        let switching = matches!(job.purpose, Purpose::Way | Purpose::Switch | Purpose::TryAgain);
        let attempt = Arc::new(Attempt { binding, speaker: job.speaker.clone(), resumes: parked.is_some(), switching, mixed_chime: Mutex::new(None) });
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
    chime_here: Option<u64>,
    parked: Option<Arc<super::carry::Parked>>,
    attempt: Arc<Attempt>,
}

impl Worker {
    fn run(&self, platform: &Platform, tx: Sender<PlaceInput>) {
        let _scope = self.attempt.binding.enter();
        let (app, target, gen) = (&self.app, self.target.as_str(), self.job.gen);
        let (ended, result) = match self.connect() {
            Ok(Ended::Ready) => (Ended::Ready, Ok(())),
            Ok(Ended::Busy { who }) => {
                let name = app.speaker_name(target).unwrap_or_else(|| target.to_string());
                let words = crate::model::view::busy_reply(crate::model::Lang::English, &name, who.as_deref());
                (Ended::Busy { who }, Err(words))
            }
            Ok(ended) => (ended, Err(format!("The speaker {target} didn't respond."))),
            Err(msg) => (ended_by(app, platform, target, self.attempt.binding.password_rejected.get().copied(), &msg), Err(msg)),
        };
        let cancelled = cancel::cancelled();
        if cancelled || ended == Ended::NotResponding {
            // A late delay reconnect still belongs to this attempt.
            if let Some(t) = app.release_job(&self.attempt.binding).1 {
                let _ = t.join();
            }
        }
        if cancelled {
            app.engine_connect_ended(self.job.speaker.as_str(), gen, Err(SUPERSEDED.into()));
        } else {
            app.engine_connect_ended(self.job.speaker.as_str(), gen, result);
        }
        let released = {
            // A delay reconnect checks cancellation and takes its session under this lock.
            let _session = app.session.lock().unwrap();
            std::mem::take(&mut *self.attempt.binding.released.lock().unwrap())
        };
        let _ = tx.send(PlaceInput::Connected { gen, ended, released });
    }

    fn connect(&self) -> Result<Ended, String> {
        let (app, target, job, own) = (&self.app, self.target.as_str(), &self.job, &self.own);
        if cancel::cancelled() {
            return Err(SUPERSEDED.into());
        }
        if !job.in_window && app.is_airplay_newcomer(target) {
            info!("not connecting to {} by itself: it announces another speaker's identity", target);
            let name = app.speaker_name(target).unwrap_or_else(|| target.to_string());
            return Err(format!("To play to “{name}”, pick it in the Stream To Speaker window."));
        }
        if let Some(number) = self.chime_here.filter(|_| job.purpose == Purpose::Chime) {
            if let Some(level) = job.welcome_level {
                if job.speaker.is_music_assistant() { app.set_ma_level(level); }
                else { app.apply_speaker_level(u32::from(level), false); }
            }
            let mut mixed = self.attempt.mixed_chime.lock().unwrap();
            if cancel::cancelled() { return Err(SUPERSEDED.into()); }
            app.play_test_sound_on(number);
            *mixed = Some(number);
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
            app.chime_beside(target, !job.look_first, job.welcome_level, job.in_window)?;
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
                    return wait_for_delay(app, target, &self.attempt.binding);
                }
            }
        }
        if let Some((_, number)) = own.as_ref().filter(|(id, _)| id != target) {
            if let Some(t) = app.release_number_then(Some(*number), true, Box::new(|| {}), Some(&self.attempt.binding)) {
                let _ = t.join();
            }
        }
        if cancel::cancelled() {
            return Err(SUPERSEDED.into());
        }
        // First run sets the chosen volume before it hands the speaker its stream.
        if let Some(number) = stopped.filter(|_| job.welcome_level.is_none()) {
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

fn wait_for_delay(app: &App, id: &str, job: &cancel::Binding) -> Result<Ended, String> {
    let end = Instant::now() + Duration::from_secs(20);
    while !cancel::cancelled() && Instant::now() < end {
        if !matches!(app.delay_status_of(id).progress, Progress::WillReconnect | Progress::Reconnecting { .. }) {
            if app.session.lock().unwrap().as_ref().is_some_and(|s| {
                s.stable_id() == id && !s.is_dead() && app.session_number() == job.number.load(Ordering::Acquire)
            }) {
                return Ok(Ended::Ready);
            }
            if let Some(why) = job.delay_failure.get() { return Err(why.clone()); }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(Ended::NotResponding)
}

fn ended_by(app: &App, platform: &Platform, id: &str, password_rejected: Option<bool>, msg: &str) -> Ended {
    if platform.network.connections().is_empty() {
        return Ended::NoNetwork;
    }
    if msg == CODE_WINDOW_CLOSED {
        return Ended::NeedsSecret(Secret::Code);
    }
    if let Some(rejected) = password_rejected {
        return if rejected { Ended::WrongSecret(Secret::Password) } else { Ended::NeedsSecret(Secret::Password) };
    }
    if crate::app::needs_the_person(msg) {
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
    if ["timed out", "too slow", "connection refused", "unreachable", "no route", "10060", "10061",
        "closed the connection", "can't be reached", "didn't answer within", "no answer to"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return Ended::NotResponding;
    }
    Ended::Refused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sendspin_pairing_prompt_from_the_window_means_the_speaker_needs_its_code() {
        let app = App::for_tests();
        let windows = crate::platform::fake::FakePlatform::new();
        let ended = ended_by(&app, &windows.platform(), "sendspin:den", None, "Den needs to be paired once — follow the prompt.");
        assert_eq!(ended, Ended::NeedsSecret(Secret::Code));
    }
}
