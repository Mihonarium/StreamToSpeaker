//! Carry out the core's decisions in their order.

use super::*;
use crate::app::TakenOff;
use crate::platform::{Outputs, Role, PLAYBACK_ROLES};

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
        self.releases.retain(|r| r.try_recv() == Err(crossbeam_channel::TryRecvError::Empty));
        for action in actions {
            match action {
                Do::TurnOnOutput => {
                    if let Err(e) = self.platform.device.turn_on() {
                        warn!("Windows couldn't turn the app's output on: {e}");
                        let _ = self.tx.send(PlaceInput::Event(Event::TurnOnFailed));
                    }
                }
                Do::RepairOutput(run) => {
                    let tx = self.tx.clone();
                    let device = self.platform.device.clone();
                    match std::thread::Builder::new().name("stream-to-speaker-repair".into()).spawn(move || {
                        let exit = device.repair(&mut || { let _ = tx.send(PlaceInput::Event(Event::RepairApproved(run))); });
                        let _ = tx.send(PlaceInput::Event(Event::RepairExited(run, exit)));
                    }) {
                        Ok(t) => self.threads.push(t),
                        Err(e) => {
                            warn!("Couldn't start output repair: {e}");
                            let _ = self.tx.send(PlaceInput::Event(Event::RepairExited(run, crate::platform::RepairExit::Failed)));
                        }
                    }
                }
                Do::PickedOutput => {
                    *self.windows_decision.lock().unwrap() += 1;
                    self.app.note_user_pick();
                }
                Do::Connect(job) => self.connect(job),
                Do::Supersede(gen) => self.supersede(gen),
                Do::Silence { speaker, then } => {
                    let (done, wait) = crossbeam_channel::bounded(1);
                    self.releases.push(wait);
                    let move_output = self.windows_later(then);
                    let move_windows: Box<dyn FnOnce() + Send> = Box::new(move || { move_output(); let _ = done.send(()); });
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
                Do::RestoreOnlySomeApps => {
                    let mut cfg = self.app.user_config.lock().unwrap();
                    cfg.only_some_apps = true;
                    cfg.save();
                }
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
                        let samples = self.app.capture_tap.get().expect("capture tap").take(ms, &on);
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
        if self.quitting { return Box::new(|| {}); }
        let Some(target) = target else { return Box::new(|| {}) };
        let decisions = self.windows_decision.clone();
        let decision = {
            let mut current = decisions.lock().unwrap();
            *current += 1;
            *current
        };
        let outputs = self.platform.outputs.clone();
        let app = self.app.clone();
        let returning_default = app.user_config.lock().unwrap().only_some_apps;
        let tx = self.tx.clone();
        let now = self.clock().mono_ms;
        let at = Instant::now();
        Box::new(move || {
            let current = decisions.lock().unwrap();
            if *current != decision { return; }
            let only_some_apps = app.user_config.lock().unwrap().only_some_apps;
            let switched = if only_some_apps {
                if !returning_default || target == WinTarget::OurDevice { return; }
                let Some(ours) = outputs.our_output() else { return };
                let mut switched = true;
                for &role in PLAYBACK_ROLES {
                    if outputs.default_for(role).as_ref() == Some(&ours) {
                        switched &= switch_windows(&*outputs, target.clone(), &[role]);
                    }
                }
                switched
            } else {
                switch_windows(&*outputs, target, PLAYBACK_ROLES)
            };
            if !switched {
                let at_ms = now + at.elapsed().as_millis() as u64;
                let _ = tx.send(PlaceInput::WindowsNotTaken { decision, at_ms });
            }
        })
    }

    pub(super) fn follow_calls(&mut self, world: &World) {
        let outputs = self.platform.outputs.clone();
        let ours = outputs.our_output();
        let (play, back) = {
            let cfg = self.app.user_config.lock().unwrap();
            (cfg.play_calls, cfg.calls_back.clone())
        };
        let console = outputs.default_output();
        let multimedia = outputs.default_for(Role::Multimedia);
        let on_ours = ours.is_some() && console == ours && multimedia == ours;
        let playback = if console == ours { multimedia } else { console };
        let left_ours = self.playback_was_ours && playback.is_some() && !on_ours;
        if playback.is_some() { self.playback_was_ours = on_ours; }
        let take = play && !world.settings.only_some_apps && self.core.place_speaker().is_some() && on_ours;
        let changed = self.calls_to_speaker.replace(take) != Some(take);
        let now = self.clock().mono_ms;
        let current = outputs.calls_output();
        let awaiting_take = take && self.calls_retry.as_ref().is_some_and(|r| r.from == current);
        if back.is_some() && current.is_some() && current != ours && !awaiting_take {
            self.save_calls_back(None);
        }
        if self.quitting { return; }
        if changed || (left_ours && !world.settings.only_some_apps) {
            self.calls_retry = None;
        } else {
            let Some(retry) = &self.calls_retry else { return };
            if current != retry.from { self.calls_retry = None; return; }
            let Some(delay) = CALL_RETRIES.get(retry.attempt) else { return };
            if now < retry.started + delay { return; }
        }

        let pc = || self.core.situation(world, self.clock()).pc_output;
        let target = if take {
            if current == ours {
                return;
            }
            ours.clone()
        } else if ours.is_none() || current != ours {
            return;
        } else if let Some(saved) = back {
            Some(saved).filter(|id| outputs.list().iter().any(|o| o.id == *id))
                .or_else(|| playback.filter(|_| !on_ours))
                .or_else(pc)
        } else if !world.settings.only_some_apps && !on_ours && (left_ours || self.calls_retry.is_some()) {
            playback
        } else {
            return;
        };
        if let Some(target) = target {
            if take {
                self.save_calls_back(current.clone().or_else(pc));
                self.prepare_handback(pc(), ours.clone(), self.lang);
            }
            if let Err(e) = outputs.set_default(&target, &[Role::Communications]) {
                warn!("couldn't change Windows' call device: {e}");
            }
            if outputs.calls_output().as_ref() == Some(&target) {
                if !take { self.save_calls_back(None); }
                self.calls_retry = None;
                return;
            }
        }
        let next = match self.calls_retry.take() {
            Some(retry) => CallRetry { attempt: retry.attempt + 1, ..retry },
            None => CallRetry { started: now, attempt: 0, from: current },
        };
        self.calls_retry = Some(next);
    }

    fn save_calls_back(&self, target: Option<PcOutputId>) {
        let mut cfg = self.app.user_config.lock().unwrap();
        if cfg.calls_back != target {
            cfg.calls_back = target;
            cfg.save();
        }
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

const CALL_RETRIES: [u64; 3] = [2_000, 5_000, 10_000];

pub(super) struct CallRetry {
    started: u64,
    attempt: usize,
    from: Option<PcOutputId>,
}

fn switch_windows(outputs: &dyn Outputs, target: WinTarget, roles: &[Role]) -> bool {
    let taking = target == WinTarget::OurDevice;
    let id = match target {
        WinTarget::OurDevice => outputs.our_output(),
        WinTarget::Output(id) => Some(id),
    };
    let Some(id) = id else { return false };
    let previous: Vec<_> = roles.iter().filter(|_| taking).filter_map(|&role| {
        outputs.default_for(role).filter(|old| *old != id).map(|old| (role, old))
    }).collect();
    if let Err(e) = outputs.set_default(&id, roles) {
        warn!("couldn't change Windows' default output: {e}");
    }
    if roles.iter().all(|&role| outputs.default_for(role).as_ref() == Some(&id)) { return true; }
    // If Windows took the speaker for only one role, return that role to its previous output.
    for (role, old) in previous {
        if outputs.default_for(role).as_ref() == Some(&id) {
            let _ = outputs.set_default(&old, &[role]);
        }
    }
    false
}
