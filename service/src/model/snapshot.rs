//! The snapshot: everything the everyday surfaces draw at one moment, with
//! their words, built from the engine's [`Facts`] by one pure function.

use super::command::{Command, FirstRunAct, Open, Refusal, VolumeOwner};
use super::delay::{delay_control, panel_delay, DelayControl, DelayView, PanelDelay};
use super::facts::{Facts, FirstRun, Level, Mode, Remembered, SpeakerFacts};
use super::places::{place_list, PlaceList};
use super::rules::volume_owner;
use super::state::{
    Apps, Badge, ConnectStage, Dropout, FellBackCause, Looking, NowState, Place, Return, Secret,
    SoundAt, SpeakerId, StayedBecause,
};
use super::surfaces::{
    cli_status, device_name, footer, notice, notification, short_status, status_line, tray, Footer,
    Notice, Notification, Tray,
};
use super::text::{t, Lang};

/// A button: its words and the command it sends.
#[derive(Clone, Debug, PartialEq)]
pub struct Button {
    pub label: String,
    /// What a screen reader says, when it differs from the label.
    pub spoken: Option<String>,
    pub command: Command,
    pub style: ButtonStyle,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonStyle {
    /// The one emphasised button of a card.
    Primary,
    Secondary,
    /// Drawn as a link.
    Link,
    /// Drawn as an icon (↺, ×); the label is its name.
    Icon,
}

impl Button {
    pub fn new(label: String, command: Command) -> Self {
        Button { label, spoken: None, command, style: ButtonStyle::Secondary, enabled: true }
    }
    pub fn primary(label: String, command: Command) -> Self {
        Button { style: ButtonStyle::Primary, ..Button::new(label, command) }
    }
    pub fn link(label: String, command: Command) -> Self {
        Button { style: ButtonStyle::Link, ..Button::new(label, command) }
    }
}

/// An item of a menu (the tray's, a row's, the footer's).
#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    pub label: String,
    pub command: Command,
    pub enabled: bool,
    pub checked: bool,
}

impl MenuItem {
    pub fn new(label: String, command: Command) -> Self {
        MenuItem { label, command, enabled: true, checked: false }
    }
}

/// How a line of text is marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// An ⓘ: advice.
    Info,
    /// Amber, with ⚠.
    Warning,
    /// Red, with ✖.
    Error,
    /// A spinner: something is in progress.
    Busy,
}

/// The icon before the Now card's title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleIcon {
    None,
    Warning,
    Error,
}

/// Everything the everyday surfaces draw at one moment. Immutable; shared
/// as `Arc<Snapshot>`.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Grows with every new snapshot.
    pub version: u64,
    pub lang: Lang,
    pub state: NowState,
    /// The top of the panel.
    pub top: Top,
    /// The state as a card with a second line and the whole delay control:
    /// what the phone remote shows.
    pub now: NowCard,
    /// First run, while it shows in the panel (in place of the top's
    /// sentence and detail).
    pub first_run: Option<FirstRunWords>,
    /// The list of places.
    pub list: PlaceList,
    /// The notice line, at most one.
    pub notice: Option<Notice>,
    pub footer: Footer,
    pub tray: Tray,
    /// Windows' name for the app's sound output; `None` while it is missing.
    pub device_name: Option<String>,
    /// The state's short sentence (the tooltip after "Stream To Speaker –").
    pub short_status: String,
    /// The one-line status for the phone remote and `status` in scripts.
    pub status: String,
    /// `stream-to-speaker status`: the status with kind, delay and volume.
    pub cli_status: String,
    /// The delay numbers of the speaker being played on, while its delay
    /// control shows.
    pub delay: Option<DelayView>,
    /// The latest notification; show it if its `seq` is new.
    pub notification: Option<Notification>,
    /// The problem badge, for the tray and the Settings window's taskbar
    /// button.
    pub badge: Badge,
    /// The names of every speaker the app knows.
    pub names: Vec<(SpeakerId, String)>,
}

impl Snapshot {
    /// A speaker's name, from the list the engine supplied.
    pub fn speaker_name<'a>(&'a self, id: &'a SpeakerId) -> &'a str {
        self.names.iter().find(|(i, _)| i == id).map(|(_, n)| n.as_str()).unwrap_or(id.as_str())
    }

