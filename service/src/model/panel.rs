//! The four parts of the panel.

use super::command::{Command, DelayStep, Open, VolumeOwner};
use super::facts::press;
use super::facts::{Applies, Progress, SearchPhase, SpeakerDelay, SpeakerKind};
use super::notes;
/// Beyond this many rows, speakers never used from this PC fold away.
pub const UNFOLDED_ROWS: usize = 8;
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

use super::scene::{RepairNow, Scene, SpeakerSeen, WaySeen};
use super::situation::{
    Ahead, Away, Cause, ConnKind, Doing, Picking, RowState, Spot, Standing, SwitchBy, WayChoice,
};
use super::state::{Place, SpeakerId};
use super::text::{t, Lang, Number};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Panel {
    pub place: PlacePart,
    pub list: ListPart,
    pub about: About,
    pub links: [Act; 3],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacePart {
    pub name: String,
    pub output: Option<String>,
    pub state: StateLine,
    pub volume: Option<VolumeView>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeView {
    /// Absent only on a dimmed PC slider with no output to control.
    pub owner: Option<VolumeOwner>,
    pub level: u8,
    pub muted: bool,
    pub number: String,
    pub limit: Option<u8>,
    pub spoken: String,
    pub mute_spoken: String,
    pub dimmed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateLine {
    pub sign: Sign,
    pub text: String,
    pub link: Option<Act>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sign {
    /// Nothing plays there: the bars at rest.
    Still,
    /// Sound plays there: the bars, moving.
    Bars,
    Spinner,
    SpinnerWarn,
    /// The place can't play now: the warning mark.
    Warn,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Act {
    pub label: String,
    pub spoken: Option<String>,
    pub command: Command,
}
impl Act {
    pub(super) fn new(label: String, command: Command) -> Self {
        Self {
            label,
            spoken: None,
            command,
        }
    }
    pub(super) fn open(label: String, page: Open) -> Self {
        Self::new(label, Command::Open(page))
    }
    pub(super) fn spoken(mut self, spoken: String) -> Self {
        self.spoken = Some(spoken);
        self
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListPart {
    pub this_pc: PcRow,
    pub rows: Vec<Row>,
    pub more: Option<Fold>,
    pub search: Option<SearchLine>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcRow {
    pub name: String,
    pub output: String,
    pub marked: bool,
    pub command: Option<Command>,
    pub tooltip: Option<String>,
    pub arrow: Option<Act>,
    pub outputs: Vec<OutputRow>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputRow {
    pub name: String,
    pub tooltip: String,
    pub marked: bool,
    pub command: Command,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub place: Place,
    pub icon: RowIcon,
    pub name: String,
    pub state: RowState,
    pub text: Option<String>,
    pub mark: Option<Mark>,
    pub link: Option<Act>,
    pub grey: bool,
    pub marked: bool,
    pub command: Option<Command>,
    pub tooltip: Option<String>,
    pub gear: Act,
    pub spoken: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fold {
    pub label: String,
    pub rows: Vec<Row>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchLine {
    pub text: String,
    pub spinner: bool,
    pub link: Option<Act>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum About {
    Delay(DelayControls),
    Note(NoteView),
    Question(QuestionView),
    Line { text: String, link: Option<Act> },
    Empty,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteView {
    pub mark: Mark,
    pub title: String,
    pub sentence: String,
    pub button: Option<Act>,
    /// The ×, with its name.
    pub close: Option<Act>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Warn,
    Info,
    Busy,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionView {
    pub title: String,
    pub sentence: String,
    pub yes: Act,
    pub no: Act,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelayControls {
    pub speaker: SpeakerId,
    pub head: DelayHead,
    pub rows: [DelayRow; 2],
    pub line: Option<String>,
    pub dimmed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelayHead {
    One(Figured),
    Ways { label: String, lines: Vec<WayLine> },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Figured {
    pub before: String,
    pub figure: String,
    pub after: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WayLine {
    pub way: WayChoice,
    pub chosen: bool,
    pub name: String,
    pub gives: Figured,
    pub command: Option<Command>,
    pub connecting: Option<Act>,
    pub spoken: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelayRow {
    pub question: String,
    pub verb: String,
    pub small: Act,
    pub large: Act,
    pub enabled: bool,
}

pub fn panel(scene: &Scene) -> Panel {
    let lang = scene.lang;
    let list = list(scene);
    Panel {
        place: place_part(scene),
        about: about(scene, &list),
        list,
        links: [
            Act::open(
                t!(lang, CantHear),
                Open::CantHear(scene.situation.place_speaker().cloned()),
            ),
            Act::open(t!(lang, SpeakerMissing), Open::SpeakerMissing),
            Act::open(t!(lang, Settings), Open::Settings),
        ],
    }
}

pub(super) fn place_part(scene: &Scene) -> PlacePart {
    let lang = scene.lang;
    let (name, output, owner, level) = match &scene.situation.spot {
        Spot::ThisPc(id) => (
            t!(lang, ThisPc),
            scene.output(id.as_ref()).map(|o| scene.output_label(o)),
            id.clone().map(VolumeOwner::PcOutput),
            scene.volume.pc,
        ),
        Spot::Speaker(id) => (
            scene.speaker_name(id),
            None,
            Some(if id.is_music_assistant() {
                VolumeOwner::MusicAssistant
            } else {
                VolumeOwner::Speaker(id.clone())
            }),
            scene.volume.place,
        ),
    };
    let volume = level.map(|level| VolumeView {
        dimmed: owner.is_none(),
        owner,
        level: level.value,
        muted: level.muted,
        number: t!(lang, VolumeNumber, level = level.value),
        limit: level.limit,
        spoken: t!(lang, PlaceVolume, place = &name),
        mute_spoken: if level.muted {
            t!(lang, UnmutePlace, place = &name)
        } else {
            t!(lang, MutePlace, place = &name)
        },
    });
    PlacePart {
        name,
        output,
        volume,
        state: state_line(scene),
    }
}

fn state_line(scene: &Scene) -> StateLine {
    let lang = scene.lang;
    let sit = &scene.situation;
    let line = |sign, text| StateLine {
        sign,
        text,
        link: None,
    };
    if let Some(id) = sit.place_speaker() {
        if matches!(sit.doing, Doing::Trying | Doing::Connecting) {
            let ahead = sit
                .note
                .as_ref()
                .filter(|n| n.ahead == Some(Ahead::InAFewSeconds));
            return match ahead.map(|n| &n.cause) {
                Some(Cause::NetworkLost { .. }) => {
                    line(Sign::SpinnerWarn, t!(lang, RetryNoNetwork))
                }
                Some(Cause::NotRespondingAtStart {
                    why: Away::NoNetwork,
                    ..
                }) => line(Sign::SpinnerWarn, t!(lang, WaitForNetwork)),
                None if sit.doing == Doing::Connecting => line(Sign::Spinner, t!(lang, Connecting)),
                _ => line(Sign::SpinnerWarn, t!(lang, RetryNotResponding)),
            };
        }
        if sit
            .standing
            .iter()
            .any(|s| matches!(s, Standing::CantReadSound { .. }))
        {
            return line(Sign::Warn, t!(lang, RowCantReadSound));
        }
        if sit
            .standing
            .iter()
            .any(|s| matches!(s, Standing::PausedAtSpeaker { .. }))
        {
            return line(Sign::Still, t!(lang, PausedOnSpeaker));
        }
        if sit.standing.contains(&Standing::MusicAssistantNotConnected) {
            return line(Sign::Warn, t!(lang, RowNotConnected));
        }
        if sit.standing.contains(&Standing::MusicAssistantNotStarted) {
            return line(Sign::Still, t!(lang, NotPlayingYet));
        }
        if let Some(away) = sit.away {
            return line(
                Sign::Warn,
                match away {
                    Away::NotResponding => t!(lang, NotResponding),
                    // The app doesn't look for Music Assistant: it connects to the app.
                    Away::NotFound if sit.place_speaker().is_some_and(|id| id.is_music_assistant()) => t!(lang, RowNotConnected),
                    Away::NotFound => t!(lang, RowNotFoundShort),
                    Away::NoNetwork => t!(lang, NoNetworkConnection),
                },
            );
        }
        if sit.doing == Doing::Playing {
            if let Some(Standing::RoomJoined { room }) = sit
                .standing
                .iter()
                .find(|s| matches!(s, Standing::RoomJoined { .. }))
            {
                return line(Sign::Bars, t!(lang, PlayingInJoinedRoom, room = room));
            }
        }
        if let Some(Standing::AppElsewhere { app, .. }) = sit
            .standing
            .iter()
            .find(|s| matches!(s, Standing::AppElsewhere { .. }))
        {
            let state = match sit.doing {
                Doing::Playing => Some((Sign::Bars, t!(lang, PlayingExcept, app = app))),
                Doing::NothingPlaying => Some((Sign::Still, t!(lang, AppPlaysOnPc, app = app))),
                _ => None,
            };
            if let Some((sign, text)) = state {
                return StateLine {
                    sign,
                    text,
                    link: Some(Act::open(t!(lang, Why), Open::CantHear(Some(id.clone())))),
                };
            }
        }
        if sit.doing == Doing::Playing
            && sit.standing.contains(&Standing::OnlySomeApps {
                none_playing: false,
            })
        {
            return line(Sign::Bars, t!(lang, PlayingChosenApps));
        }
        if sit
            .standing
            .contains(&Standing::OnlySomeApps { none_playing: true })
        {
            return StateLine {
                sign: Sign::Still,
                text: t!(lang, OnlyChosenApps),
                link: Some(Act::open(t!(lang, ChooseApps), Open::VolumeMixer)),
            };
        }
    }
    match sit.doing {
        Doing::Playing => line(Sign::Bars, t!(lang, Playing)),
        Doing::NothingToPlayOn if sit.place_speaker().is_none() => {
            line(Sign::Warn, t!(lang, NothingToPlayOn))
        }
        _ => line(Sign::Still, t!(lang, NothingPlaying)),
    }
}

pub(super) fn switch(place: Place) -> Command {
    Command::Switch { place, way: None }
}

fn cancel_on_place(scene: &Scene) -> Option<Command> {
    scene
        .situation
        .picking
        .as_ref()
        .filter(|p| p.can_cancel)
        .map(|_| Command::CancelSwitch)
}

fn pc_row(scene: &Scene) -> PcRow {
    let lang = scene.lang;
    let output_id = scene.situation.pc_output.as_ref();
    let marked = scene.situation.place_speaker().is_none();
    let arrow = (scene.outputs.len() > 1).then(|| {
        Act::new(
            t!(lang, PcOutputs),
            Command::ShowPcOutputs(!scene.outputs_open),
        )
    });
    let outputs = if scene.outputs_open && arrow.is_some() {
        scene
            .outputs
            .iter()
            .map(|o| OutputRow {
                name: scene.output_label(o),
                tooltip: o.friendly.clone(),
                marked: Some(&o.id) == output_id,
                command: Command::SwitchOutput(o.id.clone()),
            })
            .collect()
    } else {
        Vec::new()
    };
    PcRow {
        name: t!(lang, ThisPc),
        output: scene
            .output(output_id)
            .map(|o| scene.output_label(o))
            .unwrap_or_else(|| t!(lang, NoOutputConnected)),
        marked,
        command: if marked {
            cancel_on_place(scene)
        } else {
            Some(switch(Place::ThisPc))
        },
        tooltip: (!marked).then(|| t!(lang, SwitchToPc)),
        arrow,
        outputs,
    }
}

impl SpeakerSeen {
    fn has_native(&self) -> bool {
        self.ways.iter().any(|w| w.way == WayChoice::Native)
    }

    pub(super) fn adjustable_way(&self) -> Option<WayChoice> {
        self.ways.iter().find(|w| w.applies != Applies::Fixed).map(|w| w.way)
    }

    pub(super) fn way_name(&self, lang: Lang, way: WayChoice, with_native: bool) -> String {
        match way {
            WayChoice::Native => match &self.kind {
                SpeakerKind::Sonos | SpeakerKind::SonosGroup => t!(lang, KindSonos),
                SpeakerKind::Other { maker: Some(maker) } => maker.clone(),
                _ => t!(lang, Directly),
            },
            _ if with_native => t!(lang, KindAirPlay),
            WayChoice::AirPlayShort => t!(lang, LessDelay),
            WayChoice::AirPlaySteady => t!(lang, Steadier),
        }
    }

    fn icon(&self) -> RowIcon {
        if self.id.is_music_assistant() {
            return RowIcon::MusicAssistant;
        }
        match self.kind {
            SpeakerKind::SonosGroup => RowIcon::Group,
            SpeakerKind::HomePodPair => RowIcon::HomePodPair,
            SpeakerKind::AppleTv => RowIcon::Tv,
            _ => RowIcon::Speaker,
        }
    }
}

pub(crate) fn speaker_row(scene: &Scene, speaker: &SpeakerSeen) -> Row {
    let lang = scene.lang;
    let id = &speaker.id;
    let bare = speaker.display_name(lang);
    let name = speaker.row_name(scene);
    let state = speaker.row_state(scene);
    let (text, spoken) = match state {
        RowState::Plain => (None, name.clone()),
        RowState::Connecting => (
            Some(t!(lang, Connecting)),
            t!(lang, SpokenConnecting, name = &name),
        ),
        RowState::NotResponding => (
            Some(t!(lang, NotResponding)),
            t!(lang, SpokenNotResponding, name = &name),
        ),
        // The app doesn't look for Music Assistant: it connects to the app.
        RowState::NotFound if id.is_music_assistant() => (
            Some(t!(lang, RowNotConnected)),
            t!(lang, SpokenNotConnected, name = &name),
        ),
        RowState::NotFound => (
            Some(t!(lang, RowNotFoundShort)),
            t!(lang, SpokenNotFound, name = &name),
        ),
        RowState::PlayingElse => (
            Some(t!(lang, RowPlayingElse)),
            t!(lang, SpokenPlayingElse, name = &name),
        ),
        RowState::MayBeInUse => (
            Some(t!(lang, MayBeInUse)),
            t!(lang, SpokenMayBeInUse, name = &name),
        ),
        RowState::RepairFirst => (
            Some(t!(lang, RowRepairFirst)),
            t!(lang, SpokenRepairFirst, name = &name),
        ),
    };
    let mark = match state {
        RowState::Connecting => Some(Mark::Busy),
        RowState::NotResponding => Some(Mark::Warn),
        _ => None,
    };
    let link = matches!(state, RowState::NotResponding | RowState::NotFound).then(|| {
        if id.is_music_assistant() {
            // Its own page says whether it is connected and what that takes.
            Act::open(t!(lang, Why), Open::MusicAssistantSettings).spoken(t!(lang, MaWhySpoken))
        } else {
            Act::open(t!(lang, Why), Open::SpeakerMissing).spoken(t!(
                lang,
                MissingWhySpoken,
                speaker = &name
            ))
        }
    });
    let grey = state.grey();
    let marked = scene.situation.place_speaker() == Some(id);
    let place = Place::of_speaker(id);
    let command = if grey {
        None
    } else if marked {
        cancel_on_place(scene)
    } else {
        Some(switch(place.clone()))
    };
    let tooltip = (!grey && !marked).then(|| {
        if speaker.has_native() && speaker.ways.iter().any(|w| w.way != WayChoice::Native) {
            t!(lang, SwitchByWay, speaker = bare.as_ref(), way = &speaker.way_name(lang, speaker.way, true))
        } else {
            t!(lang, SwitchToSpeaker, speaker = bare.as_ref())
        }
    });
    let gear = if id.is_music_assistant() {
        Act::open(t!(lang, MaGear), Open::MusicAssistantSettings)
    } else {
        Act::open(t!(lang, SpeakerGear, speaker = bare.as_ref()), Open::SpeakerSettings(id.clone()))
    };
    Row {
        place,
        icon: speaker.icon(),
        gear,
        name,
        state,
        text,
        mark,
        link,
        grey,
        marked,
        command,
        tooltip,
        spoken,
    }
}

fn list(scene: &Scene) -> ListPart {
    let lang = scene.lang;
    let all_new = scene.speakers.iter().all(|s| !s.used_here);
    let crowded = scene.speakers.len() >= UNFOLDED_ROWS;
    let mut rows = Vec::new();
    let mut folded = Vec::new();
    for s in &scene.speakers {
        let row = speaker_row(scene, s);
        let picking = scene
            .situation
            .picking
            .as_ref()
            .is_some_and(|p| p.speaker == s.id);
        if (all_new || crowded)
            && !s.used_here
            && !row.marked
            && !picking
            && row.state != RowState::Connecting
        {
            folded.push(row);
        } else {
            rows.push(row);
        }
    }
    let more = (!folded.is_empty()).then(|| {
        let count = folded.len();
        let wifi = scene.network == Some(ConnKind::WiFi);
        let label = if !all_new {
            t!(lang, RowMoreSpeakers, count = count)
        } else if wifi {
            t!(lang, FoundOnWifi, count = count)
        } else {
            t!(lang, FoundOnNetwork, count = count)
        };
        Fold {
            label,
            rows: folded,
        }
    });
    let search = if scene.speakers.is_empty() { search_line(scene) } else { None };
    ListPart {
        this_pc: pc_row(scene),
        rows,
        more,
        search,
    }
}

pub(crate) fn search_line(scene: &Scene) -> Option<SearchLine> {
    let lang = scene.lang;
    match scene.search.phase {
        SearchPhase::Found | SearchPhase::Off => None,
        SearchPhase::Looking => Some(SearchLine { text: t!(lang, Searching), spinner: true, link: None }),
        SearchPhase::NoneFound => Some(none_found(lang)),
    }
}

fn none_found(lang: Lang) -> SearchLine {
    SearchLine {
        text: t!(lang, NoSpeakersOnNetwork),
        spinner: false,
        link: Some(Act::open(t!(lang, Why), Open::SpeakerMissing)),
    }
}

pub(crate) fn first_run_search(scene: &Scene) -> Option<SearchLine> {
    if scene.search.phase == SearchPhase::Off {
        return scene.speakers.is_empty().then(|| none_found(scene.lang));
    }
    search_line(scene).or_else(|| Some(SearchLine {
        text: if scene.speakers.is_empty() { t!(scene.lang, Searching) } else { t!(scene.lang, LookingForMore) },
        spinner: true,
        link: None,
    }))
}

fn about(scene: &Scene, list: &ListPart) -> About {
    let sit = &scene.situation;
    let lang = scene.lang;
    if let Some(q) = &sit.confirm_in_panel {
        return About::Question(notes::question(scene, q));
    }
    if sit.standing.contains(&Standing::OutputMissing) || matches!(scene.repair, RepairNow::Running | RepairNow::Worked | RepairNow::DidntWork) {
        return About::Note(notes::repair(scene));
    }
    if sit.standing.contains(&Standing::NoSpeakerChosen) {
        return About::Note(notes::no_speaker_chosen(scene));
    }
    if let Some(n) = &sit.note {
        if n.shows()
            && !sit
                .picking
                .as_ref()
                .is_some_and(|p| &p.speaker == n.cause.speaker())
        {
            return About::Note(notes::note(scene, n));
        }
    }
    if let Some(p) = &sit.picking {
        if p.by == SwitchBy::Way && Some(&p.speaker) == sit.place_speaker() {
            return delay_about(scene, &p.speaker);
        }
        let speaker = scene.speaker_name(&p.speaker);
        return About::Line {
            text: if sit.only_some_apps { t!(lang, ConnectingToSpeaker, speaker = &speaker) } else { t!(
                lang,
                ConnectingFromPlace,
                speaker = &speaker,
                place = &scene.place_name()
            ) },
            link: p.can_cancel.then(|| cancel_connecting(lang, &speaker)),
        };
    }
    if let Some(note) = notes::standing(scene) {
        return About::Note(note);
    }
    if let Some(id) = sit.place_speaker() {
        if id.is_music_assistant() {
            // Music Assistant sets its speakers' delay, so the part has
            // no delay controls; it says where they are and what the
            // volume slider sets.
            return About::Line { text: t!(lang, MaPlaceLine), link: None };
        }
        return delay_about(scene, id);
    }
    if list
        .rows
        .iter()
        .chain(list.more.iter().flat_map(|f| &f.rows))
        .any(|row| row.command.is_some())
    {
        About::Line {
            text: if sit.only_some_apps { t!(lang, ClickSpeakerApps) } else { t!(lang, ClickSpeaker) },
            link: None,
        }
    } else {
        About::Empty
    }
}

fn cancel_connecting(lang: Lang, speaker: &str) -> Act {
    Act::new(t!(lang, Cancel), Command::CancelSwitch).spoken(t!(
        lang,
        CancelConnecting,
        speaker = speaker
    ))
}

/// The panel's delay figure, with millisecond resolution below a second.
pub fn delay_figure(lang: Lang, delay: SpeakerDelay) -> String {
    format_delay(lang, delay.total_ms(), delay.measured)
}

fn format_delay(lang: Lang, ms: i64, measured: bool) -> String {
    if ms < 1000 {
        if measured {
            t!(lang, SetLabMs, ms = ms)
        } else {
            t!(lang, AboutMs, ms = ms)
        }
    } else {
        let seconds = Number {
            value: (ms + 5) / 10,
            scale: 2,
        };
        if measured {
            t!(lang, Seconds, seconds = seconds)
        } else {
            t!(lang, FigureAbout, seconds = seconds)
        }
    }
}

pub(crate) fn delay_about(scene: &Scene, id: &SpeakerId) -> About {
    let Some(speaker) = scene.speaker(id) else {
        return About::Empty;
    };
    let Some(current) = speaker.ways.iter().find(|w| w.way == speaker.way) else {
        return About::Empty;
    };
    let lang = scene.lang;
    let name = scene.speaker_name(id);
    let changing = scene
        .situation
        .picking
        .as_ref()
        .filter(|p| p.by == SwitchBy::Way && p.speaker == *id);
    let here = scene.situation.place_speaker() == Some(id);
    let dimmed = changing.is_some() || (here && (scene.situation.away.is_some()
        || matches!(scene.situation.doing, Doing::Connecting | Doing::Trying)));

    let alternative = speaker.adjustable_way().filter(|_| current.applies == Applies::Fixed);
    let head = delay_head(lang, speaker, &name, current, changing, dimmed, alternative);
    let before = current.delay.change_ms;
    let at_limit = |step| press(current.range, before, step).after_ms == before;
    let lower = at_limit(DelayStep::Remove(1));
    let upper = at_limit(DelayStep::Add(1));
    let row = |remove| {
        let button = |ms| {
            let (step, spoken) = if remove {
                (DelayStep::Remove(ms), t!(lang, RemoveMs, ms = ms))
            } else {
                (DelayStep::Add(ms), t!(lang, AddMs, ms = ms))
            };
            Act::new(
                t!(lang, SetLabMs, ms = ms),
                Command::ChangeDelay {
                    speaker: id.clone(),
                    step,
                },
            )
            .spoken(spoken)
        };
        DelayRow {
            question: if remove {
                t!(lang, SoundBehind)
            } else {
                t!(lang, SoundBreakingUp)
            },
            verb: if remove {
                t!(lang, RemoveBare)
            } else {
                t!(lang, AddBare)
            },
            small: button(scene.steps.small_ms),
            large: button(scene.steps.large_ms),
            enabled: !dimmed
                && current.applies != Applies::Fixed
                && !(if remove { lower } else { upper }),
        }
    };
    let line = match if here { scene.progress } else { Progress::Settled } {
        Progress::Live { to_go_ms } if to_go_ms < 0 => {
            Some(t!(lang, Removing, ms = to_go_ms.unsigned_abs()))
        }
        Progress::Live { to_go_ms } => Some(t!(lang, Adding, ms = to_go_ms)),
        Progress::ReconnectPending | Progress::Reconnecting => Some(t!(lang, ApplyingNewDelay)),
        Progress::Settled if current.applies == Applies::Fixed => {
            let other_line = match &head {
                DelayHead::Ways { lines, .. } => lines.iter().find(|l| Some(l.way) == alternative),
                DelayHead::One(_) => None,
            };
            Some(match other_line {
                Some(l) => t!(lang, FixedWayDelay, way = &l.name),
                None => t!(lang, FixedSpeakerDelay, speaker = &name),
            })
        }
        Progress::Settled if lower => Some(t!(lang, LowerDelayLimit, speaker = &name)),
        Progress::Settled if upper => Some(t!(lang, UpperDelayLimit, speaker = &name)),
        Progress::Settled
            if here && current.applies == Applies::Reconnect && scene.situation.doing == Doing::Playing =>
        {
            Some(t!(lang, ChangeStopsSound, speaker = &name))
        }
        Progress::Settled => None,
    };
    About::Delay(DelayControls {
        speaker: id.clone(),
        head,
        rows: [row(true), row(false)],
        line,
        dimmed,
    })
}

fn delay_head(
    lang: Lang,
    speaker: &SpeakerSeen,
    name: &str,
    current: &WaySeen,
    changing: Option<&Picking>,
    dimmed: bool,
    alternative: Option<WayChoice>,
) -> DelayHead {
    // Keep the active or pending AirPlay line visible even if the saved preference is stale.
    let has_native = speaker.has_native();
    let airplay = [
        changing.map(|p| p.way),
        Some(current.way),
        speaker.airplay,
        Some(WayChoice::AirPlaySteady),
        Some(WayChoice::AirPlayShort),
    ]
    .into_iter()
    .flatten()
    .find(|&way| way != WayChoice::Native && speaker.ways.iter().any(|w| w.way == way));
    // A remedy that says “above” must lead to a visible, adjustable line.
    let visible = speaker.ways.iter().filter(|w| {
        !has_native || w.way == WayChoice::Native || Some(w.way) == airplay || Some(w.way) == alternative
    });
    let one_airplay_with_native = visible.clone().any(|w| w.way == WayChoice::Native)
        && visible.clone().filter(|w| w.way != WayChoice::Native).count() == 1;
    if visible.clone().count() <= 1 {
        DelayHead::One(Figured {
            before: t!(lang, SpeakerIs, speaker = name),
            figure: delay_figure(lang, current.delay),
            after: t!(lang, BehindPictureEnd),
        })
    } else {
        let lines = visible
            .map(|w| {
                let chosen = w.way == current.way;
                let pending = changing.filter(|p| p.way == w.way);
                let gives = if pending.is_some() {
                    Figured {
                        before: t!(lang, Connecting),
                        figure: String::new(),
                        after: String::new(),
                    }
                } else {
                    Figured {
                        before: String::new(),
                        figure: delay_figure(lang, w.delay),
                        after: if one_airplay_with_native && w.way == WayChoice::AirPlaySteady {
                            t!(lang, BehindSteadier)
                        } else {
                            t!(lang, BehindPicture)
                        },
                    }
                };
                let way_name = speaker.way_name(lang, w.way, one_airplay_with_native);
                let spoken = lang.join_parts(&[
                    way_name.clone(),
                    [&gives.before[..], &gives.figure, &gives.after].concat(),
                ]);
                WayLine {
                    way: w.way,
                    chosen,
                    name: way_name,
                    gives,
                    spoken,
                    connecting: pending
                        .filter(|p| p.can_cancel)
                        .map(|_| cancel_connecting(lang, name)),
                    command: (!chosen && !dimmed).then(|| Command::ChooseWay {
                        speaker: speaker.id.clone(),
                        way: w.way,
                    }),
                }
            })
            .collect();
        DelayHead::Ways {
            label: t!(lang, WaysToSpeaker, speaker = name),
            lines,
        }
    }
}
