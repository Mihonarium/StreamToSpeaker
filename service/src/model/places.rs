//! The list of places: This PC and its outputs, the speakers and groups,
//! Music Assistant, and what each row says.
//!
//! A row has two lines: its name, and under it what tells it apart (a
//! speaker's kind and delay, the output This PC plays on). A short status
//! shows at the right end when there is one, and a click that failed adds
//! its line. A speaker reached two ways (a Sonos speaker by its own
//! protocol and by AirPlay) is two rows, next to each other, the way used
//! last first.

use super::command::{Command, Open};
use super::delay::{figure, row_delay_text, spoken_row_delay};
use super::facts::{
    ConnectFailure, Facts, Mode, OutputKind, PcOutput, Presence, SearchCause, SearchPhase, SpeakerFacts,
    SpeakerKind,
};
use super::snapshot::{stage_text, Button, MenuItem, Tone};
use super::state::{Dropout, FellBackCause, NowState, Place, Secret};
use super::text::{t, Lang};

/// Beyond this many rows, speakers never used from this PC fold away.
pub const UNFOLDED_ROWS: usize = 8;

/// The list.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaceList {
    /// "Play on". The panel shows no heading; the phone remote does.
    pub heading: String,
    pub this_pc: Row,
    /// This PC's outputs, under its row while its chevron is open; empty
    /// when the PC has only one.
    pub pc_outputs: Vec<OutputRow>,
    /// While nothing is found yet.
    pub searching: Option<Searching>,
    /// Speakers and groups by name; remembered ones that aren't found come
    /// last. Speakers this app can't play to are not listed.
    pub speakers: Vec<Row>,
    /// Speakers never used from this PC, folded on a crowded network.
    pub more: Option<Folded>,
    /// The last row, when Music Assistant is set up.
    pub music_assistant: Option<Section>,
    /// The name of a row's ⋯ button.
    pub menu_name: String,
}

impl PlaceList {
    /// Every row in panel order, folded ones last.
    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        std::iter::once(&self.this_pc)
            .chain(self.speakers.iter())
            .chain(self.music_assistant.iter().map(|s| &s.row))
            .chain(self.more.iter().flat_map(|m| m.rows.iter()))
    }
}

/// One row.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub place: Place,
    pub icon: RowIcon,
    /// The name; with its address when two devices answer to one name.
    pub title: String,
    /// The second line: a speaker's kind and delay (*Sonos · about 0.2 s
    /// delay*, *AirPlay · about 2 s delay*), the output This PC plays on,
    /// or what Music Assistant does.
    pub second: String,
    /// At the right end, when there is something to show.
    pub status: Option<RowStatus>,
    /// What is true now when it differs from the second line (*Trying
    /// again (2 of 3)…*), else the second line: the panel's tooltip; the
    /// phone remote shows it as the second line, in `tone`.
    pub tooltip: String,
    pub tone: Tone,
    /// Can't be played right now.
    pub dimmed: bool,
    /// Where the sound is: the accent bar.
    pub current: bool,
    /// What a click does; nothing on the current row.
    pub command: Option<Command>,
    /// After a click that failed, a second line: the words and *Try again*.
    pub error: Option<RowError>,
    /// The row's question, expanded in place under it.
    pub question: Option<Question>,
    /// The ⋯ menu (and right-click).
    pub menu: Vec<MenuItem>,
    /// This PC's chevron, when the PC has more than one output.
    pub chevron: Option<Chevron>,
    /// What a screen reader says for the row.
    pub spoken: String,
}

/// The chevron at This PC's right end, which opens its outputs.
#[derive(Clone, Debug, PartialEq)]
pub struct Chevron {
    pub open: bool,
    /// Opens or closes them (remembered).
    pub command: Command,
    /// "Show this PC's outputs" / "Hide this PC's outputs".
    pub spoken: String,
}

/// One of This PC's outputs, under its row.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputRow {
    pub icon: RowIcon,
    /// The output's name: "Speakers", "Headphones".
    pub title: String,
    /// Its device: "Realtek Audio".
    pub second: String,
    /// The output This PC plays on.
    pub checked: bool,
    /// Make it This PC's output and play there; nothing on the checked row
    /// while the sound is on this PC.
    pub command: Option<Command>,
    pub spoken: String,
}

