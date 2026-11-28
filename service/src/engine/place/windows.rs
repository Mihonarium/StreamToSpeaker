//! Windows' outputs, headphones, and the app's own life: start, sleep,
//! sign-out, quit, an update, the network.

use super::*;
use crate::model::facts::OutputKind;
use crate::model::situation::ConnKind;

/// Outputs that came or went are remembered this long, for the window in
/// which a change of Windows' default is Windows' own doing.
const MOVES_KEPT_MS: u64 = 10_000;

/// Attempts to give the PC its default back, measured from the first request.
const PC_RETURN_AT_MS: [u64; 4] = [0, 2_000, 5_000, 10_000];

pub(super) struct PcReturn {
    started: u64,
    attempt: usize,
}

impl PcReturn {
    fn new(now: Clock) -> Self { Self { started: now.mono_ms, attempt: 0 } }

    pub(super) fn due(&self) -> u64 { self.started + PC_RETURN_AT_MS[self.attempt] }
}

impl Core {
    pub(super) fn follow_routing_mode(&mut self, w: &World, out: &mut Vec<Do>) {
        let some = w.settings.only_some_apps;
        match self.only_some_apps.replace(some) {
            Some(before) if before == some => return,
            None if !some => return,
            _ if some => {
                self.pc_return = w.uses_app_output().then(|| PcReturn::new(self.now));
                self.pending_headphones = None;
                self.pending_default = None;
                self.moves.clear();
                self.start_look = None;
                self.windows_refused = None;
                self.expect_default = None;
                self.revert = None;
                self.no_speaker_chosen = false;
                if self.confirm.as_ref().is_some_and(|c| matches!(c.what, Confirm::ScreenReader { .. })) {
                    self.confirm = None;
                }
                let old_note = self.note.as_ref().is_some_and(|n| matches!(n.note.cause,
                    Cause::PlayingAtStart { .. } | Cause::HeadphonesGone { .. } | Cause::HeadphonesConnected { .. }));
                if self.headphones.take().is_some() || old_note {
                    self.clear_note();
                    out.push(Do::Save);
                }
            }
            _ => {
                self.pc_before_ours = None;
                self.pc_return = None;
                if matches!(self.repair_playback, RepairPlayback::AgainAt(_)) {
                    self.repair_playback = RepairPlayback::Idle;
                }
                self.expect_default = None;
                if self.ask_for_connecting_switch(w, out) { return; }
                if let Some(speaker) = self.place.clone().filter(|_| w.our_device) {
                    self.take_playback_for_place(speaker, w, out);
                } else if let Some(target) = self.win_pc(w) {
                    self.win(target, w, out);
                }
            }
        }
    }

