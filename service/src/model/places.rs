//! The list of places: This PC, the speakers and groups, Music Assistant,
//! and what each row says.
//!
//! A row is one line: its name, and at the right end a short status only
//! when there is one. What the row used to say on a second line (the kind
//! and the delay, or what is true now) is its tooltip and what a reader of
//! the display hears after the name; the phone remote still shows it as a
//! second line.

use super::command::{Command, Open};
use super::delay::{figure, row_delay_text, spoken_row_delay};
use super::facts::{ConnectFailure, Facts, Mode, Presence, SearchCause, SearchPhase, SpeakerFacts, SpeakerKind};
use super::snapshot::{stage_text, Button, ButtonStyle, MenuItem, Tone};
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
    /// The name; with its address when two entries share the name.
    pub title: String,
    /// At the right end, when there is something to show.
    pub status: Option<RowStatus>,
    /// The kind and delay (*Sonos · about 0.2 s delay*), or what is true
    /// now. The panel shows it as the row's tooltip; the phone remote as
    /// its second line, in `tone`.
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
    /// The *›* at the row's end: the place's own view. Its label is the
    /// tooltip (*Options for Kitchen*), its spoken name adds the kind.
    /// None for This PC, whose chevron shows its outputs.
    pub options: Option<Button>,
    /// What a screen reader says for the row.
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

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Builds the list for `facts`.
pub fn place_list(facts: &Facts) -> PlaceList {
    let lang = facts.lang;
    let state = &facts.state;
    let current = state.current_place();
    let missing = *state == NowState::OutputMissing;

    let listed: Vec<&SpeakerFacts> =
        facts.speakers.iter().filter(|s| !s.hidden && s.presence != Presence::CantPlay).collect();
    let mut sorted = listed.clone();
    sorted.sort_by_key(|s| (s.presence == Presence::NotFound, s.name.to_lowercase(), s.id.clone()));
    let mut rows: Vec<(bool, Row)> = sorted
        .iter()
        .map(|s| {
            let twin = listed.iter().any(|o| o.id != s.id && same_name(&o.name, &s.name));
            (s.used_here, speaker_row(facts, s, twin))
        })
        .collect();

    // Fold speakers never used from here when the list is long.
    let mut more = None;
    let ma_rows = usize::from(facts.music_assistant.is_some());
    if 1 + rows.len() + ma_rows > UNFOLDED_ROWS {
        let (kept, folded): (Vec<_>, Vec<_>) = rows.into_iter().partition(|(used, r)| {
            *used || r.current || r.status == Some(RowStatus::Spinner) || r.question.is_some() || r.error.is_some()
        });
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
                    status,
                    tooltip,
                    tone,
                    dimmed,
                    current: is_current,
                    command: (!is_current && !missing).then_some(Command::Play(Place::MusicAssistant)),
                    error: None,
                    question: None,
                    menu: vec![MenuItem::new(t!(lang, Options), Command::Open(Open::MusicAssistantSettings))],
                    options: Some(options_button(lang, &t!(lang, MaName), None, Open::MusicAssistantSettings)),
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
    Row {
        place: Place::ThisPc,
        icon: RowIcon::Pc,
        spoken: spoken(lang, &title, &tooltip, None),
        title,
        status: None,
        tooltip,
        tone: Tone::Plain,
        dimmed: false,
        current,
        command: (!current).then_some(Command::Play(Place::ThisPc)),
        error: None,
        question: None,
        menu: vec![],
        options: None,
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

fn speaker_row(facts: &Facts, s: &SpeakerFacts, twin: bool) -> Row {
    use NowState::*;
    let lang = facts.lang;
    let state = &facts.state;
    let id = &s.id;
    let is_current = state.current_place() == Some(Place::Speaker(id.clone()));
    let about_this = state.speaker() == Some(id);
    let kind = kind_text(lang, &s.kind);
    let title = if twin { t!(lang, RowWithAddress, name = &s.name, address = &s.address) } else { s.name.clone() };

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
    } else if twin {
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

    let mut menu = vec![MenuItem::new(t!(lang, Options), Command::Open(Open::SpeakerSettings(id.clone())))];
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
        status,
        tooltip,
        tone,
        dimmed,
        current: is_current,
        command: clickable.then(|| Command::Play(Place::Speaker(id.clone()))),
        error,
        question,
        menu,
        options: Some(options_button(lang, &s.name, Some(&kind_text(lang, &s.kind)), Open::SpeakerSettings(id.clone()))),
    }
}

/// The *›* that opens a place's own view; a reader of the display hears
/// the kind too, when there is one.
fn options_button(lang: Lang, name: &str, kind: Option<&str>, open: Open) -> Button {
    Button {
        label: t!(lang, RowOptions, place = name),
        spoken: kind.map(|kind| t!(lang, SpokenRowOptions, place = name, kind = kind)),
        command: Command::Open(open),
        style: ButtonStyle::Icon,
        enabled: true,
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
