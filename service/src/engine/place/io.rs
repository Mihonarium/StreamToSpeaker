//! What goes into the core and what comes out of it: what the engine
//! observes at a moment ([`World`]), what happened ([`Event`]), and what to
//! do ([`Do`]).

use std::sync::Arc;

use crate::model::facts::OutputKind;
use crate::model::situation::{ConnKind, WayChoice};
use crate::model::state::{PcOutputId, Secret, SpeakerId};

/// An app, by its program, as Windows names it for its audio sessions and
/// its media sessions alike (so that a player is matched to its sound by
/// app, not by process).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppKey(pub Arc<str>);

impl AppKey {
    pub fn new(app: &str) -> Self {
        AppKey(Arc::from(app))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A player Windows' media keys can pause (a media session).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlayerId(pub Arc<str>);

impl PlayerId {
    pub fn new(id: &str) -> Self {
        PlayerId(Arc::from(id))
    }
}

/// One of Windows' outputs, or the app's own device.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutputRef {
    /// The app's device (*Kitchen (Stream To Speaker)*).
    Ours,
    Pc(PcOutputId),
}

/// What the engine observes at a moment. The core reads it at each call;
/// what changed comes as an [`Event`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct World {
    /// Every speaker the app knows, in the order the list shows them.
    pub speakers: Vec<SpeakerWorld>,
    /// Windows' outputs that are switched on, never the app's own device.
    pub outputs: Vec<OutputWorld>,
    /// Windows' default output now; `None` where the platform can't tell
    /// (then the app never changes it).
    pub default_output: Option<OutputRef>,
    /// The default for apps that follow Windows’ Multimedia role.
    pub multimedia_output: Option<OutputRef>,
    /// The app’s device is in Windows’ list of sound outputs.
    pub our_device: bool,
    /// Windows lists the app’s render endpoint as disabled.
    pub output_turned_off: bool,
    pub can_repair: bool,
    pub players: Vec<PlayerWorld>,
    /// This PC's network connections now.
    pub connections: Vec<Connection>,
    pub settings: SettingsWorld,
    /// A screen reader runs (Windows' flag, or Narrator's process).
    pub screen_reader: bool,
    /// The panel is open: nothing shows by the clock meanwhile.
    pub panel_open: bool,
    /// Music Assistant plays this PC's sound (while it is the place).
    pub music_assistant_playing: bool,
    /// The app can't read this PC's sound and keeps trying to: what it
    /// sends meanwhile is silence.
    pub sound_unreadable: bool,
}

impl World {
    pub(super) fn playback_on(&self, target: &OutputRef) -> bool {
        self.default_output.as_ref() == Some(target) && self.multimedia_output.as_ref() == Some(target)
    }

    pub(super) fn uses_app_output(&self) -> bool {
        self.default_output == Some(OutputRef::Ours) || self.multimedia_output == Some(OutputRef::Ours)
    }

    pub(super) fn playback_on_pc(&self) -> bool {
        matches!(self.default_output, Some(OutputRef::Pc(_))) && matches!(self.multimedia_output, Some(OutputRef::Pc(_)))
    }
}

/// A speaker as the engine knows it now.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerWorld {
    pub id: SpeakerId,
    /// It is announced on this PC's networks now.
    pub found: bool,
    /// The ways it can be reached now, the one with the shortest delay
    /// first.
    pub ways: Vec<WayChoice>,
    /// It says what it is doing (Sonos and other UPnP speakers); AirPlay
    /// speakers, Music Assistant's players and a Sonos on its TV input
    /// don't.
    pub shows_doing: bool,
    /// A chime can play on this speaker now.
    pub can_chime: bool,
    /// The app sets its starting level, as on AirPlay and fixed-volume Sendspin players.
    pub level_unreadable: bool,
    /// Its level now, where it can be read.
    pub level: Option<u8>,
    /// *Maximum volume from this PC*.
    pub max_level: Option<u8>,
    /// The network it was last found on.
    pub network: Option<NetId>,
    /// A receiver that announces another speaker's identity: it plays only
    /// from a switch made in the app's own windows.
    pub only_from_window: bool,
}

