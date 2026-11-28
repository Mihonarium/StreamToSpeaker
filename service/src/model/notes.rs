//! The shared words of a note or a question, in the panel or by the clock.

use super::command::{Command, Open};
use super::facts::OutputKind;
use super::panel::{switch, Act, Mark, NoteView, QuestionView};
use super::scene::{RepairNow, Scene};
use super::situation::{
    Ahead, Away, Cause, Confirm, ConnKind, Note, NoteButton, PickWhy, RoutingQuestion, Standing,
};
use super::state::{Place, Secret, SpeakerId};
use super::text::{t, Lang};

pub(super) fn switch_to_pc(lang: Lang) -> Act {
    Act::new(t!(lang, SwitchToPc), switch(Place::ThisPc))
}

fn busy_title(lang: Lang, speaker: &str, who: Option<&str>) -> String {
    match who {
        Some(pc) => t!(lang, PlayingOtherPc, speaker = speaker, pc = pc),
        None => t!(lang, NotifyTakenTitle, speaker = speaker),
    }
}

fn away_title(lang: Lang, name: &str, why: Away) -> String {
    match why {
        Away::NotResponding => t!(lang, DidntRespond, speaker = name),
        Away::NotFound => t!(lang, SpeakerNotFound, speaker = name),
        Away::NoNetwork => t!(lang, PcNotConnected),
    }
}

pub(crate) fn didnt_get_ready(
    scene: &Scene,
    speaker: &SpeakerId,
    why: PickWhy,
) -> (String, String) {
    let title = failure_title(scene.lang, &scene.speaker_name(speaker), why);
    let sentence = if scene.situation.only_some_apps { t!(scene.lang, ChosenAppsOn, place = &scene.place_name()) }
        else { t!(scene.lang, SoundStillOn, place = &scene.place_name()) };
    (title, sentence)
}

fn failure_title(lang: Lang, name: &str, why: PickWhy) -> String {
    match why {
        PickWhy::NotResponding => away_title(lang, name, Away::NotResponding),
        PickWhy::NotFound => away_title(lang, name, Away::NotFound),
        PickWhy::NoNetwork => away_title(lang, name, Away::NoNetwork),
        PickWhy::NeedsSecret(Secret::Code) => t!(lang, NeedsCode, speaker = name),
        PickWhy::NeedsSecret(Secret::Password) => t!(lang, NeedsPassword, speaker = name),
        PickWhy::WrongSecret(Secret::Code) => t!(lang, CodeDidntWork),
        PickWhy::WrongSecret(Secret::Password) => t!(lang, PasswordDidntWork),
        PickWhy::CouldntStart => t!(lang, CouldntStartPlaying, speaker = name),
        PickWhy::Refused => t!(lang, RefusedConnection, speaker = name),
    }
}

fn lost_network(lang: Lang, kind: ConnKind) -> String {
    if kind == ConnKind::WiFi {
        t!(lang, LostWifi)
    } else {
        t!(lang, LostNetwork)
    }
}

fn ahead_title(lang: Lang, name: &str, cause: &Cause) -> String {
    match cause {
        Cause::NetworkLost { kind, .. } => lost_network(lang, *kind),
        Cause::NotRespondingAtStart {
            why: why @ (Away::NotFound | Away::NoNetwork),
            ..
        } => away_title(lang, name, *why),
        Cause::PlayingElse { who, .. } => busy_title(lang, name, who.as_deref()),
        Cause::GroupedInto { with, .. } => t!(lang, GroupedWith, speaker = name, room = with),
        Cause::Disconnected { .. } => t!(lang, StoppedPlayingPc, speaker = name),
        _ => t!(lang, NotRespondingTitle, speaker = name),
    }
}

