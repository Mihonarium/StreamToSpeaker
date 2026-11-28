//! Changes since the previous look. A system notice takes the same
//! observations immediately, between the regular sound samples.

use super::*;
use crate::app::ActiveSession;
use crate::delay::Progress;
use crate::engine::place::sound::Level;
use crate::engine::{driver, signals};
pub(super) use driver::Buttons;
use world::Observation;

fn changes(old: &Observation, obs: &Observation, at_ms: u64) -> Vec<Event> {
    let (before, now) = (&old.world, &obs.world);
    let mut events = Vec::new();
    for o in &now.outputs {
        if !before.outputs.iter().any(|p| p.id == o.id) {
            events.push(Event::Output(OutputEvent::Added {
                id: o.id.clone(),
                kind: o.kind,
                name: o.name.clone(),
                at_ms,
            }));
        }
    }
    for o in &before.outputs {
        if !now.outputs.iter().any(|p| p.id == o.id) {
            events.push(Event::Output(OutputEvent::Removed { id: o.id.clone(), at_ms }));
        }
    }
    if before.our_device != now.our_device {
        events.push(Event::Output(OutputEvent::DeviceChanged { present: now.our_device, at_ms }));
    }
    if before.default_output != now.default_output || before.multimedia_output != now.multimedia_output {
        events.push(Event::Output(OutputEvent::DefaultChanged { at_ms }));
    }
    for (p, raw) in now.players.iter().zip(&obs.players) {
        match old.players.iter().find(|r| r.id == raw.id) {
            Some(r) if r.status == raw.status => {
                if r.title != raw.title {
                    events.push(Event::Player(PlayerEvent::Other { id: p.id.clone() }));
                }
            }
            _ => events.push(Event::Player(PlayerEvent::Status { id: p.id.clone(), status: p.status })),
        }
    }
    for p in &before.players {
        if !now.players.iter().any(|r| r.id == p.id) {
            events.push(Event::Player(PlayerEvent::Status { id: p.id.clone(), status: PlayerStatus::Stopped }));
        }
    }
    if before.connections != now.connections {
        events.push(Event::Network);
    }
    for (from, to, signal) in [(before, now, SpeakerSignal::Announced), (now, before, SpeakerSignal::Unannounced)] {
        for s in to.speakers.iter().filter(|s| s.found) {
            if !from.speakers.iter().any(|p| p.id == s.id && p.found) {
                events.push(Event::Speaker { id: s.id.clone(), signal: signal.clone() });
            }
        }
    }
    events
}

#[derive(Default)]
pub(super) struct SessionWatch {
    number: u64,
    dead: bool,
    busy: bool,
    fetch: Option<UpnpFetch>,
}

struct UpnpFetch {
    given_at: Instant,
    played_ms: u64,
    seen_ms: Option<u64>,
    reported: bool,
    state: Option<String>,
    uri: Option<String>,
    group: Option<Vec<String>>,
}

impl PlaceDriver {
    pub(super) fn compare(&mut self, system: Option<SystemEvent>) {
        let now = self.clock();
        let obs = self.observe(now, true);
        let w = &obs.world;
        if let Some(before) = &self.seen {
            for e in changes(before, &obs, now.mono_ms) {
                self.event(e, w, now);
            }
        }
        let power = match system {
            Some(SystemEvent::Suspending) => Some(PowerEvent::Suspending),
            Some(SystemEvent::Resumed) => Some(PowerEvent::Resumed),
            Some(SystemEvent::SessionEnding) => Some(PowerEvent::SigningOut),
            _ => None,
        };
        if let Some(p) = power {
            self.event(Event::Power(p), w, now);
        }
        if system.is_none() {
            let sessions = self.platform.sessions.sessions();
            *self.app.sounding_apps.lock().unwrap() = sessions.iter()
                .filter(|s| !s.system_sounds && Some(&s.output) == obs.ours.as_ref()
                    && s.peak > place::numbers::LEVEL_THRESHOLD)
                .map(|s| s.app.clone())
                .collect();
            for s in sessions.iter().filter(|s| !s.system_sounds) {
                if self.app_names.get(&s.key) != Some(&s.app) {
                    self.app_names.insert(s.key.clone(), s.app.clone());
                }
            }
            let levels = sessions
                .into_iter()
                .map(|s| Level {
                    app: AppKey::new(&s.key),
                    output: world::output_ref(s.output, &obs.ours),
                    peak: s.peak,
                    system: s.system_sounds,
                })
                .collect::<Vec<_>>();
            if levels.is_empty() {
                let actions = self.core.tick(w, now);
                self.execute(actions);
            } else {
                self.event(Event::Sound(levels), w, now);
            }
        }
        for h in self.app.signals.take() {
            self.event(Event::Speaker { id: self.speaker_of(&h.speaker), signal: h.signal }, w, now);
        }
        for e in self.session_events(now) {
            self.event(e, w, now);
        }
        Buttons::follow(&self.app, &mut self.buttons);
        self.remember_playback(w, now);
        self.follow_calls(w);
        self.seen = Some(obs);
    }