    /// Why a delay command does nothing right now (for when [`Self::delay`]
    /// is `None`).
    pub fn delay_refusal(&self) -> Refusal {
        match self.state {
            NowState::MaPlaying { .. } | NowState::MaWaiting => Refusal::MusicAssistantDelay,
            _ => Refusal::NoDelayHere,
        }
    }
}

/// The top of the panel: the sentence, then in a state the person must know
/// about a detail and buttons, then the volume row and a third line. In the
/// everyday states it is three lines of the same height, so choosing a
/// place never moves the list.
#[derive(Clone, Debug, PartialEq)]
pub struct Top {
    pub icon: TitleIcon,
    pub sentence: String,
    /// The level meter at the sentence's right end, while this PC sends
    /// real sound.
    pub meter: bool,
    /// At the sentence's right end: *Cancel* while connecting.
    pub corner: Option<Button>,
    /// Only in a state the person must know about.
    pub detail: Option<String>,
    pub actions: Vec<Button>,
    /// A line under the buttons.
    pub note: Option<String>,
    pub volume: Option<VolumeRow>,
    pub third: Option<Third>,
    /// What the live region announces when it changes.
    pub announce: String,
}

/// The top's third line, which depends on the place.
#[derive(Clone, Debug, PartialEq)]
pub enum Third {
    Text(String),
    /// A speaker's delay: the block, always open.
    Delay(PanelDelay),
}

/// First run's words, by step.
#[derive(Clone, Debug, PartialEq)]
pub struct FirstRunWords {
    pub sentence: String,
    pub detail: String,
    /// ×, at the sentence's right end.
    pub close: Button,
    pub actions: Vec<Button>,
    pub link: Option<Button>,
    pub switch: Option<Switch>,
    /// The list's last row while speakers are still being found.
    pub looking_more: Option<String>,
    /// In the footer, beside *Speaker missing?*.
    pub not_now: Option<Button>,
}

/// A switch, with the command that flips it.
#[derive(Clone, Debug, PartialEq)]
pub struct Switch {
    pub label: String,
    pub on: bool,
    pub command: Command,
}

/// The state as a card: title, second line, buttons, volume and the whole
/// delay control. The phone remote shows this.
#[derive(Clone, Debug, PartialEq)]
pub struct NowCard {
    pub icon: TitleIcon,
    pub title: String,
    /// The level meter at the end of the title (only while this PC sends
    /// real sound).
    pub meter: bool,
    pub line2: String,
    /// At most two; the first may be primary.
    pub actions: Vec<Button>,
    /// A line under the buttons.
    pub note: Option<String>,
    pub volume: Option<VolumeRow>,
    pub delay: Option<DelayControl>,
    /// What the card's live region announces when it changes.
    pub announce: String,
}

/// The volume row: mute, slider, number.
#[derive(Clone, Debug, PartialEq)]
pub struct VolumeRow {
    pub owner: VolumeOwner,
    pub level: u8,
    pub muted: bool,
    /// "32", or "Muted".
    pub number: String,
    /// The speaker's volume limit, a tick on the track.
    pub limit: Option<u8>,
    /// Where the slider isn't the place's own volume, what it is (the
    /// panel's tooltip; the phone's label).
    pub tooltip: Option<String>,
    /// More about it, where a surface has room (the phone).
    pub caption: Option<String>,
    /// The slider's name for screen readers.
    pub spoken: String,
    /// The mute button, named for what it does.
    pub mute: Button,
}

/// Builds the snapshot for `facts`.
pub fn build(facts: &Facts, version: u64) -> Snapshot {
    let lang = facts.lang;
    let names: Vec<(SpeakerId, String)> =
        facts.speakers.iter().map(|s| (s.id.clone(), s.name.clone())).collect();
    let state = facts.state.clone();
    let badge = state.badge(facts.seen);
    let delay = delay_view(facts);
    let now = now_card(facts, delay.as_ref());
    let list = place_list(facts);
    let short = short_status(facts);
    let status = status_line(facts);
    let tray = tray(facts, &list, &short, badge);
    Snapshot {
        version,
        lang,
        cli_status: cli_status(facts, &status, delay.as_ref(), &now),
        top: top(facts, &now, delay.as_ref()),
        first_run: first_run(facts),
        now,
        notice: notice(facts, badge),
        footer: footer(facts),
        device_name: device_name(facts),
        short_status: short,
        status,
        notification: notification(facts),
        tray,
        list,
        delay,
        badge,
        names,
        state,
    }
}