fn words(scene: &Scene, n: &Note, name: &str) -> (String, String) {
    let lang = scene.lang;
    if let Some(ahead) = n.ahead {
        return (
            ahead_title(lang, name, &n.cause),
            match ahead {
                Ahead::InAFewSeconds if scene.situation.only_some_apps => t!(lang, ChosenAppsMoveSoon),
                Ahead::WhenSoundStarts if scene.situation.only_some_apps => t!(lang, ChosenAppsWhenStarting),
                Ahead::InAFewSeconds => t!(lang, MoveInSeconds),
                Ahead::WhenSoundStarts => t!(lang, MoveWhenSoundStarts),
            },
        );
    }
    let available = || {
        (
            t!(lang, AvailableAgain, speaker = name),
            if scene.situation.only_some_apps { t!(lang, ChosenAppsOn, place = &scene.place_name()) }
            else { t!(lang, StaysUntilSwitch) },
        )
    };
    if n.available_again {
        return available();
    }
    let when =
        n.at.map(|time| t!(lang, AtTime, time = &lang.time(time)))
            .unwrap_or_default();
    let moved = || if scene.situation.only_some_apps { t!(lang, ChosenAppsMoved, when = &when) }
        else { t!(lang, MovedToPc, when = &when) };
    match &n.cause {
        Cause::StoppedResponding { vpn, .. } => {
            let sentence = match vpn {
                None => moved(),
                Some(vpn) => match &vpn.name {
                    Some(name) => t!(lang, MovedAfterVpn, when = &when, vpn = name),
                    None => t!(lang, MovedAfterConnection, when = &when),
                },
            };
            (t!(lang, NotifyStoppedTitle, speaker = name), sentence)
        }
        Cause::NetworkLost { kind, .. } => (lost_network(lang, *kind), moved()),
        Cause::NotRespondingAtStart { why, .. } => (away_title(lang, name, *why), moved()),
        Cause::NeedsSecret { secret, .. } => (
            match secret {
                Secret::Code => t!(lang, AsksForCode, speaker = name),
                Secret::Password => t!(lang, AsksForPassword, speaker = name),
            },
            moved(),
        ),
        Cause::PlayingElse { who, .. } => (
            busy_title(lang, name, who.as_deref()),
            t!(lang, SwitchingBackStops),
        ),
        Cause::Disconnected { .. } => (
            t!(lang, StoppedPlayingPc, speaker = name),
            t!(lang, DeviceTookOver),
        ),
        Cause::GroupedInto { with, .. } => (
            t!(lang, GroupedWith, speaker = name, room = with),
            t!(lang, MovedAfterGroup, when = &when, speaker = name),
        ),
        Cause::OtherNetwork { .. } => available(),
        Cause::Removed { .. } => (
            t!(lang, YouRemoved, speaker = name),
            if scene.situation.only_some_apps { t!(lang, ChosenAppsMoved, when = "") } else { t!(lang, MovedToPc, when = "") },
        ),
        Cause::PlayingAtStart { .. } => (t!(lang, SoundStayedHere), t!(lang, PlayingAtStartup)),
        Cause::HeadphonesGone { name, .. } => (
            t!(lang, OutputDisconnected, name = name),
            match scene.output(scene.situation.pc_output.as_ref()) {
                Some(o) if o.kind == OutputKind::Speakers => t!(lang, MovedToPcSpeakers, when = &when),
                output => {
                    let name = output.map(|o| scene.output_label(o)).unwrap_or_else(|| t!(lang, ThisPcInSentence));
                    t!(lang, MovedToOutput, output = &name, when = &when)
                }
            },
        ),
        Cause::HeadphonesConnected { name: output, .. } => {
            let headphones = scene
                .output(scene.situation.pc_output.as_ref())
                .is_some_and(|o| o.kind == OutputKind::Headphones);
            (
                t!(lang, OutputConnected, name = output),
                if headphones {
                    t!(lang, FromSpeakerHeadphones, speaker = name)
                } else {
                    t!(lang, FromSpeakerOutput, speaker = name)
                },
            )
        }
        Cause::SwitchedAfterQuiet { .. } => (
            t!(lang, QuietForThirty),
            t!(lang, MovedFromSpeaker, speaker = name, when = &when),
        ),
        Cause::WayFailed { why, .. } => (failure_title(lang, name, *why), moved()),
        Cause::DidntGetReady { speaker, why } => didnt_get_ready(scene, speaker, *why),
    }
}

