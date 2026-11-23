//! Everything a person can ask for from the everyday surfaces, and what
//! comes back.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use super::facts::DelayChoice;
use super::snapshot::Snapshot;
use super::state::{PcOutputId, Place, SpeakerId};

/// The surface a command came from (kept with delay changes, for the
/// delay history and reports).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    Panel,
    Tray,
    Shortcut,
    Notification,
    Phone,
    CommandLine,
    Api,
    Settings,
}

/// One request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Play on a place: a row, the tray, the phone, `play`, a notice's or
    /// notification's button. The latest choice wins.
    Play(Place),
    /// Play on a speaker that is playing something else, taking it over:
    /// the answer to the row's question, or *Play on Kitchen again* after
    /// a takeover.
    PlayTakingOver(SpeakerId),
    /// Make one of Windows' outputs This PC's output and play there.
    PlayOnPcOutput(PcOutputId),
    /// Stop the connection being made; the sound stays where it is.
    CancelConnect,
    /// Leave a busy speaker alone (*Cancel* on its row's question).
    DismissQuestion(SpeakerId),
    /// Keep the sound on this PC and stop waiting to return or asking.
    StayOnThisPc,
    /// Set the volume the Now card shows. `owner` guards against a slider
    /// that was still moving when the place changed.
    SetVolume { owner: VolumeOwner, level: u8 },
    SetMuted { owner: VolumeOwner, muted: bool },
    /// Remove or add delay on a speaker.
    ChangeDelay { speaker: SpeakerId, step: DelayStep },
    /// Switch a speaker between Short delay and Steady.
    UseDelayChoice { speaker: SpeakerId, choice: DelayChoice },
    /// Connect the speaker again, keeping its delay change.
    Reconnect(SpeakerId),
    PlayTestSound(SpeakerId),
    HideFromList(SpeakerId),
    /// Let a refused device fetch the sound (the notice line's *Allow*).
    AllowDevice { key: String },
    TurnOffLab,
    DismissNotice(NoticeKind),
    /// Open a window, page or dialog, ours or Windows'.
    Open(Open),
    /// The panel was opened or closed (notifications wait while it is open;
    /// badges for things already shown clear).
    PanelShown(bool),
    /// The Settings window came to the front or left it.
    SettingsShown(bool),
    /// Give Windows' output back to this PC and let the speaker go, as
    /// Setup does before it updates the app.
    Release { for_update: bool },
    Quit,
}

/// What a delay button or command asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelayStep {
    Remove(u32),
    Add(u32),
    /// An exact total, for scripts: negative is removed.
    SetTotal(i32),
    /// Back to the speaker's own delay.
    Reset,
}

/// Whose volume a slider sets.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VolumeOwner {
    Speaker(SpeakerId),
    PcOutput(PcOutputId),
    /// The level this PC sends to Music Assistant.
    MusicAssistant,
}

/// The notice line's entries, in precedence order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NoticeKind {
    RestartPending,
    AdapterOff,
    RefusedInGroup,
    LabOn,
    UpdateReady,
    Updated,
    Donation,
}

/// Something to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Open {
    /// The panel; `amount_field` opens the delay control's amount field.
    Panel { amount_field: bool },
    Settings,
    /// A speaker's page; `delay` focuses its delay control.
    SpeakerSettings { speaker: SpeakerId, delay: bool },
    MusicAssistantSettings,
    /// How to make Music Assistant start playing this PC's sound.
    MusicAssistantHowTo,
    /// The "Can't hear it?" check, for a speaker when there is one.
    CantHear(Option<SpeakerId>),
    /// The "Can't find your speaker?" check.
    SpeakerMissing,
    AddByAddress,
    /// The code or password dialog of a speaker.
    Pair(SpeakerId),
    Help,
    About,
    WhatsNew,
    Donate,
    /// Windows: Sound settings.
    SoundSettings,
    /// Windows: the Volume mixer.
    VolumeMixer,
    /// Windows: the network's settings (to make it private).
    NetworkSettings,
    /// The repair of the sound output (asks for administrator approval).
    Repair,
    /// Restart Windows (after asking).
    RestartWindows,
}

/// Why a command did nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The current place has no delay this app can change.
    NoDelayHere,
    /// Music Assistant sets its speakers' delay.
    MusicAssistantDelay,
    /// An AirPlay speaker on Steady reached over AirPlay 2: nothing can be
    /// removed or added yet.
    FixedOnSteady { speaker: SpeakerId },
    /// `play "name"` named no speaker in the list.
    UnknownPlace { name: String },
}

/// What a delay change did, for its confirmation and the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelayOutcome {
    pub speaker: SpeakerId,
    pub step: DelayStep,
    /// Removed (negative) or added, before and after.
    pub before_ms: i32,
    pub after_ms: i32,
    /// The change the step asked for went past a limit and stopped there.
    pub stopped_at_limit: bool,
}

/// The result of a command, as the engine reports it once handled.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub result: Result<Effect, Refusal>,
    /// The snapshot right after the command was handled.
    pub snapshot: Arc<Snapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Done,
    Delay(DelayOutcome),
}

/// The way back for a command's outcome. A surface that doesn't care drops
/// it; the command line waits on it (never on a UI thread).
pub struct Ticket(mpsc::Receiver<Outcome>);

/// The engine's end of a [`Ticket`].
pub struct Reply(mpsc::SyncSender<Outcome>);

/// A connected reply and ticket.
pub fn ticket() -> (Reply, Ticket) {
    let (tx, rx) = mpsc::sync_channel(1);
    (Reply(tx), Ticket(rx))
}

impl Reply {
    /// Hands the outcome to whoever holds the ticket (if anyone still does).
    pub fn send(self, outcome: Outcome) {
        let _ = self.0.try_send(outcome);
    }
}

impl Ticket {
    /// The outcome if it has arrived; never waits.
    pub fn try_outcome(&self) -> Option<Outcome> {
        self.0.try_recv().ok()
    }

    /// Waits up to `timeout` for the outcome. Not for UI threads.
    pub fn wait(&self, timeout: Duration) -> Option<Outcome> {
        self.0.recv_timeout(timeout).ok()
    }
}
