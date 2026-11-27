//! Where this PC's sound plays, decided in one place without threads, I/O
//! or a clock of its own.
//!
//! The sound plays in one place, This PC or one speaker, and stays there
//! until a person switches. When the speaker can't play this PC's sound,
//! the app moves the sound to this PC, where it carries on, and one note
//! says why and offers the way back. The app never moves the sound to a
//! speaker by itself, and it never pauses or starts a player by itself: it
//! only passes on a pause or play pressed on the speaker. Headphones are
//! This PC. Commands and events go in with the time and what the engine
//! observes ([`World`]); what to do comes out ([`Do`]);
//! [`Core::situation`] is what every surface shows.

pub mod io;
pub mod kept;
pub mod levels;
pub mod numbers;
pub mod outputs;
pub mod sound;
pub mod titles;
pub mod trials;

mod build;
mod popups;
mod repair;
use repair::{Recovery, RepairPlayback};
mod switch;
mod trouble;
mod windows;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

pub use io::{
    AppKey, Connection, Do, Ended, Event, Job, NetId, OutputEvent, OutputRef, OutputWorld, PlayerEvent, PlayerId,
    PlayerStatus, PlayerWorld, PowerEvent, Purpose, SettingsWorld, SpeakerSignal, SpeakerWorld, StartKind, WinTarget,
    World,
};
pub use kept::{Kept, KeptHeadphones, KeptLevel, KeptNote};

use crate::model::command::{Command, Effect, Refusal, Surface};
use crate::model::situation::{
    Ahead, Away, Cause, Clock, Confirm, NetJoined, Note, PickFailed, Popup, RoutingQuestion, Shown, SwitchBy, SwitchFrom, WallTime,
    WayChoice,
};
use crate::model::state::{PcOutputId, SpeakerId};
use crate::model::text::LocalTime;

use numbers::*;
use sound::Detector;

/// The place's speaker's connection, as the engine's manners keep it.
#[derive(Clone, Debug, PartialEq)]
enum Stream {
    /// Not connected: it connects when sound starts.
    Off,
    /// A switch let this speaker go. Restore it if the newer switch leaves it the place.
    Released,
    /// Sound without a player began: if it ends within about 3 seconds it
    /// is played on the PC's own output and the speaker isn't connected.
    ShortWait { since_ms: u64 },
    /// Sound without a player began while the app knows that the speaker
    /// can't play: it is heard on the PC's own output at once, and if it
    /// lasts longer than a short sound the app moves the sound there.
    ShortOnPc { since_ms: u64 },
    /// The speaker couldn't play the sound that started and stays the
    /// place: what arrives at the app's device is passed on to the PC's own
    /// output while it plays. Only where a failed start leaves the speaker
    /// the place ([`AfterFailedStart::Speaker`]).
    OnPc,
    /// This PC has no network just after a wake, a start or a change of
    /// network: the try at a sound's start waits for it. `since_ms` is when
    /// the sound started.
    WaitNet { since_ms: u64, until_ms: u64 },
    /// The one try at a sound's start; `asked_ms` is when the speaker was
    /// asked.
    Connecting { gen: u64, asked_ms: u64, until_ms: u64, restoring: bool },
    /// Connected and sending since `since_wall`.
    On { since_wall: WallTime },
    /// The tries during play.
    Trying(Tries),
}

#[derive(Clone, Debug, PartialEq)]
struct Tries {
    since_ms: u64,
    /// Tries started so far.
    started: usize,
    /// The try running now.
    job: Option<u64>,
    /// Why the stream broke, and when: what the note says if no try gets
    /// an answer.
    cause: Cause,
    at: LocalTime,
}

/// A person's switch on its way.
#[derive(Clone, Debug, PartialEq)]
struct Switching {
    gen: u64,
    welcome_level: Option<u8>,
    speaker: SpeakerId,
    way: WayChoice,
    by: SwitchBy,
    who: Asker,
    /// For a switch from Windows' list: the output Windows was on (it plays
    /// what arrives meanwhile, and Windows goes back there if it fails).
    windows_prev: Option<PcOutputId>,
}

/// A confirmation waiting for its answer.
#[derive(Clone, Debug, PartialEq)]
struct Confirming {
    what: Confirm,
    routing: Option<RoutingQuestion>,
    by: SwitchBy,
    who: Asker,
    windows_prev: Option<PcOutputId>,
    /// By the clock (the tray, Windows' list, while the panel is closed);
    /// else in the panel under its speaker's row.
    by_clock: bool,
    seq: u64,
    since_ms: u64,
}

/// Who asked for a switch or a chime, from the surface the command came
/// from: where its confirmation and its failure show, and whether it came
/// from the app's own windows (only there may a code or password be asked
/// for, and only there may a receiver that announces another speaker's
/// identity be chosen).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Asker {
    from: SwitchFrom,
    in_window: bool,
}

impl Asker {
    fn of(surface: Surface) -> Asker {
        Asker { from: switch_from(surface), in_window: in_app_window(surface) }
    }

    /// The app's device chosen in Windows' list.
    const WINDOWS_LIST: Asker = Asker { from: SwitchFrom::WindowsList, in_window: false };
}

/// The note, with when it arose.
#[derive(Clone, Debug, PartialEq)]
struct NoteRec {
    note: Note,
    wall: WallTime,
    mono_ms: u64,
}

/// Where the note that shows comes from, which decides what its × does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoteFrom {
    /// The kept note about a move the app made: the × ends it.
    Kept,
    /// The answer to a switch that didn't get ready: the × ends it, as
    /// closing the panel does.
    FailedSwitch,
    /// What the state implies (a move still ahead, headphones connected):
    /// the × hides it for as long as it would say the same.
    State,
}

