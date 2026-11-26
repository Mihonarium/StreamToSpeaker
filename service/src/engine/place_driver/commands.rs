//! Commands whose effect also belongs to the app or Windows.

use super::*;
use crate::engine::driver::refusal;
use crate::model::command::{DelayOutcome, DelayStep, NoticeKind, VolumeOwner};

impl PlaceDriver {
    pub(super) fn command_id(&self, speaker: &SpeakerId) -> String {
        self.session_of(speaker).map(|(id, _)| id).unwrap_or_else(|| {
            let way = self.core.kept().ways.get(speaker).copied().unwrap_or(WayChoice::Native);
            self.app_id(speaker, way)
        })
    }

    pub(crate) fn command(&mut self, from: Surface, cmd: &Command) -> Result<Effect, Refusal> {
        let now = self.clock();
        let obs = self.observe(now, false);
        let app = self.app.clone();
        // Older entry points can name the AirPlay ID of a speaker with two ways.
        let wish;
        let waiting = matches!(cmd, Command::PlayWhenListed { .. });
        let cmd = if let Command::PlayWhenListed { speaker, picks_seen } = cmd {
            if app.user_pick_count() != *picks_seen { return Ok(Effect::Done); }
            wish = Command::Switch { place: crate::model::Place::of_speaker(speaker), way: None };
            &wish
        } else { cmd };
        let canonical;
        let cmd = if let Command::Switch { place: crate::model::Place::Speaker(id), way } = cmd {
            canonical = Command::Switch { place: crate::model::Place::of_speaker(&self.speaker_of(id.as_str())), way: *way };
            &canonical
        } else { cmd };
        match cmd {
            Command::SetVolume { owner, level } => {
                self.set_volume(owner, Some(*level), None);
                if matches!(owner, VolumeOwner::Speaker(_)) {
                    let (result, actions) = self.core.command(from, cmd, &obs.world, now);
                    self.execute(actions);
                    return result;
                }
                Ok(Effect::Done)
            }
            Command::SetMuted { owner, muted } => {
                self.set_volume(owner, None, Some(*muted));
                Ok(Effect::Done)
            }
            Command::ChangeDelay { speaker, step } => {
                let request = match *step {
                    DelayStep::Remove(ms) => crate::delay::Request::Remove(ms),
                    DelayStep::Add(ms) => crate::delay::Request::Add(ms),
                    DelayStep::SetTotal(total) => crate::delay::Request::Set(total),
                    DelayStep::Reset => crate::delay::Request::Set(0),
                };
                match app.change_delay(Some(&self.command_id(speaker)), request) {
                    Ok(o) => Ok(Effect::Delay(DelayOutcome {
                        speaker: speaker.clone(),
                        step: *step,
                        before_ms: o.from_ms,
                        after_ms: o.to_ms,
                        stopped_at_limit: o.clamped,
                    })),
                    Err(why) => Err(refusal(why, speaker)),
                }
            }
            Command::UseDelayChoice { speaker, choice } => {
                let choice = match choice {
                    crate::model::facts::DelayChoice::Short => crate::delay::DelayChoice::Short,
                    crate::model::facts::DelayChoice::Steady => crate::delay::DelayChoice::Steady,
                };
                app.set_delay_choice(&self.app_id(speaker, WayChoice::AirPlayShort), choice)
                    .map(|_| Effect::Done)
                    .map_err(|why| refusal(why, speaker))
            }
            Command::Reconnect(id) => {
                if self.core.connected_speaker() == Some(id) && self.session_of(id).is_some() {
                    app.reconnect_speaker();
                }
                Ok(Effect::Done)
            }
            Command::HideFromList(speaker) => {
                let id = speaker.as_str();
                let mut settings = app.speaker_settings(id);
                settings.hidden = true;
                app.set_speaker_settings(id, settings);
                self.list_at = None;
                Ok(Effect::Done)
            }
            Command::ShowPcOutputs(open) => {
                let mut uc = app.user_config.lock().unwrap();
                if uc.pc_outputs_open != *open {
                    uc.pc_outputs_open = *open;
                    uc.save();
                }
                Ok(Effect::Done)
            }
            Command::DismissNotice(NoticeKind::Donation) => {
                app.snooze_donation_prompt(30);
                Ok(Effect::Done)
            }
            Command::Open(o) => {
                let open = self.opener.lock().unwrap().take().ok_or(Refusal::WindowOnly)?;
                open(from, o.clone());
                self.opener.lock().unwrap().get_or_insert(open);
                Ok(Effect::Done)
            }
            Command::AllowDevice { key } => {
                if let Ok(address) = key.parse::<std::net::IpAddr>() {
                    app.allow_device(address);
                }
                Ok(Effect::Done)
            }
            Command::TurnOffLab => {
                app.turn_off_lab();
                Ok(Effect::Done)
            }
            Command::DismissNotice(NoticeKind::Updated) => {
                let mut uc = app.user_config.lock().unwrap();
                uc.last_version_seen = Some(crate::display_version().to_string());
                uc.save();
                Ok(Effect::Done)
            }
            Command::DismissNotice(NoticeKind::RefusedInGroup) => {
                app.dismiss_refusals();
                Ok(Effect::Done)
            }
            Command::DismissNotice(NoticeKind::UpdateReady) => {
                app.snooze_update_banner(7);
                Ok(Effect::Done)
            }
            Command::DismissNotice(_) => Ok(Effect::Done),
            Command::DelayOffer { speaker, take } => {
                let id = self.command_id(speaker);
                if *take {
                    app.take_delay_offer(&id);
                } else {
                    app.close_delay_offer(&id);
                }
                Ok(Effect::Done)
            }
            Command::SetDelay { speaker, ms } => app
                .set_delay_to(&self.command_id(speaker), *ms)
                .map(|_| Effect::Done)
                .map_err(|why| refusal(why, speaker)),
            Command::UseDefaultDelay(speaker) => app
                .use_default_delay(&self.command_id(speaker))
                .map(|_| Effect::Done)
                .map_err(|why| refusal(why, speaker)),
            _ => {
                let (result, actions) = self.core.command(from, cmd, &obs.world, now);
                if result.is_ok() {
                    if !waiting && (matches!(cmd, Command::Switch { .. } | Command::StayOnThisPc | Command::SwitchOutput(_)
                        | Command::TryAgain(_) | Command::PlayTakingOver(_) | Command::CancelSwitch)
                        || actions.iter().any(Do::starts_pick)) {
                        app.note_user_pick();
                    }
                    match cmd {
                        Command::PanelShown(open) => self.panel_open = *open,
                        Command::Release { for_update: true } | Command::ReleaseWindows { for_update: true, .. } => {
                            let mut cfg = app.user_config.lock().unwrap();
                            cfg.closed_for_update = true;
                            cfg.save();
                        }
                        _ => {}
                    }
                }
                self.execute(actions);
                if result.is_ok() {
                    if let Command::RemoveSpeaker(k) | Command::ResetSpeaker(k) = cmd {
                        let mut ids = vec![self.app_id(k, WayChoice::Native)];
                        let ap = self.app_id(k, WayChoice::AirPlaySteady);
                        if !ids.contains(&ap) {
                            ids.push(ap);
                        }
                        for id in ids {
                            if matches!(cmd, Command::RemoveSpeaker(_)) && app.find_manual_speaker(&id).is_some() {
                                app.remove_manual_speaker(&id);
                            } else {
                                let mut cfg = app.user_config.lock().unwrap();
                                if cfg.speaker_settings.remove(&id).is_some() {
                                    cfg.save();
                                }
                            }
                        }
                        self.list_at = None;
                    }
                }
                result
            }
        }
    }

