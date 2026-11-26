//! A person's switches: they try once, take effect when the speaker is
//! ready (and confirmed where the app asks first), and change nothing when
//! they don't get ready: until then the sound stays where it is (a change
//! of the way a speaker is reached can stop its sound while the other way
//! connects). A newer
//! switch, *Cancel*, a click on the row of the place, or headphones being
//! connected stops one still connecting or waiting; nothing else does. A
//! switch to a speaker that plays something else stops what plays there
//! without a question. Also the test chime and first run.

use super::*;
use crate::model::command::FirstRunAction;
use crate::model::situation::PickWhy;

impl Core {
    /// A switch to speaker `k`. `take_over` when the person already
    /// answered that it may stop what plays there. `windows_prev` is the
    /// output Windows was on, for a switch made in Windows' list.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn switch_request(
        &mut self,
        k: SpeakerId,
        way: Option<WayChoice>,
        by: SwitchBy,
        who: Asker,
        take_over: bool,
        w: &World,
        now: Clock,
        out: &mut Vec<Do>,
    ) -> Result<Effect, Refusal> {
        self.switch_request_from_list(k, way, by, who, take_over, None, w, now, out)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn switch_request_from_list(
        &mut self,
        k: SpeakerId,
        way: Option<WayChoice>,
        by: SwitchBy,
        who: Asker,
        take_over: bool,
        windows_prev: Option<PcOutputId>,
        w: &World,
        now: Clock,
        out: &mut Vec<Do>,
    ) -> Result<Effect, Refusal> {
        if self.device_missing {
            return Ok(Effect::Done);
        }
        let way = self.way_for(&k, way, w);
        // The row of the place does nothing; a switch on its way to another
        // place is cancelled, since the newest choice is where the sound is.
        if self.place.as_ref() == Some(&k) && way == self.place_way(w) {
            self.cancel_switch(w, out);
            self.confirm = None;
            return Ok(Effect::Done);
        }
        if self.speaker_world(&k, w).is_some_and(|s| s.only_from_window) && !who.in_window {
            return Err(Refusal::OnlyFromWindow { speaker: k });
        }
        if self.switch.as_ref().is_some_and(|s| s.speaker == k && s.way == way) {
            // Already on its way.
            return Ok(Effect::Done);
        }
        let confirm = if take_over { None } else { self.needs_confirm(&k, way, w) };
        // A surface that can't ask is refused before anything changes.
        if confirm.is_some() && who.from == SwitchFrom::Remote {
            return Err(Refusal::OnlyFromWindow { speaker: k });
        }
        self.cancel_switch(w, out);
        self.confirm = None;
        self.pick_failed = None;
        if let Some(what) = confirm {
            self.ask(what, by, who, windows_prev.clone(), w, now, out);
            return Ok(Effect::Done);
        }
        // Where the app doesn't ask before it takes a busy speaker, it
        // doesn't look first either: the switch stops what plays there.
        let take_over = take_over || !self.ask_before_taking;
        self.start_switch(k, way, by, who, windows_prev, take_over, now, out);
        let _ = w;
        Ok(Effect::Done)
    }

    /// The confirmation a switch to `k` needs first: where the app asks
    /// before it takes a busy speaker, that question comes first.
    fn needs_confirm(&self, k: &SpeakerId, way: WayChoice, w: &World) -> Option<Confirm> {
        if self.ask_before_taking {
            let rec = self.recs.get(k);
            if let Some(who) = rec.and_then(|r| r.busy.clone()) {
                return Some(Confirm::Busy { speaker: k.clone(), way, who });
            }
            if rec.is_some_and(|r| r.may_be_in_use.is_some()) {
                return Some(Confirm::MayBeInUse { speaker: k.clone(), way });
            }
        }
        self.screen_reader_confirm(k, way, w)
    }

    fn screen_reader_confirm(&self, k: &SpeakerId, way: WayChoice, w: &World) -> Option<Confirm> {
        (w.screen_reader && !w.settings.only_some_apps && !self.screen_reader_answered.contains(k))
            .then(|| Confirm::ScreenReader { speaker: k.clone(), way })
    }

    /// Asks `what`: in the panel, or by the clock for the tray and Windows'
    /// list while the panel is closed (Windows goes back to the output it
    /// was on meanwhile).
    #[allow(clippy::too_many_arguments)]
    fn ask(
        &mut self,
        what: Confirm,
        by: SwitchBy,
        who: Asker,
        windows_prev: Option<PcOutputId>,
        w: &World,
        now: Clock,
        out: &mut Vec<Do>,
    ) {
        if who.from == SwitchFrom::WindowsList {
            if let Some(prev) = windows_prev.clone().or_else(|| self.pc_target(w)) {
                self.win(WinTarget::Output(prev), w, out);
            }
        }
        let by_clock = matches!(who.from, SwitchFrom::Tray | SwitchFrom::WindowsList) && !self.panel_open;
        let seq = self.next_seq();
        self.confirm = Some(Confirming { what, by, who, windows_prev, by_clock, seq, since_ms: now.mono_ms });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_switch(
        &mut self,
        k: SpeakerId,
        way: WayChoice,
        by: SwitchBy,
        who: Asker,
        windows_prev: Option<PcOutputId>,
        take_over: bool,
        now: Clock,
        out: &mut Vec<Do>,
    ) {
        let _ = now;
        let gen = self.next_gen();
        let purpose = match by {
            SwitchBy::Switch | SwitchBy::FirstRun => Purpose::Switch,
            SwitchBy::TryAgain => Purpose::TryAgain,
            SwitchBy::Way => Purpose::Way,
        };
        self.rec(&k).last_try = None;
        out.push(Do::Connect(Job {
            gen,
            speaker: k.clone(),
            way,
            purpose,
            chime_level: None,
            look_first: !take_over,
            in_window: who.in_window,
        }));
        self.switch = Some(Switching { gen, speaker: k, way, by, who, windows_prev });
    }

    /// A person's switch to This PC (on `output`, or on This PC's output).
    /// The row of the place does nothing, but it stops a switch that is
    /// connecting or waiting.
    pub(super) fn switch_to_pc(&mut self, output: Option<PcOutputId>, w: &World, now: Clock, out: &mut Vec<Do>) {
        self.cancel_switch(w, out);
        self.confirm = None;
        if self.place.is_none() && (output.is_none() || output == self.pc_target(w)) {
            return;
        }
        if output.is_some() {
            self.pc_choice = output;
        }
        let target = self.win_pc(w);
        match self.place.clone() {
            Some(k) if !self.device_missing => {
                if self.stream_connected() {
                    self.expect_default = target.as_ref().map(win_ref);
                    out.push(Do::Silence { speaker: k, then: target });
                } else if let Some(t) = target {
                    self.win(t, w, out);
                }
                self.stream_off(out);
            }
            _ => {
                if let Some(t) = target {
                    self.win(t, w, out);
                }
            }
        }
        self.place = None;
        self.place_changed(now);
        out.push(Do::Save);
    }

    /// A switch on its way stops (a newer switch took its place, or the
    /// person cancelled it); the sound stays where it is.
    pub(super) fn cancel_switch(&mut self, w: &World, out: &mut Vec<Do>) {
        if let Some(s) = self.switch.take() {
            out.push(Do::Supersede(s.gen));
            if s.who.from == SwitchFrom::WindowsList {
                if let Some(prev) = s.windows_prev.or_else(|| self.pc_target(w)) {
                    self.win(WinTarget::Output(prev), w, out);
                }
            }
        }
    }

    /// The answer to the confirmation that shows.
    pub(super) fn answer(&mut self, yes: bool, w: &World, now: Clock, out: &mut Vec<Do>) -> Result<Effect, Refusal> {
        let Some(c) = self.confirm.take() else { return Ok(Effect::Done) };
        if !yes {
            return Ok(Effect::Done);
        }
        match c.what {
            Confirm::ChimeBusy { speaker, .. } => {
                self.start_chime(speaker, true, c.by, c.who.in_window, w, out);
            }
            Confirm::Busy { speaker, way, .. } | Confirm::MayBeInUse { speaker, way } => {
                if let Some(next) = self.screen_reader_confirm(&speaker, way, w) {
                    let seq = self.next_seq();
                    self.confirm = Some(Confirming { what: next, seq, since_ms: now.mono_ms, ..c });
                    return Ok(Effect::Done);
                }
                self.start_switch(speaker, way, c.by, c.who, c.windows_prev, true, now, out);
            }
            Confirm::ScreenReader { speaker, way } => {
                self.screen_reader_answered.insert(speaker.clone());
                out.push(Do::Save);
                let take_over = !self.ask_before_taking
                    || self.recs.get(&speaker).is_some_and(|r| r.busy.is_some() || r.may_be_in_use.is_some());
                self.start_switch(speaker, way, c.by, c.who, c.windows_prev, take_over, now, out);
            }
        }
        Ok(Effect::Done)
    }

    /// *Play a test chime*: on that speaker only; a busy one asks first.
    /// That the person already answered counts only from the app's
    /// windows, where the question was asked; a surface that can't ask is
    /// refused where it would have to.
    pub(super) fn chime_request(
        &mut self,
        k: SpeakerId,
        confirmed: bool,
        by: SwitchBy,
        asker: Asker,
        w: &World,
        now: Clock,
        out: &mut Vec<Do>,
    ) -> Result<Effect, Refusal> {
        if self.speaker_world(&k, w).is_some_and(|s| s.only_from_window) && !asker.in_window {
            return Err(Refusal::OnlyFromWindow { speaker: k });
        }
        let confirmed = confirmed && asker.in_window;
        if by == SwitchBy::FirstRun {
            self.leave_first_run_step(out);
        }
        if !confirmed {
            if let Some(who) = self.recs.get(&k).and_then(|r| r.busy.clone()) {
                if asker.from == SwitchFrom::Remote {
                    return Err(Refusal::OnlyFromWindow { speaker: k });
                }
                if by == SwitchBy::FirstRun { self.first_run = FirstRun::One; }
                self.ask(Confirm::ChimeBusy { speaker: k, who }, by, asker, None, w, now, out);
                return Ok(Effect::Done);
            }
        }
        self.start_chime(k, confirmed, by, asker.in_window, w, out);
        Ok(Effect::Done)
    }

    fn cancel_chime(&mut self, out: &mut Vec<Do>) {
        if let Some(job) = self.chime.take() {
            out.push(Do::Supersede(job.gen));
        }
    }

    fn start_chime(&mut self, k: SpeakerId, take_over: bool, by: SwitchBy, in_window: bool, w: &World, out: &mut Vec<Do>) {
        self.cancel_chime(out);
        let chime_level = (by == SwitchBy::FirstRun).then(|| {
            let speaker = self.speaker_world(&k, w);
            let level = match &self.first_run {
                FirstRun::Two(id, level) if *id == k => *level,
                _ => speaker.and_then(|s| s.level).unwrap_or_else(|| {
                    levels::start_level(true, None, self.levels.get(&k).copied(), None, self.now.wall)
                        .expect("an unreadable speaker has a starting level")
                }),
            };
            level.min(speaker.and_then(|s| s.max_level).unwrap_or(100))
        });
        let way = self.way_for(&k, None, w);
        let job = Job { gen: self.next_gen(), speaker: k, way, purpose: Purpose::Chime, chime_level, look_first: !take_over, in_window };
        out.push(Do::Connect(job.clone()));
        self.chime = Some(job);
    }

    fn leave_first_run_step(&mut self, out: &mut Vec<Do>) {
        if self.chime.as_ref().is_some_and(|job| job.chime_level.is_some()) {
            self.cancel_chime(out);
        }
        if self.confirm.as_ref().is_some_and(|c| c.by == SwitchBy::FirstRun) {
            self.confirm = None;
        }
    }

    /// An act in first run.
    pub(super) fn first_run_action(
        &mut self,
        act: FirstRunAction,
        who: Asker,
        w: &World,
        now: Clock,
        out: &mut Vec<Do>,
    ) -> Result<Effect, Refusal> {
        match act {
            FirstRunAction::Chime(k) => {
                if self.first_run == FirstRun::One {
                    self.pick_failed = None;
                    return self.chime_request(k, false, SwitchBy::FirstRun, who, w, now, out);
                }
            }
            FirstRunAction::ChimeAgain => {
                if let FirstRun::Two(k, _) = self.first_run.clone() {
                    return self.chime_request(k, false, SwitchBy::FirstRun, who, w, now, out);
                }
            }
            FirstRunAction::ChimeVolume(value) => {
                if let FirstRun::Two(k, _) = self.first_run.clone() {
                    let limit = self.speaker_world(&k, w).and_then(|s| s.max_level).unwrap_or(100);
                    self.first_run = FirstRun::Two(k, value.min(limit));
                }
            }
            FirstRunAction::Heard | FirstRunAction::UseWithoutChecking => {
                if let FirstRun::Two(k, level) = self.first_run.clone() {
                    // *Volume on Kitchen* set the chime's level: the sound
                    // starts on Kitchen at it.
                    self.levels.insert(k.clone(), KeptLevel { level, at: now.wall });
                    self.leave_first_run_step(out);
                    if self.place.as_ref() == Some(&k) && !self.device_missing {
                        self.cancel_switch(w, out);
                        self.first_run = FirstRun::Three(k);
                        self.first_run_done = true;
                        out.push(Do::Save);
                    } else {
                        return self.switch_request(k, None, SwitchBy::FirstRun, who, false, w, now, out);
                    }
                }
            }
            FirstRunAction::DidntHear => {}
            FirstRunAction::ChooseAnother => {
                if matches!(self.first_run, FirstRun::Two(..)) {
                    self.leave_first_run_step(out);
                    self.cancel_switch(w, out);
                    self.first_run = FirstRun::One;
                }
            }
            FirstRunAction::Close | FirstRunAction::Done => {
                self.leave_first_run_step(out);
                self.first_run = FirstRun::Off;
                self.first_run_done = true;
                out.push(Do::Save);
            }
            FirstRunAction::StartAtSignIn(on) => out.push(Do::StartAtSignIn(on)),
            FirstRunAction::ShowAgain => {
                self.leave_first_run_step(out);
                self.first_run = FirstRun::One;
                self.first_run_done = false;
                out.push(Do::Save);
            }
        }
        Ok(Effect::Done)
    }

    /// A connection the core asked for ended.
    pub(super) fn connected(&mut self, gen: u64, ended: Ended, w: &World, now: Clock, out: &mut Vec<Do>) {
        if self.switch.as_ref().is_some_and(|s| s.gen == gen) {
            let s = self.switch.take().expect("checked");
            self.switch_ended(s, ended, w, now, out);
        } else if matches!(self.stream, Stream::Connecting { gen: g, .. } if g == gen) {
            self.place_connect_ended(ended, w, now, out);
        } else if matches!(&self.stream, Stream::Trying(t) if t.job == Some(gen)) {
            self.try_ended(ended, w, now, out);
        } else if self.chime.as_ref().is_some_and(|job| job.gen == gen) {
            let job = self.chime.take().expect("checked");
            self.chime_ended(job, ended, w, now, out);
        }
    }

    fn switch_ended(&mut self, s: Switching, ended: Ended, w: &World, now: Clock, out: &mut Vec<Do>) {
        let why = match ended {
            Ended::Ready => return self.take_effect(s, w, now, out),
            Ended::Busy { who } if self.ask_before_taking => {
                self.rec(&s.speaker).busy = Some(who.clone());
                if s.who.from != SwitchFrom::Remote {
                    let what = Confirm::Busy { speaker: s.speaker.clone(), way: s.way, who };
                    self.ask(what, s.by, s.who, s.windows_prev, w, now, out);
                }
                return;
            }
            Ended::Busy { who } => {
                // The switch didn't look first, so this is the speaker's own
                // answer: it didn't take this PC's sound.
                self.rec(&s.speaker).busy = Some(who);
                PickWhy::Refused
            }
            Ended::NotResponding => PickWhy::NotResponding,
            Ended::NotFound => PickWhy::NotFound,
            Ended::NoNetwork => PickWhy::NoNetwork,
            Ended::NeedsSecret(secret) | Ended::WrongSecret(secret) => PickWhy::WrongSecret(secret),
            Ended::CouldntStart => PickWhy::CouldntStart,
            Ended::Refused => PickWhy::Refused,
        };
        // Nothing changed: the sound is where it was, the note says that
        // the switch didn't get ready (a note that stood before it is back
        // afterwards), and the speaker's row says why.
        self.rec(&s.speaker).last_try = Some(why);
        // A switch made outside the app's own window says so by the clock.
        if s.who.from != SwitchFrom::Window {
            self.raise_popup(Popup::DidntGetReady { speaker: s.speaker.clone(), why }, w, now);
        }
        if s.who.from == SwitchFrom::WindowsList {
            if let Some(prev) = s.windows_prev.clone().or_else(|| self.pc_target(w)) {
                self.win(WinTarget::Output(prev), w, out);
            }
        }
        self.pick_failed = Some((PickFailed { speaker: s.speaker, why, by: s.by, from: s.who.from }, self.panel_open));
    }

    /// The switch takes effect: the speaker is the place.
    fn take_effect(&mut self, s: Switching, w: &World, now: Clock, out: &mut Vec<Do>) {
        let k = s.speaker.clone();
        let before = self.place.clone();
        let was_here = before.is_some() && !self.device_missing;
        if let Some(p) = before.clone().filter(|p| *p != k) {
            if was_here && self.stream_connected() {
                out.push(Do::Silence { speaker: p, then: None });
            }
        }
        if before.as_ref() != Some(&k) || s.by == SwitchBy::Way {
            self.stream_off(out);
        }
        if !was_here || w.default_output.as_ref() != Some(&OutputRef::Ours) {
            self.revert = Some(if was_here { before.clone() } else { None });
            self.win(WinTarget::OurDevice, w, out);
        }
        self.place = Some(k.clone());
        self.ways.insert(k.clone(), s.way);
        self.named = Some(k.clone());
        self.stream = Stream::On { since_wall: now.wall };
        {
            let rec = self.rec(&k);
            rec.may_be_in_use = None;
            rec.last_try = None;
            rec.away = None;
            rec.busy = None;
            rec.grouped = None;
            rec.answered_ms = Some(now.mono_ms);
        }
        self.listening.remove(&k);
        self.place_changed(now);
        self.start_level(&k, w, now, out);
        if s.by == SwitchBy::FirstRun && matches!(&self.first_run, FirstRun::Two(id, _) if *id == k) {
            self.first_run = FirstRun::Three(k);
            self.first_run_done = true;
        }
        out.push(Do::Save);
    }

    fn chime_ended(&mut self, job: Job, ended: Ended, w: &World, now: Clock, out: &mut Vec<Do>) {
        let k = job.speaker;
        let in_window = job.in_window;
        let by = if job.chime_level.is_some() { SwitchBy::FirstRun } else { SwitchBy::Switch };
        let why = match ended {
            Ended::Ready => {
                self.rec(&k).last_try = None;
                self.rec(&k).busy = None;
                if let (FirstRun::One, Some(level)) = (&self.first_run, job.chime_level) {
                    self.first_run = FirstRun::Two(k, level);
                }
                return;
            }
            Ended::Busy { who } => {
                self.rec(&k).busy = Some(who.clone());
                if by == SwitchBy::FirstRun && matches!(&self.first_run, FirstRun::Two(id, _) if *id == k) {
                    self.first_run = FirstRun::One;
                }
                let asker = Asker { from: if in_window { SwitchFrom::Window } else { SwitchFrom::Remote }, in_window };
                if asker.from != SwitchFrom::Remote {
                    self.ask(Confirm::ChimeBusy { speaker: k, who }, by, asker, None, w, now, out);
                }
                return;
            }
            Ended::NotResponding => PickWhy::NotResponding,
            Ended::NotFound => PickWhy::NotFound,
            Ended::NoNetwork => PickWhy::NoNetwork,
            Ended::NeedsSecret(secret) | Ended::WrongSecret(secret) => PickWhy::WrongSecret(secret),
            Ended::CouldntStart => PickWhy::CouldntStart,
            Ended::Refused => PickWhy::Refused,
        };
        self.rec(&k).last_try = Some(match (by, why) {
            (SwitchBy::FirstRun, PickWhy::Refused | PickWhy::CouldntStart | PickWhy::NoNetwork) => PickWhy::NotResponding,
            _ => why,
        });
        if by == SwitchBy::FirstRun && matches!(&self.first_run, FirstRun::Two(id, _) if *id == k) {
            self.first_run = FirstRun::One;
        }
        self.pick_failed = Some((
            PickFailed { speaker: k, why, by: SwitchBy::FirstRun, from: SwitchFrom::Window },
            self.panel_open,
        ));
    }

    /// The level `k` starts at, as it starts playing.
    pub(super) fn start_level(&mut self, k: &SpeakerId, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some(s) = self.speaker_world(k, w) else { return };
        let level = levels::start_level(s.level_unreadable, s.level, self.levels.get(k).copied(), s.max_level, now.wall);
        if let Some(level) = level {
            if s.level_unreadable {
                // The level it plays at now, from now.
                self.levels.insert(k.clone(), KeptLevel { level, at: now.wall });
            }
            out.push(Do::SetLevel { speaker: k.clone(), level });
        }
    }
}

/// The output a Windows target names.
pub(super) fn win_ref(t: &WinTarget) -> OutputRef {
    match t {
        WinTarget::OurDevice => OutputRef::Ours,
        WinTarget::Output(o) => OutputRef::Pc(o.clone()),
    }
}
