//! Where this PC's sound plays and what happened to it, as facts with no
//! words: one value the core computes at each moment, from which every
//! surface makes its own sentences, buttons and pop-ups.
//!
//! The place is always one: This PC (one of its outputs) or one speaker.
//! Music Assistant counts as a speaker here, under
//! [`SpeakerId::music_assistant`]. Names are looked up by id in the lists
//! the engine supplies beside this value, so a renamed speaker reads the
//! same everywhere at once.

use serde::{Deserialize, Serialize};

use super::state::{PcOutputId, Secret, SpeakerId};
use super::text::LocalTime;

/// A moment on the wall clock, in milliseconds since 1970 (UTC). It runs on
/// across sleep, power-off and restarts, so whatever must last that long is
/// measured with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WallTime(pub i64);

impl WallTime {
    /// `ms` later.
    pub fn plus_ms(self, ms: i64) -> WallTime {
        WallTime(self.0.saturating_add(ms))
    }

    /// Milliseconds from `earlier` to this moment (negative if `earlier` is
    /// later, as after the clock was set back).
    pub fn ms_since(self, earlier: WallTime) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// The time at one step of the core, as the engine reads it: a monotonic
/// count of milliseconds for tries and pop-ups, the wall clock for what
/// must run across sleep and restarts, and the time of day for sentences.
/// The core has no clock of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub mono_ms: u64,
    pub wall: WallTime,
    pub local: LocalTime,
}

impl Clock {
    /// The same clock `ms` later, the three readings moved together: the
    /// time of day moves by the whole minutes the wall clock passes (it
    /// wraps at midnight). For scripted runs and tests.
    pub fn later(self, ms: u64) -> Clock {
        let wall = self.wall.plus_ms(ms as i64);
        let minutes_passed = wall.0.div_euclid(60_000) - self.wall.0.div_euclid(60_000);
        let minutes = (self.local.hour as i64 * 60 + self.local.minute as i64 + minutes_passed).rem_euclid(24 * 60);
        Clock {
            mono_ms: self.mono_ms.saturating_add(ms),
            wall,
            local: LocalTime::new((minutes / 60) as u8, (minutes % 60) as u8),
        }
    }
}

/// How this PC's sound reaches a speaker: one part of the first line of the
/// delay block, and one entry of a row's right-click menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WayChoice {
    /// The speaker's own way (Sonos and other UPnP speakers, Sendspin,
    /// Music Assistant).
    Native,
    /// AirPlay with the longer, steady delay.
    AirPlaySteady,
    /// AirPlay with the short delay that can cut out.
    AirPlayShort,
}

/// Line 1: where this PC's sound is heard now, or will be heard when it
/// starts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Spot {
    /// This PC, on this output; `None` while it has no output of its own.
    ThisPc(Option<PcOutputId>),
    /// A speaker (Music Assistant among them).
    Speaker(SpeakerId),
}

/// Line 2: what the sound is doing in line 1's place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Doing {
    /// Sound is playing in line 1's place. It follows the sound at once, a
    /// short sound too.
    Playing,
    NothingPlaying,
    /// The place's speaker connects because sound started (or waits for
    /// this PC's network to do so). Nothing is held meanwhile: what plays
    /// before the speaker is ready isn't heard.
    Connecting,
    /// The tries after the place's speaker stopped responding during play.
    /// What plays meanwhile isn't heard.
    Trying,
    /// This PC is the place and has no output of its own.
    NothingToPlayOn,
}

/// Why the place's speaker can't play, while the app knows it and nothing
/// plays: it stays the place, and sound that starts now plays on this PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Away {
    /// It is on the network and didn't answer.
    NotResponding,
    /// It isn't on this network.
    NotFound,
    /// This PC has no network connection.
    NoNetwork,
}

/// The kind of a network connection of this PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnKind {
    WiFi,
    Wired,
    /// A VPN or another connection that isn't a network of its own here.
    Other,
}

