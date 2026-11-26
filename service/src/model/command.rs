//! Everything a person can ask for from the everyday surfaces, and what
//! comes back.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use super::facts::DelayChoice;
use super::situation::WayChoice;
use super::Snapshot;
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
    /// Play on a speaker that is playing something else, taking it over:
    /// the answer to the row's question, or *Play on Kitchen again* after
    /// a takeover.
    PlayTakingOver(SpeakerId),
    /// Open or close the list of This PC's outputs under its row.
    ShowPcOutputs(bool),
    /// Keep the sound on this PC and stop waiting to return or asking.
    StayOnThisPc,
    /// Set the volume the volume row shows. `owner` guards against a slider
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
    /// The ordinary window was closed and the app continues in the tray.
    WindowClosed,
    /// The ordinary window opened.
    WindowOpened,
    /// Give Windows' output back to this PC and let the speaker go, as
    /// Setup does before it updates the app.
    Release { for_update: bool },
    Quit,

    /// A person's switch: a row, a tray entry, a *Switch to …* button, the
    /// phone page, the command line, the API. `way` is the way to use
    /// (a part of the delay block, a right-click entry); `None` uses the
    /// way last chosen for that speaker. It takes effect when the speaker
    /// is ready and, where the app asks first, confirmed.
    Switch { place: Place, way: Option<WayChoice> },
    /// The speaker named by `--player` appeared. Give way to any newer pick.
    PlayWhenListed { speaker: SpeakerId, picks_seen: u64 },
    /// The person finished pairing a speaker whose connection request already ended.
    PlayAfterPairing { speaker: SpeakerId, picks_seen: u64 },
    /// A switch to This PC on one of its outputs (one of This PC's outputs
    /// under its row).
    SwitchOutput(PcOutputId),
    /// *Try Kitchen again*: a switch to that speaker, tried once.
    TryAgain(SpeakerId),
    /// *Cancel* while a switch connects: the sound stays where it is.
    CancelSwitch,
    /// The answer to the confirmation that shows (*Switch to Kitchen* or
    /// *Play a test chime*: `yes`; *Cancel* or ×: not).
    Confirm { yes: bool },
    /// The × of the note that says why the sound is on this PC: the note
    /// goes, and the sound stays where it is.
    CloseNote,
    /// The × of a pop-up by the clock: only the pop-up goes.
    ClosePopup,
    /// A way or base delay chosen in the delay block or a row's menu: for
    /// the place's speaker a switch to that way, for another speaker the
    /// way its next switch uses.
    ChooseWay { speaker: SpeakerId, way: WayChoice },
    /// *Play a test chime*: a chime on that speaker only, whatever the
    /// place. `confirmed` when the person already answered the speaker's
    /// confirmation.
    PlayChime { speaker: SpeakerId, confirmed: bool },
    /// The delay block's offer after the sound cut out: `take` for *Add 50
    /// ms* (or the steady delay), not for its ×.
    DelayOffer { speaker: SpeakerId, take: bool },
    /// *Set the delay…*: the delay itself, in ms.
    SetDelay { speaker: SpeakerId, ms: u32 },
    /// *Use it* beside *Default: 200 ms*.
    UseDefaultDelay(SpeakerId),
    /// *Remove Kitchen…*, answered: the speaker and its settings go.
    RemoveSpeaker(SpeakerId),
    /// *Reset Kitchen's settings…*, answered.
    ResetSpeaker(SpeakerId),
    /// An act in first run.
    FirstRunAction(FirstRunAction),
    /// Setup's `release`: before an update (`for_update`; `new_driver` when
    /// it also replaces the sound driver) or at uninstall.
    ReleaseWindows { for_update: bool, new_driver: bool },
}

/// What a person does in first run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirstRunAction {
    /// A click on a row in step 1: a chime on that speaker only.
    Chime(SpeakerId),
    /// *Play it again* in step 2.
    ChimeAgain,
    /// Volume for the next test chime on this speaker.
    ChimeVolume(u8),
    /// *I heard it*: the speaker becomes the place.
    Heard,
    /// *Use Kitchen without checking*: the speaker becomes the place.
    UseWithoutChecking,
    /// *I didn't hear it*: the playback check opens; first run stays on
    /// step 2.
    DidntHear,
    /// *Choose another speaker* (or *Cancel* in a code window): back to
    /// step 1.
    ChooseAnother,
    /// The × of a step: first run ends.
    Close,
    /// The switch *Start when I sign in to Windows*.
    StartAtSignIn(bool),
    /// *Done*.
    Done,
    /// *Show the welcome again*: first run from step 1.
    ShowAgain,
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
    /// The ordinary window, showing the panel.
    Panel,
    Settings,
    /// A speaker's page in Settings.
    SpeakerSettings(SpeakerId),
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
    WhatsNew,
    Donate,
    /// Windows: Sound settings.
    SoundSettings,
    /// Windows: the Volume mixer.
    VolumeMixer,
    /// Windows: the network's settings (to make it private).
    NetworkSettings,
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
    /// A receiver that announces another speaker's identity plays only
    /// when a person picks it in the app's own window.
    /// Also the answer to a switch from the phone page, the command line or
    /// the API that would need a confirmation: only the app's own windows
    /// ask one, so nothing changed.
    OnlyFromWindow { speaker: SpeakerId },
    /// The command belongs to the app's own windows (a confirmation's
    /// answer where it shows, first run, the × of a note or a pop-up,
    /// removing or resetting a speaker) and came from elsewhere: nothing
    /// changed.
    WindowOnly,
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