/// Headphones (or another output Windows switched to when it came) that
/// took the sound from a speaker and are still connected.
#[derive(Clone, Debug, PartialEq)]
struct Headphones {
    output: PcOutputId,
    /// The speaker that was the place before them.
    before: SpeakerId,
    /// Windows' name for the output.
    name: String,
}

/// What the core knows about one speaker.
#[derive(Clone, Debug, Default, PartialEq)]
struct Rec {
    /// It is away, as a request to it or its own announcement showed.
    away: Option<Away>,
    /// It plays something else; with the other PC's name where it says.
    busy: Option<Option<String>>,
    /// It was taken into another room's group, and when.
    grouped: Option<(String, LocalTime)>,
    /// It disconnected this PC and can't show what it is doing.
    may_be_in_use: Option<WallTime>,
    /// What its last try found.
    last_try: Option<crate::model::situation::PickWhy>,
    /// When it last answered a request or announced itself.
    answered_ms: Option<u64>,
}

/// Asking a speaker that couldn't play whether it can play again.
#[derive(Clone, Debug, PartialEq)]
struct Listen {
    since_ms: u64,
    next_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum FirstRun {
    Off,
    One,
    Two { speaker: SpeakerId, level: Option<u8> },
    Three(SpeakerId),
}

/// A change of Windows' default that may still turn out to be Windows'
/// own doing (an output comes or goes within the window).
#[derive(Clone, Debug, PartialEq)]
struct PendingDefault {
    to: Option<OutputRef>,
    prev: Option<OutputRef>,
    at_ms: u64,
}

/// The core: one place, why the sound is there, and what is on its way.
pub struct Core {
    /// The place: `None` is This PC.
    place: Option<SpeakerId>,
    /// The output chosen under This PC (else the PC's own output).
    pc_choice: Option<PcOutputId>,
    /// Headphones that took the sound from a speaker, until they go or a
    /// person switches.
    headphones: Option<Headphones>,
    /// The speaker the app's device in Windows is named after.
    named: Option<SpeakerId>,
    stream: Stream,
    switch: Option<Switching>,
    confirm: Option<Confirming>,
    /// A switch that didn't get ready, and whether the panel has shown it.
    pick_failed: Option<(PickFailed, bool)>,
    /// Windows didn't take the app's device for a switch to this speaker.
    windows_refused: Option<SpeakerId>,
    window_open: bool,
    no_speaker_chosen: bool,
    /// The kept note: why the sound is on this PC, and the way back.
    note: Option<NoteRec>,
    /// A note the state implies that a person closed: it stays away while
    /// the state implies the same note.
    closed: Option<Note>,
    /// The players that a pause pressed on the speaker stopped and nobody
    /// touched since: play pressed on the speaker starts them.
    passed_pause: Vec<PlayerId>,
    /// The speaker was paused with its own controls while sound that no
    /// player's pause stops went on.
    paused_at_speaker: bool,
    seq: u64,
    /// What shows by the clock, and until when.
    popup: Option<(Shown<Popup>, u64)>,
    /// Sound started after a break: a speaker that plays it says so by the
    /// clock.
    after_break: bool,
    /// A room that joined the playing group, while that sound lasts.
    joined: Option<String>,
    recs: BTreeMap<SpeakerId, Rec>,
    ways: BTreeMap<SpeakerId, WayChoice>,
    levels: BTreeMap<SpeakerId, KeptLevel>,
    /// A level was set from this PC and isn't saved yet: since when.
    level_set_ms: Option<u64>,
    screen_reader_answered: BTreeSet<SpeakerId>,
    listening: BTreeMap<SpeakerId, Listen>,
    last_outputs: Vec<PcOutputId>,
    /// Outputs that came or went, and when.
    moves: Vec<(PcOutputId, bool, u64)>,
    pending_default: Option<PendingDefault>,
    /// Headphones Windows didn't switch to yet, and since when.
    pending_headphones: Option<(PcOutputId, u64)>,
    /// A change of Windows' default the app asked for.
    expect_default: Option<OutputRef>,
    /// Windows' default as last seen.
    last_default: Option<OutputRef>,
    pc_before_ours: Option<(PcOutputId, u64)>,
    pass: Option<PcOutputId>,
    /// Windows' names of the outputs seen, for an output that has gone.
    names: BTreeMap<PcOutputId, String>,
    /// The place before a switch whose Windows change is still to be
    /// confirmed: Windows not taking the app's device puts it back.
    revert: Option<Option<SpeakerId>>,
    /// Real sound played in the place at the last call.
    was_real: bool,
    detector: Detector,
    last_real_wall: Option<WallTime>,
    last_switch_wall: Option<WallTime>,
    connections: Vec<Connection>,
    /// The minute after a wake or a start ends here.
    grace_until_ms: u64,
    net_changed_ms: Option<u64>,
    /// This PC's last connection went away: its kind and when.
    net_lost: Option<(crate::model::situation::ConnKind, u64)>,
    vpn: Option<(NetJoined, u64)>,
    /// The minute in which this speaker (the place's, or the one the note
    /// is about) must be found on this PC's new networks ends here.
    other_net: Option<(SpeakerId, u64)>,
    start_look: Option<u64>,
    first_run: FirstRun,
    first_run_done: bool,
    window_close_explained: bool,
    /// A chime on its way: its job, its speaker, and whether the app's own
    /// windows asked for it.
    chime: Option<(Job, SwitchBy, SwitchFrom)>,
    gen: u64,
    panel_open: bool,
    device_missing: bool,
    recovery: Recovery,
    repair_playback: RepairPlayback,
    /// Windows can restore its default during Repair and just after it ends.
    repair_defaults: Option<std::ops::RangeInclusive<u64>>,
    /// When the core was made.
    started_wall: WallTime,
    /// The Settings option, as last seen.
    switch_back_on: bool,
    only_some_apps: Option<bool>,
    pc_return: Option<windows::PcReturn>,
    /// The fixed choices, from [`numbers`].
    after_headphones: AfterHeadphones,
    after_failed_start: AfterFailedStart,
    ask_before_taking: bool,
    cancel_while_connecting: bool,
    now: Clock,
}

impl Core {
    /// The core as it was kept, at `now`. The note stands across a restart;
    /// what was on its way ends with the app.
    pub fn new(kept: Kept, now: Clock) -> Core {
        let mut recs: BTreeMap<SpeakerId, Rec> = BTreeMap::new();
        for (id, at) in &kept.disconnected {
            recs.entry(id.clone()).or_default().may_be_in_use = Some(*at);
        }
        let note = kept.note.map(|k| NoteRec {
            note: k.note,
            wall: k.at,
            mono_ms: now.mono_ms.saturating_sub(now.wall.ms_since(k.at).max(0) as u64),
        });
        Core {
            place: kept.place,
            pc_choice: kept.pc_output,
            headphones: kept.headphones.map(|h| Headphones { output: h.output, before: h.before, name: h.name }),
            named: kept.named,
            stream: Stream::Off,
            switch: None,
            confirm: None,
            pick_failed: None,
            windows_refused: None,
            window_open: false,
            no_speaker_chosen: false,
            note,
            closed: kept.closed_note,
            passed_pause: Vec::new(),
            paused_at_speaker: false,
            seq: 0,
            popup: None,
            after_break: false,
            joined: None,
            recs,
            ways: kept.ways,
            levels: kept.levels,
            level_set_ms: None,
            screen_reader_answered: kept.screen_reader_answered,
            listening: BTreeMap::new(),
            last_outputs: kept.last_outputs,
            moves: Vec::new(),
            pending_default: None,
            pending_headphones: None,
            expect_default: None,
            last_default: None,
            pc_before_ours: None,
            pass: None,
            names: BTreeMap::new(),
            revert: None,
            was_real: false,
            detector: Detector::new(),
            last_real_wall: kept.last_sound,
            last_switch_wall: kept.last_switch,
            connections: Vec::new(),
            grace_until_ms: now.mono_ms + AFTER_WAKE_MS,
            net_changed_ms: None,
            net_lost: None,
            vpn: None,
            other_net: None,
            start_look: None,
            first_run: if kept.first_run_done { FirstRun::Off } else { FirstRun::One },
            first_run_done: kept.first_run_done,
            window_close_explained: kept.window_close_explained,
            chime: None,
            gen: 0,
            panel_open: false,
            device_missing: false,
            recovery: Recovery::Idle,
            repair_playback: RepairPlayback::Idle,
            repair_defaults: None,
            started_wall: now.wall,
            switch_back_on: false,
            only_some_apps: None,
            pc_return: None,
            after_headphones: AFTER_HEADPHONES,
            after_failed_start: PLACE_AFTER_FAILED_START,
            ask_before_taking: ASK_BEFORE_TAKING_A_BUSY_SPEAKER,
            cancel_while_connecting: CANCEL_WHILE_CONNECTING,
            now,
        }
    }