/// A connection this PC made around the time something happened (a VPN
/// that may be why a speaker stopped answering).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetJoined {
    /// Its name, where Windows gives one.
    pub name: Option<String>,
    #[serde(with = "local_time")]
    pub at: LocalTime,
}

/// What the one note is about: why the sound is on this PC (the reason for
/// a move the app made, or for one it left as it found it), why it is about
/// to move there, or what came of a person's switch. The time belongs to
/// the note that carries the cause.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Cause {
    /// The speaker stopped responding while it played, and didn't respond
    /// within the tries.
    StoppedResponding {
        speaker: SpeakerId,
        /// A connection this PC made shortly before.
        vpn: Option<NetJoined>,
    },
    /// This PC's own network went away while the speaker played; the words
    /// blame the PC, not the speaker.
    NetworkLost { speaker: SpeakerId, kind: ConnKind },
    /// Sound started and the speaker couldn't play it (the try at the
    /// start failed, or the app already knew it was away).
    NotRespondingAtStart { speaker: SpeakerId, why: Away },
    /// The speaker asks for its code or password again.
    NeedsSecret { speaker: SpeakerId, secret: Secret },
    /// Someone else plays on the speaker. `who` names another PC with this
    /// app, where the speaker says it.
    PlayingElse { speaker: SpeakerId, who: Option<String> },
    /// A speaker that can't show what it is doing ended this PC's
    /// connection while it could still be reached.
    Disconnected { speaker: SpeakerId },
    /// A Sonos room was taken into another room's group.
    GroupedInto {
        speaker: SpeakerId,
        /// The room it was grouped with, as the speaker names it.
        with: String,
    },
    /// This PC is on another network than the speaker's. The speaker can't
    /// be meant there, so nothing is said until it can play again.
    OtherNetwork { speaker: SpeakerId },
    /// The person removed the speaker while it was the place.
    Removed { speaker: SpeakerId },
    /// Sound already played on this PC when the app started while `speaker`
    /// was the place: the sound stayed where it played.
    PlayingAtStart { speaker: SpeakerId },
    /// Headphones (or another output Windows switched to when it was
    /// connected) went away; `before` was the place before them. `name` is
    /// Windows' name for the output (it is no longer in the list).
    HeadphonesGone { before: SpeakerId, name: String },
    /// Headphones (or another output Windows switched to when it was
    /// connected) took the sound from `before` and are still connected.
    /// `name` is Windows' name for the output.
    HeadphonesConnected { before: SpeakerId, name: String },
    /// The Settings option switched to this PC after 30 minutes without
    /// sound.
    SwitchedAfterQuiet { from: SpeakerId },
    /// Changing the way the speaker plays failed after its previous session ended.
    WayFailed { speaker: SpeakerId, why: PickWhy },
    /// A person's switch to `speaker` didn't get ready: nothing changed,
    /// and the sound is still in line 1's place.
    DidntGetReady { speaker: SpeakerId, why: PickWhy },
}

impl Cause {
    /// The speaker the sound was on before, or was meant for.
    pub fn speaker(&self) -> &SpeakerId {
        use Cause::*;
        match self {
            StoppedResponding { speaker, .. }
            | NetworkLost { speaker, .. }
            | NotRespondingAtStart { speaker, .. }
            | NeedsSecret { speaker, .. }
            | PlayingElse { speaker, .. }
            | Disconnected { speaker }
            | GroupedInto { speaker, .. }
            | OtherNetwork { speaker }
            | Removed { speaker }
            | PlayingAtStart { speaker }
            | WayFailed { speaker, .. }
            | DidntGetReady { speaker, .. } => speaker,
            HeadphonesGone { before, .. } | HeadphonesConnected { before, .. } => before,
            SwitchedAfterQuiet { from } => from,
        }
    }

    /// The speaker couldn't be reached: once the app has a sign that it can
    /// play, the way back is a plain switch.
    pub fn was_unreachable(&self) -> bool {
        matches!(
            self,
            Cause::StoppedResponding { .. }
                | Cause::NetworkLost { .. }
                | Cause::NotRespondingAtStart { .. }
                | Cause::OtherNetwork { .. }
        )
    }
}