pub(crate) fn find<'a>(facts: &'a Facts, id: &SpeakerId) -> Option<&'a SpeakerFacts> {
    facts.speakers.iter().find(|s| &s.id == id)
}

/// A speaker's name for sentences.
pub(crate) fn name_of<'a>(facts: &'a Facts, id: &'a SpeakerId) -> &'a str {
    match find(facts, id) {
        Some(s) => &s.name,
        None => {
            debug_assert!(false, "the state names a speaker missing from the list: {id:?}");
            id.as_str()
        }
    }
}

fn delay_view(facts: &Facts) -> Option<DelayView> {
    if !facts.state.shows_delay() {
        return None;
    }
    let id = facts.state.speaker()?;
    let control = facts.delay.as_ref().filter(|d| &d.speaker == id)?;
    let speaker = find(facts, id)?;
    Some(DelayView {
        speaker: id.clone(),
        name: speaker.name.clone(),
        delay: speaker.delay?,
        range: control.range,
        applies: control.applies,
        progress: control.progress,
        steps: facts.delay_steps,
        choice: control.choice,
    })
}

/// "Spotify is playing", "Spotify and Edge are playing", "Sound is playing".
pub fn apps_playing(lang: Lang, apps: &Apps) -> String {
    if apps.0.is_empty() {
        return t!(lang, AppsUnknown);
    }
    t!(lang, AppsPlaying, apps = &apps_list(lang, apps), count = apps.0.len())
}

/// "Spotify", "Spotify and Edge", "Spotify and 2 other apps".
pub fn apps_list(lang: Lang, apps: &Apps) -> String {
    match apps.0.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => t!(lang, AppsTwo, first = first, second = second),
        [first, rest @ ..] => t!(lang, AppsMany, first = first, others = rest.len()),
    }
}

fn title_icon(state: &NowState) -> TitleIcon {
    use NowState::*;
    match state {
        OutputMissing => TitleIcon::Error,
        NotTaking { .. } | NotResponding { .. } | FellBack { .. } | Taken { .. }
        | NeedsSecret { .. } | WindowsRefused { .. } | MaAway { .. } => TitleIcon::Warning,
        LookingFor { looking: Looking::NotFound(_), .. } => TitleIcon::Warning,
        _ => TitleIcon::None,
    }
}