    pub(super) fn output_event(&mut self, e: OutputEvent, w: &World, now: Clock, out: &mut Vec<Do>) {
        for o in &w.outputs {
            self.names.insert(o.id.clone(), o.name.clone());
        }
        if w.settings.only_some_apps {
            // A change of PC output affects the local apps and the pass-through.
            // The apps sent to our device keep their speaker.
            if let Some(OutputRef::Pc(id)) = &w.default_output {
                outputs::used(&mut self.last_outputs, id, OUTPUTS_REMEMBERED);
            }
            if let OutputEvent::NotTaken { .. } = e {
                self.windows_not_taken(w, now, out);
                if let Some(retry) = &mut self.pc_return {
                    retry.attempt += 1;
                    if retry.attempt == PC_RETURN_AT_MS.len() { self.pc_return = None; }
                }
            }
            if w.default_output == Some(OutputRef::Ours) {
                if let Some(OutputRef::Pc(id)) = &self.last_default {
                    self.pc_before_ours = Some((id.clone(), now.mono_ms));
                }
                if let Some((id, at)) = &self.pc_before_ours {
                    if now.mono_ms.saturating_sub(*at) > WINDOWS_DOING_MS {
                        self.pc_before_ours = None;
                    } else if !w.outputs.iter().any(|o| o.id == *id) {
                        self.pc_return = Some(PcReturn::new(now));
                        self.pc_before_ours = None;
                    }
                }
            } else if w.playback_on_pc() {
                self.pc_before_ours = None;
                self.expect_default = None;
                self.pc_return = None;
            }
            if w.default_output.is_some() { self.last_default = w.default_output.clone(); }
            if let OutputEvent::Removed { id, .. } = e {
                if self.pc_choice.as_ref() == Some(&id) { self.pc_choice = None; }
                self.detector.forget_output(&OutputRef::Pc(id));
            }
            return;
        }
        match e {
            OutputEvent::Added { id, kind, name, at_ms } => {
                self.names.insert(id.clone(), name);
                self.note_move(id.clone(), true, at_ms);
                if kind == OutputKind::Headphones && trials::T1_APP_SWITCHES_TO_HEADPHONES {
                    self.pending_headphones = Some((id.clone(), at_ms));
                }
                // Windows may have switched to it just before saying it
                // came.
                if w.default_output == Some(OutputRef::Pc(id.clone())) {
                    self.pending_default = None;
                    self.last_default = w.default_output.clone();
                    self.pending_headphones = None;
                    self.headphones_connected(id, w, now, out);
                }
            }
            OutputEvent::Removed { id, at_ms } => {
                self.note_move(id.clone(), false, at_ms);
                if self.pending_headphones.as_ref().is_some_and(|(o, _)| *o == id) {
                    self.pending_headphones = None;
                }
                // A change of the default that came just before this is
                // Windows' doing.
                let pending = match &self.pending_default {
                    Some(p) if self.moved_near(p.at_ms) => self.pending_default.take(),
                    _ => None,
                };
                if let Some(p) = pending {
                    self.windows_doing(p.to, w, now, out);
                }
                if self.pc_choice.as_ref() == Some(&id) {
                    self.pc_choice = None;
                }
                if self.headphones.as_ref().is_some_and(|h| h.output == id) {
                    self.headphones_gone(true, w, now, out);
                }
                self.detector.forget_output(&OutputRef::Pc(id));
            }
            OutputEvent::DefaultChanged { at_ms } => self.default_changed(w.default_output.clone(), at_ms, w, now, out),
            OutputEvent::NotTaken { .. } => self.windows_not_taken(w, now, out),
            OutputEvent::DeviceChanged { .. } => {}
        }
    }

    fn note_move(&mut self, id: PcOutputId, added: bool, at_ms: u64) {
        self.moves.push((id, added, at_ms));
        let now = self.now.mono_ms.max(at_ms);
        self.moves.retain(|(_, _, t)| now.saturating_sub(*t) <= MOVES_KEPT_MS);
    }

    /// An output came or went within the window of `at_ms`.
    fn moved_near(&self, at_ms: u64) -> bool {
        self.moves.iter().any(|(_, _, t)| {
            if trials::T1_OUTPUT_CHANGE_EITHER_SIDE {
                t.abs_diff(at_ms) <= WINDOWS_DOING_MS
            } else {
                at_ms >= *t && at_ms - t <= WINDOWS_DOING_MS
            }
        })
    }

    fn in_repair_window(&self, at_ms: u64) -> bool {
        self.repair_defaults.as_ref().is_some_and(|window| window.contains(&at_ms))
    }