/// The one note: what the app did about where the sound is, or is about to
/// do, and the one way on. A note about a move the app made (or about sound
/// it left where it found it) stands until a person switches, by any route,
/// or closes it, and the app's restart keeps it. The others last as long as
/// what they say is so, or until they are closed: a move still [`Ahead`],
/// headphones that are connected, the answer to a switch that didn't get
/// ready.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Note {
    pub cause: Cause,
    /// When the app saw it happen; `None` where it only found it so
    /// (someone was already playing on the speaker when sound started) and
    /// where the note has no moment to name.
    #[serde(default, with = "opt_local_time")]
    pub at: Option<LocalTime>,
    /// The app has a sign that the speaker can play again: the note says
    /// so, and its button is a plain switch.
    pub available_again: bool,
    /// The app hasn't moved the sound yet: the speaker is still the place,
    /// `cause` is why it can't play, and the note says when the sound will
    /// move.
    #[serde(default)]
    pub ahead: Option<Ahead>,
}

/// When the app will move the sound to this PC, in a note about a speaker
/// that can't play and is still the place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Ahead {
    /// The app is trying the speaker: unless it answers, the sound moves in
    /// a few seconds.
    InAFewSeconds,
    /// Nothing plays: sound that starts now plays on this PC.
    WhenSoundStarts,
}

/// What a note's one button does.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NoteButton {
    /// A switch to the speaker, tried once: the app has no sign that it can
    /// play.
    TryAgain(SpeakerId),
    /// A switch to the speaker. Where someone else plays on it, the switch
    /// stops what plays there (the note says so before the click).
    SwitchTo(SpeakerId),
    /// A switch to the speaker that first asks for its code or password.
    EnterSecret(SpeakerId, Secret),
    /// A switch to This PC, while the app hasn't moved the sound yet. Where
    /// the move is seconds away ([`Ahead::InAFewSeconds`]) the button only
    /// does at once what the app is about to do, and its words say *now*.
    SwitchToThisPc,
}

impl Note {
    /// The way on, if there is one: to This PC while the move is still
    /// ahead, else to the speaker. A removed speaker has none, and a room
    /// inside another room's group has none until it leaves the group.
    pub fn button(&self) -> Option<NoteButton> {
        if self.ahead.is_some() {
            return Some(NoteButton::SwitchToThisPc);
        }
        let speaker = self.cause.speaker().clone();
        match &self.cause {
            Cause::Removed { .. } => None,
            Cause::NeedsSecret { secret, .. } | Cause::DidntGetReady { why: PickWhy::WrongSecret(secret) | PickWhy::NeedsSecret(secret), .. } => {
                Some(NoteButton::EnterSecret(speaker, *secret))
            }
            Cause::WayFailed { .. } | Cause::DidntGetReady { .. } => Some(NoteButton::TryAgain(speaker)),
            Cause::GroupedInto { .. } if !self.available_again => None,
            cause if cause.was_unreachable() && !self.available_again => Some(NoteButton::TryAgain(speaker)),
            _ => Some(NoteButton::SwitchTo(speaker)),
        }
    }

    /// The note reports a failure that still stands (its mark warns); else
    /// it only informs.
    pub fn failure(&self) -> bool {
        !self.available_again
            && matches!(
                self.cause,
                Cause::StoppedResponding { .. }
                    | Cause::NetworkLost { .. }
                    | Cause::NotRespondingAtStart { .. }
                    | Cause::NeedsSecret { .. }
                    | Cause::PlayingElse { .. }
                    | Cause::Disconnected { .. }
                    | Cause::GroupedInto { .. }
                    | Cause::WayFailed { .. }
                    | Cause::DidntGetReady { .. }
            )
    }

    /// Whether the note is said at all. On another network the speaker
    /// can't be meant, so its note waits until the speaker can play again.
    pub fn shows(&self) -> bool {
        !matches!(self.cause, Cause::OtherNetwork { .. }) || self.available_again
    }