    /// Sets two of the fixed choices, to check that each can be turned back
    /// by itself.
    #[cfg(test)]
    pub(super) fn choose(&mut self, headphones: AfterHeadphones, failed_start: AfterFailedStart) {
        self.after_headphones = headphones;
        self.after_failed_start = failed_start;
    }

    /// Sets whether a switch asks before it takes a busy speaker, to check
    /// the other value of that fixed choice.
    #[cfg(test)]
    pub(super) fn ask_before_taking(&mut self, ask: bool) {
        self.ask_before_taking = ask;
    }

    /// Sets whether a switch that connects can be cancelled by itself, to
    /// check the other value of that fixed choice.
    #[cfg(test)]
    pub(super) fn cancel_while_connecting(&mut self, can: bool) {
        self.cancel_while_connecting = can;
    }

    /// A command from a surface.
    pub fn command(&mut self, from: Surface, cmd: &Command, w: &World, now: Clock) -> (Result<Effect, Refusal>, Vec<Do>) {
        // A command that belongs to another surface than the one it came
        // from changes nothing.
        if let Some(refusal) = self.refused_from(from, cmd) {
            return (Err(refusal), Vec::new());
        }
        let mut out = Vec::new();
        let panel = match cmd {
            Command::PanelShown(open) => *open,
            _ => w.panel_open,
        };
        self.begin(w, panel, now, &mut out);
        let result = self.handle(from, cmd, w, now, &mut out);
        self.finish(w, now, &mut out);
        (result, out)
    }