impl SpeakerWorld {
    /// A speaker that is found, by one way, and says what it is doing.
    pub fn found(id: &str) -> Self {
        SpeakerWorld {
            id: SpeakerId::new(id),
            found: true,
            ways: vec![WayChoice::Native],
            shows_doing: true,
            can_chime: true,
            level_unreadable: false,
            level: None,
            max_level: None,
            network: None,
            only_from_window: false,
        }
    }
}

/// One of Windows' outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputWorld {
    pub id: PcOutputId,
    pub kind: OutputKind,
    /// Windows' name for it.
    pub name: String,
}

/// A player and what it does, as Windows' media sessions show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerWorld {
    pub id: PlayerId,
    pub app: AppKey,
    /// Windows' name for it (*Spotify*), where it gives one.
    pub name: Option<String>,
    pub status: PlayerStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlayerStatus {
    Playing,
    Paused,
    Stopped,
    Other,
}

/// A network, by the id Windows gives its profile.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct NetId(pub String);

/// A network connection of this PC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub net: NetId,
    /// Its name, where Windows gives one.
    pub name: Option<String>,
    pub kind: ConnKind,
}

/// The person's settings the behaviour reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsWorld {
    /// *Switch back to this PC after 30 minutes without sound* (off).
    pub switch_back_after_quiet: bool,
    /// *Show pop-ups by the clock about where the sound is playing* (on).
    pub notes: bool,
    /// *Only send some apps to the speaker* (off): Windows' output is never
    /// changed.
    pub only_some_apps: bool,
}

impl Default for SettingsWorld {
    fn default() -> Self {
        SettingsWorld { switch_back_after_quiet: false, notes: true, only_some_apps: false }
    }
}

/// Something that happened.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    RepairApproved(u64),
    RepairExited(u64, crate::platform::RepairExit),
    TurnOnFailed,
    /// A connection the core asked for ended.
    Connected { gen: u64, ended: Ended },
    /// A switch released this place's session before another choice took effect.
    PlaceReleased(SpeakerId),
    /// A change of way ended with neither session bound.
    WayLost { speaker: SpeakerId, ended: Ended },
    /// A speaker said something, or a request to it told something.
    Speaker { id: SpeakerId, signal: SpeakerSignal },
    Output(OutputEvent),
    Player(PlayerEvent),
    /// A sample of every app's level.
    Sound(Vec<super::sound::Level>),
    /// This PC's network connections changed ([`World::connections`] has
    /// the new ones).
    Network,
    Power(PowerEvent),
    /// The app started.
    Started(StartKind),
}

/// How a connection ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The speaker is ready and plays this PC's stream (or the chime).
    Ready,
    /// The look found it playing something else; nothing was sent.
    Busy { who: Option<String> },
    NotResponding,
    NotFound,
    /// This PC has no network connection.
    NoNetwork,
    /// It asks for a code or password and nobody can be asked (or the
    /// dialog was closed).
    NeedsSecret(Secret),
    /// The code or password was refused.
    WrongSecret(Secret),
    /// The app couldn't start the attempt.
    CouldntStart,
    /// The speaker refused, for a reason of its own.
    Refused,
}

pub use crate::engine::signals::SpeakerSignal;

/// What Windows did with its outputs. `at_ms` is when Windows said it, on
/// the core's monotonic clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    Added { id: PcOutputId, kind: OutputKind, name: String, at_ms: u64 },
    Removed { id: PcOutputId, at_ms: u64 },
    /// Windows' default output changed ([`World::default_output`] has it).
    DefaultChanged { at_ms: u64 },
    /// A change the app asked for ([`Do::Windows`]) wasn't taken.
    NotTaken { at_ms: u64 },
    /// The app's device left Windows' list, or came back.
    DeviceChanged { present: bool, at_ms: u64 },
}