/// The right end of a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowStatus {
    /// *Not found*, *In use*, *1 of 2 found*.
    Text(String),
    /// Connecting.
    Spinner,
    /// The amber icon: something about this place needs a look (the top
    /// or the tooltip has the words).
    Attention,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowIcon {
    Pc,
    Headphones,
    Display,
    Speaker,
    Group,
    HomePodPair,
    Tv,
    MusicAssistant,
}

/// "Kitchen is playing something else right now." and its two buttons.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub title: String,
    pub line: String,
    pub confirm: Button,
    pub cancel: Button,
}

/// A click that failed.
#[derive(Clone, Debug, PartialEq)]
pub struct RowError {
    pub text: String,
    pub retry: Button,
}

/// The search, while nothing is found.
#[derive(Clone, Debug, PartialEq)]
pub struct Searching {
    pub title: String,
    pub tooltip: String,
    /// After 10 seconds without a speaker.
    pub none_found: Option<NoneFound>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NoneFound {
    pub title: String,
    /// Causes the app tested and found.
    pub causes: Vec<String>,
    pub buttons: Vec<Button>,
}

/// *More speakers (12)*.
#[derive(Clone, Debug, PartialEq)]
pub struct Folded {
    pub title: String,
    pub tooltip: String,
    pub rows: Vec<Row>,
}

/// Music Assistant's row. The panel shows no heading over it; the phone
/// remote shows *Through Music Assistant*.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub heading: String,
    pub row: Row,
}

/// A kind of speaker as words ("Sonos", "HomePod pair").
pub fn kind_text(lang: Lang, kind: &SpeakerKind) -> String {
    match kind {
        SpeakerKind::Sonos => t!(lang, KindSonos),
        SpeakerKind::SonosGroup => t!(lang, KindSonosGroup),
        SpeakerKind::HomePod => t!(lang, KindHomePod),
        SpeakerKind::HomePodPair => t!(lang, KindHomePodPair),
        SpeakerKind::AppleTv => t!(lang, KindAppleTv),
        SpeakerKind::AirPlay => t!(lang, KindAirPlay),
        SpeakerKind::Sendspin => t!(lang, KindSendspin),
        SpeakerKind::Other { maker: Some(maker) } => maker.clone(),
        SpeakerKind::Other { maker: None } => t!(lang, KindSpeaker),
    }
}

fn kind_icon(kind: &SpeakerKind) -> RowIcon {
    match kind {
        SpeakerKind::SonosGroup => RowIcon::Group,
        SpeakerKind::HomePodPair => RowIcon::HomePodPair,
        SpeakerKind::AppleTv => RowIcon::Tv,
        _ => RowIcon::Speaker,
    }
}

/// "Sonos · about 0.2 s delay" (the kind alone while the delay isn't known):
/// a speaker's kind and delay, as a row's tooltip, the headless picker and
/// `--list-speakers` give them.
pub fn kind_and_delay(lang: Lang, s: &SpeakerFacts) -> String {
    let kind = kind_text(lang, &s.kind);
    match s.delay {
        Some(d) => t!(lang, KindAndDelay, kind = &kind, delay = &row_delay_text(lang, figure(d))),
        None => kind,
    }
}

fn spoken_kind_and_delay(lang: Lang, s: &SpeakerFacts) -> String {
    let kind = kind_text(lang, &s.kind);
    match s.delay {
        Some(d) => t!(lang, SpokenKindDelay, kind = &kind, delay = &spoken_row_delay(lang, figure(d))),
        None => kind,
    }
}

