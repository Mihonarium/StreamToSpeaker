//! What the engine tells the model: plain facts, no words. The model turns
//! one [`Facts`] value into a [`Snapshot`](super::Snapshot) with every
//! surface's words.

use super::state::{NowState, PcOutputId, Secret, SpeakerId};
use super::text::{Lang, LocalTime};

/// Everything the everyday surfaces draw at one moment, as facts.
#[derive(Clone, Debug)]
pub struct Facts {
    pub lang: Lang,
    pub state: NowState,
    pub pc: PcFacts,
    /// Every speaker the app knows, hidden ones included (they are left
    /// out of the list but keep their names for the state's sentences).
    pub speakers: Vec<SpeakerFacts>,
    /// `Some` when Music Assistant is set up (paired).
    pub music_assistant: Option<MaFacts>,
    pub search: SearchFacts,
    pub volume: VolumeFacts,
    /// The delay control's facts for the speaker the state is about.
    pub delay: Option<DelayFacts>,
    pub delay_steps: DelaySteps,
    /// Something has been removed or added on some speaker at some time, so
    /// the panel no longer explains the two columns.
    pub delay_hint_retired: bool,
    pub mode: Mode,
    /// The place Windows' device is named after: the last place chosen.
    pub remembered: Remembered,
    /// The panel has been opened since the current state began.
    pub seen: bool,
    /// This PC is sending real sound (not silence) to the current place.
    pub sending_sound: bool,
    pub notices: NoticeFacts,
    /// The latest notification, if the engine raised one; surfaces show a
    /// notification whose `seq` they haven't shown yet.
    pub notification: Option<NotificationFacts>,
    /// First run, while it shows in the panel.
    pub first_run: Option<FirstRun>,
}

/// The step of first run the panel shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirstRun {
    /// Pick a speaker; the list fills as speakers answer.
    Pick,
    /// The chime played on `speaker`.
    Hear { speaker: SpeakerId },
    /// Where the app lives, and starting at sign-in.
    FindIt { speaker: SpeakerId, start_at_sign_in: bool },
}

/// This PC.
#[derive(Clone, Debug)]
pub struct PcFacts {
    /// The output This PC plays on: Windows' default while it isn't this
    /// app's, otherwise the one the app gives the sound back to.
    pub output: PcOutput,
}

/// One of Windows' sound outputs, with Windows' own names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcOutput {
    pub id: PcOutputId,
    /// Windows' full name, e.g. "Speakers (Realtek Audio)".
    pub friendly: String,
    /// The endpoint's own name, e.g. "Speakers", "Headphones".
    pub name: String,
}

/// A speaker, group or pair the app knows.
#[derive(Clone, Debug)]
pub struct SpeakerFacts {
    pub id: SpeakerId,
    /// The name as the speaker's own app shows it (or the person's *Show
    /// as* name).
    pub name: String,
    pub kind: SpeakerKind,
    /// Its network address, shown only when two entries share a name.
    pub address: String,
    pub delay: Option<SpeakerDelay>,
    pub presence: Presence,
    /// Played on from this PC before; speakers never used fold away on a
    /// crowded network.
    pub used_here: bool,
    pub hidden: bool,
    /// It says it is playing something else (where it can tell).
    pub busy: bool,
    /// A click on it failed; shown until the person acts or picks
    /// something else.
    pub failure: Option<ConnectFailure>,
    /// A click on it is waiting for the person's answer because it is
    /// playing something else.
    pub question: Option<TakeOver>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeakerKind {
    Sonos,
    SonosGroup,
    HomePod,
    HomePodPair,
    AppleTv,
    /// Any other AirPlay speaker.
    AirPlay,
    Sendspin,
    /// Any other network speaker; `maker` is its manufacturer's name when
    /// it reports one ("Denon").
    Other { maker: Option<String> },
}

/// The speaker's delay as rows show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeakerDelay {
    /// The speaker's own delay with nothing removed or added, in ms:
    /// estimated, or derived from a sync check when `measured`.
    pub own_ms: u32,
    pub measured: bool,
    /// What the person removed (negative) or added, in ms; the target when
    /// a change is still being applied.
    pub change_ms: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Found,
    /// Remembered, not found right now.
    NotFound,
    /// Found, but this app can't play to it (left out of the panel; its
    /// page in Settings says why).
    CantPlay,
    /// A stereo pair with one HomePod missing.
    WaitingForPair,
}

/// Why a click on a speaker failed, where the app knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectFailure {
    NoAnswer,
    WrongPassword,
    /// The app couldn't start the attempt.
    CouldntStart,
    /// *Reconnect* failed.
    CouldntReconnect,
    Other,
}

/// The question a busy speaker raises on its row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakeOver {
    /// Another PC running this app, when the stream's title names it.
    pub other_pc: Option<String>,
}

/// Music Assistant, when set up.
#[derive(Clone, Debug)]
pub struct MaFacts {
    pub connected: bool,
    pub last_seen: Option<LocalTime>,
}

