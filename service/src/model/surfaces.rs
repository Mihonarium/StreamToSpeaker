//! The state's words on the other surfaces: the tray icon, its tooltip and
//! menu, the footer, the notice line, notifications, Windows' name for the
//! sound output, and the one-line status of the phone remote and the
//! command line.

use super::command::{Command, NoticeKind, Open};
use super::delay::{change_text, figure, figure_text, DelayView};
use super::facts::{Applies, Facts, NotificationKind, Remembered};
use super::places::{kind_text, PlaceList};
use super::snapshot::{find, name_of, Button, ButtonStyle, MenuItem, NowCard, Tone};
use super::state::{Arcs, Badge, FellBackCause, Looking, NowState, Place, Secret, SpeakerId};
use super::text::{t, Lang};

/// The tray menu shows at most this many places, then *More speakers…*.
pub const TRAY_PLACES: usize = 8;

/// The tray icon, its tooltip and its menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Tray {
    pub arcs: Arcs,
    pub badge: Badge,
    pub tooltip: String,
    pub menu: Vec<MenuEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MenuEntry {
    /// Greyed text.
    Label(String),
    Item(MenuItem),
    Separator,
}

/// The panel's footer. Help and About are pages of Settings; Quit is in
/// the tray icon's menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Footer {
    /// Only while the place is a speaker or Music Assistant.
    pub cant_hear: Option<Button>,
    pub speaker_missing: Button,
    /// The gear.
    pub settings: Button,
}

/// The notice line.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub kind: NoticeKind,
    /// The lead, drawn strong unless the tone is plain.
    pub text: String,
    /// What follows the lead, drawn normally.
    pub detail: Option<String>,
    pub tone: Tone,
    pub actions: Vec<Button>,
    /// ×, for the notices that can be dismissed.
    pub dismiss: Option<Button>,
}

/// A Windows notification. Clicking its body opens the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub seq: u64,
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
    pub buttons: Vec<Button>,
    /// A button that only closes the notification ("Later").
    pub close: Option<String>,
    /// The speaker it is about: a newer notification about the same
    /// speaker replaces the older (a speaker back replaces it leaving).
    pub tag: Option<SpeakerId>,
}

/// The state's short sentence: the tooltip after "Stream To Speaker –".
pub fn short_status(facts: &Facts) -> String {
    use NowState::*;
    let lang = facts.lang;
    let name = |id: &SpeakerId| name_of(facts, id).to_string();
    match &facts.state {
        OnThisPc => t!(lang, OnPcTitle),
        Connecting { target, .. } => t!(lang, TipConnecting, speaker = &name(target)),
        Playing { speaker, .. } => t!(lang, PlayingTitle, speaker = &name(speaker)),
        ApplyingDelay { speaker, .. } => t!(lang, TipApplying, speaker = &name(speaker)),
        Ready { speaker } | ReadyPlayingElsewhere { speaker, .. } | ReadyNoAppSent { speaker } => {
            t!(lang, ReadyTitle, speaker = &name(speaker))
        }
        NotTaking { speaker, .. } => t!(lang, NotTakingTitle, speaker = &name(speaker)),
        NotResponding { speaker, .. } => t!(lang, NotRespondingTitle, speaker = &name(speaker)),
        FellBack { speaker, cause: FellBackCause::StoppedResponding, .. } => {
            t!(lang, TipFellBack, speaker = &name(speaker))
        }
        FellBack { speaker, cause: FellBackCause::RefusedPairing, .. } => {
            t!(lang, TipRefusedPairing, speaker = &name(speaker))
        }
        SpeakerBack { speaker, .. } => t!(lang, TipSpeakerBack, speaker = &name(speaker)),
        Taken { speaker, .. } => t!(lang, TipTaken, speaker = &name(speaker)),
        NeedsSecret { speaker, secret: Secret::Code } => {
            t!(lang, NeedsPairingTitle, speaker = &name(speaker))
        }
        NeedsSecret { speaker, secret: Secret::Password } => {
            t!(lang, NeedsPasswordTitle, speaker = &name(speaker))
        }
        LookingFor { speaker, looking: Looking::Searching } => {
            t!(lang, TipLooking, speaker = &name(speaker))
        }
        LookingFor { speaker, looking: Looking::NotFound(_) } => {
            t!(lang, TipCantFind, speaker = &name(speaker))
        }
        WindowsSwitched { .. } => t!(lang, OnOutputTitle, output = &facts.pc.output.name),
        ChooseSpeaker => t!(lang, TipChoose),
        WindowsRefused { speaker } => t!(lang, RefusedTitle, speaker = &name(speaker)),
        MaPlaying { .. } => t!(lang, MaPlayingTitle),
        MaWaiting => t!(lang, TipMaWaiting),
        MaAway { .. } => t!(lang, MaAwayTitle),
        MaBack => t!(lang, TipMaBack),
        OutputMissing => t!(lang, TipOutputMissing),
        PlayingSentApps { speaker, apps } => sent_apps_title(lang, &name(speaker), apps),
    }
}