    /// Something happened.
    pub fn event(&mut self, e: Event, w: &World, now: Clock) -> Vec<Do> {
        let mut out = Vec::new();
        self.begin(w, w.panel_open, now, &mut out);
        match e {
            Event::Connected { gen, ended } => self.connected(gen, ended, w, now, &mut out),
            Event::RepairApproved(run) => {
                let until = now.mono_ms + u64::from(crate::platform::REPAIR_TIMEOUT_MS);
                if let RepairPlayback::Helper { run: id, until: deadline } = &mut self.repair_playback {
                    if *id == run { *deadline = until; }
                }
                if matches!(self.recovery, Recovery::Asking { run: id, .. } if id == run) {
                    self.stay_on_pc_for_repair(now, &mut out);
                    self.recovery = Recovery::Running { run, until };
                }
            }
            Event::RepairExited(run, exit) => {
                if matches!(self.repair_playback, RepairPlayback::Helper { run: id, .. } if id == run) {
                    self.repair_playback = RepairPlayback::WhenFinished;
                }
                self.recovery.exited(run, exit, now.mono_ms);
            }
            Event::TurnOnFailed => self.recovery = Recovery::TurnOnFailed,
            Event::PlaceReleased(id) => {
                if self.connected_speaker() == Some(&id) {
                    self.stream_off(&mut out);
                    self.stream = Stream::Released;
                }
            }
            Event::WayLost { speaker, ended } => self.way_lost(speaker, ended, w, now, &mut out),
            Event::Speaker { id, signal } => self.speaker_said(id, signal, w, now, &mut out),
            Event::Output(o) => self.output_event(o, w, now, &mut out),
            Event::Player(p) => self.player_event(p, w, now, &mut out),
            Event::Sound(levels) => self.sound(levels, w, now, &mut out),
            Event::Network => self.network(w, now, &mut out),
            Event::Power(p) => self.power(p, w, now, &mut out),
            Event::Started(kind) => self.started(kind, w, now, &mut out),
        }
        self.finish(w, now, &mut out);
        out
    }

    /// Time passes.
    pub fn tick(&mut self, w: &World, now: Clock) -> Vec<Do> {
        let mut out = Vec::new();
        self.begin(w, w.panel_open, now, &mut out);
        self.finish(w, now, &mut out);
        out
    }

    // Drop the old speaker choice without asking Windows to change any role.
    fn stay_on_pc_for_repair(&mut self, now: Clock, out: &mut Vec<Do>) {
        self.repair_defaults = Some(now.mono_ms..=now.mono_ms + WINDOWS_DOING_MS);
        self.start_look = None;
        self.confirm = None;
        if let Some(s) = self.switch.take() { out.push(Do::Supersede(s.gen)); }
        if self.pending_default.as_ref().is_some_and(|p| p.to == Some(OutputRef::Ours)) {
            self.pending_default = None;
        }
        if self.expect_default == Some(OutputRef::Ours) { self.expect_default = None; }
        let named = self.named.take();
        if self.place.take().is_some() || named.is_some() {
            self.place_changed(now);
            out.push(Do::Save);
        }
    }

    pub(super) fn repair_note(&self, w: &World) -> crate::model::scene::RepairNow {
        self.recovery.note(w)
    }

    /// What is kept across a restart.
    pub fn kept(&self) -> Kept {
        Kept {
            place: self.place.clone(),
            pc_output: self.pc_choice.clone(),
            headphones: self
                .headphones
                .as_ref()
                .map(|h| KeptHeadphones { output: h.output.clone(), before: h.before.clone(), name: h.name.clone() }),
            named: self.named.clone(),
            ways: self.ways.clone(),
            note: self.note.as_ref().map(|n| KeptNote { note: n.note.clone(), at: n.wall }),
            closed_note: self.closed.clone(),
            disconnected: self.recs.iter().filter_map(|(id, r)| r.may_be_in_use.map(|at| (id.clone(), at))).collect(),
            last_sound: self.last_real_wall,
            last_switch: self.last_switch_wall,
            levels: self.levels.clone(),
            first_run_done: self.first_run_done,
            window_close_explained: self.window_close_explained,
            last_outputs: self.last_outputs.clone(),
            screen_reader_answered: self.screen_reader_answered.clone(),
        }
    }

    /// The speaker the app's device in Windows is named after.
    pub fn named(&self) -> Option<&SpeakerId> {
        self.named.as_ref()
    }

    /// The next moment, on the monotonic clock, at which [`Core::tick`] has
    /// something to do (it may be called more often).
    pub fn next_due(&self) -> Option<u64> {
        let mut due: Vec<u64> = Vec::new();
        match &self.stream {
            Stream::ShortWait { since_ms } | Stream::ShortOnPc { since_ms } => due.push(since_ms + SHORT_SOUND_MS),
            Stream::WaitNet { since_ms: from_ms, until_ms } | Stream::Connecting { asked_ms: from_ms, until_ms, .. } => {
                due.push(*until_ms);
                // The moment the note may start saying that it takes long.
                due.push(from_ms + SLOW_CONNECT_NOTE_MS);
            }
            Stream::Trying(t) => {
                due.push(t.since_ms + TRIES_AT_MS.get(t.started).copied().unwrap_or(TRIES_DURING_PLAY_MS));
                due.push(t.since_ms + TRIES_DURING_PLAY_MS);
            }
            Stream::On { since_wall } => {
                let quiet_from = self.last_real_wall.map_or(*since_wall, |r| r.max(*since_wall));
                due.push(self.mono_at(quiet_from.plus_ms(STREAM_REST_AFTER_WALL_MS)));
            }
            Stream::Off | Stream::OnPc | Stream::Released => {}
        }
        if self.place.is_some() && self.switch_back_on {
            let quiet_from = self.quiet_from();
            due.push(self.mono_at(quiet_from.plus_ms(SWITCH_BACK_AFTER_WALL_MS)));
        }
        if let Some((_, until)) = &self.popup {
            due.push(*until);
        }
        if let Some(c) = &self.confirm {
            if c.by_clock {
                due.push(c.since_ms + CONFIRMATION_POPUP_MS);
            }
        }
        due.extend(self.recovery.due());
        due.extend(self.repair_playback.due());
        due.extend(self.listening.values().map(|l| l.next_ms));
        due.extend(self.pending_default.as_ref().map(|p| p.at_ms + WINDOWS_DOING_MS));
        due.extend(self.pending_headphones.as_ref().map(|(_, at)| at + WINDOWS_DOING_MS));
        due.extend(self.pc_return.as_ref().map(windows::PcReturn::due));
        due.extend(self.other_net.as_ref().map(|(_, until)| *until));
        due.extend(self.start_look.map(|t| t + START_LOOK_MS));
        due.extend(self.level_set_ms.map(|t| t + LEVEL_SAVED_AFTER_MS));
        if self.now.mono_ms < self.grace_until_ms {
            due.push(self.grace_until_ms);
        }
        // What is due now was done by the call that just ran; what is left
        // waits for a condition, not for time.
        due.into_iter().filter(|t| *t > self.now.mono_ms).min()
    }