fn output_icon(kind: OutputKind) -> RowIcon {
    match kind {
        OutputKind::Headphones => RowIcon::Headphones,
        OutputKind::Display => RowIcon::Display,
        OutputKind::Speakers | OutputKind::Other => RowIcon::Pc,
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// `a` and `b` are two ways to one speaker.
fn twins(facts: &Facts, a: &Place, b: &Place) -> bool {
    let (Place::Speaker(a), Place::Speaker(b)) = (a, b) else { return false };
    facts.speakers.iter().any(|s| &s.id == a && s.twin.as_ref() == Some(b))
        && facts.speakers.iter().any(|s| &s.id == b && s.twin.as_ref() == Some(a))
}

/// The speakers in list order: by name, remembered ones that aren't found
/// last, and two ways to one speaker next to each other, the one used last
/// first (the one with less delay when neither was used).
fn in_list_order<'a>(listed: &[&'a SpeakerFacts]) -> Vec<&'a SpeakerFacts> {
    let twin_of = |s: &SpeakerFacts| -> Option<&'a SpeakerFacts> {
        let t = s.twin.as_ref()?;
        listed.iter().copied().find(|o| &o.id == t && o.twin.as_ref() == Some(&s.id))
    };
    let goes_first = |a: &SpeakerFacts, b: &SpeakerFacts| {
        let delay = |s: &SpeakerFacts| s.delay.map(|d| d.own_ms).unwrap_or(u32::MAX);
        (std::cmp::Reverse(a.last_used), delay(a), &a.id) <= (std::cmp::Reverse(b.last_used), delay(b), &b.id)
    };
    let mut units: Vec<Vec<&'a SpeakerFacts>> = Vec::new();
    for s in listed.iter().copied() {
        match twin_of(s) {
            Some(t) if goes_first(t, s) => {} // listed after its twin
            Some(t) => units.push(vec![s, t]),
            None => units.push(vec![s]),
        }
    }
    units.sort_by_key(|u| {
        let gone = u.iter().all(|s| s.presence == Presence::NotFound);
        (gone, u[0].name.to_lowercase(), u[0].id.clone())
    });
    units.into_iter().flatten().collect()
}

/// Builds the list for `facts`.
pub fn place_list(facts: &Facts) -> PlaceList {
    let lang = facts.lang;
    let state = &facts.state;
    let current = state.current_place();
    let missing = *state == NowState::OutputMissing;

    let listed: Vec<&SpeakerFacts> =
        facts.speakers.iter().filter(|s| !s.hidden && s.presence != Presence::CantPlay).collect();
    let mut rows: Vec<(bool, Row)> = in_list_order(&listed)
        .into_iter()
        .map(|s| {
            // Two devices answering to one name, other than one speaker
            // reached two ways: their names carry the address.
            let clash = listed
                .iter()
                .any(|o| o.id != s.id && same_name(&o.name, &s.name) && s.twin.as_ref() != Some(&o.id));
            (s.used_here, speaker_row(facts, s, clash))
        })
        .collect();

    // Fold speakers never used from here when the list is long.
    let mut more = None;
    let ma_rows = usize::from(facts.music_assistant.is_some());
    if 1 + rows.len() + ma_rows > UNFOLDED_ROWS {
        let keep = |(used, r): &(bool, Row)| {
            *used || r.current || r.status == Some(RowStatus::Spinner) || r.question.is_some() || r.error.is_some()
        };
        // Two ways to one speaker stay together.
        let stays: Vec<bool> = (0..rows.len())
            .map(|i| {
                keep(&rows[i])
                    || [i.checked_sub(1), Some(i + 1)].into_iter().flatten().any(|j| {
                        j < rows.len() && keep(&rows[j]) && twins(facts, &rows[i].1.place, &rows[j].1.place)
                    })
            })
            .collect();
        let (kept, folded): (Vec<_>, Vec<_>) =
            rows.into_iter().zip(stays).partition(|(_, stays)| *stays);
        let kept: Vec<(bool, Row)> = kept.into_iter().map(|(r, _)| r).collect();
        let folded: Vec<(bool, Row)> = folded.into_iter().map(|(r, _)| r).collect();
        rows = kept;
        if !folded.is_empty() {
            more = Some(Folded {
                title: t!(lang, RowMoreSpeakers, count = folded.len()),
                tooltip: t!(lang, RowMoreSpeakersLine),
                rows: folded.into_iter().map(|(_, r)| r).collect(),
            });
        }
    }

    PlaceList {
        heading: t!(lang, ListHeading),
        this_pc: this_pc_row(facts, current == Some(Place::ThisPc)),
        pc_outputs: if facts.pc.outputs.len() > 1 {
            facts.pc.outputs.iter().map(|o| output_row(facts, o, current == Some(Place::ThisPc))).collect()
        } else {
            Vec::new()
        },
        searching: searching(facts),
        speakers: rows.into_iter().map(|(_, r)| r).collect(),
        more,
        music_assistant: facts.music_assistant.as_ref().map(|ma| {
            let is_current = current == Some(Place::MusicAssistant);
            let (tooltip, tone, status, dimmed) = if missing {
                (t!(lang, RowRepairFirst), Tone::Plain, None, true)
            } else {
                match state {
                    NowState::MaPlaying { .. } => (t!(lang, RowMaPlaying), Tone::Plain, None, false),
                    NowState::MaWaiting => (t!(lang, RowMaOffered), Tone::Plain, None, false),
                    NowState::MaAway { .. } => {
                        (away_line(lang, ma.last_seen), Tone::Warning, Some(RowStatus::Attention), false)
                    }
                    _ if !ma.connected => (
                        away_line(lang, ma.last_seen),
                        Tone::Warning,
                        Some(RowStatus::Text(t!(lang, RowMaAway))),
                        true,
                    ),
                    _ => (t!(lang, RowMaChooses), Tone::Plain, None, false),
                }
            };
            let title = t!(lang, MaName);
            Section {
                heading: t!(lang, MaHeading),
                row: Row {
                    place: Place::MusicAssistant,
                    icon: RowIcon::MusicAssistant,
                    spoken: spoken(lang, &title, &tooltip, status_word(lang, state, is_current)),
                    title,
                    second: tooltip.clone(),
                    status,
                    tooltip,
                    tone,
                    dimmed,
                    current: is_current,
                    command: (!is_current && !missing).then_some(Command::Play(Place::MusicAssistant)),
                    error: None,
                    question: None,
                    menu: vec![MenuItem::new(t!(lang, MaSettings), Command::Open(Open::MusicAssistantSettings))],
                    chevron: None,
                },
            }
        }),
        menu_name: t!(lang, More),
    }
}

