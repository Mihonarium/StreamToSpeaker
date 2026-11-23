//! The list under *Play on*: This PC with Windows' own outputs, the
//! speakers and groups, Music Assistant, and what each row says.

use super::command::{Command, Open};
use super::delay::{figure, row_delay_text, spoken_row_delay};
use super::facts::{
    ConnectFailure, Facts, Mode, OutputKind, PcOutput, Presence, SearchCause, SearchPhase,
    SpeakerFacts, SpeakerKind,
};
use super::snapshot::{stage_text, Button, MenuItem, Tone};
use super::state::{Dropout, FellBackCause, NowState, Place, Secret};
use super::text::{t, Lang};

/// Beyond this many rows, speakers never used from this PC fold away.
pub const UNFOLDED_ROWS: usize = 6;

/// The list.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaceList {
    /// "Play on".
    pub heading: String,
    pub this_pc: Row,
    /// Windows' outputs, under This PC's chevron; the one playing is
    /// checked.
    pub pc_outputs: Vec<OutputRow>,
    /// While nothing is found yet.
    pub searching: Option<Searching>,
    /// Speakers and groups by name; remembered ones that aren't found come
    /// last.
    pub speakers: Vec<Row>,
    /// Speakers never used from this PC, folded on a crowded network.
    pub more: Option<Folded>,
    pub music_assistant: Option<Section>,
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

/// One row (52 px in the panel).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub place: Place,
    pub icon: RowIcon,
    pub title: String,
    pub line2: String,
    /// Amber for trouble, red for a click that failed.
    pub tone: Tone,
    /// Can't be played right now.
    pub dimmed: bool,
    /// Where the sound is: the accent bar.
    pub current: bool,
    /// Something is in progress on it.
    pub spinner: bool,
    /// What a click does; nothing on the current row.
    pub command: Option<Command>,
    /// A button on the row itself: *Try again*, or × to stop connecting.
    pub inline: Option<Button>,
    /// The row's question, expanded in place under it.
    pub question: Option<Question>,
    /// The ⋯ menu (and right-click).
    pub menu: Vec<MenuItem>,
    /// This PC's chevron opens Windows' outputs.
    pub chevron: bool,
    /// What a screen reader says for the row.
    pub spoken: String,
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

/// A row under This PC's chevron.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputRow {
    pub icon: RowIcon,
    pub title: String,
    pub line2: String,
    pub checked: bool,
    pub command: Option<Command>,
}

/// The search, while nothing is found.
#[derive(Clone, Debug, PartialEq)]
pub struct Searching {
    pub title: String,
    pub line2: String,
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

/// *More speakers on this network (12)*.
#[derive(Clone, Debug, PartialEq)]
pub struct Folded {
    pub title: String,
    pub line2: String,
    pub rows: Vec<Row>,
}

/// *Through Music Assistant* and its row.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub heading: String,
    pub row: Row,
}

/// A kind of speaker as rows say it ("Sonos", "HomePod pair").
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

fn output_icon(kind: OutputKind) -> RowIcon {
    match kind {
        OutputKind::Headphones => RowIcon::Headphones,
        OutputKind::Display => RowIcon::Display,
        OutputKind::Speakers | OutputKind::Other => RowIcon::Pc,
    }
}

/// "Sonos · about 0.2 s delay" (the kind alone while the delay isn't known).
pub fn normal_line(lang: Lang, s: &SpeakerFacts) -> String {
    let kind = kind_text(lang, &s.kind);
    match s.delay {
        Some(d) => t!(lang, KindAndDelay, kind = &kind, delay = &row_delay_text(lang, figure(d))),
        None => kind,
    }
}