fn sent_apps_title(lang: Lang, speaker: &str, apps: &super::state::Apps) -> String {
    if apps.0.is_empty() {
        t!(lang, PlayingTitle, speaker = speaker)
    } else {
        t!(lang, SentAppsTitle, apps = &super::snapshot::apps_list(lang, apps), speaker = speaker)
    }
}

/// The phone remote's and `status`'s one line.
pub fn status_line(facts: &Facts) -> String {
    use NowState::*;
    let lang = facts.lang;
    let name = |id: &SpeakerId| name_of(facts, id).to_string();
    match &facts.state {
        Connecting { target, .. } => t!(lang, ConnectingTitle, speaker = &name(target)),
        ApplyingDelay { speaker, from_ms, to_ms } => {
            let n = name(speaker);
            if *to_ms == 0 {
                t!(lang, StatusApplyingOwn, speaker = &n)
            } else if to_ms < from_ms {
                t!(lang, StatusApplyingRemove, speaker = &n, ms = (from_ms - to_ms).unsigned_abs())
            } else {
                t!(lang, StatusApplyingAdd, speaker = &n, ms = (to_ms - from_ms).unsigned_abs())
            }
        }
        FellBack { speaker, cause: FellBackCause::StoppedResponding, .. } => {
            t!(lang, StatusFellBack, speaker = &name(speaker))
        }
        FellBack { speaker, cause: FellBackCause::RefusedPairing, .. } => {
            t!(lang, StatusRefusedPairing, speaker = &name(speaker))
        }
        SpeakerBack { speaker, .. } => t!(lang, StatusSpeakerBack, speaker = &name(speaker)),
        Taken { speaker, .. } => t!(lang, StatusTaken, speaker = &name(speaker)),
        NeedsSecret { speaker, secret: Secret::Code } => {
            t!(lang, StatusNeedsPairing, speaker = &name(speaker))
        }
        NeedsSecret { speaker, secret: Secret::Password } => {
            t!(lang, StatusNeedsPassword, speaker = &name(speaker))
        }
        LookingFor { speaker, looking: Looking::Searching } => {
            t!(lang, LookingLine, speaker = &name(speaker))
        }
        ChooseSpeaker => t!(lang, StatusChoose),
        MaBack => t!(lang, StatusMaBack),
        _ => short_status(facts),
    }
}