/// What a player did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerEvent {
    /// Its status changed ([`World::players`] has it).
    Status { id: PlayerId, status: PlayerStatus },
    /// Anything else changed: a skip, a seek, another track.
    Other { id: PlayerId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    Suspending,
    Resumed,
    SigningOut,
    ShuttingDown,
}

/// How the app started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartKind {
    /// At sign-in, from the Start menu, or the first time.
    Normal,
    /// After an update (Windows stayed on the app's device).
    AfterUpdate,
    /// The watcher started it again after a crash.
    AfterCrash,
    /// The watcher couldn't keep it running and gave Windows the PC's own
    /// output: the start begins on this PC.
    AfterGivingUp,
}

/// What the engine does, in the order given.
#[derive(Clone, Debug, PartialEq)]
pub enum Do {
    TurnOnOutput,
    RepairOutput(u64),
    /// A person or Windows chose an output.
    PickedOutput,
    /// Connect a speaker (on a thread of its own); the end comes back as
    /// [`Event::Connected`] with the job's `gen`.
    Connect(Job),
    /// The connection of that `gen` is no longer wanted.
    Supersede(u64),
    /// Make the speaker silent where it still plays this PC's stream (it
    /// answers [`SpeakerSignal::Silenced`]), then, once it confirmed or
    /// after a short wait, move Windows to `then`.
    Silence { speaker: SpeakerId, then: Option<WinTarget> },
    /// Send the speaker nothing more; `take_off` also takes the stream off
    /// it where it lets that happen (after 30 minutes without sound).
    StopSending { speaker: SpeakerId, take_off: bool },
    /// Windows' default output.
    Windows(WinTarget),
    /// Pause these players as a pause pressed in them would: only ever the
    /// pause a person pressed on the speaker itself, passed on.
    Pause(Vec<PlayerId>),
    /// Start these players: only ever play pressed on the speaker itself,
    /// for the players its pause stopped.
    Play(Vec<PlayerId>),
    /// Play what arrives at the app's device on this output (`None`: stop).
    PassThrough(Option<PcOutputId>),
    /// Play the short sound that just ended on this output: what arrived at
    /// the app's device in the last `ms` milliseconds.
    ReplayShort { on: PcOutputId, ms: u64 },
    SetLevel { speaker: SpeakerId, level: u8 },
    /// Ask a speaker whether it responds and is free; it answers with a
    /// [`SpeakerSignal`].
    Look(SpeakerId),
    /// The switch *Start when I sign in to Windows*.
    StartAtSignIn(bool),
    /// Turn Only send some apps back on.
    RestoreOnlySomeApps,
    /// What is kept changed ([`super::Core::kept`]).
    Save,
}

/// A connection to make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub gen: u64,
    pub speaker: SpeakerId,
    pub way: WayChoice,
    pub purpose: Purpose,
    /// First run sets the level for both the chime and the switch.
    pub welcome_level: Option<u8>,
    /// Look first whether it is playing something else (it ends
    /// [`Ended::Busy`] then, without sending anything).
    pub look_first: bool,
    /// A person asked for it in the app's own windows: only then may a code
    /// or password be asked, and only then may a receiver that announces
    /// another speaker's identity be played to.
    pub in_window: bool,
}

/// Why a connection is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// A person's switch.
    Switch,
    /// Sound started on the place's speaker, which wasn't connected: the
    /// one try.
    SoundStarted,
    /// One of the tries during play.
    Retry,
    /// *Try … again*.
    TryAgain,
    /// Another way of the place's speaker.
    Way,
    /// A test chime only.
    Chime,
}

/// Where Windows' default output goes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WinTarget {
    /// The app's device.
    OurDevice,
    /// One of the PC's outputs.
    Output(PcOutputId),
}