    /// History belongs to the folded row. The protocol sessions also read
    /// their own saved starting level before the core sees the connection.
    pub(super) fn remember_playback(&mut self, w: &World, now: Clock) {
        let playing = self.core.connected_speaker().filter(|id| self.session_of(id).is_some()
            && self.parked.as_ref().is_none_or(|p| p.speaker != **id)).cloned();
        let seconds = now.wall.0.max(0) as u64 / 1000;
        if self.played != playing {
            if let Some(id) = &playing {
                if !crate::airplay::discovery::is_instance_id(id.as_str()) {
                    let mut cfg = self.app.user_config.lock().unwrap();
                    let s = cfg.speaker_settings.entry(id.as_str().into()).or_default();
                    s.last_played = Some(seconds);
                    // Music Assistant's network is recorded when its server
                    // connects: its output starts with no server connected,
                    // on whatever network this PC is on.
                    let net = w.connections.iter().find(|c| c.kind != crate::model::situation::ConnKind::Other);
                    if let Some(net) = net.filter(|_| !id.is_music_assistant()) {
                        s.network = Some(net.net.clone());
                    }
                    cfg.save();
                    self.list_at = None;
                }
            }
            self.played = playing.clone();
        }
        let Some((id, level)) = playing.filter(|id| !id.is_music_assistant()).zip(self.app.current_volume()) else {
            self.level = None;
            return;
        };
        let level = level.min(100) as u8;
        let since = match &self.level {
            Some((speaker, value, since)) if *speaker == id && *value == level => *since,
            _ => {
                self.level = Some((id, level, now.mono_ms));
                return;
            }
        };
        if now.mono_ms.saturating_sub(since) < place::numbers::LEVEL_SAVED_AFTER_MS || !self.core.sounding_at_speaker(now) {
            return;
        }
        let ids = self.ids.iter().find(|s| s.id == id).or(self.bound_ids.as_ref().filter(|s| s.id == id));
        let mut cfg = self.app.user_config.lock().unwrap();
        let mut changed = false;
        for key in std::iter::once(id.as_str()).chain(ids.and_then(|s| s.airplay.as_deref()).filter(|k| *k != id.as_str())) {
            if crate::airplay::discovery::is_instance_id(key) { continue; }
            let saved = cfg.speaker_settings.entry(key.to_owned()).or_default();
            if saved.level != Some(level) || saved.level_at.is_none_or(|at| seconds.saturating_sub(at) >= 60) {
                saved.level = Some(level);
                saved.level_at = Some(seconds);
                changed = true;
            }
        }
        if changed {
            cfg.save();
            drop(cfg);
            // Keep the rules' shared starting level current when the change
            // came from Windows or the speaker instead of a panel command.
            let cmd = Command::SetVolume { owner: crate::model::command::VolumeOwner::Speaker(id), level };
            let (_, actions) = self.core.command(Surface::Panel, &cmd, w, now);
            self.execute(actions);
        }
    }

