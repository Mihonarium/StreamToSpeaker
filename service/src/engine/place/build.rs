//! The situation every surface shows, from the core's state and what the
//! engine observes.

use super::*;
use crate::model::situation::{Doing, Picking, Popups, RowState, Situation, SpeakerNow, Spot, Standing};
use crate::model::situation::{FirstRunStep, PickWhy};

impl Core {
    /// Where the sound is and why, at `now`.
    pub fn situation(&self, w: &World, now: Clock) -> Situation {
        let pc_output = self.pc_target(w);
        let spot = match &self.place {
            Some(k) if !self.device_missing => Spot::Speaker(k.clone()),
            _ => Spot::ThisPc(pc_output.clone()),
        };
        // *Playing* follows the sound at once; on a speaker only while the
        // speaker is connected, since what arrives before isn't heard there.
        let at_speaker = self.sent_to_speaker(now);
        let doing = match &spot {
            Spot::ThisPc(None) => Doing::NothingToPlayOn,
            Spot::Speaker(_) => match self.stream {
                Stream::Trying(_) => Doing::Trying,
                Stream::Connecting { .. } | Stream::WaitNet { .. } => Doing::Connecting,
                // Heard on this PC while the speaker can't play it.
                Stream::On { .. } | Stream::OnPc if at_speaker => Doing::Playing,
                _ => Doing::NothingPlaying,
            },
            _ if self.detector.real_where(now.mono_ms, |_| true) || self.detector.audible_where(|_| true) => Doing::Playing,
            _ => Doing::NothingPlaying,
        };
        let away = match (&spot, &self.stream) {
            (Spot::Speaker(k), Stream::Off | Stream::ShortWait { .. } | Stream::ShortOnPc { .. })
                if doing == Doing::NothingPlaying =>
            {
                self.known_away(k, w, now)
            }
            (Spot::Speaker(k), Stream::OnPc) => self.known_away(k, w, now),
            _ => None,
        };
        let note = self.note_now(w, now).map(|(note, _)| note);
        // The badge: sound isn't heard where it should be, or the app moved
        // it for a failure that still stands. A closed note changes neither.
        let unheard = self.implied_note(w, now).is_some_and(|n| n.ahead == Some(Ahead::InAFewSeconds));
        let amber = unheard || self.note.as_ref().is_some_and(|n| n.note.shows() && n.note.failure());
        let picking = self.switch.as_ref().map(|s| Picking {
            speaker: s.speaker.clone(),
            way: s.way,
            by: s.by,
            from: s.who.from,
            can_cancel: self.cancel_while_connecting,
        });
        let speakers = w
            .speakers
            .iter()
            .map(|s| SpeakerNow {
                id: s.id.clone(),
                state: self.row_state(s, now),
                way: self.way_for(&s.id, None, w),
                is_place: spot == Spot::Speaker(s.id.clone()),
            })
            .collect();
        let confirm_in_panel = self.confirm.as_ref().filter(|c| !c.by_clock).map(|c| c.what.clone());
        let popups = if self.panel_open {
            Popups::default()
        } else {
            Popups {
                popup: self.popup.as_ref().map(|(p, _)| p.clone()),
                confirmation: self
                    .confirm
                    .as_ref()
                    .filter(|c| c.by_clock)
                    .map(|c| Shown { seq: c.seq, what: c.what.clone(), since_ms: c.since_ms }),
            }
        };
        let first_run = match &self.first_run {
            FirstRun::Off => None,
            FirstRun::One => Some(FirstRunStep::One),
            FirstRun::Two(k, level) => {
                let limit = self.speaker_world(k, w).and_then(|s| s.max_level).unwrap_or(100);
                Some(FirstRunStep::Two { speaker: k.clone(), level: (*level).min(limit), limit })
            }
            FirstRun::Three(k) => Some(FirstRunStep::Three { speaker: k.clone() }),
        };
        Situation {
            spot,
            pc_output,
            away,
            doing,
            amber,
            note,
            picking,
            pick_failed: self.pick_failed.as_ref().map(|(p, _)| p.clone()),
            standing: self.standing(w, now),
            speakers,
            confirm_in_panel,
            popups,
            first_run,
        }
    }

    /// Sound started, the place's speaker isn't playing it yet, and what
    /// keeps it has lasted long enough to be said: the speaker was asked
    /// and hasn't answered anything since, or the try still waits for this
    /// PC's network. A speaker that answered is getting ready: nothing is
    /// wrong with it yet.
    pub(super) fn slow_start(&self, now: Clock) -> Option<Away> {
        let (since_ms, why) = match &self.stream {
            Stream::Connecting { asked_ms, .. } => {
                let k = self.place.as_ref()?;
                let answered = self.recs.get(k).and_then(|r| r.answered_ms).is_some_and(|t| t >= *asked_ms);
                if answered {
                    return None;
                }
                (*asked_ms, Away::NotResponding)
            }
            Stream::WaitNet { since_ms, .. } => (*since_ms, Away::NoNetwork),
            _ => return None,
        };
        (now.mono_ms.saturating_sub(since_ms) >= SLOW_CONNECT_NOTE_MS).then_some(why)
    }