pub fn note(scene: &Scene, n: &Note) -> NoteView {
    let lang = scene.lang;
    let name = scene.speaker_name(n.cause.speaker());
    let (title, sentence) = words(scene, n, &name);
    let button = n.button().map(|b| match b {
        NoteButton::TryAgain(id) => Act::new(
            t!(lang, TrySpeakerAgain, speaker = &name),
            Command::TryAgain(id),
        ),
        NoteButton::SwitchTo(id) => Act::new(
            t!(lang, SwitchToSpeaker, speaker = &name),
            switch(Place::of_speaker(&id)),
        ),
        NoteButton::EnterSecret(id, secret) => Act::open(
            match secret {
                Secret::Code => t!(lang, EnterTheCode),
                Secret::Password => t!(lang, EnterThePassword),
            },
            Open::Pair(id),
        ),
        NoteButton::SwitchToThisPc if n.ahead == Some(Ahead::InAFewSeconds) => {
            Act::new(t!(lang, SwitchToPcNow), switch(Place::ThisPc))
        }
        NoteButton::SwitchToThisPc => switch_to_pc(lang),
    });
    NoteView {
        mark: if n.failure() { Mark::Warn } else { Mark::Info },
        title,
        sentence,
        button,
        close: n
            .can_close()
            .then(|| Act::new(t!(lang, Close), Command::CloseNote)),
    }
}

pub(super) fn no_speaker_chosen(scene: &Scene) -> NoteView {
    NoteView {
        mark: Mark::Info,
        title: t!(scene.lang, NoSpeakerChosen),
        sentence: t!(scene.lang, ChooseInList, place = &scene.place_name()),
        button: None,
        close: Some(Act::new(t!(scene.lang, Close), Command::CloseNote)),
    }
}

pub(crate) fn repair(scene: &Scene) -> NoteView {
    let lang = scene.lang;
    let repair = || Some(Act::new(t!(lang, RepairOutput), Command::RepairOutput));
    // An output that went missing again needs a new repair.
    let state = if scene.repair == RepairNow::Worked
        && scene.situation.standing.contains(&Standing::OutputMissing)
    {
        RepairNow::NotAsked
    } else {
        scene.repair
    };
    let (mark, title, sentence, button) = match state {
        RepairNow::Unavailable => (
            Mark::Warn,
            t!(lang, AppOutputMissing),
            t!(lang, RestoreInSettings),
            Some(Act::open(t!(lang, OpenWindowsSound), Open::SoundSettings)),
        ),
        RepairNow::NotAsked => (
            Mark::Warn,
            t!(lang, AppOutputMissing),
            t!(lang, RestoreOutputLine),
            repair(),
        ),
        RepairNow::TurnedOff => (
            Mark::Warn,
            t!(lang, OutputTurnedOff),
            t!(lang, TurnOnOutputLine),
            Some(Act::new(t!(lang, RepairOutput), Command::TurnOnOutput)),
        ),
        RepairNow::TurnOnFailed => (
            Mark::Warn,
            t!(lang, OutputTurnedOff),
            t!(lang, TurnOnFailedLine),
            Some(Act::open(t!(lang, OpenWindowsSound), Open::SoundSettings)),
        ),
        RepairNow::Running => (
            Mark::Busy,
            t!(lang, RepairingOutput),
            t!(lang, StaysMeanwhile),
            None,
        ),
        RepairNow::DidntWork => (
            Mark::Warn,
            t!(lang, RepairDidntWork),
            t!(lang, RestartForOutput),
            Some(Act::open(t!(lang, SetReportProblem), Open::ReportProblem)),
        ),
        RepairNow::NotApproved => (
            Mark::Warn,
            t!(lang, RepairNeedsApproval),
            t!(lang, ApprovalNotGiven),
            repair(),
        ),
        RepairNow::Worked => (
            Mark::Info,
            t!(lang, OutputIsBack),
            t!(lang, CanSwitchAgain),
            None,
        ),
    };
    NoteView {
        mark,
        title,
        sentence,
        button,
        close: None,
    }
}

