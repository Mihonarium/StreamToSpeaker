//! What the Settings window reads and does, as plain values.
//!
//! [`SettingsPort`] is the window's only way to the rest of the app: each
//! page reads one view (a plain value, no words) and every control sends
//! one [`Act`]. The window composes every sentence itself from the string
//! table. A value the app can't supply yet is `None`, and the page then
//! leaves that control out, so a control that is shown always works.
//!
//! [`super::scripted::ScriptedSettings`] answers in memory, for pictures
//! and tests; [`super::real`] answers from the running app.

pub use crate::equalizer::{EqSettings, EQ_BANDS};
pub use crate::model::facts::SpeakerKind;
use crate::model::text::{Lang, LocalTime};

/// Everything the Settings window and its dialogs read and do.
pub trait SettingsPort: Send + Sync {
    /// The language the window speaks.
    fn lang(&self) -> Lang {
        Lang::English
    }
    fn speakers(&self) -> SpeakersView;
    /// One speaker's page; `None` once it is gone (forgotten or removed).
    fn speaker(&self, id: &str) -> Option<SpeakerView>;
    fn general(&self) -> GeneralView;
    /// Try the recorded keys while the normal shortcuts are released.
    fn accepts_shortcuts(&self, keys: &crate::platform::DelayKeys) -> bool;
    /// The Windows key held while recording a shortcut.
    fn windows_key_down(&self) -> bool { false }
    fn music_assistant(&self) -> MaView;
    fn privacy(&self) -> PrivacyView;
    fn about(&self) -> AboutView;
    fn lab(&self) -> LabView;
    fn playback_check(&self) -> PlaybackCheck;
    fn discovery_check(&self) -> DiscoveryCheck;
    /// A dialog the app needs now (a speaker asks for its code or password
    /// while connecting; Music Assistant is pairing).
    fn wanted_dialog(&self) -> Option<Wanted>;
    /// What the add-by-address dialog's last *Find* found so far.
    fn probe(&self) -> Probe;
    /// A speaker's tone, for the tone dialog.
    fn tone(&self, id: &str) -> Option<ToneView>;
    /// The report's text with these parts, exactly as it would be copied.
    fn report(&self, parts: ReportParts) -> String;
    fn save_report(&self, text: &str) -> ReportSave;
    /// Do something. Returns at once; what changed shows in the next read.
    fn act(&self, act: Act);
}

