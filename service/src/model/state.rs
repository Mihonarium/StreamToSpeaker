//! What is true about the sound right now: one value the engine computes,
//! and every surface renders with the same words.
//!
//! Each variant carries exactly the facts its sentences need. Names are
//! looked up by id in the speaker list the engine supplies alongside, so a
//! renamed speaker reads the same everywhere at once.

use std::sync::Arc;

use super::text::LocalTime;

/// A speaker's stable id, as the engine keys it (a UPnP device id, an
/// AirPlay identifier, a Sendspin client id).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpeakerId(pub Arc<str>);

impl SpeakerId {
    pub fn new(id: &str) -> Self {
        SpeakerId(Arc::from(id))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Windows sound output's endpoint id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PcOutputId(pub Arc<str>);

impl PcOutputId {
    pub fn new(id: &str) -> Self {
        PcOutputId(Arc::from(id))
    }
}

/// A place the PC's sound can play: the person's choice.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Place {
    ThisPc,
    Speaker(SpeakerId),
    MusicAssistant,
}

/// Where the sound is while a connection is being made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SoundAt {
    ThisPc,
    Speaker(SpeakerId),
    MusicAssistant,
}

/// How far a connection has got, as the speaker's row says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectStage {
    /// Locating the speaker on the network.
    Finding,
    /// Asking it to play this PC's stream.
    Asking,
    /// It agreed; waiting for it to start (a UPnP speaker's first fetch).
    Waiting,
    /// A code or password dialog is open.
    Pairing,
}

/// The apps whose sound is playing, by the names Windows gives them. Empty
/// when the app isn't known.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Apps(pub Vec<String>);

impl Apps {
    pub fn of(names: &[&str]) -> Self {
        Apps(names.iter().map(|s| s.to_string()).collect())
    }
}

/// The retries after a speaker stopped responding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropout {
    /// The quick tries (rule: three, at about 2, 5 and 10 s).
    QuickTries { attempt: u8, of: u8, then: AfterTries },
    /// "Keep trying, and stay silent": the quick tries are over and the app
    /// tries every `every_secs` seconds.
    KeepTrying { every_secs: u16 },
}

/// What happens when the quick tries fail (General → *When a speaker stops
/// responding*).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterTries {
    BackToPc,
    StaySilent,
}

/// Whether the app would go back to the chosen place by itself when it is
/// back: only while nothing has played on this PC since (otherwise it asks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Return {
    ByItself,
    WillAsk,
}

/// Why the sound came back to this PC after the tries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FellBackCause {
    StoppedResponding,
    /// The speaker refused this PC's pairing (a fault it reported, not a
    /// silence); the remedy is to pair again.
    RefusedPairing,
}

/// Why the sound stayed on this PC when the chosen speaker was available
/// again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StayedBecause {
    /// Something played here after the speaker stopped responding.
    PlayedAfterDropout,
    /// At sign-in, wake or app start, something was playing here when the
    /// speaker was found.
    PlayingWhenFound,
    /// The speaker's *Comes back by itself* is *Never*.
    SetNotToReturn,
}

/// What a speaker asks for before it plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Secret {
    /// A one-time code shown on a TV or said by a speaker.
    Code,
    /// An AirPlay password.
    Password,
}

/// Looking for the chosen speaker at sign-in, wake or app start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Looking {
    /// The first 10 seconds.
    Searching,
    /// After 10 seconds without it.
    NotFound(Return),
}