    fn default_changed(&mut self, to: Option<OutputRef>, at_ms: u64, w: &World, now: Clock, out: &mut Vec<Do>) {
        let prev = std::mem::replace(&mut self.last_default, to.clone());
        if prev == to && self.expect_default.is_none() {
            // Nothing changed.
            return;
        }
        if let Some(OutputRef::Pc(o)) = &to {
            outputs::used(&mut self.last_outputs, o, OUTPUTS_REMEMBERED);
        }
        if self.expect_default.is_some() && self.expect_default == to {
            if !to.as_ref().is_some_and(|target| w.playback_on(target)) { return; }
            // The change the app asked for.
            self.expect_default = None;
            if to == Some(OutputRef::Ours) {
                self.revert = None;
                self.windows_refused = None;
            }
            return;
        }
        if to == Some(OutputRef::Ours) && self.in_repair_window(at_ms) {
            self.pending_default = None;
            return self.windows_doing(to, w, now, out);
        }
        if self.moved_near(at_ms) {
            return self.windows_doing(to, w, now, out);
        }
        // On this PC the new default is already where the sound plays.
        if self.place.is_none() && prev != to && matches!(to, Some(OutputRef::Pc(_))) {
            out.push(Do::PickedOutput);
        }
        // An output may still come or go within the window: decide then.
        self.pending_default = Some(PendingDefault { to, prev, at_ms });
    }

    fn check_playback_after_repair(&mut self, w: &World, now: Clock) {
        match self.repair_playback {
            RepairPlayback::WhenFinished if self.recovery.due().is_none() => {
                self.repair_playback = if w.settings.only_some_apps {
                    RepairPlayback::AgainAt(now.mono_ms + WINDOWS_DOING_MS)
                } else {
                    RepairPlayback::Idle
                };
            }
            RepairPlayback::AgainAt(at) if now.mono_ms >= at => self.repair_playback = RepairPlayback::Idle,
            _ => return,
        }
        if w.settings.only_some_apps {
            self.pc_return = w.uses_app_output().then(|| PcReturn::new(now));
            self.expect_default = None;
        }
    }

    /// The timers of Windows' changes.
    pub(super) fn windows_timers(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        self.check_playback_after_repair(w, now);
        if self.pc_return.as_ref().is_some_and(|retry| now.mono_ms >= retry.due())
            && self.expect_default.is_none() && w.uses_app_output() {
            if let Some(pc) = self.pc_target(w) {
                self.expect_default = Some(OutputRef::Pc(pc.clone()));
                out.push(Do::Windows(WinTarget::Output(pc)));
            } else {
                self.pc_return = None;
            }
        }
        if self.pending_default.as_ref().is_some_and(|p| now.mono_ms >= p.at_ms + WINDOWS_DOING_MS) {
            let p = self.pending_default.take().expect("checked");
            if self.moved_near(p.at_ms) {
                self.windows_doing(p.to, w, now, out);
            } else {
                self.person_chose(p.to, p.prev, w, now, out);
            }
        }
        if self.pending_headphones.as_ref().is_some_and(|(_, at)| now.mono_ms >= at + WINDOWS_DOING_MS) {
            let (id, _) = self.pending_headphones.take().expect("checked");
            let connected = w.outputs.iter().any(|o| o.id == id);
            if connected && w.default_output != Some(OutputRef::Pc(id.clone())) {
                // Windows kept to the output it was on: the app switches to
                // the headphones itself.
                self.headphones_connected(id.clone(), w, now, out);
                self.win(WinTarget::Output(id), w, out);
            }
        }
    }

    /// A change of Windows' output that Windows made because an output
    /// came or went.
    fn windows_doing(&mut self, to: Option<OutputRef>, w: &World, now: Clock, out: &mut Vec<Do>) {
        match to {
            Some(OutputRef::Pc(o)) => {
                let just_came = self.moves.iter().any(|(id, added, _)| *added && *id == o);
                if just_came {
                    if self.pending_headphones.as_ref().is_some_and(|(id, _)| *id == o) {
                        self.pending_headphones = None;
                    }
                    self.headphones_connected(o, w, now, out);
                } else if self.place.is_some() && !self.device_missing {
                    // An output went and Windows moved off the app's device:
                    // the sound stays on the speaker.
                    self.expect_default = None;
                    self.win(WinTarget::OurDevice, w, out);
                } else if self.place.is_none() {
                    out.push(Do::PickedOutput);
                }
            }
            Some(OutputRef::Ours) => {
                if self.place.is_none() {
                    // Windows landed on the app's device only because
                    // another output went away: to the PC's own output.
                    if let Some(t) = self.pc_target(w) {
                        self.expect_default = None;
                        self.win(WinTarget::Output(t), w, out);
                    }
                }
            }
            None => {}
        }
    }