    // ----- the steps every call makes

    /// Before handling a call: the time, the panel, the app's device.
    fn begin(&mut self, w: &World, panel_open: bool, now: Clock, out: &mut Vec<Do>) {
        self.now = now;
        if matches!(self.repair_playback, RepairPlayback::Helper { until, .. } if now.mono_ms >= until) {
            self.repair_playback = RepairPlayback::WhenFinished;
        }
        self.recovery.observe_approval(w, now.mono_ms);
        if self.recovery.changing_output() {
            if let Some(window) = &mut self.repair_defaults {
                *window = *window.start()..=now.mono_ms + WINDOWS_DOING_MS;
            }
        }
        self.switch_back_on = w.settings.switch_back_after_quiet;
        self.follow_routing_mode(w, out);
        if panel_open != self.panel_open {
            self.panel_open = panel_open;
            if panel_open {
                self.panel_opened();
            } else {
                self.panel_closed();
            }
        }
        let missing = !w.our_device;
        if missing != self.device_missing {
            self.device_missing = missing;
            if !missing {
                if self.recovery == Recovery::Idle || self.recovery == Recovery::NotApproved {
                    self.start_place(w, now, out);
                }
            } else if let Some(k) = self.place.clone() {
                if self.stream_connected() {
                    out.push(Do::StopSending { speaker: k, take_off: false });
                }
                self.stream_off(out);
            }
        }
    }

    /// After handling a call: what time decides, then what follows from
    /// the state.
    fn finish(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        self.recovery.observe(w, now.mono_ms);
        let changes = self.detector.advance(now.mono_ms);
        self.sound_changes(changes, w, now, out);
        self.timers(w, now, out);
        self.follow_sound(w, now);
        self.check_available(w, now, out);
        // A closed note stays away only while the state implies the same.
        if self.closed.is_some() && self.implied_note(w, now) != self.closed {
            self.closed = None;
            out.push(Do::Save);
        }
        self.sync_pass_through(w, out);
        self.expire_popups(now);
        // One save at most, at the end.
        let saves = out.iter().filter(|d| matches!(d, Do::Save)).count();
        if saves > 1 {
            let mut seen = false;
            out.retain(|d| {
                if matches!(d, Do::Save) {
                    if seen {
                        return false;
                    }
                    seen = true;
                }
                true
            });
        }
    }

    /// The commands only the app's own windows send, refused from anywhere
    /// else: a confirmation is answered only where it shows (in the panel
    /// or a view, or by the clock), first run and the × of the note or of
    /// a pop-up exist only in the app's own surfaces, removing or resetting
    /// a speaker and its test chime only in its view, and the switch that
    /// skips every question only as the answer a window got to one.
    fn refused_from(&self, from: Surface, cmd: &Command) -> Option<Refusal> {
        let app_window = in_app_window(from);
        let allowed = match cmd {
            Command::Confirm { .. } => match &self.confirm {
                Some(c) if c.by_clock => from == Surface::Notification,
                Some(_) => app_window,
                None => true,
            },
            Command::CloseNote | Command::ClosePopup => app_window || from == Surface::Notification,
            Command::FirstRunAction(_)
            | Command::RepairOutput
            | Command::TurnOnOutput
            | Command::RemoveSpeaker(_)
            | Command::ResetSpeaker(_)
            | Command::WindowOpened
            | Command::WindowClosed
            | Command::PanelShown(_)
            | Command::PlayChime { .. }
            | Command::PlayTestSound(_)
            | Command::PlayTakingOver(_) => app_window,
            _ => true,
        };
        (!allowed).then_some(Refusal::WindowOnly)
    }