    fn session_events(&mut self, now: Clock) -> Vec<Event> {
        let app = &self.app;
        let change = app.take_transport_change();
        let groups = app.take_topology();
        if change.as_ref().and_then(|c| c.state.as_deref()).is_some_and(|s| s.eq_ignore_ascii_case("TRANSITIONING")) {
            app.delay.catch_up().speaker_rebuffered();
        }
        let mut events = Vec::new();
        let Some(place) = self.core.connected_speaker() else { return events };
        let (id, number, dead, ended, fault, busy, upnp) = {
            let guard = app.session.lock().unwrap();
            let Some(s) = guard.as_ref() else { return events };
            let id = s.stable_id();
            let number = app.session_number();
            if self.speaker_of(&id) != *place || self.parked.as_ref().is_some_and(|p| p.number == number) {
                return events;
            }
            let upnp = match s {
                ActiveSession::Upnp(r) => Some((
                    r.stream_given_at,
                    r.played_at,
                    r.renderer.stream_peers(),
                    r.renderer.udn.clone(),
                    driver::upnp_info(s),
                )),
                _ => None,
            };
            (id, number, s.is_dead(), s.ended_by_speaker(), s.final_fault(), s.reports_busy(), upnp)
        };
        if self.session.number != number {
            self.session = SessionWatch { number, ..Default::default() };
        }
        let reconnecting = matches!(app.delay_status_of(&id).progress, Progress::Reconnecting { .. });
        let mut say = |signal| events.push(Event::Speaker { id: place.clone(), signal });
        if dead && !self.session.dead && !reconnecting {
            self.session.dead = true;
            let signal = ended_signal(ended.as_deref(), fault.as_ref());
            if matches!(signal, SpeakerSignal::NotResponding | SpeakerSignal::EndedStream { reachable: false }) {
                app.note_cut_out(&id);
            }
            say(signal);
        }
        if busy && !self.session.busy && !reconnecting {
            self.session.busy = true;
            say(SpeakerSignal::PlayingElse { who: None });
        }
        let Some((given_at, played_at, peers, uuid, ours)) = upnp else { return events };
        if self.session.fetch.as_ref().is_none_or(|u| u.given_at != given_at) {
            self.session.fetch = Some(UpnpFetch {
                given_at,
                played_ms: now.mono_ms.saturating_sub(played_at.elapsed().as_millis() as u64),
                seen_ms: None,
                reported: false,
                state: None,
                uri: None,
                group: None,
            });
        }
        let u = self.session.fetch.as_mut().expect("the bound UPnP stream");
        if let Some(mut change) = change {
            if let Some(state) = &change.state {
                u.state = Some(state.to_ascii_uppercase());
            }
            if let Some(uri) = &change.uri {
                u.uri = Some(uri.clone());
            } else {
                change.uri.clone_from(&u.uri);
            }
            if let Some(signal) = ours.and_then(|ours| signals::upnp_transport(&change, &ours)) {
                say(signal);
            }
        }
        if let (Some(groups), Some(uuid)) = (groups, uuid) {
            match signals::sonos_group(&groups, &uuid, u.group.as_deref().unwrap_or_default()) {
                Some(signals::SonosRoom::TakenInto { with }) => say(SpeakerSignal::GroupedInto { with }),
                Some(signals::SonosRoom::Leads { members, joined }) => {
                    if u.group.replace(members).is_some() {
                        for room in joined {
                            say(SpeakerSignal::JoinedGroup { room });
                        }
                    }
                }
                None => {}
            }
        }
        if app.hub.has_listener_from(&peers) {
            u.seen_ms = Some(now.mono_ms);
            u.reported = false;
        } else if !u.reported
            && !reconnecting
            && !matches!(u.state.as_deref(), Some("STOPPED" | "PAUSED_PLAYBACK" | "NO_MEDIA_PRESENT"))
        {
            let (since, wait) =
                u.seen_ms.map_or((u.played_ms, driver::UPNP_NOT_TAKING_AFTER), |ms| (ms, driver::UPNP_GONE_AFTER));
            if now.mono_ms.saturating_sub(since) >= wait.as_millis() as u64 {
                app.note_cut_out(&id);
                u.reported = true;
                say(SpeakerSignal::NotResponding);
            }
        }
        events
    }
}

fn ended_signal(ended: Option<&str>, fault: Option<&crate::airplay::ap2_health::Ap2Fault>) -> SpeakerSignal {
    match ended {
        Some(crate::app::ENDED_WHILE_REACHABLE) => SpeakerSignal::EndedStream { reachable: true },
        Some("another_server") => SpeakerSignal::PlayingElse { who: None },
        Some("user_request") => SpeakerSignal::StoppedAtSpeaker,
        Some(_) => SpeakerSignal::EndedStream { reachable: false },
        None if fault.is_some_and(|f| f.code == "authentication_failed") => SpeakerSignal::AsksSecret(Secret::Code),
        None => SpeakerSignal::NotResponding,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::ap2_health::{Ap2Fault, FaultChannel};

    #[test]
    fn only_refused_authentication_asks_for_a_code() {
        for (code, channel, signal) in [
            ("authentication_failed", FaultChannel::Feedback, SpeakerSignal::AsksSecret(Secret::Code)),
            ("anchor_failed", FaultChannel::Media, SpeakerSignal::NotResponding),
        ] {
            let fault = Ap2Fault::new(code, channel, false, "session ended");
            assert_eq!(ended_signal(None, Some(&fault)), signal);
        }
    }
}