    /// A switch got ready while a change of Windows' output to one of this
    /// PC's own waits to be told from Windows' own changes, and no output
    /// came or went so far: the change is the person's newer choice, and
    /// This PC is the place. Returns whether it was so.
    pub(super) fn this_pc_picked_meanwhile(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) -> bool {
        let picked = self
            .pending_default
            .as_ref()
            .is_some_and(|p| matches!(p.to, Some(OutputRef::Pc(_))) && !self.moved_near(p.at_ms));
        if picked {
            let p = self.pending_default.take().expect("checked");
            self.person_chose(p.to, p.prev, w, now, out);
        }
        picked
    }

    /// A person chose an output in Windows' list.
    fn person_chose(&mut self, to: Option<OutputRef>, prev: Option<OutputRef>, w: &World, now: Clock, out: &mut Vec<Do>) {
        let prev_pc = match &prev {
            Some(OutputRef::Pc(o)) => Some(o.clone()),
            _ => self.pc_target(w),
        };
        match to {
            Some(OutputRef::Ours) => {
                if self.place.is_some() {
                    return;
                }
                match self.named.clone() {
                    Some(k) => {
                        out.push(Do::PickedOutput);
                        let _ = self.switch_request_from_list(
                            k,
                            None,
                            SwitchBy::Switch,
                            Asker::WINDOWS_LIST,
                            false,
                            prev_pc,
                            w,
                            now,
                            out,
                        );
                    }
                    None => {
                        if self.panel_open || self.window_open {
                            self.no_speaker_chosen = true;
                        } else {
                            self.raise_popup(Popup::NoSpeakerChosen, w, now);
                        }
                        if let Some(p) = prev_pc {
                            self.expect_default = None;
                            self.win(WinTarget::Output(p), w, out);
                        }
                    }
                }
            }
            Some(OutputRef::Pc(o)) => {
                // A switch to This PC, on that output.
                self.cancel_switch(w, out);
                self.confirm = None;
                if let Some(k) = self.place.clone() {
                    out.push(Do::PickedOutput);
                    if self.stream_connected() {
                        out.push(Do::Silence { speaker: k, then: None });
                    }
                    self.stream_off(out);
                }
                let changed = self.place.is_some() || prev != Some(OutputRef::Pc(o));
                self.place = None;
                self.pc_choice = None;
                if changed {
                    self.place_changed(now);
                    out.push(Do::Save);
                }
            }
            None => {}
        }
    }

    /// Windows didn't take the app's device: the sound stays where it was.
    fn windows_not_taken(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let _ = (w, now);
        if self.expect_default != Some(OutputRef::Ours) {
            self.expect_default = None;
            return;
        }
        self.expect_default = None;
        if let (Some(k), Some(before)) = (self.place.clone(), self.revert.take()) {
            if self.stream_connected() {
                out.push(Do::StopSending { speaker: k.clone(), take_off: false });
            }
            self.stream_off(out);
            self.place = before;
            self.windows_refused = Some(k);
            out.push(Do::Save);
        }
    }

    /// Headphones (or an output Windows switched to when it came) were
    /// connected: This PC is the place, on them, as a click on This PC's
    /// row would make it. The app remembers the speaker that was the place,
    /// for the way back when they go.
    pub(super) fn headphones_connected(&mut self, o: PcOutputId, w: &World, now: Clock, out: &mut Vec<Do>) {
        out.push(Do::PickedOutput);
        // Being connected cancels a switch connecting or waiting.
        self.cancel_switch(w, out);
        self.confirm = None;
        outputs::used(&mut self.last_outputs, &o, OUTPUTS_REMEMBERED);
        self.pc_choice = None;
        let Some(k) = self.place.take() else {
            // This PC was the place already: only its output changed.
            out.push(Do::Save);
            return;
        };
        if !self.device_missing {
            if self.stream_connected() {
                out.push(Do::Silence { speaker: k.clone(), then: None });
            }
            self.stream_off(out);
        }
        self.place_changed(now);
        let name = self.names.get(&o).cloned().unwrap_or_default();
        self.headphones = Some(Headphones { output: o, before: k, name });
        out.push(Do::Save);
    }