/// `stream-to-speaker status`: the status, then the speaker's kind, its
/// delay and the volume where the panel shows them.
pub fn cli_status(facts: &Facts, status: &str, delay: Option<&DelayView>, now: &NowCard) -> String {
    let lang = facts.lang;
    let mut parts = vec![status.to_string()];
    if let (Some(id), Some(Place::Speaker(current))) =
        (facts.state.speaker(), facts.state.current_place())
    {
        if *id == current {
            if let Some(s) = find(facts, id) {
                parts.push(kind_text(lang, &s.kind));
            }
        }
    }
    if let Some(v) = delay {
        let fig = figure_text(lang, figure(v.delay));
        let change = change_text(lang, v.delay.change_ms).filter(|_| v.applies != Applies::Fixed);
        parts.push(match change {
            Some(change) => t!(lang, CliPartDelayChange, figure = &fig, change = &change),
            None => t!(lang, CliPartDelay, figure = &fig),
        });
    }
    if let Some(vol) = &now.volume {
        parts.push(if vol.muted { t!(lang, CliPartMuted) } else { t!(lang, CliPartVolume, level = vol.level) });
    }
    lang.join_parts(&parts)
}

/// Windows' name for the app's sound output: the place a person picks by
/// picking it. `None` while the output is missing.
pub fn device_name(facts: &Facts) -> Option<String> {
    let lang = facts.lang;
    if facts.state == NowState::OutputMissing {
        return None;
    }
    Some(match &facts.remembered {
        Remembered::Nothing => t!(lang, AppName),
        Remembered::Speaker { name } => t!(lang, DeviceForPlace, place = name),
        Remembered::MusicAssistant => t!(lang, DeviceForPlace, place = &t!(lang, MaName)),
    })
}

/// Windows' name for the sound output while the app isn't running (set at
/// quit and by the crash handler).
pub fn device_name_not_running(lang: Lang) -> String {
    t!(lang, DeviceNotRunning)
}

/// The phone remote's line while the PC can't be reached. `time` is the
/// last update, as the phone writes it.
pub fn phone_cant_reach(lang: Lang, pc: &str, time: &str) -> String {
    t!(lang, PhoneCantReach, pc = pc, time = time)
}

/// The phone remote's page for a link that was replaced.
pub fn phone_link_revoked(lang: Lang) -> String {
    t!(lang, PhoneLinkRevoked)
}

/// The console program's line when the app isn't running (exit code 1).
pub fn cli_not_running(lang: Lang) -> String {
    t!(lang, NotRunning)
}

/// The console program's line when the app didn't answer in time (exit
/// code 4).
pub fn cli_no_reply(lang: Lang) -> String {
    t!(lang, NoReply)
}

/// The headless picker's first line.
pub fn picker_header(lang: Lang, version: &str, network: &str) -> String {
    t!(lang, PickerHeader, version = version, network = network)
}

/// The headless picker's question, for `count` speakers.
pub fn picker_prompt(lang: Lang, count: usize) -> String {
    t!(lang, PickerPrompt, count = count)
}

/// *Can't hear it?*, while the place is a speaker or Music Assistant.
fn cant_hear(facts: &Facts) -> Option<Command> {
    match facts.state.current_place() {
        Some(Place::Speaker(id)) => Some(Command::Open(Open::CantHear(Some(id)))),
        Some(Place::MusicAssistant) => Some(Command::Open(Open::CantHear(None))),
        _ => None,
    }
}

fn quit_label(facts: &Facts) -> String {
    let lang = facts.lang;
    if facts.state.sound_on_this_pc() {
        t!(lang, QuitPlain)
    } else {
        t!(lang, QuitReturns)
    }
}

/// The panel's footer.
pub fn footer(facts: &Facts) -> Footer {
    let lang = facts.lang;
    Footer {
        cant_hear: cant_hear(facts).map(|c| Button::link(t!(lang, CantHear), c)),
        speaker_missing: Button::link(t!(lang, SpeakerMissing), Command::Open(Open::SpeakerMissing)),
        settings: Button { style: ButtonStyle::Icon, ..Button::new(t!(lang, Settings), Command::Open(Open::Settings)) },
    }
}