    fn set_volume(&mut self, owner: &VolumeOwner, level: Option<u8>, muted: Option<bool>) {
        match owner {
            VolumeOwner::Speaker(id) => {
                if self.core.connected_speaker() != Some(id) || self.session_of(id).is_none() {
                    return;
                }
                if let Some(level) = level {
                    self.app.set_speaker_volume(level.min(100) as u32);
                }
                if let Some(muted) = muted {
                    self.app.set_speaker_muted(muted);
                }
            }
            VolumeOwner::PcOutput(id) => {
                let (old, was_muted) = self.platform.outputs.volume(id).unwrap_or((0, false));
                if let Err(e) = self.platform.outputs.set_volume(id, level.unwrap_or(old), muted.unwrap_or(was_muted)) {
                    debug!("couldn't set this PC's level: {}", e);
                }
            }
            VolumeOwner::MusicAssistant => {
                if let Some(level) = level {
                    self.ma_before_mute = None;
                    self.app.set_ma_level(level);
                }
                if muted == Some(true) && self.ma_before_mute.is_none() {
                    self.ma_before_mute = Some(self.app.ma_level());
                    self.app.set_ma_level(0);
                } else if muted == Some(false) {
                    if let Some(level) = self.ma_before_mute.take().filter(|_| self.app.ma_level() == 0) {
                        self.app.set_ma_level(level);
                    }
                }
            }
        }
    }
}
