//! When sound starts on a speaker that isn't connected, when a speaker
//! stops responding or someone else plays on it, and the one way the app
//! moves the sound off a speaker: to this PC, where it carries on.

use super::*;
use crate::model::situation::{ConnKind, PickWhy};
use sound::{Level, SoundChange};

impl Core {
    /// A sample of every app's level.
    pub(super) fn sound(&mut self, levels: Vec<Level>, w: &World, now: Clock, out: &mut Vec<Do>) {
        let changes = self.detector.sample(now.mono_ms, &levels);
        self.sound_changes(changes, w, now, out);
    }

    pub(super) fn sound_changes(&mut self, changes: Vec<SoundChange>, w: &World, now: Clock, out: &mut Vec<Do>) {
        for change in changes {
            match change {
                SoundChange::Began { app, output: OutputRef::Ours } => self.sound_began(app, w, now, out),
                SoundChange::Ended { output: OutputRef::Ours, short: true, .. } => {
                    if self.detector.audible_on(&OutputRef::Ours) {
                        continue;
                    }
                    match self.stream {
                        Stream::ShortWait { since_ms } => {
                            // A short sound doesn't connect the speaker: it
                            // is played on the PC's own output once it has
                            // ended. It began up to one sample before it
                            // was first heard.
                            self.stream = Stream::Off;
                            if let Some(on) = self.pc_target(w) {
                                let ms = now.mono_ms.saturating_sub(since_ms) + MAX_SAMPLE_MS;
                                out.push(Do::ReplayShort { on, ms });
                            }
                        }
                        // It was heard on this PC as it played.
                        Stream::ShortOnPc { .. } => self.stream = Stream::Off,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    /// Sound began at the app's device.
    fn sound_began(&mut self, app: AppKey, w: &World, now: Clock, out: &mut Vec<Do>) {
        if self.place.is_none() || self.device_missing || self.start_look.is_some() {
            return;
        }
        if self.stream == Stream::Off {
            // A player that is playing makes no short sound.
            let player_playing = w.players.iter().any(|p| p.app == app && p.status == PlayerStatus::Playing);
            self.sound_starts(!player_playing, w, now, out);
        }
    }

    /// A player's status changed, or something else about it. The app asks
    /// nothing of a player here: a player that starts only tells it that
    /// what begins is no short sound.
    pub(super) fn player_event(&mut self, e: PlayerEvent, w: &World, now: Clock, out: &mut Vec<Do>) {
        let (id, status) = match e {
            PlayerEvent::Status { id, status } => (id, Some(status)),
            PlayerEvent::Other { id } => (id, None),
        };
        // A player that the speaker's own pause stopped: its pause is what
        // the app asked for; anything else is a person touching it, and
        // play pressed on the speaker no longer starts it.
        if status != Some(PlayerStatus::Paused) && (status.is_some() || trials::T2_ANY_UNASKED_CHANGE_IS_A_TOUCH) {
            self.passed_pause.retain(|p| *p != id);
        }
        // In per-app mode a player may have started on a PC output. Sound
        // arriving at our device starts the connection.
        if w.settings.only_some_apps || status != Some(PlayerStatus::Playing) {
            return;
        }
        if self.place.is_none() || self.device_missing || self.start_look.is_some() {
            return;
        }
        if matches!(self.stream, Stream::Off | Stream::ShortWait { .. } | Stream::ShortOnPc { .. }) {
            self.sound_starts(false, w, now, out);
        }
    }

    /// What the app already knows makes the place's speaker unable to play,
    /// outside the minute after a wake or a start.
    pub(super) fn known_away(&self, k: &SpeakerId, w: &World, now: Clock) -> Option<Away> {
        if self.in_grace(now) {
            return None;
        }
        if w.connections.is_empty() && !self.just_changed_network(now) {
            return Some(Away::NoNetwork);
        }
        self.recs.get(k).and_then(|r| r.away)
    }

    fn just_changed_network(&self, now: Clock) -> bool {
        self.net_changed_ms.is_some_and(|t| now.mono_ms < t + AFTER_NETWORK_CHANGE_MS)
    }

    /// Why speaker `k` can't play what starts now, where the app knows it
    /// without trying, and when the app saw that happen (`None` where it
    /// only found it so).
    pub(super) fn known_cant_play(&self, k: &SpeakerId, w: &World, now: Clock) -> Option<(Cause, Option<LocalTime>)> {
        if let Some(why) = self.known_away(k, w, now) {
            return Some((Cause::NotRespondingAtStart { speaker: k.clone(), why }, Some(now.local)));
        }
        let rec = self.recs.get(k)?;
        if let Some(who) = rec.busy.clone() {
            return Some((Cause::PlayingElse { speaker: k.clone(), who }, None));
        }
        if let Some((with, at)) = rec.grouped.clone() {
            return Some((Cause::GroupedInto { speaker: k.clone(), with }, Some(at)));
        }
        if rec.may_be_in_use.is_some() {
            return Some((Cause::Disconnected { speaker: k.clone() }, None));
        }
        None
    }

    /// Sound starts on the place's speaker, which isn't connected: where
    /// the app knows the speaker can't play, the sound goes to this PC
    /// without a try; else the one try. Nothing is held meanwhile.
    /// `may_be_short` for sound no player started: a short sound neither
    /// connects the speaker nor moves the place.
    pub(super) fn sound_starts(&mut self, may_be_short: bool, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some(k) = self.place.clone() else { return };
        let fresh = self.stream == Stream::Off;
        if let Some((cause, at)) = self.known_cant_play(&k, w, now) {
            if may_be_short && fresh && self.pc_target(w).is_some() {
                self.stream = Stream::ShortOnPc { since_ms: now.mono_ms };
                return;
            }
            return self.cant_play_at_start(k, cause, at, w, now, out);
        }
        let waiting_for_network = w.connections.is_empty() && (self.in_grace(now) || self.just_changed_network(now));
        if waiting_for_network {
            if !matches!(self.stream, Stream::WaitNet { .. }) {
                self.stream = Stream::WaitNet { since_ms: now.mono_ms, until_ms: now.mono_ms + NETWORK_WAIT_MS };
            }
            return;
        }
        if may_be_short && fresh && self.pc_target(w).is_some() {
            self.stream = Stream::ShortWait { since_ms: now.mono_ms };
            return;
        }
        self.connect_place(w, now, out);
    }

    /// The one try at the place's speaker when sound starts. The speaker
    /// gets as long as the tries during play to be ready; an answer that it
    /// can't play ends the try at once.
    pub(super) fn connect_place(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some(k) = self.place.clone() else { return };
        if self.switch.as_ref().is_some_and(|s| s.speaker == k) {
            self.stream = Stream::Released;
            return;
        }
        let gen = self.next_gen();
        let way = self.place_way(w);
        out.push(Do::Connect(Job { gen, speaker: k, way, welcome_level: None, purpose: Purpose::SoundStarted, look_first: true, in_window: false }));
        self.stream = Stream::Connecting { gen, asked_ms: now.mono_ms, until_ms: now.mono_ms + START_TRY_MS, restoring: self.stream == Stream::Released };
    }

    pub(super) fn place_connect_ended(&mut self, ended: Ended, w: &World, now: Clock, out: &mut Vec<Do>) {
        if !matches!(self.stream, Stream::Connecting { .. }) {
            return;
        }
        if ended != Ended::Ready {
            return self.place_connect_failed(ended, w, now, out);
        }
        let Some(k) = self.place.clone() else { return };
        self.stream = Stream::On { since_wall: now.wall };
        {
            let rec = self.rec(&k);
            rec.away = None;
            rec.answered_ms = Some(now.mono_ms);
        }
        // The speaker plays again: a note about its failed start has
        // nothing left to say.
        if self.note.as_ref().is_some_and(|n| n.note.cause.speaker() == &k) {
            self.clear_note();
            out.push(Do::Save);
        }
        self.start_level(&k, w, now, out);
    }

    /// The try at a sound's start failed: the sound that started goes on
    /// from this PC.
    pub(super) fn place_connect_failed(&mut self, ended: Ended, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some(k) = self.place.clone() else { return };
        self.stream = Stream::Off;
        let (cause, at) = match ended {
            Ended::Busy { who } => {
                self.rec(&k).busy = Some(who.clone());
                (Cause::PlayingElse { speaker: k.clone(), who }, None)
            }
            Ended::NeedsSecret(secret) | Ended::WrongSecret(secret) => {
                (Cause::NeedsSecret { speaker: k.clone(), secret }, Some(now.local))
            }
            Ended::NotFound => (Cause::NotRespondingAtStart { speaker: k.clone(), why: Away::NotFound }, Some(now.local)),
            Ended::NoNetwork => (Cause::NotRespondingAtStart { speaker: k.clone(), why: Away::NoNetwork }, Some(now.local)),
            _ => (Cause::NotRespondingAtStart { speaker: k.clone(), why: Away::NotResponding }, Some(now.local)),
        };
        if let Cause::NotRespondingAtStart { why: why @ (Away::NotResponding | Away::NotFound), .. } = &cause {
            let why = *why;
            if !self.in_grace(now) {
                self.rec(&k).away = Some(why);
            }
            self.rec(&k).last_try = Some(if why == Away::NotFound { PickWhy::NotFound } else { PickWhy::NotResponding });
        }
        self.cant_play_at_start(k, cause, at, w, now, out);
    }

    /// Sound started and the place's speaker can't play it: the sound goes
    /// on from this PC, and the one fixed choice says what the place is
    /// from then on.
    fn cant_play_at_start(
        &mut self,
        k: SpeakerId,
        cause: Cause,
        at: Option<LocalTime>,
        w: &World,
        now: Clock,
        out: &mut Vec<Do>,
    ) {
        match self.after_failed_start {
            AfterFailedStart::Speaker if matches!(cause, Cause::NotRespondingAtStart { .. }) => {
                // The speaker stays the place; what plays is passed on to
                // the PC's own output for as long as it plays.
                self.stream_off(out);
                self.stream = Stream::OnPc;
                if self.note.as_ref().is_none_or(|n| n.note.cause.speaker() != &k) {
                    let note = Note { cause, at, available_again: false, ahead: None };
                    self.note = Some(NoteRec { note: note.clone(), wall: now.wall, mono_ms: now.mono_ms });
                    self.raise_popup(Popup::Note(note), w, now);
                }
                self.listen_to(&k, now);
                out.push(Do::Save);
            }
            // This PC is the place until a person switches. (Someone else
            // playing on the speaker, or a code it asks for, never leaves
            // it the place: waiting doesn't make it play.)
            AfterFailedStart::ThisPc | AfterFailedStart::Speaker => self.switched_to_pc(k, cause, at, true, w, now, out),
        }
    }

    /// The place's speaker stopped responding while it played: the tries.
    /// Nothing is paused; what plays meanwhile isn't heard.
    fn start_tries(&mut self, k: SpeakerId, w: &World, now: Clock) {
        if self.switch.as_ref().is_some_and(|s| s.speaker == k) {
            self.stream = Stream::Released;
            return;
        }
        let cause = self.cause_for_drop(&k, w, now);
        self.stream = Stream::Trying(Tries { since_ms: now.mono_ms, started: 0, job: None, cause, at: now.local });
    }

    /// Why the stream to `k` broke: this PC's own network, or the speaker.
    fn cause_for_drop(&self, k: &SpeakerId, w: &World, now: Clock) -> Cause {
        if w.connections.is_empty() {
            let kind = self.net_lost.map_or(ConnKind::WiFi, |(kind, _)| kind);
            return Cause::NetworkLost { speaker: k.clone(), kind };
        }
        Cause::StoppedResponding {
            speaker: k.clone(),
            vpn: self
                .vpn
                .as_ref()
                .filter(|(_, t)| now.mono_ms.saturating_sub(*t) <= VPN_NAMED_WITHIN_MS)
                .map(|(v, _)| v.clone()),
        }
    }

    pub(super) fn tries_due(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Stream::Trying(mut t) = self.stream.clone() else { return };
        let Some(k) = self.place.clone() else { return };
        let elapsed = now.mono_ms.saturating_sub(t.since_ms);
        if elapsed >= TRIES_DURING_PLAY_MS {
            return self.tries_end(w, now, out);
        }
        if t.started < TRIES_AT_MS.len() && elapsed >= TRIES_AT_MS[t.started] {
            // A try that stalls counts as failed when the next is due.
            if let Some(gen) = t.job.take() {
                out.push(Do::Supersede(gen));
            }
            let gen = self.next_gen();
            let way = self.place_way(w);
            out.push(Do::Connect(Job { gen, speaker: k, way, welcome_level: None, purpose: Purpose::Retry, look_first: false, in_window: false }));
            t.started += 1;
            t.job = Some(gen);
            self.stream = Stream::Trying(t);
        }
    }

    /// The tries end without an answer: the sound goes on from this PC.
    /// Where nothing plays any more there is no sound to move: the speaker
    /// stays the place, known to be away.
    fn tries_end(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Stream::Trying(t) = self.stream.clone() else { return };
        let Some(k) = self.place.clone() else { return };
        if self.sent_to_speaker(now) {
            // It ends the stream as it moves the sound: the try on its way
            // is no longer wanted, and the connection the tries were for is
            // let go.
            return self.switched_to_pc(k, t.cause, Some(t.at), true, w, now, out);
        }
        // The connection that stopped answering is let go: a speaker that
        // comes back is not left holding a stream nothing is sent on.
        out.push(Do::StopSending { speaker: k.clone(), take_off: false });
        self.stream_off(out);
        self.rec(&k).away = Some(Away::NotResponding);
        self.listen_to(&k, now);
    }

    pub(super) fn try_ended(&mut self, ended: Ended, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some(k) = self.place.clone() else { return };
        match ended {
            Ended::Ready => {
                // It answered within the tries: the sound plays on.
                self.stream = Stream::On { since_wall: now.wall };
                let rec = self.rec(&k);
                rec.away = None;
                rec.answered_ms = Some(now.mono_ms);
            }
            Ended::Busy { who } => {
                self.stream = Stream::Off;
                self.cant_play(k.clone(), Cause::PlayingElse { speaker: k, who }, w, now, out);
            }
            Ended::NeedsSecret(secret) | Ended::WrongSecret(secret) => {
                self.stream = Stream::Off;
                self.cant_play(k.clone(), Cause::NeedsSecret { speaker: k, secret }, w, now, out);
            }
            _ => {
                if let Stream::Trying(t) = &mut self.stream {
                    t.job = None;
                }
            }
        }
    }

    pub(super) fn way_lost(&mut self, speaker: SpeakerId, ended: Ended, w: &World, now: Clock, out: &mut Vec<Do>) {
        if self.connected_speaker() != Some(&speaker) { return; }
        let cause = match ended {
            Ended::NeedsSecret(secret) | Ended::WrongSecret(secret) => Cause::NeedsSecret { speaker: speaker.clone(), secret },
            Ended::Busy { who } => Cause::PlayingElse { speaker: speaker.clone(), who },
            Ended::Refused => Cause::WayFailed { speaker: speaker.clone(), why: PickWhy::Refused },
            Ended::CouldntStart => Cause::WayFailed { speaker: speaker.clone(), why: PickWhy::CouldntStart },
            Ended::NotFound => Cause::WayFailed { speaker: speaker.clone(), why: PickWhy::NotFound },
            Ended::NotResponding | Ended::NoNetwork => self.cause_for_drop(&speaker, w, now),
            Ended::Ready => return,
        };
        self.cant_play(speaker, cause, w, now, out);
    }

    /// The place's speaker, connected until now, can't play this PC's sound
    /// any more and can be reached (someone else plays on it, it asks for
    /// its code, it was grouped away). Sound that is playing goes on from
    /// this PC. While nothing plays there is nothing to move: the stream
    /// ends, the speaker stays the place, and the next start of sound finds
    /// out what is so then.
    fn cant_play(&mut self, k: SpeakerId, cause: Cause, w: &World, now: Clock, out: &mut Vec<Do>) {
        match &cause {
            Cause::PlayingElse { who, .. } => self.rec(&k).busy = Some(who.clone()),
            Cause::Disconnected { .. } => self.rec(&k).may_be_in_use = Some(now.wall),
            Cause::GroupedInto { with, .. } => self.rec(&k).grouped = Some((with.clone(), now.local)),
            _ => {}
        }
        if self.sent_to_speaker(now) {
            return self.switched_to_pc(k, cause, Some(now.local), true, w, now, out);
        }
        if self.stream_connected() {
            out.push(Do::StopSending { speaker: k, take_off: false });
        }
        self.stream_off(out);
        out.push(Do::Save);
    }

    /// The app moves the sound off speaker `k`, to this PC, because the
    /// speaker can't play this PC's sound (or shouldn't any more: the
    /// Settings option, a removal). Nothing is paused: Windows goes to the
    /// PC's own output at once and what plays carries on there. One note
    /// says what happened, at `at`, and offers the way back; where sound is
    /// `playing` (or starting) it also shows by the clock. The app never
    /// moves the sound back by itself.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn switched_to_pc(
        &mut self,
        k: SpeakerId,
        cause: Cause,
        at: Option<LocalTime>,
        playing: bool,
        w: &World,
        now: Clock,
        out: &mut Vec<Do>,
    ) {
        let here = self.place.as_ref() == Some(&k) && !self.device_missing;
        // A speaker that is removed can still be told to stop; the others
        // can't play or can't be reached.
        let reachable = matches!(cause, Cause::Removed { .. });
        if here {
            let target = self.win_pc(w);
            if reachable && self.stream_connected() {
                self.expect_default = target.as_ref().map(switch::win_ref);
                out.push(Do::Silence { speaker: k.clone(), then: target });
            } else {
                if self.stream_connected() {
                    out.push(Do::StopSending { speaker: k.clone(), take_off: false });
                }
                if let Some(t) = target {
                    self.win(t, w, out);
                }
            }
        }
        self.stream_off(out);
        if self.place.as_ref() == Some(&k) {
            self.place = None;
            self.pc_choice = None;
        }
        match &cause {
            Cause::StoppedResponding { .. } | Cause::NetworkLost { .. } => self.rec(&k).away = Some(Away::NotResponding),
            Cause::PlayingElse { who, .. } => self.rec(&k).busy = Some(who.clone()),
            Cause::Disconnected { .. } => self.rec(&k).may_be_in_use = Some(now.wall),
            _ => {}
        }
        // What was about the speaker as the place goes; a confirmation
        // that waits for its answer stays.
        self.clear_note();
        self.popup = None;
        self.pick_failed = None;
        self.passed_pause.clear();
        self.paused_at_speaker = false;
        self.after_break = false;
        self.joined = None;
        self.last_switch_wall = Some(now.wall);
        let note = Note { cause, at, available_again: false, ahead: None };
        if playing && note.shows() {
            self.raise_popup(Popup::Note(note.clone()), w, now);
        }
        if can_be_available_again(&note.cause) {
            self.listen_to(&k, now);
        }
        self.note = Some(NoteRec { note, wall: now.wall, mono_ms: now.mono_ms });
        out.push(Do::Save);
    }

    /// Starts asking `k` whether it can play again.
    fn listen_to(&mut self, k: &SpeakerId, now: Clock) {
        self.listening.insert(k.clone(), Listen { since_ms: now.mono_ms, next_ms: now.mono_ms + LISTEN_FAST_EVERY_MS });
    }

    /// A speaker said something.
    pub(super) fn speaker_said(&mut self, id: SpeakerId, signal: SpeakerSignal, w: &World, now: Clock, out: &mut Vec<Do>) {
        let is_place = self.place.as_ref() == Some(&id) && !self.device_missing;
        let playing_here = is_place && matches!(self.stream, Stream::On { .. } | Stream::Trying(_));
        let grace = self.in_grace(now);
        match signal {
            SpeakerSignal::Responding | SpeakerSignal::PlayingOurStream => {
                let rec = self.rec(&id);
                rec.away = None;
                rec.last_try = None;
                rec.answered_ms = Some(now.mono_ms);
                if is_place {
                    self.listening.remove(&id);
                    if signal == SpeakerSignal::PlayingOurStream && (self.paused_at_speaker || !self.passed_pause.is_empty()) {
                        self.play_from_speaker(w, out);
                    }
                }
            }
            SpeakerSignal::Announced => {
                let rec = self.rec(&id);
                if rec.away == Some(Away::NotFound) {
                    rec.away = None;
                }
                if rec.last_try == Some(PickWhy::NotFound) {
                    rec.last_try = None;
                }
                if rec.away.is_none() {
                    rec.answered_ms = Some(now.mono_ms);
                }
            }
            SpeakerSignal::NotResponding | SpeakerSignal::EndedStream { reachable: false } | SpeakerSignal::Unannounced => {
                let away = if signal == SpeakerSignal::Unannounced { Away::NotFound } else { Away::NotResponding };
                if is_place {
                    match self.stream {
                        Stream::On { .. } => {
                            if self.sent_to_speaker(now) {
                                return self.start_tries(id, w, now);
                            }
                            // Nothing plays: the speaker stays the place.
                            out.push(Do::StopSending { speaker: id.clone(), take_off: false });
                            self.stream = Stream::Off;
                        }
                        Stream::Connecting { .. } | Stream::Trying(_) => return,
                        _ => {}
                    }
                }
                if !grace || !trials::T7_AWAY_ONLY_AFTER_A_FAILED_REQUEST {
                    self.rec(&id).away = Some(away);
                    if is_place && !self.listening.contains_key(&id) {
                        // The app asks whether it can play again, as after
                        // moving the sound off it.
                        self.listen_to(&id, now);
                    }
                }
            }
            SpeakerSignal::Silenced => {}
            SpeakerSignal::AsksSecret(secret) => {
                if is_place && self.stream_connected() {
                    self.cant_play(id.clone(), Cause::NeedsSecret { speaker: id, secret }, w, now, out);
                }
            }
            SpeakerSignal::PlayingElse { who } => {
                self.rec(&id).busy = Some(who.clone());
                if playing_here {
                    self.cant_play(id.clone(), Cause::PlayingElse { speaker: id, who }, w, now, out);
                }
            }
            SpeakerSignal::Free => {
                let rec = self.rec(&id);
                rec.busy = None;
                rec.answered_ms = Some(now.mono_ms);
            }
            SpeakerSignal::PausedAtSpeaker | SpeakerSignal::StoppedAtSpeaker => {
                if is_place && matches!(self.stream, Stream::On { .. }) {
                    self.pause_from_speaker(w, now, out);
                }
            }
            SpeakerSignal::PlayPressed => {
                if is_place {
                    self.play_from_speaker(w, out);
                }
            }
            SpeakerSignal::EndedStream { reachable: true } => {
                if playing_here {
                    let shows = self.speaker_world(&id, w).is_some_and(|s| s.shows_doing);
                    if !shows && trials::T4_REACHABLE_END_IS_SOMEONE_ELSE {
                        self.cant_play(id.clone(), Cause::Disconnected { speaker: id }, w, now, out);
                    } else {
                        // A stop that can't be told from a stream that
                        // broke counts as a stop pressed on the speaker.
                        self.stream = Stream::On { since_wall: now.wall };
                        self.pause_from_speaker(w, now, out);
                    }
                }
            }
            SpeakerSignal::JoinedGroup { room } => {
                if is_place && self.real_at_place(now) {
                    self.joined = Some(room.clone());
                    self.raise_popup(Popup::AlsoPlayingIn { room }, w, now);
                }
            }
            SpeakerSignal::GroupedInto { with } => {
                self.rec(&id).grouped = Some((with.clone(), now.local));
                if playing_here {
                    self.cant_play(id.clone(), Cause::GroupedInto { speaker: id, with }, w, now, out);
                }
            }
            SpeakerSignal::LeftGroup => {
                let rec = self.rec(&id);
                rec.grouped = None;
                rec.answered_ms = Some(now.mono_ms);
            }
        }
    }

    /// Pause (or stop) was pressed on the place's speaker itself: the app
    /// passes it on to the players whose sound was playing there, as a
    /// pause pressed in them would, and does nothing more. Sound that no
    /// player's pause stops goes on, sent and not heard: the situation says
    /// so while it lasts.
    fn pause_from_speaker(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let mut players: Vec<PlayerId> = Vec::new();
        let mut other = false;
        for app in self.detector.apps_on(&OutputRef::Ours, now.mono_ms) {
            let playing: Vec<&PlayerWorld> =
                w.players.iter().filter(|p| p.app == app && p.status == PlayerStatus::Playing).collect();
            if playing.is_empty() {
                // A player the pause already stopped still counts as its
                // app's sound for a moment.
                other |= !w.players.iter().any(|p| p.app == app && self.passed_pause.contains(&p.id));
            }
            players.extend(playing.into_iter().map(|p| p.id.clone()));
        }
        self.paused_at_speaker = other;
        if players.is_empty() {
            return;
        }
        out.push(Do::Pause(players.clone()));
        for p in players {
            if !self.passed_pause.contains(&p) {
                self.passed_pause.push(p);
            }
        }
    }

    /// Play was pressed on the place's speaker itself: where that is
    /// counted on, the players its pause stopped, and nobody touched
    /// since, start again.
    fn play_from_speaker(&mut self, w: &World, out: &mut Vec<Do>) {
        self.paused_at_speaker = false;
        let players: Vec<PlayerId> = std::mem::take(&mut self.passed_pause)
            .into_iter()
            .filter(|id| w.players.iter().any(|p| &p.id == id && p.status == PlayerStatus::Paused))
            .collect();
        if trials::T3_PLAY_ON_SPEAKER_STARTS_PLAYER && !players.is_empty() {
            out.push(Do::Play(players));
        }
    }

    /// What follows from the sound now: **Playing on …** when a speaker
    /// plays sound that started after a break, the time sound last played,
    /// a joined room while its sound lasts.
    pub(super) fn follow_sound(&mut self, w: &World, now: Clock) {
        let real = self.real_at_place(now);
        if real && !self.was_real {
            self.after_break = now.wall.ms_since(self.quiet_from()) >= BREAK_WALL_MS;
        }
        if !real {
            self.after_break = false;
            self.joined = None;
        }
        let on_speaker = !self.device_missing && matches!(self.stream, Stream::On { .. });
        if !on_speaker || (self.was_real && !real) {
            self.paused_at_speaker = false;
        }
        self.was_real = real;
        if self.after_break && on_speaker {
            self.after_break = false;
            if let Some(k) = self.place.clone() {
                self.raise_popup(Popup::PlayingOn { speaker: k }, w, now);
            }
        }
        if self.detector.real_where(now.mono_ms, |_| true) {
            self.last_real_wall = Some(now.wall);
            if let Some(k) = self.place.clone() {
                if real {
                    if let Some(l) = self.levels.get_mut(&k) {
                        l.at = now.wall;
                    }
                }
            }
        }
    }

    /// The note follows what the app knows about its speaker: once there
    /// is a sign that the speaker can play again the note says so and its
    /// button is a plain switch, and it goes back when the speaker fails
    /// again. Nothing shows by the clock for either.
    pub(super) fn check_available(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some(n) = &self.note else { return };
        let k = n.note.cause.speaker().clone();
        let rec = self.recs.get(&k).cloned().unwrap_or_default();
        let answered = rec.answered_ms.is_some_and(|t| t >= n.mono_ms);
        let found = self.speaker_world(&k, w).is_some_and(|s| s.found);
        // What the app knows now, where it knows anything.
        let sign = match &n.note.cause {
            Cause::StoppedResponding { .. } | Cause::NetworkLost { .. } | Cause::NotRespondingAtStart { .. } => {
                if rec.away.is_some() || (!found && !self.in_grace(now)) {
                    Some(false)
                } else if answered && found {
                    Some(true)
                } else {
                    None
                }
            }
            Cause::PlayingElse { .. } => match (&rec.busy, answered) {
                (Some(_), _) => Some(false),
                (None, true) => Some(true),
                (None, false) => None,
            },
            Cause::GroupedInto { .. } => match (&rec.grouped, answered) {
                (Some(_), _) => Some(false),
                (None, true) => Some(true),
                (None, false) => None,
            },
            Cause::OtherNetwork { .. } => Some(found && !w.connections.is_empty()),
            _ => None,
        };
        let Some(can) = sign.filter(|can| *can != n.note.available_again) else { return };
        if can {
            self.listening.remove(&k);
            self.rec(&k).last_try = None;
        } else {
            // It failed again: the app goes back to asking.
            self.listen_to(&k, now);
        }
        if let Some(n) = &mut self.note {
            n.note.available_again = can;
            // A pop-up that shows the note says the same as the panel.
            if let Some((Shown { what: Popup::Note(shown), .. }, _)) = &mut self.popup {
                *shown = n.note.clone();
            }
        }
        out.push(Do::Save);
    }

    /// Asks the speakers that couldn't play whether they can play again.
    pub(super) fn listen_due(&mut self, now: Clock, out: &mut Vec<Do>) {
        let t = now.mono_ms;
        let mut done = Vec::new();
        for (k, l) in self.listening.iter_mut() {
            if t.saturating_sub(l.since_ms) >= LISTEN_FOR_MS {
                done.push(k.clone());
                continue;
            }
            if t >= l.next_ms {
                out.push(Do::Look(k.clone()));
                let every =
                    if t.saturating_sub(l.since_ms) < LISTEN_FAST_FOR_MS { LISTEN_FAST_EVERY_MS } else { LISTEN_SLOW_EVERY_MS };
                l.next_ms = t + every;
            }
        }
        for k in done {
            self.listening.remove(&k);
        }
    }
}

/// Whether the app listens for the speaker to play again after this move:
/// never after a code or a disconnect, never for its own option, a removal,
/// or sound that simply stayed on this PC.
fn can_be_available_again(cause: &Cause) -> bool {
    matches!(
        cause,
        Cause::StoppedResponding { .. }
            | Cause::NetworkLost { .. }
            | Cause::NotRespondingAtStart { .. }
            | Cause::PlayingElse { .. }
            | Cause::GroupedInto { .. }
            | Cause::OtherNetwork { .. }
    )
}