    /// Whether the note has a ×. The one about a move seconds ahead has
    /// none: closing it could be taken for stopping the move, and it goes
    /// by itself when the speaker answers or the sound has moved.
    pub fn can_close(&self) -> bool {
        self.ahead != Some(Ahead::InAFewSeconds)
    }
}

/// The act that started a switch, for the words of its progress and its
/// failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SwitchBy {
    /// A row, the tray, Windows' list, *Switch to …*, the phone page, the
    /// command line, the API.
    Switch,
    /// *Try … again*.
    TryAgain,
    /// Another way of the place's own speaker (*Connecting by AirPlay…*).
    Way,
    /// First run's *I heard it* or *Use … without checking*.
    FirstRun,
}

/// Where a switch came from, for where its confirmation and its failure
/// show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SwitchFrom {
    /// The panel or a view: the app's own windows.
    Window,
    /// The tray menu, or a button of a pop-up by the clock: confirmations
    /// and failures show by the clock.
    Tray,
    /// Windows' list of outputs: as the tray, and Windows goes back to the
    /// output it was on while the switch waits or when it fails.
    WindowsList,
    /// The phone page, the command line, the API: a switch that needs a
    /// confirmation changes nothing.
    Remote,
}

/// A switch on its way: the sound stays where it is until it takes effect.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Picking {
    pub speaker: SpeakerId,
    pub way: WayChoice,
    pub by: SwitchBy,
    pub from: SwitchFrom,
    /// *Cancel* is offered with the words that say it connects. Without it
    /// the way out is the row of the place, which stops the switch either
    /// way.
    pub can_cancel: bool,
}

/// Why a switch didn't get ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PickWhy {
    NotResponding,
    NotFound,
    /// This PC has no network connection.
    NoNetwork,
    /// The speaker asks for its code or password and none was given (its
    /// dialog was closed, or nobody was there to answer).
    NeedsSecret(Secret),
    /// The code or password was refused.
    WrongSecret(Secret),
    /// The app couldn't start the attempt (this PC is short of resources).
    CouldntStart,
    /// The speaker gave a reason of its own.
    Refused,
}

/// A switch that didn't get ready: nothing changed. It shows until the
/// next switch, or until the panel closes after showing it. For a switch or
/// a try again the note says it ([`Cause::DidntGetReady`]); a change of way
/// and first run say it in their own places.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PickFailed {
    pub speaker: SpeakerId,
    pub why: PickWhy,
    /// The act that started it.
    pub by: SwitchBy,
    pub from: SwitchFrom,
}

/// Something that is so for as long as it lasts, said in line 2 after the
/// state (or in its place).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Standing {
    /// The app's sound output was chosen in Windows while no speaker was
    /// chosen: the sound stays on this PC.
    NoSpeakerChosen,
    /// An app plays on another output of this PC while a speaker is the
    /// place.
    AppElsewhere { app: String, output: PcOutputId },
    /// *Only send some apps to the speaker* is on and a speaker is the
    /// place; `none_playing` while no app is playing there.
    OnlySomeApps { none_playing: bool },
    /// The app's sound output is missing from Windows: the sound stays on
    /// this PC and every speaker's row reads *Repair first*.
    OutputMissing,
    /// Windows didn't take the app's device for a switch to `speaker`; the
    /// sound stays where it was.
    WindowsDidntSwitch { speaker: SpeakerId },
    /// Music Assistant is the place and hasn't started playing this PC's
    /// sound.
    MusicAssistantNotStarted,
    /// A room joined the group that is playing; said while that sound
    /// lasts.
    RoomJoined { room: String },
    /// An app that plays on the speaker uses a microphone (a call).
    CallOn { speaker: SpeakerId },
    /// The speaker was paused or stopped with its own controls while sound
    /// that no player's pause stops goes on: it is sent and not heard.
    PausedAtSpeaker { speaker: SpeakerId },
}