fn spoken_normal(lang: Lang, s: &SpeakerFacts) -> String {
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

    let visible: Vec<&SpeakerFacts> = facts.speakers.iter().filter(|s| !s.hidden).collect();
    let mut sorted = visible.clone();
    sorted.sort_by_key(|s| (s.presence == Presence::NotFound, s.name.to_lowercase(), s.id.clone()));
    let mut rows: Vec<(bool, Row)> = sorted
        .iter()
        .map(|s| {
            let twin = visible.iter().any(|o| o.id != s.id && same_name(&o.name, &s.name));
            (s.used_here, speaker_row(facts, s, twin))
        })
        .collect();

    // Fold speakers never used from here when the list is long.
    let mut more = None;
    if rows.len() + 1 > UNFOLDED_ROWS {
        let (kept, folded): (Vec<_>, Vec<_>) = rows.into_iter().partition(|(used, r)| {
            *used || r.current || r.spinner || r.question.is_some() || r.inline.is_some()
        });
        rows = kept;
        if !folded.is_empty() {
            more = Some(Folded {
                title: t!(lang, RowMoreSpeakers, count = folded.len()),
                line2: t!(lang, RowMoreSpeakersLine),
                rows: folded.into_iter().map(|(_, r)| r).collect(),
            });
        }
    }

    PlaceList {
        heading: t!(lang, ListHeading),
        this_pc: this_pc_row(facts, current == Some(Place::ThisPc)),
        pc_outputs: std::iter::once(&facts.pc.output)
            .chain(facts.pc.others.iter())
            .map(|o| output_row(facts, o))
            .collect(),
        searching: searching(facts),
        speakers: rows.into_iter().map(|(_, r)| r).collect(),
        more,
        music_assistant: facts.music_assistant.as_ref().map(|ma| {
            let is_current = current == Some(Place::MusicAssistant);
            let (line2, tone) = if missing {
                (t!(lang, RowRepairFirst), Tone::Plain)
            } else {
                match state {
                    NowState::MaPlaying { .. } => (t!(lang, RowMaPlaying), Tone::Plain),
                    NowState::MaWaiting => (t!(lang, RowMaOffered), Tone::Plain),
                    NowState::MaAway { .. } | NowState::MaBack => away_line(lang, ma.last_seen),
                    _ if !ma.connected => away_line(lang, ma.last_seen),
                    _ => (t!(lang, RowMaChooses), Tone::Plain),
                }
            };
            let title = t!(lang, MaName);
            Section {
                heading: t!(lang, MaHeading),
                row: Row {
                    place: Place::MusicAssistant,
                    icon: RowIcon::MusicAssistant,
                    spoken: spoken(lang, &title, &line2, status_word(lang, state, is_current)),
                    title,
                    line2,
                    tone,
                    dimmed: missing,
                    current: is_current,
                    spinner: false,
                    command: (!is_current && !missing).then_some(Command::Play(Place::MusicAssistant)),
                    inline: None,
                    question: None,
                    menu: vec![MenuItem::new(
                        t!(lang, MaSettings),
                        Command::Open(Open::MusicAssistantSettings),
                    )],
                    chevron: false,
                },
            }
        }),
    }
}