/// The search for speakers.
#[derive(Clone, Debug, Default)]
pub struct SearchFacts {
    pub phase: SearchPhase,
    /// The network searched, when there is one; `None` for several.
    pub network: Option<NetworkLabel>,
    /// Causes the app tested and found, shown when nothing was found.
    pub causes: Vec<SearchCause>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchPhase {
    /// Speakers were found (or the search isn't news).
    #[default]
    Found,
    /// Searching, nothing found yet (the first 10 seconds).
    Looking,
    /// Nothing found after 10 seconds.
    NoneFound,
}

/// A network by its adapter and, for Wi-Fi, its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkLabel {
    pub adapter: String,
    pub network: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchCause {
    /// Windows treats this network as public.
    PublicNetwork { network: String },
    FirewallBlocks,
}

/// Volume levels the Now card may show; the state decides whose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeFacts {
    /// This PC's output.
    pub pc: Level,
    /// The current speaker's own level, or the level sent to Music
    /// Assistant.
    pub place: Option<Level>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    /// 0–100.
    pub value: u8,
    pub muted: bool,
    /// The speaker's volume limit, shown as a tick.
    pub limit: Option<u8>,
}

impl Level {
    pub const fn at(value: u8) -> Self {
        Level { value, muted: false, limit: None }
    }
}

/// The delay control's facts for the speaker being played on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelayFacts {
    pub speaker: SpeakerId,
    pub range: DelayRange,
    pub applies: Applies,
    /// Short delay or Steady, for AirPlay speakers and single HomePods.
    pub choice: Option<ChoiceFacts>,
    pub progress: Progress,
    pub note: Option<DelayNote>,
}

/// How far the delay can go from the speaker's own, in ms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelayRange {
    pub max_removed_ms: u32,
    pub max_added_ms: u32,
}

/// How a change reaches the speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    /// While playing, without a gap (Sonos and other UPnP speakers).
    Live,
    /// By reconnecting: a few seconds of silence (AirPlay, HomePods,
    /// Sendspin until it takes changes live).
    Reconnect,
    /// Not at all yet: an AirPlay speaker on Steady reached over AirPlay 2.
    Fixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelayChoice {
    Short,
    Steady,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChoiceFacts {
    pub current: DelayChoice,
    /// The speaker's delay on Steady and on Short delay, with nothing
    /// removed or added, in ms.
    pub steady_ms: u32,
    pub short_ms: u32,
}

/// A change on its way to the speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Settled,
    /// Being applied while playing; `to_go_ms` is still to remove
    /// (negative) or to add.
    Live { to_go_ms: i32 },
    /// Waiting the moment before the reconnect that applies it.
    ReconnectPending,
    /// The reconnect is running.
    Reconnecting,
}

/// Something to say under the delay buttons that the numbers alone don't
/// show. Each lasts until the next change, or while it is true; the engine
/// drops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelayNote {
    /// The speaker didn't accept `tried_ms`; the change is back to
    /// `back_to_ms`.
    NotAccepted { tried_ms: i32, back_to_ms: i32 },
    /// It ran dry soon after a removal with `with_ms`; some went back.
    DroppedOut { with_ms: i32, back_to_ms: i32 },
    /// *Remove* was pressed on Steady.
    SteadyRemovePressed,
    /// The delay choice was just switched; the other choice keeps
    /// `other_kept_ms`.
    ChoiceSwitched { other_kept_ms: i32 },
    /// Dropouts the app saw in the last 10 minutes, with nothing removed
    /// in the 30 seconds before them.
    Dropouts { count: u32 },
}

/// The delay buttons' step sizes (25 and 100 unless the Lab changed them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelaySteps {
    pub small_ms: u32,
    pub large_ms: u32,
}

impl Default for DelaySteps {
    fn default() -> Self {
        DelaySteps { small_ms: 25, large_ms: 100 }
    }
}

/// A speaker's *Comes back by itself*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComesBack {
    /// When it was the last place chosen (the default).
    LastChosen,
    /// Whenever it appears and nothing else is playing.
    WheneverAppears,
    Never,
}

/// General → *What plays on your speaker*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Everything,
    /// "Only apps I send there"; `pass_through` once the sent apps also
    /// play on This PC while no speaker is chosen.
    OnlySentApps { pass_through: bool },
}

/// The place Windows' device is named after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remembered {
    /// No speaker chosen yet, or forgotten.
    Nothing,
    Speaker { name: String },
    MusicAssistant,
}

/// Background facts for the notice line.
#[derive(Clone, Debug, Default)]
pub struct NoticeFacts {
    /// A newer sound driver is installed but not running.
    pub restart_pending: bool,
    /// The adapter chosen in Settings → Speakers is off.
    pub adapter_off: Option<String>,
    /// A device in the current speaker's group asked for the sound and was
    /// refused.
    pub refused_in_group: Option<RefusedDevice>,
    pub lab_on: bool,
    pub update_ready: Option<String>,
    /// Shown once after an update, until dismissed.
    pub updated_to: Option<String>,
    /// The donation line is due (two weeks of use, at most monthly, not in
    /// first run); the model still hides it while a problem shows.
    pub donation_due: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedDevice {
    pub name: String,
    /// The engine's key for allowing it.
    pub key: String,
}

/// A notification the engine raised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationFacts {
    pub seq: u64,
    pub kind: NotificationKind,
    /// The speaker it is about, for the kinds that are about one.
    pub speaker: Option<SpeakerId>,
}

/// The six cases that notify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationKind {
    /// The tries failed while something was playing; sound back on this PC.
    StoppedResponding,
    /// The same, with "stay silent" chosen.
    StoppedRespondingSilent,
    /// The speaker switched to another source while something played.
    Taken,
    /// A lost speaker is back within an hour; the app didn't return to it
    /// because something played here.
    SpeakerBack,
    /// An automatic return needs a code or password.
    NeedsSecret(Secret),
    /// Once, after an update that installed a new sound driver.
    RestartToFinish,
    /// The sound output went missing while the app ran.
    OutputMissing,
}