/// The one state name a speaker's row shows, or a spinner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RowState {
    Plain,
    /// A spinner where the state name goes.
    Connecting,
    /// It is on the network and a request to it got no answer (during
    /// play, when sound started, at a switch to it, or when the app asked
    /// it afterwards). Its row carries the ⚠.
    NotResponding,
    /// Grey: not on this network.
    NotFound,
    PlayingElse,
    /// A speaker that can't show what it is doing, from its disconnecting
    /// this PC until a person switches to it.
    MayBeInUse,
    /// The app's sound output is missing.
    RepairFirst,
}

/// A speaker as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SpeakerNow {
    pub can_chime: bool,
    pub id: SpeakerId,
    pub state: RowState,
    /// The way a click on its row uses (its tooltip names it).
    pub way: WayChoice,
    /// It is line 1's place (its row is marked).
    pub is_place: bool,
}

/// What shows by the clock besides a confirmation: it says something and
/// loses nothing when missed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Popup {
    /// The ordinary window closed for the first time.
    WindowClosed,
    /// The note, at the moment the sound that was playing moved to this PC
    /// (or stayed there): the same facts as the panel's note. A note about
    /// a move still ahead never shows by the clock.
    Note(Note),
    /// Sound started on a speaker after a break.
    PlayingOn { speaker: SpeakerId },
    /// A room joined the group that is playing.
    AlsoPlayingIn { room: String },
    /// A switch made outside the app's own window didn't get ready; the
    /// sound is still in line 1's place.
    DidntGetReady { speaker: SpeakerId, why: PickWhy },
    /// The app's device was chosen in Windows' list while it names no
    /// speaker; the sound is still in line 1's place.
    NoSpeakerChosen,
    /// An app that plays on the speaker uses a microphone.
    CallOn { speaker: SpeakerId },
}

impl Popup {
    /// Whether the pop-up carries a button (it then stays longer).
    pub fn has_button(&self) -> bool {
        match self {
            Popup::Note(note) => note.button().is_some(),
            Popup::WindowClosed | Popup::PlayingOn { .. } | Popup::CallOn { .. } => true,
            Popup::AlsoPlayingIn { .. } | Popup::DidntGetReady { .. } | Popup::NoSpeakerChosen => false,
        }
    }

    /// Whether the Settings switch *Show notes by the clock about where the
    /// sound is playing* turns this pop-up off. A failure that moved the
    /// sound, and an answer to something the person just did, always show.
    pub fn governed_by_switch(&self) -> bool {
        match self {
            Popup::Note(note) => !note.failure(),
            Popup::PlayingOn { .. } | Popup::AlsoPlayingIn { .. } | Popup::CallOn { .. } => true,
            Popup::WindowClosed | Popup::DidntGetReady { .. } | Popup::NoSpeakerChosen => false,
        }
    }
}

/// A confirmation: nothing changes until it is answered.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Confirm {
    /// A switch to a speaker that says it plays something else; `who` is
    /// another PC with this app, where the speaker names it. Only where the
    /// app asks before it takes a busy speaker.
    Busy { speaker: SpeakerId, way: WayChoice, who: Option<String> },
    /// A switch to a speaker that may be in use. Only where the app asks
    /// before it takes a busy speaker.
    MayBeInUse { speaker: SpeakerId, way: WayChoice },
    /// A test chime on a speaker that plays something else.
    ChimeBusy { speaker: SpeakerId, who: Option<String> },
    /// A switch while a screen reader runs: its voice would play there too.
    ScreenReader { speaker: SpeakerId, way: WayChoice },
}

impl Confirm {
    pub fn speaker(&self) -> &SpeakerId {
        match self {
            Confirm::Busy { speaker, .. }
            | Confirm::MayBeInUse { speaker, .. }
            | Confirm::ChimeBusy { speaker, .. }
            | Confirm::ScreenReader { speaker, .. } => speaker,
        }
    }
}

/// A pop-up by the clock. `seq` grows with each new pop-up of any kind, so
/// a surface shows one it hasn't shown yet; `since_ms` is on the core's
/// monotonic clock.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Shown<T> {
    pub seq: u64,
    pub what: T,
    pub since_ms: u64,
}

