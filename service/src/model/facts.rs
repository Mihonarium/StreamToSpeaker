//! Observations shared by the place driver, views and settings.
use super::situation::ConnKind;
use super::state::{PcOutputId, SpeakerId};
/// One of Windows' sound outputs, with Windows' own names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcOutput {
    pub id: PcOutputId,
    /// Windows' full name, e.g. "Speakers (Realtek Audio)".
    pub friendly: String,
    /// The endpoint's own name, e.g. "Speakers", "Headphones".
    pub name: String,
    /// The device it belongs to, e.g. "Realtek Audio".
    pub device: String,
    pub kind: OutputKind,
}

/// One of this PC's network connections, as Windows describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkSeen {
    /// Windows' id of the adapter it is on, where known.
    pub adapter: Option<String>,
    /// The network's name: a Wi-Fi network's own, or the one Windows gave
    /// it.
    pub name: Option<String>,
    /// Whether Windows treats it as a private network (`true`) or a public
    /// one (`false`); `None` where it can't be told or is neither.
    pub private: Option<bool>,
    pub kind: ConnKind,
}

/// What an output is, for its icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    Speakers,
    Headphones,
    /// A monitor or TV over HDMI or DisplayPort.
    Display,
    Other,
}

/// A speaker, group or pair the app knows.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerFacts {
    pub id: SpeakerId,
    /// The name as the speaker's own app shows it (or the person's *Show
    /// as* name).
    pub name: String,
    pub kind: SpeakerKind,
    /// Its network address, shown only when two entries share a name.
    pub address: String,
    pub presence: Presence,
    /// Played on from this PC before; speakers never used fold away on a
    /// crowded network.
    pub used_here: bool,
    pub hidden: bool,
    /// The same speaker reached another way (a Sonos speaker by its own
    /// protocol and by AirPlay): the two are listed next to each other.
    pub twin: Option<SpeakerId>,
    /// When it was last played on from this PC, as a number that grows with
    /// time; of two ways to one speaker, the one used last is listed first.
    pub last_used: Option<u64>,
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

impl SpeakerDelay {
    pub(crate) fn total_ms(self) -> i64 {
        (i64::from(self.own_ms) + i64::from(self.change_ms)).max(0)
    }
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

/// The search for speakers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchFacts {
    pub phase: SearchPhase,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchPhase {
    /// All speaker searches are off for this run.
    Off,
    /// Speakers were found (or the search isn't news).
    #[default]
    Found,
    /// Searching, nothing found yet (the first 10 seconds).
    Looking,
    /// Nothing found after 10 seconds.
    NoneFound,
}
/// Volume levels the Now card may show; the state decides whose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeFacts {
    /// This PC's output, when its level can be read.
    pub pc: Option<Level>,
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
    /// The delay choice was just switched; the other choice keeps
    /// `other_kept_ms`.
    ChoiceSwitched { other_kept_ms: i32 },
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


use super::DelayStep;
/// What a press does: the change after it, stopped at the speaker's limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pressed {
    pub before_ms: i32,
    pub after_ms: i32,
    /// The step wanted to go past a limit and stopped at it.
    pub stopped_at_limit: bool,
}

/// What `step` does to a change of `before_ms` within `range`. A step that
/// would pass a limit stops at it; one that starts beyond a limit (the
/// range shrank) never moves further past it.
pub fn press(range: DelayRange, before_ms: i32, step: DelayStep) -> Pressed {
    let lower = -(range.max_removed_ms as i64);
    let upper = range.max_added_ms as i64;
    let before = before_ms as i64;
    let (target, after) = match step {
        DelayStep::Remove(ms) => {
            let target = before - ms as i64;
            (target, if target >= lower { target } else { lower.min(before) })
        }
        DelayStep::Add(ms) => {
            let target = before + ms as i64;
            (target, if target <= upper { target } else { upper.max(before) })
        }
        DelayStep::SetTotal(total) => {
            let target = total as i64;
            (target, target.clamp(lower, upper))
        }
        DelayStep::Reset => (0, 0),
    };
    Pressed { before_ms, after_ms: after as i32, stopped_at_limit: after != target }
}