fn away_line(lang: Lang, last_seen: Option<super::text::LocalTime>) -> String {
    match last_seen {
        Some(time) => t!(lang, RowMaAwaySince, time = &lang.time(time)),
        None => t!(lang, RowMaAway),
    }
}

/// "Playing." or "Ready." after a current row's words, for screen readers.
fn status_word(lang: Lang, state: &NowState, current: bool) -> Option<String> {
    use NowState::*;
    if !current {
        return None;
    }
    match state {
        Playing { .. } | ApplyingDelay { .. } | MaPlaying { .. } | PlayingSentApps { .. } => {
            Some(t!(lang, SpokenPlaying))
        }
        Ready { .. } | ReadyPlayingElsewhere { .. } | ReadyNoAppSent { .. } => Some(t!(lang, SpokenReady)),
        _ => None,
    }
}

fn spoken(lang: Lang, name: &str, detail: &str, status: Option<String>) -> String {
    let detail = detail.trim_end_matches('.');
    match status {
        Some(status) => t!(lang, SpokenRowStatus, name = name, detail = detail, status = &status),
        None => t!(lang, SpokenRow, name = name, detail = detail),
    }
}

fn this_pc_row(facts: &Facts, current: bool) -> Row {
    let lang = facts.lang;
    let (title, tooltip) = match facts.mode {
        Mode::OnlySentApps { pass_through: false } => {
            (t!(lang, RowStopPlayingTitle), t!(lang, RowStopPlayingLine))
        }
        _ => (t!(lang, ThisPc), facts.pc.output.friendly.clone()),
    };
    let open = facts.pc.outputs_open;
    let chevron = (facts.pc.outputs.len() > 1).then(|| Chevron {
        open,
        command: Command::ShowPcOutputs(!open),
        spoken: if open { t!(lang, HidePcOutputs) } else { t!(lang, ShowPcOutputs) },
    });
    Row {
        place: Place::ThisPc,
        icon: output_icon(facts.pc.output.kind),
        spoken: spoken(lang, &title, &tooltip, None),
        title,
        second: tooltip.clone(),
        status: None,
        tooltip,
        tone: Tone::Plain,
        dimmed: false,
        current,
        command: (!current).then_some(Command::Play(Place::ThisPc)),
        error: None,
        question: None,
        menu: vec![],
        chevron,
    }
}