/// What shows by the clock: at most one of each kind. Nothing does while
/// the panel is open.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Popups {
    pub popup: Option<Shown<Popup>>,
    /// A confirmation for a switch from the tray menu or Windows' list.
    pub confirmation: Option<Shown<Confirm>>,
}

/// The step of first run the window shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FirstRunStep {
    /// Click a speaker to hear a chime on it.
    One,
    /// The chime played at `level`. Without a chime the page offers a switch.
    Two { speaker: SpeakerId, level: Option<u8>, limit: u8 },
    /// `speaker` now plays this PC's sound.
    Three { speaker: SpeakerId },
}

/// Everything a surface needs to say where the sound is and what happened,
/// at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Situation {
    /// Line 1.
    pub spot: Spot,
    /// This PC's output now: the one a switch to This PC plays on (This
    /// PC's row shows it); `None` while it has none.
    pub pc_output: Option<PcOutputId>,
    /// The place's speaker can't play and nothing plays: it stays the
    /// place, and sound that starts now plays on this PC.
    pub away: Option<Away>,
    pub doing: Doing,
    /// The amber badge on the icon by the clock: sound isn't heard where
    /// it should be (the tries, or a start of sound that waits long enough
    /// for the note to say so), or the app moved the sound off a speaker
    /// for a failure that still stands. Line 2 has its own amber, for
    /// [`Doing::Trying`] and for a place that is `away`; the note's mark is
    /// [`Note::failure`].
    pub amber: bool,
    /// The one note, while there is one to show: the part of the panel
    /// about the place holds it in place of anything else.
    pub note: Option<Note>,
    /// A switch on its way: its row shows that it connects, and the place
    /// is unchanged until the speaker is ready.
    pub picking: Option<Picking>,
    pub pick_failed: Option<PickFailed>,
    pub standing: Vec<Standing>,
    /// Every speaker the app knows, in the engine's order.
    pub speakers: Vec<SpeakerNow>,
    /// A confirmation in the panel, under its speaker's row (or under the
    /// bar of that speaker's view).
    pub confirm_in_panel: Option<Confirm>,
    pub popups: Popups,
    pub first_run: Option<FirstRunStep>,
}

impl Situation {
    /// The password or code the speaker asked for after a pick or a move to this PC.
    pub(crate) fn secret_failure(&self, id: &SpeakerId) -> Option<PickWhy> {
        if let Some(PickFailed { speaker, why: why @ (PickWhy::WrongSecret(_) | PickWhy::NeedsSecret(_)), .. }) = &self.pick_failed {
            if speaker == id { return Some(*why); }
        }
        match &self.note.as_ref()?.cause {
            Cause::NeedsSecret { speaker, secret } if speaker == id => Some(PickWhy::NeedsSecret(*secret)),
            Cause::DidntGetReady { speaker, why: why @ (PickWhy::WrongSecret(_) | PickWhy::NeedsSecret(_)) }
                if speaker == id => Some(*why),
            _ => None,
        }
    }

    pub fn secret_for(&self, id: &SpeakerId) -> Option<Secret> {
        match self.secret_failure(id)? {
            PickWhy::WrongSecret(secret) | PickWhy::NeedsSecret(secret) => Some(secret),
            _ => None,
        }
    }

    pub fn place_speaker(&self) -> Option<&SpeakerId> {
        match &self.spot {
            Spot::Speaker(id) => Some(id),
            _ => None,
        }
    }

    /// The row of `id`, if the speaker is known.
    pub fn speaker(&self, id: &SpeakerId) -> Option<&SpeakerNow> {
        self.speakers.iter().find(|s| &s.id == id)
    }
}

/// A time of day stored as `[hour, minute]`.
mod local_time {
    use super::LocalTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &LocalTime, s: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&[t.hour, t.minute], s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<LocalTime, D::Error> {
        let [hour, minute] = <[u8; 2]>::deserialize(d)?;
        Ok(LocalTime::new(hour.min(23), minute.min(59)))
    }
}