/// The state of the PC's sound: one value, rendered on every surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NowState {
    /// Sound is on this PC, resting.
    OnThisPc,
    /// Connecting to `target`; the sound stays where it is meanwhile.
    Connecting { target: SpeakerId, from: SoundAt, stage: ConnectStage },
    /// This PC sends real sound and the speaker takes it, as far as the
    /// speaker can show.
    Playing { speaker: SpeakerId, apps: Apps },
    /// An AirPlay speaker or HomePods reconnecting to apply a delay change
    /// while playing. Not a dropout. `from_ms` and `to_ms` are what was
    /// removed (negative) or added before and after the change.
    ApplyingDelay { speaker: SpeakerId, from_ms: i32, to_ms: i32 },
    /// Connected, nothing playing on this PC.
    Ready { speaker: SpeakerId },
    /// Connected, but an app plays on another output (the PC's own).
    ReadyPlayingElsewhere { speaker: SpeakerId, app: String },
    /// Connected and sending, but the speaker stopped taking the sound
    /// (only speakers that can show it).
    NotTaking { speaker: SpeakerId, since: LocalTime },
    /// The speaker stopped responding; retrying.
    NotResponding { speaker: SpeakerId, dropout: Dropout },
    /// The tries failed and the sound came back to this PC.
    FellBack { speaker: SpeakerId, at: LocalTime, cause: FellBackCause, returns: Return },
    /// The chosen speaker is available again, but the sound stayed here.
    SpeakerBack { speaker: SpeakerId, because: StayedBecause },
    /// Someone else started something on the speaker; the sound came back.
    Taken { speaker: SpeakerId, at: LocalTime },
    /// An automatic return needs a code or password.
    NeedsSecret { speaker: SpeakerId, secret: Secret },
    /// At sign-in, wake or app start, the chosen speaker isn't found yet.
    LookingFor { speaker: SpeakerId, looking: Looking },
    /// Windows' output was switched to another output (by the person, or
    /// by Windows when headphones arrived); the app let go of `speaker`.
    /// The output is This PC's output in the facts.
    WindowsSwitched { speaker: SpeakerId, at: LocalTime },
    /// "Stream To Speaker" was picked in Windows before any speaker was
    /// ever chosen.
    ChooseSpeaker,
    /// Windows refused to switch its output to this app.
    WindowsRefused { speaker: SpeakerId },
    /// Music Assistant chosen and playing.
    MaPlaying { apps: Apps, room: Option<String> },
    /// Music Assistant chosen; it hasn't started playing this PC's sound.
    MaWaiting,
    /// Music Assistant chosen; its server is away.
    MaAway { returns: Return },
    /// Music Assistant is back, but something played here meanwhile.
    MaBack,
    /// The app's sound output (driver) is missing from Windows.
    OutputMissing,
    /// "Only apps I send there": apps are playing on the speaker.
    PlayingSentApps { speaker: SpeakerId, apps: Apps },
    /// "Only apps I send there": connected, no app sent here.
    ReadyNoAppSent { speaker: SpeakerId },
}

/// The tray icon's arcs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arcs {
    /// Sound on this PC.
    Ink,
    /// Connecting, or waiting for Music Assistant.
    GreenFaint,
    /// On a speaker, playing or ready.
    Green,
}

/// A problem badge, told apart by shape as well as colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    None,
    /// Amber "!": needs a look.
    Attention,
    /// Red "×": not working.
    Failure,
}

impl NowState {
    /// The speaker this state is about, if any.
    pub fn speaker(&self) -> Option<&SpeakerId> {
        use NowState::*;
        match self {
            Connecting { target, .. } => Some(target),
            Playing { speaker, .. }
            | ApplyingDelay { speaker, .. }
            | Ready { speaker }
            | ReadyPlayingElsewhere { speaker, .. }
            | NotTaking { speaker, .. }
            | NotResponding { speaker, .. }
            | FellBack { speaker, .. }
            | SpeakerBack { speaker, .. }
            | Taken { speaker, .. }
            | NeedsSecret { speaker, .. }
            | LookingFor { speaker, .. }
            | WindowsSwitched { speaker, .. }
            | WindowsRefused { speaker }
            | PlayingSentApps { speaker, .. }
            | ReadyNoAppSent { speaker } => Some(speaker),
            OnThisPc | ChooseSpeaker | MaPlaying { .. } | MaWaiting | MaAway { .. } | MaBack
            | OutputMissing => None,
        }
    }