fn output_row(facts: &Facts, o: &PcOutput, on_pc: bool) -> OutputRow {
    let lang = facts.lang;
    let checked = o.id == facts.pc.output.id;
    OutputRow {
        icon: output_icon(o.kind),
        spoken: spoken(lang, &o.name, &o.device, checked.then(|| t!(lang, SpokenPlaysHere))),
        title: o.name.clone(),
        second: o.device.clone(),
        checked,
        command: (!(checked && on_pc)).then(|| Command::PlayOnPcOutput(o.id.clone())),
    }
}

fn searching(facts: &Facts) -> Option<Searching> {
    let lang = facts.lang;
    let search = &facts.search;
    if search.phase == SearchPhase::Found {
        return None;
    }
    let title = match &search.network {
        Some(n) => {
            let network = match &n.network {
                Some(name) => t!(lang, NetworkNamed, adapter = &n.adapter, network = name),
                None => n.adapter.clone(),
            };
            t!(lang, SearchingOn, network = &network)
        }
        None => t!(lang, Searching),
    };
    let none_found = (search.phase == SearchPhase::NoneFound).then(|| {
        let mut buttons = Vec::new();
        let causes = search
            .causes
            .iter()
            .map(|c| match c {
                SearchCause::PublicNetwork { network } => {
                    buttons.push(Button::primary(t!(lang, FixInWindows), Command::Open(Open::NetworkSettings)));
                    t!(lang, CausePublicNetwork, network = network)
                }
                SearchCause::FirewallBlocks => t!(lang, CauseFirewall),
            })
            .collect();
        buttons.push(Button::new(t!(lang, CantFindYourSpeaker), Command::Open(Open::SpeakerMissing)));
        buttons.push(Button::new(t!(lang, AddByAddress), Command::Open(Open::AddByAddress)));
        NoneFound { title: t!(lang, NoneFoundTitle), causes, buttons }
    });
    Some(Searching {
        title,
        tooltip: t!(lang, SearchingLine, seconds = super::rules::SEARCH_EVERY_MS / 1000),
        none_found,
    })
}