    fn handle(&mut self, from: Surface, cmd: &Command, w: &World, now: Clock, out: &mut Vec<Do>) -> Result<Effect, Refusal> {
        let who = Asker::of(from);
        match cmd {
            Command::PlayWhenListed { speaker, .. } => self.switch_request(speaker.clone(), None, SwitchBy::Switch, who, false, w, now, out),
            Command::Switch { place, way } => match place.speaker() {
                Some(k) => self.switch_request(k, *way, SwitchBy::Switch, who, false, w, now, out),
                None => {
                    self.switch_to_pc(None, w, now, out);
                    Ok(Effect::Done)
                }
            },
            Command::RepairOutput => {
                if w.can_repair && !w.our_device && !w.output_turned_off && !self.recovery.repairing() {
                    let run = self.next_seq();
                    let until = now.mono_ms + 5 * 60_000;
                    self.recovery = Recovery::Asking { run, until };
                    self.repair_playback = RepairPlayback::Helper { run, until };
                    out.push(Do::RepairOutput(run));
                }
                Ok(Effect::Done)
            }
            Command::TurnOnOutput => {
                if !w.our_device && w.output_turned_off && !self.recovery.repairing()
                    && !matches!(self.recovery, Recovery::TurningOn { .. }) {
                    self.stay_on_pc_for_repair(now, out);
                    self.recovery = Recovery::turn_on(now.mono_ms);
                    self.repair_playback = RepairPlayback::WhenFinished;
                    out.push(Do::TurnOnOutput);
                }
                Ok(Effect::Done)
            }
            Command::StayOnThisPc => {
                self.switch_to_pc(None, w, now, out);
                Ok(Effect::Done)
            }
            Command::PlayTakingOver(k) => self.switch_request(k.clone(), None, SwitchBy::Switch, who, true, w, now, out),
            Command::SwitchOutput(o) => {
                self.switch_to_pc(Some(o.clone()), w, now, out);
                Ok(Effect::Done)
            }
            Command::TryAgain(k) => self.switch_request(k.clone(), None, SwitchBy::TryAgain, who, false, w, now, out),
            Command::CancelSwitch => {
                // The attempt is dropped; nothing else changes.
                if self.cancel_while_connecting {
                    self.cancel_switch(w, out);
                }
                Ok(Effect::Done)
            }
            Command::Confirm { yes } => self.answer(*yes, w, now, out),
            Command::CloseNote if self.no_speaker_chosen => {
                self.no_speaker_chosen = false;
                Ok(Effect::Done)
            }
            Command::CloseNote => {
                match self.note_now(w, now) {
                    // No × there: the note goes by itself within seconds.
                    Some((note, _)) if !note.can_close() => {}
                    Some((_, NoteFrom::Kept)) => {
                        self.clear_note();
                        out.push(Do::Save);
                    }
                    Some((note, NoteFrom::FailedSwitch)) => {
                        self.pick_failed = None;
                        // A note that stood about the same speaker would
                        // only say again what the person just closed.
                        if self.note.as_ref().is_some_and(|n| n.note.cause.speaker() == note.cause.speaker()) {
                            self.clear_note();
                            out.push(Do::Save);
                        }
                    }
                    Some((note, NoteFrom::State)) => {
                        self.closed = Some(note);
                        out.push(Do::Save);
                    }
                    None => {}
                }
                Ok(Effect::Done)
            }
            Command::WindowOpened => {
                self.window_open = true;
                self.keep_no_speaker_popup();
                Ok(Effect::Done)
            }
            Command::WindowClosed => {
                self.window_open = false;
                self.no_speaker_chosen = false;
                // Counted as told once it has shown: while another pop-up or
                // a question shows, the next close tells it.
                if !self.window_close_explained && self.popup.is_none() && self.confirm.is_none() {
                    self.raise_popup(Popup::WindowClosed, w, now);
                    if matches!(&self.popup, Some((Shown { what: Popup::WindowClosed, .. }, _))) {
                        self.window_close_explained = true;
                        out.push(Do::Save);
                    }
                }
                Ok(Effect::Done)
            }
            Command::ClosePopup => {
                self.popup = None;
                Ok(Effect::Done)
            }
            Command::ChooseWay { speaker, way } => {
                // For the place's speaker a switch of way, which records the
                // way when it takes effect; for another, its next switch's.
                if self.place.as_ref() == Some(speaker) {
                    return self.switch_request(speaker.clone(), Some(*way), SwitchBy::Way, who, false, w, now, out);
                }
                self.ways.insert(speaker.clone(), *way);
                out.push(Do::Save);
                Ok(Effect::Done)
            }
            Command::PlayChime { speaker, confirmed } => self.chime_request(speaker.clone(), *confirmed, SwitchBy::Switch, who, w, now, out),
            Command::PlayTestSound(k) => self.chime_request(k.clone(), false, SwitchBy::Switch, who, w, now, out),
            Command::FirstRunAction(act) => self.first_run_action(act.clone(), who, w, now, out),
            Command::RemoveSpeaker(k) => {
                // A note about it, and the way back to it after headphones,
                // go with it.
                if self.note.as_ref().is_some_and(|n| n.note.cause.speaker() == k) {
                    self.clear_note();
                }
                if self.headphones.as_ref().is_some_and(|h| &h.before == k) {
                    self.headphones = None;
                }
                if self.pick_failed.as_ref().is_some_and(|(p, _)| &p.speaker == k) {
                    self.pick_failed = None;
                }
                if self.place.as_ref() == Some(k) {
                    // The person is in the app's window: nothing shows by
                    // the clock.
                    self.switched_to_pc(k.clone(), Cause::Removed { speaker: k.clone() }, Some(now.local), false, w, now, out);
                }
                if self.switch.as_ref().is_some_and(|s| &s.speaker == k) {
                    self.cancel_switch(w, out);
                }
                self.recs.remove(k);
                self.ways.remove(k);
                self.levels.remove(k);
                self.screen_reader_answered.remove(k);
                self.listening.remove(k);
                if self.named.as_ref() == Some(k) {
                    self.named = None;
                }
                out.push(Do::Save);
                Ok(Effect::Done)
            }
            Command::ResetSpeaker(k) => {
                if self.place.as_ref() != Some(k) {
                    self.ways.remove(k);
                    self.levels.remove(k);
                    self.screen_reader_answered.remove(k);
                    out.push(Do::Save);
                }
                Ok(Effect::Done)
            }
            Command::SetVolume { owner: crate::model::command::VolumeOwner::Speaker(k), level } => {
                self.levels.insert(k.clone(), KeptLevel { level: *level, at: now.wall });
                self.level_set_ms = Some(now.mono_ms);
                Ok(Effect::Done)
            }
            Command::Quit => {
                self.quit(w, now, out);
                Ok(Effect::Done)
            }
            Command::ReleaseWindows { for_update, .. } | Command::Release { for_update } => {
                self.release(*for_update, w, out);
                Ok(Effect::Done)
            }
            // The panel's state is read in `begin`; the delay, the volume
            // of This PC, hiding and opening things are the engine's.
            _ => Ok(Effect::Done),
        }
    }