    /// The place marked as current in the list (the accent bar): where the
    /// sound is, or is meant to be while a speaker is being retried.
    pub fn current_place(&self) -> Option<Place> {
        use NowState::*;
        match self {
            Connecting { from, .. } => Some(match from {
                SoundAt::ThisPc => Place::ThisPc,
                SoundAt::Speaker(id) => Place::Speaker(id.clone()),
                SoundAt::MusicAssistant => Place::MusicAssistant,
            }),
            Playing { speaker, .. }
            | ApplyingDelay { speaker, .. }
            | Ready { speaker }
            | ReadyPlayingElsewhere { speaker, .. }
            | NotTaking { speaker, .. }
            | NotResponding { speaker, .. }
            | WindowsRefused { speaker }
            | PlayingSentApps { speaker, .. }
            | ReadyNoAppSent { speaker } => Some(Place::Speaker(speaker.clone())),
            MaPlaying { .. } | MaWaiting => Some(Place::MusicAssistant),
            ChooseSpeaker => None,
            OnThisPc | FellBack { .. } | SpeakerBack { .. } | Taken { .. } | NeedsSecret { .. }
            | LookingFor { .. } | WindowsSwitched { .. } | MaAway { .. } | MaBack
            | OutputMissing => Some(Place::ThisPc),
        }
    }

    /// Whether the sound is already on this PC's own output, so quitting
    /// moves nothing.
    pub fn sound_on_this_pc(&self) -> bool {
        use NowState::*;
        match self {
            Connecting { from, .. } => *from == SoundAt::ThisPc,
            // Windows' own output plays everything but the sent apps.
            PlayingSentApps { .. } | ReadyNoAppSent { .. } => true,
            // Windows kept the PC's output.
            WindowsRefused { .. } => true,
            other => other.current_place() == Some(Place::ThisPc),
        }
    }

    /// The tray icon's arcs.
    pub fn arcs(&self) -> Arcs {
        use NowState::*;
        match self {
            Connecting { .. } | MaWaiting => Arcs::GreenFaint,
            Playing { .. } | ApplyingDelay { .. } | Ready { .. } | ReadyPlayingElsewhere { .. }
            | NotTaking { .. } | NotResponding { .. } | WindowsRefused { .. } | MaPlaying { .. }
            | PlayingSentApps { .. } | ReadyNoAppSent { .. } => Arcs::Green,
            OnThisPc | FellBack { .. } | SpeakerBack { .. } | Taken { .. } | NeedsSecret { .. }
            | LookingFor { .. } | WindowsSwitched { .. } | ChooseSpeaker | MaAway { .. }
            | MaBack | OutputMissing => Arcs::Ink,
        }
    }

    /// The problem badge. `seen` says whether the panel has been opened
    /// since this state began: after a fallback, a takeover or a speaker
    /// coming back, the badge stays only until then.
    pub fn badge(&self, seen: bool) -> Badge {
        use NowState::*;
        match self {
            OutputMissing => Badge::Failure,
            NotTaking { .. } | NotResponding { .. } | NeedsSecret { .. } | WindowsRefused { .. }
            | MaAway { .. } => Badge::Attention,
            LookingFor { looking: Looking::NotFound(_), .. } => Badge::Attention,
            FellBack { .. } | SpeakerBack { .. } | Taken { .. } | MaBack => {
                if seen {
                    Badge::None
                } else {
                    Badge::Attention
                }
            }
            _ => Badge::None,
        }
    }

    /// Whether the Now card shows the delay control (when the place is a
    /// speaker whose delay this app can change).
    pub fn shows_delay(&self) -> bool {
        use NowState::*;
        matches!(
            self,
            Playing { .. }
                | ApplyingDelay { .. }
                | Ready { .. }
                | ReadyPlayingElsewhere { .. }
                | WindowsRefused { .. }
                | PlayingSentApps { .. }
                | ReadyNoAppSent { .. }
        )
    }

    /// Whether the card's title carries the level meter (the meter shows
    /// only while this PC really sends sound; the engine says when).
    pub fn may_show_meter(&self) -> bool {
        use NowState::*;
        match self {
            Playing { .. } | MaPlaying { .. } | PlayingSentApps { .. } => true,
            Connecting { from, .. } => *from != SoundAt::ThisPc,
            _ => false,
        }
    }
}