fn speaker_row(facts: &Facts, s: &SpeakerFacts, clash: bool) -> Row {
    use NowState::*;
    let lang = facts.lang;
    let state = &facts.state;
    let id = &s.id;
    let is_current = state.current_place() == Some(Place::Speaker(id.clone()));
    let about_this = state.speaker() == Some(id);
    let kind = kind_text(lang, &s.kind);
    let title = if clash { t!(lang, RowWithAddress, name = &s.name, address = &s.address) } else { s.name.clone() };

    let mut tone = Tone::Plain;
    let mut dimmed = false;
    let mut status = None;
    let mut error = None;
    let mut question = None;
    let mut clickable = !is_current;
    let mut situational = true;

    let tooltip = if *state == OutputMissing {
        dimmed = true;
        clickable = false;
        t!(lang, RowRepairFirst)
    } else if let (true, Connecting { stage, .. }) = (about_this, state) {
        status = Some(RowStatus::Spinner);
        clickable = false;
        stage_text(lang, *stage, &s.name)
    } else if let (true, Some(line)) = (about_this, state_line(facts, state, &mut tone)) {
        if tone == Tone::Warning {
            status = Some(RowStatus::Attention);
        }
        line
    } else if let Some(failure) = s.failure {
        tone = Tone::Error;
        let retry = match failure {
            ConnectFailure::CouldntReconnect => Command::Reconnect(id.clone()),
            _ => Command::Play(Place::Speaker(id.clone())),
        };
        let text = match failure {
            ConnectFailure::NoAnswer => t!(lang, RowNoAnswer),
            ConnectFailure::WrongPassword => t!(lang, RowWrongPassword),
            ConnectFailure::CouldntStart => t!(lang, RowCouldntStart),
            ConnectFailure::CouldntReconnect => t!(lang, RowCouldntReconnect, speaker = &s.name),
            ConnectFailure::Other => t!(lang, RowCouldntConnect),
        };
        error = Some(RowError { text: text.clone(), retry: Button::new(t!(lang, TryAgain), retry) });
        text
    } else if s.presence == Presence::NotFound {
        dimmed = true;
        status = Some(RowStatus::Text(t!(lang, RowNotFoundShort)));
        t!(lang, RowNotFound)
    } else if s.presence == Presence::WaitingForPair {
        dimmed = true;
        clickable = false;
        status = Some(RowStatus::Text(t!(lang, RowPartsFound, found = 1u32, all = 2u32)));
        t!(lang, RowWaitingForPair)
    } else if clash {
        tone = Tone::Warning;
        status = Some(RowStatus::Attention);
        t!(lang, RowTwoDevices, kind = &kind, name = &s.name)
    } else if s.busy || s.question.is_some() {
        status = Some(RowStatus::Text(t!(lang, RowInUse)));
        t!(lang, RowBusy, kind = &kind)
    } else {
        situational = false;
        kind_and_delay(lang, s)
    };

    if let Some(q) = &s.question {
        if !dimmed {
            clickable = false;
            question = Some(Question {
                title: match &q.other_pc {
                    Some(pc) => t!(lang, BusyOtherPcTitle, speaker = &s.name, pc = pc),
                    None => t!(lang, BusyTitle, speaker = &s.name),
                },
                line: t!(lang, BusyLine, speaker = &s.name),
                confirm: Button::primary(
                    t!(lang, PlayOnSpeaker, speaker = &s.name),
                    Command::PlayTakingOver(id.clone()),
                ),
                cancel: Button::new(t!(lang, Cancel), Command::DismissQuestion(id.clone())),
            });
        }
    }

    let mut menu = vec![MenuItem::new(t!(lang, SpeakerSettings), Command::Open(Open::SpeakerSettings(id.clone())))];
    menu.push(MenuItem {
        enabled: s.presence == Presence::Found && *state != OutputMissing,
        ..MenuItem::new(t!(lang, TestSound), Command::PlayTestSound(id.clone()))
    });
    if is_current {
        menu.push(MenuItem::new(t!(lang, Reconnect), Command::Reconnect(id.clone())));
    }
    menu.push(MenuItem::new(t!(lang, HideFromList), Command::HideFromList(id.clone())));

    // Windows refused to switch to it: the current place has a problem.
    if about_this && status.is_none() && matches!(state, WindowsRefused { .. }) {
        status = Some(RowStatus::Attention);
    }

    let detail = if situational { tooltip.clone() } else { spoken_kind_and_delay(lang, s) };
    Row {
        place: Place::Speaker(id.clone()),
        icon: kind_icon(&s.kind),
        spoken: spoken(lang, &title, &detail, status_word(lang, state, is_current)),
        title,
        second: kind_and_delay(lang, s),
        status,
        tooltip,
        tone,
        dimmed,
        current: is_current,
        command: clickable.then(|| Command::Play(Place::Speaker(id.clone()))),
        error,
        question,
        menu,
        chevron: None,
    }
}

/// What the row says when the state is about this speaker, if the state
/// has words for its row.
fn state_line(facts: &Facts, state: &NowState, tone: &mut Tone) -> Option<String> {
    use NowState::*;
    let lang = facts.lang;
    Some(match state {
        NotResponding { dropout, .. } => {
            *tone = Tone::Warning;
            match dropout {
                Dropout::QuickTries { attempt, of, .. } => t!(lang, RowRetrying, attempt = *attempt, tries = *of),
                Dropout::KeepTrying { every_secs } => t!(lang, RowKeepTrying, seconds = *every_secs),
            }
        }
        NotTaking { .. } => {
            *tone = Tone::Warning;
            t!(lang, RowNotTaking)
        }
        WindowsSwitched { at, .. } => t!(lang, RowLetGo, time = &lang.time(*at)),
        FellBack { at, cause: FellBackCause::StoppedResponding, .. } => {
            *tone = Tone::Warning;
            t!(lang, RowStoppedResponding, time = &lang.time(*at))
        }
        FellBack { cause: FellBackCause::RefusedPairing, .. } => {
            *tone = Tone::Warning;
            t!(lang, RowNeedsPairing)
        }
        Taken { at, .. } => {
            *tone = Tone::Warning;
            t!(lang, RowPlayingElseSince, time = &lang.time(*at))
        }
        NeedsSecret { secret, .. } => {
            *tone = Tone::Warning;
            match secret {
                Secret::Code => t!(lang, RowNeedsPairing),
                Secret::Password => t!(lang, RowNeedsPassword),
            }
        }
        _ => return None,
    })
}