// -------------------------------------------------------------------------
// Speakers
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct SpeakersView {
    /// `None` when the app doesn't look for speakers (`--no-discovery`).
    pub networks: Option<Networks>,
    pub speakers: Vec<SpeakerRow>,
    pub hidden: Vec<SpeakerRow>,
    /// The *Sendspin speakers* switch; `None` where Sendspin is off for
    /// this run.
    pub sendspin: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Networks {
    /// The adapters to choose from (*All networks* comes first by itself).
    pub adapters: Vec<Adapter>,
    /// The chosen adapter's key; `None` = all networks.
    pub chosen: Option<String>,
    pub looking: Looking,
    /// Looking is pinned to one address by a command-line option; the
    /// choice is then not offered.
    pub pinned: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adapter {
    pub key: String,
    /// *Wi-Fi*, *Ethernet 2*.
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Looking {
    /// Looking on these networks.
    On(Vec<NetworkPlace>),
    /// The chosen adapter is off or unplugged.
    AdapterOff { adapter: String },
    /// No network to look on.
    Nowhere,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkPlace {
    /// The adapter: *Wi-Fi*.
    pub adapter: String,
    /// The network's own name, when known: *HomeNet*.
    pub network: Option<String>,
    /// This PC's address there.
    pub address: String,
}

/// One speaker in Settings → Speakers.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerRow {
    pub id: String,
    pub name: String,
    pub kind: SpeakerKind,
    pub presence: Presence,
    pub added_by_address: bool,
    /// Why this app can't play to it, when it can't.
    pub cant_play: Option<CantPlay>,
}

/// Where a speaker is, as its page header says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    /// The current place, sound flowing.
    PlayingNow,
    /// The current place, nothing playing.
    Ready,
    /// Being connected to.
    Connecting,
    /// Found, not the current place.
    Available,
    /// Not found right now; when it was last seen, if known.
    NotFound { last_seen: Option<Seen> },
}

/// A moment, as rows say it: *today 21:10*, *yesterday 22:10*, *3 October*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    Today(LocalTime),
    Yesterday(LocalTime),
    On(Date),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    pub day: u8,
    /// 1–12.
    pub month: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CantPlay {
    /// An Apple TV with AirPlay turned off for this PC (or that only takes
    /// encrypted audio this app can't send).
    AirPlayOff,
}

// -------------------------------------------------------------------------
// A speaker's page
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerView {
    pub row: SpeakerRow,
    /// The volume limit (0–100; 100 = none); `None` where the app can't
    /// limit this speaker.
    pub volume_limit: Option<u8>,
    /// Bass and treble in dB; `None` where tone isn't kept per speaker.
    pub tone: Option<(f32, f32)>,
    /// *Show song titles on Kitchen*; `None` where it can't show them.
    pub song_titles: Option<bool>,
    /// *Show as*: the name in this app; `None` where it can't be renamed.
    pub show_as: Option<String>,
    pub details: Details,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Details {
    /// *Sonos One*.
    pub model: Option<String>,
    pub address: Option<String>,
    /// *UPnP, Sonos*, *AirPlay (RAOP)*, *AirPlay 2, buffered*.
    pub connection: Option<String>,
}

// -------------------------------------------------------------------------
// General
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct GeneralView {
    pub what_plays: Option<WhatPlays>,
    /// *Play calls on the speaker too*.
    pub play_calls: Option<bool>,
    /// *Start when I sign in to Windows*, or why it can't be read
    /// or set.
    pub start_at_sign_in: Option<Result<bool, String>>,
    pub switch_back_after_quiet: bool,
    pub notes_by_clock: bool,
    pub shortcuts: Option<Shortcuts>,
    /// The language chosen; `None` follows Windows. `None` as a whole
    /// where there is only one language.
    pub language: Option<Option<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhatPlays {
    Everything,
    OnlySentApps,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shortcuts {
    pub on: bool,
    /// The keys, as Windows names them: `["Ctrl", "Win", ","]`.
    pub remove: Vec<String>,
    pub add: Vec<String>,
    /// The modifier for 100 ms: `Shift`.
    pub larger: String,
    /// Registration results for remove, add and the larger steps.
    pub status: [crate::platform::KeyStatus; 3],
}

// -------------------------------------------------------------------------
// Music Assistant
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct MaView {
    /// *Let Music Assistant play this PC’s sound*; `None` where Sendspin is off
    /// for this run.
    pub offer: Option<bool>,
    /// Why the offer couldn't start.
    pub failed: Option<String>,
    /// The name this PC appears as in Music Assistant.
    pub appears_as: String,
    /// Step 1: found by this Music Assistant (and its address).
    pub found_by: Option<(String, Option<String>)>,
    /// Step 2: the code to type there, and the seconds it has left.
    pub code: Option<(String, Option<u32>)>,
    /// Step 2 done: paired with these servers.
    pub paired_with: Vec<String>,
    /// Step 3 done: it has played this PC's sound (on that day, if known).
    pub played: Option<Option<Date>>,
    /// Too many wrong codes: pairing with a code is paused.
    pub paused: bool,
    /// The token for another Sendspin server.
    pub token: Option<String>,
}

// -------------------------------------------------------------------------
// Privacy & remote
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct PrivacyView {
    /// `true`: only the speaker you're playing on.
    pub only_playing: bool,
    /// Whether a fresh install's default is shown as recommended.
    pub refused: Option<Vec<Refused>>,
    pub phone: Option<Phone>,
    pub api: Option<Api>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Refused {
    /// The device's name, when it said one.
    pub name: Option<String>,
    pub address: String,
    pub at: LocalTime,
    /// The speaker whose group it belongs to: it can be allowed for it.
    pub group_of: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Phone {
    pub on: bool,
    /// The link, while on.
    pub link: Option<String>,
    /// Why it couldn't start.
    pub problem: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Api {
    pub on: bool,
    pub key: Option<String>,
}

// -------------------------------------------------------------------------
// About
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct AboutView {
    pub version: String,
    pub show_lab: bool,
    pub updates: Updates,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Updates {
    /// *Check for updates once a day*; `None` on a development build.
    pub daily: Option<bool>,
    pub state: UpdateState,
    /// *Update now* downloads and checks the installer here; otherwise the
    /// release page opens.
    pub installs_here: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    DevBuild,
    NeverChecked,
    Checking,
    UpToDate {
        at: Seen,
    },
    Available {
        version: String,
        at: Option<Seen>,
    },
    Failed {
        why: String,
    },
    /// *Update now* is downloading or checking the installer.
    Downloading,
    /// The download is checked: ready to close and install.
    ReadyToInstall {
        version: String,
        speaker: Option<String>,
    },
    /// The download isn't signed by this project.
    NotSigned,
}

// -------------------------------------------------------------------------
// Lab
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct LabView {
    /// Sonos and other UPnP speakers apply a change at once, with a short
    /// skip, instead of gradually.
    pub at_once: Option<bool>,
    /// Frames added or dropped per packet while a change applies (until
    /// the app quits).
    pub catch_up_frames: Option<u32>,
    /// Clock drift, ppm (until the app quits).
    pub drift_ppm: Option<i32>,
    /// Silence pacing, ms per 10 ms of silence (until the app quits).
    pub silence_pace_ms: Option<u32>,
    /// The AirPlay 2 stream: `false` = Automatic, `true` = Realtime.
    pub realtime: Option<bool>,
    pub mfi: Option<bool>,
    pub uncompressed_alac: Option<bool>,
    pub counters: Option<Vec<(String, String)>>,
    /// Values from an earlier version that still give some speakers their
    /// delay, when they aren't the defaults.
    pub carried_over: Option<CarriedOver>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CarriedOver {
    /// The app-wide AirPlay buffer, when not 2000 ms.
    pub airplay_ms: Option<u32>,
    /// The app-wide HomePod delay, when not 200 ms.
    pub homepod_ms: Option<u32>,
    /// Single HomePods on the buffered stream.
    pub homepod_buffered: bool,
    /// The speakers whose sound changes when they go back to the defaults.
    pub speakers: Vec<String>,
}

impl LabView {
    /// Whether a Lab setting differs from its default (what makes the
    /// panel say *Lab settings are on*). Values carried over from an
    /// earlier version are not Lab settings and don't count.
    pub fn differs(&self) -> bool {
        self.at_once == Some(true)
            || self.catch_up_frames.is_some_and(|v| v != LAB_CATCH_UP_DEFAULT)
            || self.drift_ppm.is_some_and(|v| v != 0)
            || self.silence_pace_ms.is_some_and(|v| v != LAB_SILENCE_PACE_DEFAULT)
            || self.realtime == Some(true)
            || self.mfi == Some(true)
            || self.uncompressed_alac == Some(true)
    }
}

/// The defaults of the Lab's numbers.
pub const LAB_CATCH_UP_DEFAULT: u32 = 4;
pub const LAB_SILENCE_PACE_DEFAULT: u32 = 10;

// -------------------------------------------------------------------------
// Help: the two checks
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackCheck {
    /// The speaker (or Music Assistant) the sound should play on; `None`
    /// while the sound is on this PC.
    pub speaker: Option<String>,
    pub playing: Link<Option<String>>,
    pub windows: Link<WindowsSends>,
    pub connected: Link<()>,
    pub taking: Link<Taking>,
    pub volume: Link<Volume>,
    /// Routes nobody has tried on real hardware.
    pub untested: bool,
    /// An Apple TV that is connected and may not pass sound to HomePods.
    pub apple_tv: bool,
    /// Single HomePod or AirPlay speaker on Short delay: *Try Steady
    /// instead* helps.
    pub on_short: bool,
    /// Music Assistant is the speaker and its server isn't connected.
    pub server_away: bool,
}

/// One link of the playback check: `None` while not known (an earlier
/// link is broken).
pub type Link<T> = Option<Result<T, T>>;

#[derive(Clone, Debug, PartialEq)]
pub enum WindowsSends {
    ToUs,
    /// Windows plays on another output.
    Elsewhere {
        output: String,
    },
    /// An app still plays on another output. Found while the rest goes to
    /// the speaker, it doesn't end the check.
    AppElsewhere {
        app: String,
        output: String,
    },
    /// The app can't read the sound Windows sends it and is trying again.
    InputLost,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Taking {
    Yes,
    /// Accepted the connection but doesn't ask for the sound.
    NotAsking {
        two_ways: bool,
    },
    /// A member of its group asked and was refused.
    Refused {
        device: String,
        address: String,
    },
    /// AirPlay speakers and HomePods don't report it.
    CantTell,
    /// Paused with the speaker's own buttons or app while sound is sent.
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volume {
    Level(u8),
    Muted,
    Zero,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveryCheck {
    pub lines: Vec<FindLine>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FindLine {
    /// Looking on this network; found this many speakers there.
    LookingOn { place: NetworkPlace, found: usize },
    /// Also connected to a VPN; offer to look only on `only`.
    Vpn { vpn: NetworkPlace, only: Adapter },
    /// Windows treats the network as private (fine) or public.
    Profile { network: String, private: bool },
    /// The firewall lets speakers reach this PC, or blocks them.
    Firewall { open: bool },
    /// AirPlay discovery couldn't start.
    AirPlaySearchFailed,
    /// The chosen adapter is off.
    AdapterOff { adapter: String },
    /// Searching failed on a network, with the plain cause.
    Failed { adapter: String, why: String },
}

// -------------------------------------------------------------------------
// Dialogs
// -------------------------------------------------------------------------

/// A dialog the app needs open.
#[derive(Clone, Debug, PartialEq)]
pub enum Wanted {
    /// A TV shows a 4-digit code.
    PairingCode(CodeAsk),
    /// An AirPlay speaker asks for its password.
    Password(PasswordAsk),
    /// A Sendspin speaker shows or says a code.
    SendspinCode(SendspinAsk),
    /// Music Assistant is pairing: show its code.
    MaCode(MaCodeShow),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CodeAsk {
    pub id: String,
    pub name: String,
    pub stage: CodeStage,
    pub seconds_left: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeStage {
    /// Asking the TV to show a code.
    Starting,
    /// The TV shows a code: type it.
    Showing,
    /// The code went in; pairing runs.
    Checking,
    /// The last code didn't work; the TV shows a new one.
    Wrong,
    /// The time ran out.
    TimedOut,
    /// The TV didn’t show a code.
    NoAnswer,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PasswordAsk {
    pub id: String,
    pub name: String,
    /// The last password didn't work.
    pub wrong: bool,
    /// What is typed is used once and not saved (a device answering with
    /// another speaker's identity).
    pub once: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SendspinAsk {
    pub id: String,
    pub name: String,
    /// The speaker shows or says a code now (otherwise only a token
    /// works).
    pub live_code: bool,
    pub seconds_left: Option<u32>,
    pub identity_changed: bool,
    /// It was paired but forgot it.
    pub lost: bool,
    pub wrong: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MaCodeShow {
    pub code: String,
    pub this_pc: String,
    pub seconds_left: Option<u32>,
}

/// What *Find* found at an address.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Probe {
    #[default]
    Idle,
    Checking {
        address: String,
    },
    Found {
        address: String,
        ways: Vec<Found>,
    },
    Nothing {
        address: String,
    },
    /// The address or what was typed can't be used (today's validation
    /// words, already a sentence).
    Invalid(String),
    /// Added; the dialog can close.
    Added,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub name: String,
    pub kind: SpeakerKind,
    pub delay_ms: u32,
    /// The way of connecting, for the choice (*Sonos*, *AirPlay*).
    pub way: Way,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    Upnp,
    AirPlay,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToneView {
    pub name: String,
    pub eq: EqSettings,
    /// The automatic headroom in dB (negative), when it applies.
    pub headroom_db: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportSave {
    Opened,
    SavedOnly,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportParts {
    pub diagnostics: bool,
    pub speakers: bool,
    pub versions: bool,
    pub log: bool,
}

impl Default for ReportParts {
    fn default() -> Self {
        ReportParts { diagnostics: true, speakers: true, versions: true, log: false }
    }
}

// -------------------------------------------------------------------------
// What the window does
// -------------------------------------------------------------------------

/// Every change and act the window and its dialogs ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    // Speakers
    LookOn(Option<String>),
    SearchAgain,
    ShowSpeaker(String),
    HideSpeaker(String),
    SendspinSpeakers(bool),
    // A speaker's page
    VolumeLimit(String, u8),
    Tone(String, ToneAct),
    SongTitles(String, bool),
    PairAgain(String),
    ClearSpeakerCredentials(String),
    ShowAs(String, String),
    // General
    WhatPlays(WhatPlays),
    PlayCalls(bool),
    StartAtSignIn(bool),
    SwitchBackAfterQuiet(bool),
    NotesByClock(bool),
    ShowLab(bool),
    Shortcuts(bool),
    RecordShortcuts(bool),
    ShortcutKeys(crate::platform::DelayKeys),
    Language(Option<String>),
    // Music Assistant
    OfferToMa(bool),
    Unpair(String),
    AllowPairingAgain,
    AppearsAs(String),
    OpenMusicAssistant,
    CancelMaPairing,
    // Privacy & remote
    OnlyPlaying(bool),
    Allow {
        address: String,
        speaker: String,
    },
    Phone(bool),
    NewLink,
    Api(bool),
    NewKey,
    // Help
    OpenLogFolder,
    Reconnect,
    TryAirPlay,
    TrySteady,
    Unmute,
    SetVolume(u8),
    SendToUs,
    LookOnlyOn(String),
    OpenNetworkSettings,
    AllowThroughFirewall,
    OpenApiHelp,
    // About
    DailyUpdateCheck(bool),
    CheckNow,
    UpdateNow,
    Install,
    CancelInstall,
    WhatsNew,
    SkipVersion,
    Donate,
    LicenceAndSource,
    // Lab
    Lab(LabAct),
    // Dialogs
    SubmitCode {
        id: String,
        code: String,
    },
    NewCode(String),
    CancelPairing(String),
    /// A password typed in the dialog; `connect`: connect with it now
    /// (otherwise it is only kept for next time).
    SubmitPassword {
        id: String,
        password: String,
        connect: bool,
    },
    CancelPassword(String),
    SubmitSendspin {
        id: String,
        code: String,
    },
    CancelSendspin(String),
    Find {
        address: String,
        name: String,
    },
    AddFound {
        way: Way,
        name: String,
    },
    AddByHand {
        airplay: bool,
        host: String,
        port: Option<u16>,
        description: String,
        name: String,
    },
    ResetProbe,
    OpenIssue(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ToneAct {
    Bass(f32),
    Treble(f32),
    Band(usize, f32),
    Preamp(f32),
    Preset(usize),
    Flat,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LabAct {
    AtOnce(bool),
    CatchUpFrames(u32),
    DriftPpm(i32),
    SilencePace(u32),
    Realtime(bool),
    Mfi(bool),
    UncompressedAlac(bool),
    ResetAll,
    /// Return the values carried over from an earlier version to the
    /// defaults.
    ResetCarriedOver,
}