    /// The note that follows from the state alone, where one does. While a
    /// speaker is the place and can't play: the sound moves to this PC in a
    /// few seconds (the tries, a slow start), or when sound starts (nothing
    /// plays). While This PC is the place: headphones took the sound from a
    /// speaker and are still connected.
    pub(super) fn implied_note(&self, w: &World, now: Clock) -> Option<Note> {
        let ahead = |cause: Cause, at: Option<LocalTime>, ahead: Ahead| Note { cause, at, available_again: false, ahead: Some(ahead) };
        match &self.place {
            Some(k) if !self.device_missing => {
                if let Stream::Trying(t) = &self.stream {
                    return Some(ahead(t.cause.clone(), Some(t.at), Ahead::InAFewSeconds));
                }
                if let Some(why) = self.slow_start(now) {
                    let cause = Cause::NotRespondingAtStart { speaker: k.clone(), why };
                    return Some(ahead(cause, None, Ahead::InAFewSeconds));
                }
                if matches!(self.stream, Stream::Off | Stream::ShortWait { .. } | Stream::ShortOnPc { .. }) {
                    // The cause the move will have; its moment is still to
                    // come.
                    let (cause, _) = self.known_cant_play(k, w, now)?;
                    return Some(ahead(cause, None, Ahead::WhenSoundStarts));
                }
                None
            }
            Some(_) => None,
            None => {
                let h = self.headphones.as_ref()?;
                let cause = Cause::HeadphonesConnected { before: h.before.clone(), name: h.name.clone() };
                Some(Note { cause, at: None, available_again: false, ahead: None })
            }
        }
    }

    /// The one note to show now, and where it comes from. What is about to
    /// happen in seconds comes first, then the answer to a switch a person
    /// just made, then the kept note about a move the app made, then
    /// whatever else the state implies.
    pub(super) fn note_now(&self, w: &World, now: Clock) -> Option<(Note, NoteFrom)> {
        let implied = self.implied_note(w, now).filter(|n| self.closed.as_ref() != Some(n));
        if let Some(n) = implied.as_ref().filter(|n| n.ahead == Some(Ahead::InAFewSeconds)) {
            return Some((n.clone(), NoteFrom::State));
        }
        if let Some((p, _)) = &self.pick_failed {
            if matches!(p.by, SwitchBy::Switch | SwitchBy::TryAgain) {
                let cause = Cause::DidntGetReady { speaker: p.speaker.clone(), why: p.why };
                return Some((Note { cause, at: None, available_again: false, ahead: None }, NoteFrom::FailedSwitch));
            }
        }
        if let Some(n) = self.note.as_ref().map(|n| n.note.clone()).filter(Note::shows) {
            return Some((n, NoteFrom::Kept));
        }
        implied.map(|n| (n, NoteFrom::State))
    }

    fn standing(&self, w: &World, now: Clock) -> Vec<Standing> {
        let mut standing = Vec::new();
        if self.device_missing {
            standing.push(Standing::OutputMissing);
        }
        if let Some(k) = &self.windows_refused {
            standing.push(Standing::WindowsDidntSwitch { speaker: k.clone() });
        }
        if let (Some(k), false) = (&self.place, self.device_missing) {
            if w.settings.only_some_apps {
                standing.push(Standing::OnlySomeApps { none_playing: !self.detector.audible_on(&OutputRef::Ours) });
            } else if let Some((app, output)) = w.outputs.iter().find_map(|o| {
                let at = OutputRef::Pc(o.id.clone());
                if self.detector.real_on(&at, now.mono_ms) {
                    self.detector.apps_on(&at, now.mono_ms).into_iter().next().map(|a| (a, o.id.clone()))
                } else {
                    None
                }
            }) {
                standing.push(Standing::AppElsewhere { app: app.as_str().to_string(), output });
            }
            if k.is_music_assistant() && matches!(self.stream, Stream::On { .. }) && !w.music_assistant_playing {
                standing.push(Standing::MusicAssistantNotStarted);
            }
            if self.paused_at_speaker && matches!(self.stream, Stream::On { .. }) {
                standing.push(Standing::PausedAtSpeaker { speaker: k.clone() });
            }
        }
        if let Some(room) = &self.joined {
            standing.push(Standing::RoomJoined { room: room.clone() });
        }
        standing
    }

    /// The one state name a speaker's row shows.
    fn row_state(&self, s: &SpeakerWorld, now: Clock) -> RowState {
        if self.device_missing {
            return RowState::RepairFirst;
        }
        let id = &s.id;
        let is_place = self.place.as_ref() == Some(id);
        let connecting = self.switch.as_ref().is_some_and(|x| &x.speaker == id)
            || (is_place && matches!(self.stream, Stream::Connecting { .. } | Stream::WaitNet { .. }));
        if self.chime.as_ref().is_some_and(|job| job.chime_level.is_some() && &job.speaker == id) {
            return RowState::Connecting;
        }
        if connecting {
            // The place's speaker was asked when sound started and has been
            // silent long enough for the note to say so: its row says it too.
            let switching = self.switch.as_ref().is_some_and(|x| &x.speaker == id);
            if !switching && self.slow_start(now) == Some(Away::NotResponding) {
                return RowState::NotResponding;
            }
            return RowState::Connecting;
        }
        if !s.found && !self.in_grace(now) {
            return RowState::NotFound;
        }
        if is_place && matches!(self.stream, Stream::Trying(_)) {
            return RowState::NotResponding;
        }
        // The note says it can play again: its row loses its state name.
        let available = self.note.as_ref().is_some_and(|n| n.note.available_again && n.note.cause.speaker() == id);
        let Some(r) = self.recs.get(id) else { return RowState::Plain };
        if !available && (r.away == Some(Away::NotResponding) || r.last_try == Some(PickWhy::NotResponding)) {
            return RowState::NotResponding;
        }
        if !available && (r.away == Some(Away::NotFound) || r.last_try == Some(PickWhy::NotFound)) {
            return RowState::NotFound;
        }
        if r.busy.is_some() || r.grouped.is_some() {
            return RowState::PlayingElse;
        }
        if r.may_be_in_use.is_some() {
            return RowState::MayBeInUse;
        }
        RowState::Plain
    }
}