fn now_card(facts: &Facts, view: Option<&DelayView>) -> NowCard {
    use NowState::*;
    let lang = facts.lang;
    let state = &facts.state;
    let pc = &facts.pc.output;
    let name = |id: &SpeakerId| name_of(facts, id).to_string();
    let stay = || Button::new(t!(lang, StayOnPc), Command::StayOnThisPc);
    let play = |id: &SpeakerId| {
        Button::primary(t!(lang, PlayOnSpeaker, speaker = &name(id)), Command::Play(Place::Speaker(id.clone())))
    };
    let device = |id: &SpeakerId| t!(lang, DeviceForPlace, place = &name(id));
    let muted = facts.volume.place.map(|l| l.muted).unwrap_or(false);
    let playing_line = |id: &SpeakerId, apps: &Apps| {
        let playing = apps_playing(lang, apps);
        if muted {
            t!(lang, PlayingMuted, playing = &playing, speaker = &name(id))
        } else {
            playing
        }
    };

    let mut note = None;
    let (title, line2, actions): (String, String, Vec<Button>) = match state {
        OnThisPc => {
            let line = match facts.mode {
                Mode::Everything => t!(lang, OnPcLine),
                Mode::OnlySentApps { .. } => t!(lang, OnPcLineSentApps),
            };
            (t!(lang, OnPcTitle), line, vec![])
        }
        Connecting { target, from, .. } => {
            let speaker = name(target);
            let line = match from {
                SoundAt::ThisPc => t!(lang, ConnectingFromPc, speaker = &speaker),
                SoundAt::Speaker(id) => {
                    t!(lang, ConnectingFromSpeaker, from = &name(id), speaker = &speaker)
                }
                SoundAt::MusicAssistant => t!(lang, ConnectingFromMa, speaker = &speaker),
            };
            (
                t!(lang, ConnectingTitle, speaker = &speaker),
                line,
                vec![Button::new(t!(lang, Cancel), Command::CancelConnect)],
            )
        }
        Playing { speaker, apps } => {
            (t!(lang, PlayingTitle, speaker = &name(speaker)), playing_line(speaker, apps), vec![])
        }
        ApplyingDelay { speaker, from_ms, to_ms } => {
            let n = name(speaker);
            let line = if *to_ms == 0 {
                t!(lang, ApplyingOwn, speaker = &n)
            } else if to_ms < from_ms {
                t!(lang, ApplyingRemove, ms = (from_ms - to_ms).unsigned_abs())
            } else {
                t!(lang, ApplyingAdd, ms = (to_ms - from_ms).unsigned_abs())
            };
            (t!(lang, PlayingTitle, speaker = &n), line, vec![])
        }
        Ready { speaker } => (t!(lang, ReadyTitle, speaker = &name(speaker)), t!(lang, ReadyLine), vec![]),
        ReadyPlayingElsewhere { speaker, app } => (
            t!(lang, ReadyTitle, speaker = &name(speaker)),
            t!(lang, ReadyElsewhereLine, app = app, output = &pc.friendly),
            vec![Button::new(t!(lang, OpenVolumeMixer), Command::Open(Open::VolumeMixer))],
        ),
        NotTaking { speaker, since } => {
            let n = name(speaker);
            (
                t!(lang, NotTakingTitle, speaker = &n),
                t!(lang, NotTakingLine, speaker = &n, time = &lang.time(*since)),
                vec![
                    Button::primary(t!(lang, Reconnect), Command::Reconnect(speaker.clone())),
                    Button::new(t!(lang, WhatElse), Command::Open(Open::CantHear(Some(speaker.clone())))),
                ],
            )
        }
        NotResponding { speaker, dropout } => {
            let n = name(speaker);
            let (line, button) = match dropout {
                Dropout::QuickTries { attempt, of, then: super::state::AfterTries::BackToPc } => (
                    t!(
                        lang,
                        RetryingLine,
                        attempt = *attempt,
                        tries = *of,
                        speaker = &n,
                        seconds = super::rules::FALL_BACK_AFTER_MS / 1000
                    ),
                    t!(lang, PlayOnPcNow),
                ),
                Dropout::QuickTries { attempt, of, then: super::state::AfterTries::StaySilent } => (
                    t!(lang, RetryingSilentLine, attempt = *attempt, tries = *of, speaker = &n),
                    t!(lang, PlayOnPc),
                ),
                Dropout::KeepTrying { every_secs } => (
                    t!(lang, KeepTryingLine, seconds = *every_secs, speaker = &n),
                    t!(lang, PlayOnPc),
                ),
            };
            (
                t!(lang, NotRespondingTitle, speaker = &n),
                line,
                vec![Button::new(button, Command::Play(Place::ThisPc))],
            )
        }
        FellBack { speaker, at, cause, returns } => {
            let n = name(speaker);
            let time = lang.time(*at);
            let (line, first) = match cause {
                FellBackCause::StoppedResponding => (
                    t!(lang, FellBackLine, speaker = &n, time = &time, output = &pc.friendly),
                    Button::primary(
                        t!(lang, TrySpeakerAgain, speaker = &n),
                        Command::Play(Place::Speaker(speaker.clone())),
                    ),
                ),
                FellBackCause::RefusedPairing => (
                    t!(lang, RefusedPairingLine, speaker = &n, time = &time, output = &pc.friendly),
                    Button::primary(t!(lang, PairAgain), Command::Open(Open::Pair(speaker.clone()))),
                ),
            };
            note = Some(match returns {
                Return::ByItself => t!(lang, ReturnsByItself, speaker = &n),
                Return::WillAsk => t!(lang, AskedWhenBack, speaker = &n),
            });
            (t!(lang, BackOnPcTitle), line, vec![first, stay()])
        }
        SpeakerBack { speaker, because } => {
            let n = name(speaker);
            let line = match because {
                StayedBecause::PlayedAfterDropout => t!(lang, StayedAfterDropout, speaker = &n),
                StayedBecause::PlayingWhenFound => t!(lang, StayedPlayingWhenFound, speaker = &n),
                StayedBecause::SetNotToReturn => t!(lang, StayedSetNotToReturn, speaker = &n),
            };
            (t!(lang, SpeakerBackTitle, speaker = &n), line, vec![play(speaker), stay()])
        }
        Taken { speaker, at } => {
            let n = name(speaker);
            (
                t!(lang, BackOnPcTitle),
                t!(lang, TakenLine, speaker = &n, time = &lang.time(*at), output = &pc.friendly),
                vec![
                    Button::primary(
                        t!(lang, PlayOnSpeakerAgain, speaker = &n),
                        Command::PlayTakingOver(speaker.clone()),
                    ),
                    stay(),
                ],
            )
        }
        NeedsSecret { speaker, secret } => {
            let n = name(speaker);
            let pair = Command::Open(Open::Pair(speaker.clone()));
            match secret {
                Secret::Code => (
                    t!(lang, NeedsPairingTitle, speaker = &n),
                    t!(lang, NeedsPairingLine, speaker = &n),
                    vec![Button::primary(t!(lang, PairEllipsis), pair), stay()],
                ),
                Secret::Password => (
                    t!(lang, NeedsPasswordTitle, speaker = &n),
                    t!(lang, NeedsPasswordLine, speaker = &n),
                    vec![Button::primary(t!(lang, EnterPasswordEllipsis), pair), stay()],
                ),
            }
        }
        LookingFor { speaker, looking } => {
            let n = name(speaker);
            match looking {
                Looking::Searching => (t!(lang, OnPcTitle), t!(lang, LookingLine, speaker = &n), vec![]),
                Looking::NotFound(returns) => (
                    t!(lang, CantFindTitle, speaker = &n),
                    match returns {
                        Return::ByItself => t!(lang, CantFindByItself, speaker = &n),
                        Return::WillAsk => t!(lang, CantFindWillAsk, speaker = &n),
                    },
                    vec![
                        stay(),
                        Button::new(t!(lang, SpeakerMissing), Command::Open(Open::SpeakerMissing)),
                    ],
                ),
            }
        }
        WindowsSwitched { speaker, at } => (
            t!(lang, OnOutputTitle, output = &pc.name),
            t!(
                lang,
                WindowsSwitchedLine,
                output = &pc.friendly,
                speaker = &name(speaker),
                time = &lang.time(*at)
            ),
            vec![Button::new(
                t!(lang, PlayOnSpeaker, speaker = &name(speaker)),
                Command::Play(Place::Speaker(speaker.clone())),
            )],
        ),
        ChooseSpeaker => (t!(lang, ChooseTitle), t!(lang, ChooseLine, output = &pc.friendly), vec![]),
        WindowsRefused { speaker } => (
            t!(lang, RefusedTitle, speaker = &name(speaker)),
            t!(lang, RefusedLine, output = &pc.friendly, device = &device(speaker)),
            vec![Button::primary(t!(lang, OpenSoundSettings), Command::Open(Open::SoundSettings))],
        ),
        MaPlaying { apps, room } => {
            let playing = apps_playing(lang, apps);
            let line = match room {
                Some(room) => t!(lang, MaRoomLine, playing = &playing, room = room),
                None => t!(lang, MaChoosesLine, playing = &playing),
            };
            (t!(lang, MaPlayingTitle), line, vec![])
        }
        MaWaiting => (
            t!(lang, MaWaitingTitle),
            t!(lang, MaWaitingLine),
            vec![
                Button::new(t!(lang, How), Command::Open(Open::MusicAssistantHowTo)),
                Button::new(t!(lang, PlayOnPc), Command::Play(Place::ThisPc)),
            ],
        ),
        MaAway { returns } => (
            t!(lang, MaAwayTitle),
            match returns {
                Return::ByItself => t!(lang, MaAwayByItself),
                Return::WillAsk => t!(lang, MaAwayWillAsk),
            },
            vec![
                stay(),
                Button::new(t!(lang, MaSettings), Command::Open(Open::MusicAssistantSettings)),
            ],
        ),
        MaBack => (
            t!(lang, MaBackTitle),
            t!(lang, MaBackLine),
            vec![
                Button::primary(t!(lang, PlayThroughMa), Command::Play(Place::MusicAssistant)),
                stay(),
            ],
        ),
        OutputMissing => (
            t!(lang, OutputMissingTitle),
            t!(lang, OutputMissingLine),
            vec![Button::primary(t!(lang, Repair), Command::Open(Open::Repair))],
        ),
        PlayingSentApps { speaker, apps } => {
            let n = name(speaker);
            let title = if apps.0.is_empty() {
                t!(lang, PlayingTitle, speaker = &n)
            } else {
                t!(lang, SentAppsTitle, apps = &apps_list(lang, apps), speaker = &n)
            };
            (
                title,
                t!(lang, SentAppsLine, speaker = &n),
                vec![Button::link(t!(lang, ChooseApps), Command::Open(Open::VolumeMixer))],
            )
        }
        ReadyNoAppSent { speaker } => (
            t!(lang, ReadyTitle, speaker = &name(speaker)),
            t!(lang, NoAppSentLine, device = &device(speaker)),
            vec![Button::new(t!(lang, OpenVolumeMixer), Command::Open(Open::VolumeMixer))],
        ),
    };

    let volume = volume_owner(state, &facts.pc.output.id).and_then(|owner| volume_row(facts, owner));
    let delay = view.and_then(|v| {
        let control = facts.delay.as_ref()?;
        Some(delay_control(lang, &v.name, v.delay, control, facts.delay_steps, facts.delay_hint_retired))
    });
    let announce = format!("{} {}", lang.sentence(&title), lang.sentence(&line2));
    NowCard {
        icon: title_icon(state),
        meter: facts.sending_sound && state.may_show_meter(),
        title,
        line2,
        actions,
        note,
        volume,
        delay,
        announce,
    }
}

