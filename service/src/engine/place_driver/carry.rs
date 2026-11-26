//! Carry out the core's decisions in their order.

use super::*;
use crate::app::TakenOff;
use crate::platform::{Outputs, ALL_ROLES, PLAYBACK_ROLES};

pub(super) struct Parked {
    pub speaker: SpeakerId,
    pub number: u64,
    pub done: crossbeam_channel::Receiver<()>,
    pub how: std::sync::OnceLock<Option<TakenOff>>,
}

impl PlaceDriver {
    pub(super) fn execute(&mut self, actions: Vec<Do>) {
        for finished in self.threads.extract_if(.., |t| t.is_finished()) {
            let _ = finished.join();
        }
        for action in actions {
            match action {
                Do::PickedOutput => {
                    *self.windows_decision.lock().unwrap() += 1;
                    self.app.note_user_pick();
                }
                Do::Connect(job) => self.connect(job),
                Do::Supersede(gen) => self.supersede(gen),
                Do::Silence { speaker, then } => {
                    let move_windows = self.windows_later(then);
                    if let Some((_, number)) = self.session_of(&speaker) {
                        self.threads.extend(self.app.release_speaker_number_then(Some(number), true, move_windows, None, Some(speaker.clone())));
                    } else {
                        move_windows();
                    }
                }
                Do::StopSending { speaker, take_off } => {
                    let Some((_, number)) = self.session_of(&speaker) else { continue };
                    if take_off {
                        let (tx, rx) = crossbeam_channel::bounded(1);
                        let parked = Arc::new(Parked { speaker, number, done: rx, how: Default::default() });
                        let worker = parked.clone();
                        let app = self.app.clone();
                        match std::thread::Builder::new().name("stream-to-speaker-take-off".into()).spawn(move || {
                            let _ = worker.how.set(app.take_stream_off_number(Some(number)));
                            drop(tx);
                        }) {
                            Ok(t) => {
                                self.parked = Some(parked);
                                self.threads.push(t);
                            }
                            Err(e) => warn!("couldn't take the stream off: {e}"),
                        }
                    } else {
                        self.threads.extend(self.app.release_speaker_number_then(Some(number), false, Box::new(|| {}), None, Some(speaker.clone())));
                    }
                }
                Do::Windows(target) => self.windows_later(Some(target))(),
                Do::Pause(players) => {
                    for p in players {
                        if let Err(e) = self.platform.players.pause(&p.0) {
                            debug!("couldn't pause the player: {e}");
                        }
                    }
                }
                Do::Play(players) => {
                    for p in players {
                        if let Err(e) = self.platform.players.play(&p.0) {
                            debug!("couldn't start the player: {e}");
                        }
                    }
                }
                Do::SetLevel { speaker, level } => {
                    if self.session_of(&speaker).is_some() {
                        if speaker.is_music_assistant() {
                            if self.ma_before_mute.is_some() && self.app.ma_level() == 0 {
                                self.ma_before_mute = Some(level);
                            } else {
                                self.ma_before_mute = None;
                                self.app.set_ma_level(level);
                            }
                        } else {
                            self.app.apply_speaker_level(level.into(), false);
                        }
                    }
                }
                Do::StartAtSignIn(on) => {
                    if let Err(e) = (self.start_at_sign_in)(on) {
                        warn!("couldn't change start at sign-in: {e}");
                    }
                    let mut cfg = self.app.user_config.lock().unwrap();
                    if cfg.auto_reconnect_on_launch != on {
                        cfg.auto_reconnect_on_launch = on;
                        cfg.save();
                    }
                }
                Do::Look(speaker) => {
                    let id = self.command_id(&speaker);
                    let (app, tx) = (self.app.clone(), self.tx.clone());
                    match std::thread::Builder::new().name("stream-to-speaker-look".into()).spawn(move || {
                        let signal = app.look_at_speaker(&id);
                        let say = |signal| {
                            let _ = tx.send(PlaceInput::Event(Event::Speaker { id: speaker.clone(), signal }));
                        };
                        if matches!(signal, SpeakerSignal::Free | SpeakerSignal::PlayingElse { .. }) {
                            say(SpeakerSignal::Responding);
                        }
                        say(signal);
                    }) {
                        Ok(t) => self.threads.push(t),
                        Err(e) => warn!("couldn't start looking at the speaker: {e}"),
                    }
                }
                Do::PassThrough(output) => {
                    self.passing = output.clone();
                    if let Some(worker) = &mut self.pass {
                        worker.send(pass::Message::PassTo(output));
                    }
                }
                Do::ReplayShort { on, ms } => {
                    if let Some(worker) = &mut self.pass {
                        let samples = self.app.capture_tap.get().expect("capture tap").take(ms);
                        // A sound of which nothing was captured opens no output.
                        if !samples.is_empty() {
                            worker.send(pass::Message::Replay { on, samples, resume: self.passing.clone() });
                        }
                    }
                }
                Do::Save => self.save(),
            }
        }
    }

    fn windows_later(&self, target: Option<WinTarget>) -> Box<dyn FnOnce() + Send> {
        let Some(target) = target else { return Box::new(|| {}) };
        let decisions = self.windows_decision.clone();
        let decision = {
            let mut current = decisions.lock().unwrap();
            *current += 1;
            *current
        };
        let outputs = self.platform.outputs.clone();
        let tx = self.tx.clone();
        let now = self.clock().mono_ms;
        let at = Instant::now();
        Box::new(move || {
            let current = decisions.lock().unwrap();
            if *current == decision && !switch_windows(&*outputs, target) {
                let at_ms = now + at.elapsed().as_millis() as u64;
                let _ = tx.send(PlaceInput::WindowsNotTaken { decision, at_ms });
            }
        })
    }

    fn save(&self) {
        let kept = self.core.kept();
        let named = kept.named.as_ref().map(|k| self.app_id(k, kept.ways.get(k).copied().unwrap_or(WayChoice::Native)));
        let can_name = named
            .as_deref()
            .is_none_or(|id| !self.app.is_airplay_newcomer(id) && !crate::airplay::discovery::is_instance_id(id));
        let mut cfg = self.app.user_config.lock().unwrap();
        if can_name {
            cfg.last_speaker_id = named;
        }
        cfg.chose_this_pc = kept.place.is_none();
        cfg.pc_output = kept.pc_output.as_ref().or(kept.last_outputs.first()).map(|id| id.as_str().into());
        cfg.onboarding_dismissed = kept.first_run_done;
        cfg.sound_place = Some(kept);
        cfg.save();
    }
}

fn switch_windows(outputs: &dyn Outputs, target: WinTarget) -> bool {
    let (id, roles) = match target {
        WinTarget::OurDevice => (outputs.our_output(), PLAYBACK_ROLES),
        WinTarget::Output(id) => (Some(id), ALL_ROLES),
    };
    let Some(id) = id else { return false };
    if let Err(e) = outputs.set_default(&id, roles) {
        warn!("couldn't change Windows' default output: {e}");
    }
    outputs.default_output().as_ref() == Some(&id)
}