    /// The timers: whatever is due at `now`.
    fn timers(&mut self, w: &World, now: Clock, out: &mut Vec<Do>) {
        let t = now.mono_ms;
        // Windows' changes first: they are decided by what was so when they
        // came.
        self.windows_timers(w, now, out);
        // The minute after a wake or a start.
        if self.start_look.is_some_and(|since| t >= since + START_LOOK_MS) {
            self.start_look = None;
            self.start_look_ends(w, now, out);
        }
        if self.level_set_ms.is_some_and(|since| t >= since + LEVEL_SAVED_AFTER_MS) {
            self.level_set_ms = None;
            out.push(Do::Save);
        }
        match self.stream.clone() {
            Stream::ShortWait { since_ms } | Stream::ShortOnPc { since_ms } if t >= since_ms + SHORT_SOUND_MS => {
                // Sound that is still heard lasts: no short sound. In the
                // quiet after it, its end decides: a sound that ends there
                // was a short one, and one that goes on lasts.
                if self.detector.sounding_on(&OutputRef::Ours, t) {
                    self.sound_starts(false, w, now, out);
                } else if !self.detector.audible_on(&OutputRef::Ours) {
                    self.stream = Stream::Off;
                }
            }
            Stream::Released if self.switch.is_none() => self.connect_place(w, now, out),
            Stream::OnPc if !self.sent_to_speaker(now) => self.stream = Stream::Off,
            Stream::WaitNet { until_ms, .. } if t >= until_ms => self.connect_place(w, now, out),
            Stream::Connecting { gen, until_ms, .. } if t >= until_ms => {
                out.push(Do::Supersede(gen));
                self.place_connect_failed(Ended::NotResponding, w, now, out);
            }
            Stream::Trying(_) => self.tries_due(w, now, out),
            Stream::On { since_wall } => {
                let quiet_from = self.last_real_wall.map_or(since_wall, |r| r.max(since_wall));
                if !self.real_at_place(now) && now.wall.ms_since(quiet_from) >= STREAM_REST_AFTER_WALL_MS {
                    if let Some(k) = self.place.clone() {
                        out.push(Do::StopSending { speaker: k, take_off: true });
                    }
                    self.stream = Stream::Off;
                }
            }
            _ => {}
        }
        // The Settings option.
        if w.settings.switch_back_after_quiet && self.switch.is_none() && !self.device_missing {
            if let Some(k) = self.place.clone() {
                let quiet = !self.real_at_place(now) && !matches!(self.stream, Stream::Trying(_) | Stream::Connecting { .. });
                if quiet && now.wall.ms_since(self.quiet_from()) >= SWITCH_BACK_AFTER_WALL_MS {
                    self.switched_to_pc(k.clone(), Cause::SwitchedAfterQuiet { from: k }, Some(now.local), false, w, now, out);
                }
            }
        }
        // A confirmation by the clock waits 30 minutes at most.
        if self.confirm.as_ref().is_some_and(|c| c.by_clock && t >= c.since_ms + CONFIRMATION_POPUP_MS) {
            self.confirm = None;
        }
        self.listen_due(now, out);
        self.other_network_due(w, now, out);
    }

    // ----- small helpers shared by the parts

    fn next_gen(&mut self) -> u64 {
        self.gen += 1;
        self.gen
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The wall-clock moment `wall` on the monotonic clock, as of the last
    /// call.
    fn mono_at(&self, wall: WallTime) -> u64 {
        let ahead = wall.ms_since(self.now.wall);
        if ahead <= 0 {
            self.now.mono_ms
        } else {
            self.now.mono_ms.saturating_add(ahead as u64)
        }
    }

    /// Nothing has played and nobody switched since this moment (at the
    /// earliest the app's start, when neither is known).
    fn quiet_from(&self) -> WallTime {
        match (self.last_real_wall, self.last_switch_wall) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => self.started_wall,
        }
    }

    /// The minute after a wake or a start, in which no speaker is called
    /// away on its own.
    fn in_grace(&self, now: Clock) -> bool {
        now.mono_ms < self.grace_until_ms
    }

    /// The speaker whose stream the engine should follow at the session.
    pub(crate) fn connected_speaker(&self) -> Option<&SpeakerId> {
        self.place.as_ref().filter(|_| self.stream_connected())
    }

    pub(crate) fn place_speaker(&self) -> Option<&SpeakerId> {
        self.place.as_ref()
    }

    /// A change of Windows' output waits to be told apart: a person's
    /// choice, or Windows following an output that came or went.
    pub(crate) fn deciding_output(&self) -> bool {
        self.pending_default.is_some() || self.pending_headphones.is_some()
    }