fn volume_row(facts: &Facts, owner: VolumeOwner) -> Option<VolumeRow> {
    let lang = facts.lang;
    let level: Level = match owner {
        VolumeOwner::PcOutput(_) => facts.volume.pc?,
        VolumeOwner::Speaker(_) | VolumeOwner::MusicAssistant => facts.volume.place?,
    };
    let ma = owner == VolumeOwner::MusicAssistant;
    let tooltip = ma.then(|| t!(lang, MaLevelLabel));
    Some(VolumeRow {
        level: level.value,
        muted: level.muted,
        number: if level.muted { t!(lang, Muted) } else { level.value.to_string() },
        limit: level.limit,
        spoken: tooltip.clone().unwrap_or_else(|| t!(lang, VolumeName)),
        caption: ma.then(|| t!(lang, MaLevelCaption)),
        tooltip,
        mute: Button {
            label: if level.muted { t!(lang, Unmute) } else { t!(lang, Mute) },
            spoken: None,
            command: Command::SetMuted { owner: owner.clone(), muted: !level.muted },
            style: ButtonStyle::Icon,
            enabled: true,
        },
        owner,
    })
}

/// The top of the panel. Everyday states keep three lines of fixed height;
/// every other state shows the card's second line as the detail, with its
/// buttons, except the two the footer already has.
fn top(facts: &Facts, now: &NowCard, view: Option<&DelayView>) -> Top {
    use NowState::*;
    let lang = facts.lang;
    let state = &facts.state;
    let everyday = state.is_everyday();
    let connecting = matches!(state, Connecting { .. });
    let in_footer = |b: &Button| {
        matches!(b.command, Command::Open(Open::CantHear(_)) | Command::Open(Open::SpeakerMissing))
    };
    let third = if facts.first_run.is_some() {
        None
    } else {
        match state {
            Connecting { .. } => Some(Third::Text(t!(lang, TopConnecting))),
            MaPlaying { room: Some(room), .. } => Some(Third::Text(t!(lang, TopMaRoom, room = room))),
            MaPlaying { room: None, .. } => Some(Third::Text(t!(lang, TopMaChooses))),
            _ => match (view, facts.delay.as_ref()) {
                (Some(v), Some(control)) => {
                    let reconnecting = match state {
                        ApplyingDelay { from_ms, to_ms, .. } => Some((*from_ms, *to_ms)),
                        _ => None,
                    };
                    Some(Third::Delay(panel_delay(lang, v, control, reconnecting)))
                }
                _ if state.current_place() == Some(Place::ThisPc) => {
                    let first_time = *state == OnThisPc && facts.remembered == Remembered::Nothing;
                    Some(Third::Text(if first_time {
                        t!(lang, TopPickSpeaker)
                    } else {
                        facts.pc.output.friendly.clone()
                    }))
                }
                _ => None,
            },
        }
    };
    let detail = (!everyday).then(|| now.line2.clone());
    let announce = match &detail {
        Some(d) => format!("{} {}", lang.sentence(&now.title), lang.sentence(d)),
        None => lang.sentence(&now.title),
    };
    Top {
        icon: now.icon,
        sentence: now.title.clone(),
        meter: now.meter && !connecting,
        corner: connecting.then(|| Button::new(t!(lang, Cancel), Command::CancelConnect)),
        detail,
        actions: if everyday { vec![] } else { now.actions.iter().filter(|b| !in_footer(b)).cloned().collect() },
        note: if everyday { None } else { now.note.clone() },
        volume: now.volume.clone(),
        third,
        announce,
    }
}