/// The tray icon, tooltip and menu: the state, the places, Open, Settings
/// and Quit. The panel is one click on the same icon for the rest.
pub fn tray(facts: &Facts, list: &PlaceList, short: &str, badge: Badge) -> Tray {
    let lang = facts.lang;
    let mut menu = vec![MenuEntry::Label(short.to_string()), MenuEntry::Separator];

    let place_item = |row: &super::places::Row| MenuItem {
        checked: row.current,
        ..MenuItem::new(row.title.clone(), Command::Play(row.place.clone()))
    };
    let mut places: Vec<MenuItem> = std::iter::once(&list.this_pc)
        .chain(list.speakers.iter().filter(|r| !r.dimmed))
        .chain(list.music_assistant.iter().map(|s| &s.row).filter(|r| !r.dimmed))
        .map(place_item)
        .collect();
    let mut more = list.more.is_some();
    if places.len() > TRAY_PLACES {
        places.truncate(TRAY_PLACES);
        more = true;
    }
    menu.extend(places.into_iter().map(MenuEntry::Item));
    if more {
        menu.push(MenuEntry::Item(MenuItem::new(t!(lang, MoreSpeakersEllipsis), Command::Open(Open::Panel))));
    }
    menu.push(MenuEntry::Separator);
    menu.push(MenuEntry::Item(MenuItem::new(t!(lang, OpenShort), Command::Open(Open::Panel))));
    menu.push(MenuEntry::Item(MenuItem::new(t!(lang, Settings), Command::Open(Open::Settings))));
    menu.push(MenuEntry::Separator);
    menu.push(MenuEntry::Item(MenuItem::new(quit_label(facts), Command::Quit)));

    Tray {
        arcs: facts.state.arcs(),
        badge,
        tooltip: t!(lang, TooltipFormat, status = short),
        menu,
    }
}

/// The notice line: the first that applies, in the order below.
pub fn notice(facts: &Facts, badge: Badge) -> Option<Notice> {
    let lang = facts.lang;
    let n = &facts.notices;
    let dismiss = |kind| Some(Button { style: ButtonStyle::Icon, ..Button::new(t!(lang, Dismiss), Command::DismissNotice(kind)) });
    let simple = |kind, text: String, tone, actions: Vec<Button>| Notice {
        kind,
        text,
        detail: None,
        tone,
        actions,
        dismiss: None,
    };
    if n.restart_pending {
        return Some(simple(
            NoticeKind::RestartPending,
            t!(lang, NoticeRestart),
            Tone::Info,
            vec![Button::link(t!(lang, RestartEllipsis), Command::Open(Open::RestartWindows))],
        ));
    }
    if let Some(adapter) = &n.adapter_off {
        return Some(Notice {
            detail: Some(t!(lang, NoticeAdapterOff, adapter = adapter)),
            ..simple(
                NoticeKind::AdapterOff,
                t!(lang, NoticeNotLooking),
                Tone::Warning,
                vec![Button::link(t!(lang, Settings), Command::Open(Open::Settings))],
            )
        });
    }
    if let Some(device) = &n.refused_in_group {
        return Some(simple(
            NoticeKind::RefusedInGroup,
            t!(lang, NoticeRefused, device = &device.name),
            Tone::Warning,
            vec![Button::link(t!(lang, Allow), Command::AllowDevice { key: device.key.clone() })],
        ));
    }
    if n.lab_on {
        return Some(simple(
            NoticeKind::LabOn,
            t!(lang, NoticeLab),
            Tone::Warning,
            vec![Button::link(t!(lang, TurnOff), Command::TurnOffLab)],
        ));
    }
    if let Some(version) = &n.update_ready {
        return Some(simple(
            NoticeKind::UpdateReady,
            t!(lang, NoticeUpdate, version = version),
            Tone::Info,
            vec![Button::link(t!(lang, WhatsNew), Command::Open(Open::WhatsNew))],
        ));
    }
    if let Some(version) = &n.updated_to {
        return Some(Notice {
            dismiss: dismiss(NoticeKind::Updated),
            ..simple(
                NoticeKind::Updated,
                t!(lang, NoticeUpdated, version = version),
                Tone::Info,
                vec![Button::link(t!(lang, WhatsNew), Command::Open(Open::WhatsNew))],
            )
        });
    }
    // Never while a problem shows.
    if n.donation_due && badge == Badge::None {
        return Some(Notice {
            dismiss: dismiss(NoticeKind::Donation),
            ..simple(
                NoticeKind::Donation,
                t!(lang, NoticeDonate),
                Tone::Plain,
                vec![Button::link(t!(lang, Donate), Command::Open(Open::Donate))],
            )
        });
    }
    None
}