    /// The headphones that took the sound from a speaker went, and nobody
    /// switched since. The one fixed choice says where the sound goes.
    /// `seen` unless they went while the app wasn't running (no time is
    /// known then).
    fn headphones_gone(&mut self, seen: bool, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some(h) = self.headphones.take() else { return };
        if self.place.is_some() {
            return;
        }
        let playing = self.detector.real_where(now.mono_ms, |_| true) || self.detector.audible_where(|_| true);
        match self.after_headphones {
            AfterHeadphones::StayOnThisPc => {
                // The sound stays on this PC, on its other output; the
                // note offers the way back.
                if w.uses_app_output() && self.expect_default.is_none() {
                    if let Some(t) = self.pc_target(w) {
                        self.win(WinTarget::Output(t), w, out);
                    }
                }
                let cause = Cause::HeadphonesGone { before: h.before, name: h.name };
                let note = Note { cause, at: seen.then_some(now.local), available_again: false, ahead: None };
                self.clear_note();
                if playing {
                    self.raise_popup(Popup::Note(note.clone()), w, now);
                }
                self.note = Some(NoteRec { note, wall: now.wall, mono_ms: now.mono_ms });
            }
            AfterHeadphones::BackToSpeaker => {
                // The speaker is the place again; what plays connects it
                // as any start of sound does.
                self.place = Some(h.before);
                self.place_changed(now);
                if !self.device_missing {
                    self.expect_default = None;
                    self.win(WinTarget::OurDevice, w, out);
                    if playing {
                        self.sound_starts(false, w, now, out);
                    }
                }
            }
        }
        out.push(Do::Save);
    }

    /// The app started.
    pub(super) fn started(&mut self, kind: StartKind, w: &World, now: Clock, out: &mut Vec<Do>) {
        self.grace_until_ms = now.mono_ms + AFTER_WAKE_MS;
        self.connections = w.connections.clone();
        self.last_default = w.default_output.clone();
        for o in &w.outputs {
            self.names.insert(o.id.clone(), o.name.clone());
        }
        if let Some(OutputRef::Pc(o)) = &w.default_output {
            outputs::used(&mut self.last_outputs, o, OUTPUTS_REMEMBERED);
        }
        if kind == StartKind::AfterGivingUp && !w.settings.only_some_apps && self.place.is_some() {
            // The watcher gave Windows the PC's own output: the start
            // begins on this PC.
            self.place = None;
            self.clear_note();
            out.push(Do::Save);
        }
        if let Some(h) = self.headphones.clone() {
            if !w.outputs.iter().any(|x| x.id == h.output) {
                // They went while the app wasn't running.
                self.headphones_gone(false, w, now, out);
            } else if w.default_output != Some(OutputRef::Pc(h.output)) {
                // Someone chose another output meanwhile.
                self.headphones = None;
            }
        }
        self.start_place(w, now, out);
        self.watch_other_network(w, now);
    }

    /// At a start (or when the app's device is back): a speaker that is the
    /// place gets Windows' output, unless sound already plays on this PC;
    /// nothing connects until sound starts.
    pub(super) fn start_place(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        if self.device_missing {
            return;
        }
        match &self.place {
            None => {
                if w.uses_app_output() {
                    if let Some(t) = self.pc_target(w) {
                        self.win(WinTarget::Output(t), w, out);
                    }
                }
            }
            Some(_) => {
                if w.default_output.is_some() && !w.playback_on(&OutputRef::Ours) && !w.settings.only_some_apps {
                    // Look first at what plays on this PC.
                    self.start_look = Some(now.mono_ms);
                }
            }
        }
    }