/// First run's words for its step.
fn first_run(facts: &Facts) -> Option<FirstRunWords> {
    let lang = facts.lang;
    let step = facts.first_run.as_ref()?;
    let act = |a| Command::FirstRun(a);
    let mut words = FirstRunWords {
        sentence: String::new(),
        detail: String::new(),
        close: Button { style: ButtonStyle::Icon, ..Button::new(t!(lang, Close), act(FirstRunAct::Close)) },
        actions: vec![],
        link: None,
        switch: None,
        looking_more: None,
        not_now: None,
    };
    match step {
        FirstRun::Pick => {
            words.sentence = t!(lang, FirstPickTitle);
            words.detail = t!(lang, FirstPickLine);
            words.looking_more = Some(t!(lang, FirstLookingMore));
            words.not_now = Some(Button::link(t!(lang, NotNow), act(FirstRunAct::NotNow)));
        }
        FirstRun::Hear { speaker } => {
            let n = name_of(facts, speaker);
            words.sentence = t!(lang, FirstHearTitle, speaker = n);
            words.detail = t!(lang, FirstHearLine, speaker = n);
            words.actions = vec![
                Button::primary(t!(lang, HeardIt), act(FirstRunAct::Heard)),
                Button::new(t!(lang, DidntHear), Command::Open(Open::CantHear(Some(speaker.clone())))),
            ];
            words.link = Some(Button::link(t!(lang, PlayItAgain), act(FirstRunAct::PlayChimeAgain)));
        }
        FirstRun::FindIt { speaker, start_at_sign_in } => {
            words.sentence = t!(lang, FirstFindTitle);
            words.detail = t!(lang, FirstFindLine);
            words.switch = Some(Switch {
                label: t!(lang, FirstStartSwitch, speaker = name_of(facts, speaker)),
                on: *start_at_sign_in,
                command: act(FirstRunAct::StartAtSignIn(!start_at_sign_in)),
            });
            words.actions = vec![Button::primary(t!(lang, Done), act(FirstRunAct::Done))];
        }
    }
    Some(words)
}

/// The row's stage words while connecting.
pub(crate) fn stage_text(lang: Lang, stage: ConnectStage, speaker: &str) -> String {
    match stage {
        ConnectStage::Finding => t!(lang, RowFinding, speaker = speaker),
        ConnectStage::Asking => t!(lang, RowAsking, speaker = speaker),
        ConnectStage::Waiting => t!(lang, RowWaiting, speaker = speaker),
        ConnectStage::Pairing => t!(lang, RowPairing),
    }
}