/// The notification the engine raised, in words.
pub fn notification(facts: &Facts) -> Option<Notification> {
    let lang = facts.lang;
    let raised = facts.notification.as_ref()?;
    let speaker = raised.speaker.as_ref();
    let name = speaker.map(|id| name_of(facts, id).to_string()).unwrap_or_default();
    let play = |label: String| {
        speaker.map(|id| Button::primary(label, Command::Play(Place::Speaker(id.clone()))))
    };
    let (title, body, buttons) = match raised.kind {
        NotificationKind::StoppedResponding => (
            t!(lang, NotifyStoppedTitle, speaker = &name),
            t!(lang, NotifyBackOnPc),
            play(t!(lang, TrySpeakerAgain, speaker = &name)).into_iter().collect(),
        ),
        NotificationKind::StoppedRespondingSilent => (
            t!(lang, NotifyStoppedTitle, speaker = &name),
            t!(lang, NotifySilent),
            vec![Button::primary(t!(lang, PlayOnPc), Command::Play(Place::ThisPc))],
        ),
        NotificationKind::Taken => (
            t!(lang, NotifyTakenTitle, speaker = &name),
            t!(lang, NotifyBackOnPc),
            speaker
                .map(|id| {
                    Button::primary(
                        t!(lang, PlayOnSpeakerAgain, speaker = &name),
                        Command::PlayTakingOver(id.clone()),
                    )
                })
                .into_iter()
                .collect(),
        ),
        NotificationKind::SpeakerBack => (
            t!(lang, SpeakerBackTitle, speaker = &name),
            t!(lang, NotifyStillOnPc),
            play(t!(lang, PlayOnSpeaker, speaker = &name)).into_iter().collect(),
        ),
        NotificationKind::NeedsSecret(secret) => {
            let pair = speaker.map(|id| Command::Open(Open::Pair(id.clone())));
            let (title, body, label) = match secret {
                Secret::Code => (
                    t!(lang, NeedsPairingTitle, speaker = &name),
                    t!(lang, NotifyUntilPaired),
                    t!(lang, Pair),
                ),
                Secret::Password => (
                    t!(lang, NeedsPasswordTitle, speaker = &name),
                    t!(lang, NotifyUntilPassword),
                    t!(lang, EnterPassword),
                ),
            };
            (title, body, pair.map(|c| Button::primary(label, c)).into_iter().collect())
        }
        NotificationKind::RestartToFinish => (
            t!(lang, NotifyRestartTitle),
            t!(lang, NotifyRestartBody),
            vec![Button::primary(t!(lang, RestartNow), Command::Open(Open::RestartWindows))],
        ),
        NotificationKind::OutputMissing => (
            t!(lang, NotifyCantPlayTitle),
            t!(lang, NotifyCantPlayBody),
            vec![Button::primary(t!(lang, Repair), Command::Open(Open::Repair))],
        ),
    };
    let close = (raised.kind == NotificationKind::RestartToFinish).then(|| t!(lang, Later));
    Some(Notification { seq: raised.seq, kind: raised.kind, title, body, buttons, close, tag: raised.speaker.clone() })
}