    /// The look at a start is over. Sound that already plays on this PC
    /// stays where it plays, This PC is the place, and the note offers the
    /// way to the speaker; else Windows goes to the app's device.
    pub(super) fn start_look_ends(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some(k) = self.place.clone() else { return };
        let since = now.mono_ms.saturating_sub(START_LOOK_MS);
        let playing = !self.detector.heard_since(since, |o| matches!(o, OutputRef::Pc(_))).is_empty();
        if !playing {
            self.win(WinTarget::OurDevice, w, out);
            return;
        }
        self.place = None;
        self.pc_choice = None;
        self.place_changed(now);
        let note = Note { cause: Cause::PlayingAtStart { speaker: k }, at: Some(now.local), available_again: false, ahead: None };
        self.raise_popup(Popup::Note(note.clone()), w, now);
        self.note = Some(NoteRec { note, wall: now.wall, mono_ms: now.mono_ms });
        out.push(Do::Save);
    }

    pub(super) fn power(&mut self, p: PowerEvent, w: &World, now: Clock, out: &mut Vec<Do>) {
        match p {
            PowerEvent::Suspending => {
                // Windows stays on the app's device across a sleep.
                if let Some(k) = self.place.clone() {
                    if self.stream_connected() {
                        out.push(Do::StopSending { speaker: k, take_off: true });
                    }
                }
                self.stream_off(out);
                out.push(Do::Save);
            }
            PowerEvent::Resumed => {
                self.grace_until_ms = now.mono_ms + AFTER_WAKE_MS;
                self.stream_off(out);
            }
            PowerEvent::SigningOut | PowerEvent::ShuttingDown => {
                // The switch stays; Windows gets the PC's own output.
                self.give_windows_back(w, out);
                out.push(Do::Save);
            }
        }
    }

    /// Windows to the PC's own output, the place's speaker silent first.
    fn give_windows_back(&mut self, w: &World, out: &mut Vec<Do>) {
        let target = self.win_pc(w);
        match self.place.clone() {
            Some(k) if !self.device_missing => {
                if self.stream_connected() {
                    self.expect_default = target.as_ref().map(switch::win_ref);
                    out.push(Do::Silence { speaker: k, then: target });
                } else if let Some(t) = target {
                    self.win(t, w, out);
                }
            }
            _ => {}
        }
        self.stream_off(out);
    }

    /// Quitting: Windows gets the PC's own output, where what plays goes
    /// on, and This PC is the place. Nothing is paused and nothing is said.
    /// A note that already stands (This PC being the place) stays for the
    /// next start.
    pub(super) fn quit(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        self.give_windows_back(w, out);
        if self.place.take().is_some() {
            self.place_changed(now);
        }
        out.push(Do::Save);
    }

    /// Setup's `release`: before an update Windows stays on the app's
    /// device and the place stays (if the update replaces the sound driver,
    /// Windows moves by itself while the device is away, and what plays is
    /// heard on this PC meanwhile); at uninstall Windows gets the PC's own
    /// output.
    pub(super) fn release(&mut self, for_update: bool, w: &World, out: &mut Vec<Do>) {
        if !for_update {
            self.give_windows_back(w, out);
        }
        out.push(Do::Save);
    }

    /// This PC's network connections changed.
    pub(super) fn network(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let before = std::mem::replace(&mut self.connections, w.connections.clone());
        self.net_changed_ms = Some(now.mono_ms);
        if w.connections.is_empty() {
            if let Some(c) = before.first() {
                self.net_lost = Some((c.kind, now.mono_ms));
            }
        }
        for c in &w.connections {
            if c.kind == ConnKind::Other && !before.iter().any(|b| b.net == c.net) {
                self.vpn = Some((NetJoined { name: c.name.clone(), at: now.local }, now.mono_ms));
            }
        }
        if !w.connections.is_empty() && matches!(self.stream, Stream::WaitNet { .. }) {
            // The connection is there: the speaker is asked now.
            self.connect_place(w, now, out);
        }
        self.watch_other_network(w, now);
    }