    /// The place's speaker has a connection the app made (or is making).
    fn stream_connected(&self) -> bool {
        matches!(self.stream, Stream::On { .. } | Stream::Trying(_) | Stream::Connecting { .. })
    }

    /// The place's stream ends: a connection on its way is no longer
    /// wanted.
    fn stream_off(&mut self, out: &mut Vec<Do>) {
        match &self.stream {
            Stream::Connecting { gen, .. } => out.push(Do::Supersede(*gen)),
            Stream::Trying(t) => {
                if let Some(gen) = t.job {
                    out.push(Do::Supersede(gen));
                }
            }
            _ => {}
        }
        self.stream = Stream::Off;
    }

    /// The bound stream carries sound from an app in this sample.
    pub(super) fn sounding_at_speaker(&self, now: Clock) -> bool {
        matches!(self.stream, Stream::On { .. }) && self.detector.sounding_on(&OutputRef::Ours, now.mono_ms)
    }

    /// Real sound plays where the place is: on the app's device while a
    /// speaker is the place, else on this PC's outputs.
    fn real_at_place(&self, now: Clock) -> bool {
        if self.place.is_some() && !self.device_missing {
            self.detector.real_on(&OutputRef::Ours, now.mono_ms)
        } else {
            self.detector.real_where(now.mono_ms, |_| true)
        }
    }

    /// Sound is arriving at the app's device, for the place's speaker: real
    /// sound, or sound that has only just begun.
    fn sent_to_speaker(&self, now: Clock) -> bool {
        self.detector.real_on(&OutputRef::Ours, now.mono_ms) || self.detector.audible_on(&OutputRef::Ours)
    }

    fn rec(&mut self, id: &SpeakerId) -> &mut Rec {
        self.recs.entry(id.clone()).or_default()
    }

    /// The way a switch to `id` uses: `asked` if given, else the way last
    /// chosen while it is found, else the one with the shortest delay.
    fn way_for(&self, id: &SpeakerId, asked: Option<WayChoice>, w: &World) -> WayChoice {
        if let Some(way) = asked {
            return way;
        }
        let found: Vec<WayChoice> = w.speakers.iter().find(|s| &s.id == id).map(|s| s.ways.clone()).unwrap_or_default();
        match self.ways.get(id) {
            Some(way) if found.is_empty() || found.contains(way) => *way,
            _ => found.first().copied().or_else(|| self.ways.get(id).copied()).unwrap_or(WayChoice::Native),
        }
    }

    /// The way the place's speaker is played by.
    fn place_way(&self, w: &World) -> WayChoice {
        match &self.place {
            Some(k) => self.way_for(k, None, w),
            None => WayChoice::Native,
        }
    }

    fn speaker_world<'w>(&self, id: &SpeakerId, w: &'w World) -> Option<&'w SpeakerWorld> {
        w.speakers.iter().find(|s| &s.id == id)
    }

    /// This PC's output now: the one chosen under This PC while it is
    /// connected, else the PC's own output.
    fn pc_target(&self, w: &World) -> Option<PcOutputId> {
        match &self.pc_choice {
            Some(o) if w.outputs.iter().any(|x| &x.id == o) => Some(o.clone()),
            _ => outputs::pc_own_output(&self.last_outputs, &w.outputs),
        }
    }

    /// Move Windows' default for a place switch in whole-PC mode.
    fn win(&mut self, target: WinTarget, w: &World, out: &mut Vec<Do>) {
        if w.settings.only_some_apps || w.default_output.is_none() {
            return;
        }
        let want = match &target {
            WinTarget::OurDevice => OutputRef::Ours,
            WinTarget::Output(o) => OutputRef::Pc(o.clone()),
        };
        if w.playback_on(&want) && self.expect_default.is_none() {
            return;
        }
        self.expect_default = Some(want);
        out.push(Do::Windows(target));
    }

    /// The Windows target for This PC now, if it has an output.
    fn win_pc(&self, w: &World) -> Option<WinTarget> {
        if w.settings.only_some_apps || w.default_output.is_none() {
            return None;
        }
        self.pc_target(w).map(WinTarget::Output)
    }

    /// A switch took effect, or the place changed: what was about the old
    /// place goes, the note and the way back after headphones with it.
    fn place_changed(&mut self, now: Clock) {
        self.recovery.switched();
        self.note = None;
        self.closed = None;
        self.headphones = None;
        self.no_speaker_chosen = false;
        self.pick_failed = None;
        self.windows_refused = None;
        self.popup = None;
        if self.confirm.as_ref().is_some_and(|c| c.by_clock) {
            self.confirm = None;
        }
        self.passed_pause.clear();
        self.paused_at_speaker = false;
        self.after_break = false;
        self.joined = None;
        self.last_switch_wall = Some(now.wall);
    }
}

/// Where a command's surface puts its switch's confirmation and failure:
/// the panel and the views in the panel, the tray and the pop-ups by the
/// clock; the delay shortcuts, the phone page, the command line and the
/// API ask nothing.
fn switch_from(from: Surface) -> SwitchFrom {
    match from {
        Surface::Panel | Surface::Settings => SwitchFrom::Window,
        Surface::Tray | Surface::Notification => SwitchFrom::Tray,
        Surface::Shortcut | Surface::Phone | Surface::CommandLine | Surface::Api => SwitchFrom::Remote,
    }
}

/// The app's own windows: the panel and its views.
fn in_app_window(from: Surface) -> bool {
    matches!(from, Surface::Panel | Surface::Settings)
}