fn away_line(lang: Lang, last_seen: Option<super::text::LocalTime>) -> (String, Tone) {
    match last_seen {
        Some(time) => (t!(lang, RowMaAwaySince, time = &lang.time(time)), Tone::Warning),
        None => (t!(lang, RowMaAway), Tone::Warning),
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
    let out = &facts.pc.output;
    let (title, line2) = match facts.mode {
        Mode::OnlySentApps { pass_through: false } => {
            (t!(lang, RowStopPlayingTitle), t!(lang, RowStopPlayingLine))
        }
        _ => (t!(lang, ThisPc), out.friendly.clone()),
    };
    Row {
        place: Place::ThisPc,
        icon: RowIcon::Pc,
        spoken: spoken(lang, &title, &line2, None),
        title,
        line2,
        tone: Tone::Plain,
        dimmed: false,
        current,
        spinner: false,
        command: (!current).then_some(Command::Play(Place::ThisPc)),
        inline: None,
        question: None,
        menu: vec![],
        chevron: true,
    }
}

fn output_row(facts: &Facts, o: &PcOutput) -> OutputRow {
    let checked = o.id == facts.pc.output.id;
    let on_pc = facts.state.current_place() == Some(Place::ThisPc);
    OutputRow {
        icon: output_icon(o.kind),
        title: o.name.clone(),
        line2: o.device.clone(),
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
        line2: t!(lang, SearchingLine, seconds = super::rules::SEARCH_EVERY_MS / 1000),
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
    let title = if twin {
        t!(lang, RowWithAddress, name = &s.name, address = &s.address)
    } else {
        s.name.clone()
    };

    let mut tone = Tone::Plain;
    let mut dimmed = false;
    let mut spinner = false;
    let mut inline = None;
    let mut question = None;
    let mut clickable = !is_current;
    let mut situational = true;

    let line2 = if *state == OutputMissing {
        dimmed = true;
        clickable = false;
        t!(lang, RowRepairFirst)
    } else if let (true, Connecting { stage, .. }) = (about_this, state) {
        spinner = true;
        clickable = false;
        inline = Some(Button {
            style: super::snapshot::ButtonStyle::Icon,
            ..Button::new(t!(lang, Cancel), Command::CancelConnect)
        });
        stage_text(lang, *stage, &s.name)
    } else if let (true, Some(line)) = (about_this, state_line(facts, state, &mut tone, &mut spinner)) {
        line
    } else if let Some(failure) = s.failure {
        tone = Tone::Error;
        let retry = match failure {
            ConnectFailure::CouldntReconnect => Command::Reconnect(id.clone()),
            _ => Command::Play(Place::Speaker(id.clone())),
        };
        inline = Some(Button::new(t!(lang, TryAgain), retry));
        match failure {
            ConnectFailure::NoAnswer => t!(lang, RowNoAnswer),
            ConnectFailure::WrongPassword => t!(lang, RowWrongPassword),
            ConnectFailure::CouldntStart => t!(lang, RowCouldntStart),
            ConnectFailure::CouldntReconnect => t!(lang, RowCouldntReconnect, speaker = &s.name),
            ConnectFailure::Other => t!(lang, RowCouldntConnect),
        }
    } else if s.presence != Presence::Found {
        dimmed = true;
        match s.presence {
            Presence::NotFound => t!(lang, RowNotFound),
            Presence::CantPlay => t!(lang, RowCantPlay),
            Presence::WaitingForPair => {
                clickable = false;
                t!(lang, RowWaitingForPair)
            }
            Presence::Found => unreachable!(),
        }
    } else if twin {
        tone = Tone::Warning;
        t!(lang, RowTwoDevices, kind = &kind, name = &s.name)
    } else if s.busy || s.question.is_some() {
        t!(lang, RowBusy, kind = &kind)
    } else {
        situational = false;
        normal_line(lang, s)
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

    let command = if !clickable {
        None
    } else if s.presence == Presence::CantPlay {
        Some(Command::Open(Open::SpeakerSettings { speaker: id.clone(), delay: false }))
    } else {
        Some(Command::Play(Place::Speaker(id.clone())))
    };

    let mut menu = vec![MenuItem::new(
        t!(lang, SpeakerSettings),
        Command::Open(Open::SpeakerSettings { speaker: id.clone(), delay: false }),
    )];
    menu.push(MenuItem {
        enabled: s.presence == Presence::Found && *state != OutputMissing,
        ..MenuItem::new(t!(lang, TestSound), Command::PlayTestSound(id.clone()))
    });
    if is_current {
        menu.push(MenuItem::new(t!(lang, Reconnect), Command::Reconnect(id.clone())));
    }
    menu.push(MenuItem::new(t!(lang, HideFromList), Command::HideFromList(id.clone())));

    let detail = if situational { line2.clone() } else { spoken_normal(lang, s) };
    Row {
        place: Place::Speaker(id.clone()),
        icon: kind_icon(&s.kind),
        spoken: spoken(lang, &title, &detail, status_word(lang, state, is_current)),
        title,
        line2,
        tone,
        dimmed,
        current: is_current,
        spinner,
        command,
        inline,
        question,
        menu,
        chevron: false,
    }
}

/// The row's line when the state is about this speaker, if the state has
/// one for its row.
fn state_line(facts: &Facts, state: &NowState, tone: &mut Tone, spinner: &mut bool) -> Option<String> {
    use NowState::*;
    let lang = facts.lang;
    Some(match state {
        NotResponding { dropout, .. } => {
            *tone = Tone::Warning;
            *spinner = true;
            match dropout {
                Dropout::QuickTries { attempt, of, .. } => {
                    t!(lang, RowRetrying, attempt = *attempt, tries = *of)
                }
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