    /// The speaker the app means: the place's, or the one a note says it
    /// couldn't reach.
    fn meant_speaker(&self) -> Option<SpeakerId> {
        self.place.clone().or_else(|| {
            self.note.as_ref().filter(|n| n.note.cause.was_unreachable()).map(|n| n.note.cause.speaker().clone())
        })
    }

    /// Another network: none of this PC's connections is the one where the
    /// speaker the app means was last found, and it isn't found on them
    /// within a minute. That minute starts when the connections change (or
    /// the app starts).
    fn watch_other_network(&mut self, w: &World, now: Clock) {
        self.other_net = None;
        let Some(k) = self.meant_speaker() else { return };
        let s = self.speaker_world(&k, w);
        let last = s.and_then(|s| s.network.clone());
        let found = s.is_some_and(|s| s.found);
        let on_it = last.as_ref().is_some_and(|n| w.connections.iter().any(|c| &c.net == n));
        let real_networks = w.connections.iter().any(|c| c.kind != ConnKind::Other || !trials::T11_VPN_IS_NOT_ANOTHER_NETWORK);
        if last.is_some() && !on_it && !found && real_networks {
            self.other_net = Some((k, now.mono_ms + OTHER_NETWORK_AFTER_MS));
        }
    }

    /// The minute is over and the speaker wasn't found: this PC is on
    /// another network, where the speaker can't be meant. While nothing
    /// plays the app moves to this PC and says nothing. Sound that plays
    /// breaks the stream first; once the app has moved it for that, the
    /// note takes this truer cause, and so says nothing either until the
    /// speaker can play again.
    pub(super) fn other_network_due(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let Some((k, until)) = self.other_net.clone() else { return };
        let found = self.speaker_world(&k, w).is_some_and(|s| s.found);
        if found || self.meant_speaker().as_ref() != Some(&k) {
            self.other_net = None;
            return;
        }
        if now.mono_ms < until {
            return;
        }
        if self.place.as_ref() == Some(&k) {
            let busy = self.sent_to_speaker(now) || matches!(self.stream, Stream::Trying(_) | Stream::Connecting { .. });
            if !busy {
                self.other_net = None;
                self.switched_to_pc(k.clone(), Cause::OtherNetwork { speaker: k }, None, false, w, now, out);
            }
            return;
        }
        self.other_net = None;
        if let Some(n) = &mut self.note {
            n.note = Note { cause: Cause::OtherNetwork { speaker: k.clone() }, at: None, available_again: false, ahead: None };
            if matches!(&self.popup, Some((Shown { what: Popup::Note(_), .. }, _))) {
                self.popup = None;
            }
            out.push(Do::Save);
        }
    }

    /// What arrives at the app's device plays on the PC's own output while
    /// This PC is the place (and while sound is heard there because the
    /// place's speaker can't play it), and on the output Windows was on
    /// while a switch made in Windows' list connects.
    pub(super) fn sync_pass_through(&mut self, w: &World, out: &mut Vec<Do>) {
        let wanted = if !trials::T9_PASS_THROUGH || self.device_missing {
            None
        } else if let Some(prev) = self.switch.as_ref().filter(|s| s.who.from == SwitchFrom::WindowsList).and_then(|s| s.windows_prev.clone()) {
            Some(prev)
        } else if self.place.is_none() || matches!(self.stream, Stream::ShortOnPc { .. } | Stream::OnPc) {
            self.pc_target(w)
        } else {
            None
        };
        if wanted != self.pass {
            self.pass = wanted.clone();
            out.push(Do::PassThrough(wanted));
        }
    }
}