/// An optional time of day stored as `[hour, minute]` or `null`.
mod opt_local_time {
    use super::LocalTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<LocalTime>, s: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&t.map(|t| [t.hour, t.minute]), s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<LocalTime>, D::Error> {
        let t = <Option<[u8; 2]>>::deserialize(d)?;
        Ok(t.map(|[hour, minute]| LocalTime::new(hour.min(23), minute.min(59))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_moves_its_three_readings_together() {
        // 23:59:00 on the wall clock.
        let c = Clock { mono_ms: 1_000, wall: WallTime(1_799_999_940_000), local: LocalTime::new(23, 59) };
        let d = c.later(61_000);
        assert_eq!(d.mono_ms, 62_000);
        assert_eq!(d.wall, WallTime(1_800_000_001_000));
        assert_eq!(d.local, LocalTime::new(0, 0), "past midnight");
        assert_eq!(c.later(30 * 60_000).local, LocalTime::new(0, 29));
        let mut e = c;
        for _ in 0..240 {
            e = e.later(250);
        }
        assert_eq!(e.local, LocalTime::new(0, 0), "small steps add up to a minute");
    }

    #[test]
    fn a_note_reads_back_as_it_was_stored() {
        let k = SpeakerId::new("kitchen");
        let notes = [
            Note {
                cause: Cause::StoppedResponding {
                    speaker: k.clone(),
                    vpn: Some(NetJoined { name: Some("Corp VPN".into()), at: LocalTime::new(21, 13) }),
                },
                at: Some(LocalTime::new(21, 14)),
                available_again: false,
                ahead: None,
            },
            Note { cause: Cause::PlayingElse { speaker: k.clone(), who: Some("LAPTOP-7Q2K".into()) }, at: None, available_again: true, ahead: None },
            Note {
                cause: Cause::HeadphonesGone { before: k, name: "Headphones".into() },
                at: Some(LocalTime::new(22, 5)),
                available_again: false,
                ahead: None,
            },
        ];
        for note in notes {
            let text = serde_json::to_string(&note).unwrap();
            assert_eq!(serde_json::from_str::<Note>(&text).unwrap(), note, "{text}");
        }
        // A note stored before notes could tell of a move still ahead.
        let old = r#"{"cause":{"Removed":{"speaker":"kitchen"}},"at":[21,14],"available_again":false}"#;
        assert_eq!(serde_json::from_str::<Note>(old).unwrap().ahead, None);
    }

    #[test]
    fn a_notes_button_is_the_way_back() {
        let k = SpeakerId::new("kitchen");
        let note = |cause: Cause, available_again: bool| Note { cause, at: None, available_again, ahead: None };
        let stopped = Cause::StoppedResponding { speaker: k.clone(), vpn: None };
        assert_eq!(note(stopped.clone(), false).button(), Some(NoteButton::TryAgain(k.clone())));
        assert!(note(stopped.clone(), false).failure());
        assert_eq!(note(stopped.clone(), true).button(), Some(NoteButton::SwitchTo(k.clone())));
        assert!(!note(stopped, true).failure(), "available again: it only informs");

        // A speaker that was never failing: a plain switch from the start.
        let gone = Cause::HeadphonesGone { before: k.clone(), name: "Headphones".into() };
        assert_eq!(note(gone.clone(), false).button(), Some(NoteButton::SwitchTo(k.clone())));
        assert!(!note(gone, false).failure());
        let busy = Cause::PlayingElse { speaker: k.clone(), who: None };
        assert_eq!(note(busy.clone(), false).button(), Some(NoteButton::SwitchTo(k.clone())));
        assert!(note(busy, false).failure());

        let code = Cause::NeedsSecret { speaker: k.clone(), secret: Secret::Code };
        assert_eq!(note(code, false).button(), Some(NoteButton::EnterSecret(k.clone(), Secret::Code)));
        assert_eq!(note(Cause::Removed { speaker: k.clone() }, false).button(), None);
        let grouped = Cause::GroupedInto { speaker: k.clone(), with: "Living Room".into() };
        assert_eq!(note(grouped.clone(), false).button(), None, "the room has no row while it is in the group");
        assert_eq!(note(grouped, true).button(), Some(NoteButton::SwitchTo(k.clone())));

        // Another network: nothing is said until the speaker can play again.
        let away = Cause::OtherNetwork { speaker: k.clone() };
        assert!(!note(away.clone(), false).shows());
        assert!(note(away.clone(), true).shows());
        assert_eq!(note(away, true).button(), Some(NoteButton::SwitchTo(k)));
    }

    #[test]
    fn a_note_about_a_move_still_ahead_offers_this_pc() {
        let k = SpeakerId::new("kitchen");
        // Whatever the reason, the way on is not to wait for the app.
        let causes = [
            Cause::StoppedResponding { speaker: k.clone(), vpn: None },
            Cause::NotRespondingAtStart { speaker: k.clone(), why: Away::NotFound },
            Cause::PlayingElse { speaker: k.clone(), who: None },
            Cause::GroupedInto { speaker: k.clone(), with: "Living Room".into() },
        ];
        for cause in causes {
            for ahead in [Ahead::InAFewSeconds, Ahead::WhenSoundStarts] {
                let note = Note { cause: cause.clone(), at: None, available_again: false, ahead: Some(ahead) };
                assert_eq!(note.button(), Some(NoteButton::SwitchToThisPc), "{note:?}");
                assert!(note.failure() && note.shows(), "{note:?}");
            }
        }
    }

    #[test]
    fn the_notes_that_answer_a_switch_and_headphones() {
        let k = SpeakerId::new("kitchen");
        let note = |cause: Cause| Note { cause, at: None, available_again: false, ahead: None };
        // A switch that didn't get ready: the same switch again.
        let failed = note(Cause::DidntGetReady { speaker: k.clone(), why: PickWhy::NotResponding });
        assert_eq!(failed.button(), Some(NoteButton::TryAgain(k.clone())));
        assert!(failed.failure());
        let code = note(Cause::DidntGetReady { speaker: k.clone(), why: PickWhy::WrongSecret(Secret::Password) });
        assert_eq!(code.button(), Some(NoteButton::EnterSecret(k.clone(), Secret::Password)));
        // Headphones took the sound from Kitchen: the way back is a plain
        // switch, and nothing failed.
        let headphones = note(Cause::HeadphonesConnected { before: k.clone(), name: "Headphones".into() });
        assert_eq!(headphones.button(), Some(NoteButton::SwitchTo(k)));
        assert!(!headphones.failure());
        assert!(Popup::Note(headphones).governed_by_switch());
    }

    #[test]
    fn which_popups_the_switch_governs() {
        let k = SpeakerId::new("kitchen");
        assert!(Popup::PlayingOn { speaker: k.clone() }.governed_by_switch());
        assert!(!Popup::DidntGetReady { speaker: k.clone(), why: PickWhy::NotFound }.governed_by_switch());
        assert!(!Popup::DidntGetReady { speaker: k.clone(), why: PickWhy::NotFound }.has_button());
        assert!(!Popup::NoSpeakerChosen.governed_by_switch());
        let failed = Note { cause: Cause::StoppedResponding { speaker: k.clone(), vpn: None }, at: None, available_again: false, ahead: None };
        assert!(!Popup::Note(failed.clone()).governed_by_switch(), "a failure that moved the sound always shows");
        assert!(Popup::Note(failed).has_button());
        let gone = Note { cause: Cause::HeadphonesGone { before: k.clone(), name: "Headphones".into() }, at: None, available_again: false, ahead: None };
        assert!(Popup::Note(gone).governed_by_switch());
        let removed = Note { cause: Cause::Removed { speaker: k }, at: None, available_again: false, ahead: None };
        assert!(!Popup::Note(removed).has_button());
    }
}