pub(super) fn call_on(scene: &Scene, speaker: &SpeakerId) -> NoteView {
    let lang = scene.lang;
    NoteView {
        mark: Mark::Info,
        title: t!(lang, MicInUse),
        sentence: t!(lang, HearCallLate, speaker = &scene.speaker_name(speaker)),
        button: Some(switch_to_pc(lang)),
        close: None,
    }
}

pub(super) fn standing(scene: &Scene) -> Option<NoteView> {
    let lang = scene.lang;
    let conditions = &scene.situation.standing;
    let (title, sentence, button) = if let Some(Standing::WindowsDidntSwitch { speaker }) =
        conditions
            .iter()
            .find(|s| matches!(s, Standing::WindowsDidntSwitch { .. }))
    {
        (
            t!(lang, RefusedTitle, speaker = &scene.speaker_name(speaker)),
            t!(lang, WindowsDidntSwitchLine),
            Act::open(t!(lang, OpenWindowsSound), Open::SoundSettings),
        )
    } else if let Some(Standing::CantReadSound { speaker }) = conditions
        .iter()
        .find(|s| matches!(s, Standing::CantReadSound { .. }))
    {
        // The words of the playback check's step, and its way out.
        (
            t!(lang, CantReadSound),
            t!(
                lang,
                SetCheckInputLostLine,
                speaker = &scene.speaker_name(speaker)
            ),
            switch_to_pc(lang),
        )
    } else if conditions.contains(&Standing::MusicAssistantNotConnected) {
        (
            t!(lang, MaNotConnected),
            t!(lang, MaNotConnectedLine),
            Act::open(t!(lang, HowToConnect), Open::MusicAssistantSettings),
        )
    } else if conditions.contains(&Standing::MusicAssistantNotStarted) {
        (
            t!(lang, MaNotStarted),
            t!(lang, MaStartPcSound),
            Act::open(t!(lang, HowToStart), Open::MusicAssistantHowTo),
        )
    } else if let Some(Standing::PausedAtSpeaker { speaker }) = conditions
        .iter()
        .find(|s| matches!(s, Standing::PausedAtSpeaker { .. }))
    {
        (
            t!(
                lang,
                SpeakerWasPaused,
                speaker = &scene.speaker_name(speaker)
            ),
            t!(lang, PausedBySpeaker),
            switch_to_pc(lang),
        )
    } else if let Some(Standing::CallOn { speaker }) = conditions
        .iter()
        .find(|s| matches!(s, Standing::CallOn { .. }))
    {
        return Some(call_on(scene, speaker));
    } else {
        return None;
    };
    Some(NoteView {
        mark: Mark::Warn,
        title,
        sentence,
        button: Some(button),
        close: None,
    })
}

pub fn question(scene: &Scene, confirm: &Confirm) -> QuestionView {
    let lang = scene.lang;
    let name = scene.speaker_name(confirm.speaker());
    let switch = || t!(lang, SwitchToSpeaker, speaker = &name);
    let (title, sentence, yes) = match confirm {
        Confirm::ChimeBusy { who, .. } => (
            busy_title(lang, &name, who.as_deref()),
            t!(lang, ChimeStopsIt),
            t!(lang, PlayTestChime),
        ),
        Confirm::Busy { who, .. } => (
            busy_title(lang, &name, who.as_deref()),
            t!(lang, SwitchStopsSound, speaker = &name),
            switch(),
        ),
        Confirm::MayBeInUse { .. } => (
            t!(lang, SpeakerMayBeInUse, speaker = &name),
            t!(lang, MayBeInUseLine, speaker = &name),
            switch(),
        ),
        Confirm::ScreenReader { .. } => (
            t!(lang, ReaderWouldPlay, speaker = &name),
            t!(lang, ReaderWouldPlayLine),
            if scene.situation.confirm_under_routing == Some(RoutingQuestion::TakePlayback) {
                t!(lang, PlayPcSoundOnSpeaker, speaker = &name)
            } else {
                switch()
            },
        ),
    };
    QuestionView {
        title,
        sentence,
        yes: Act::new(yes, Command::Confirm { yes: true }),
        no: Act::new(t!(lang, Cancel), Command::Confirm { yes: false }),
    }
}
