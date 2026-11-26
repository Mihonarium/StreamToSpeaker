//! The current scene and the words shared by all of its surfaces.

use super::by_clock::{self, PopupView, WindowWords};
use super::panel::{panel, About, DelayHead, Panel, QuestionView};
use super::scene::Scene;
use super::situation::Shown;
use super::state::SpeakerId;
use super::text::{t, Lang};

/// Everything the surfaces show now, made from one scene.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub version: u64,
    pub lang: Lang,
    pub scene: Scene,
    pub panel: Panel,
    pub tray: by_clock::Tray,
    pub popup: Option<Shown<PopupView>>,
    pub confirmation: Option<Shown<QuestionView>>,
    pub window: WindowWords,
    /// Windows' name for the app's output while the app runs.
    pub device_name: Option<String>,
    /// The panel's place and state on one line.
    pub status: String,
    /// The status with the delay and volume currently shown in the panel.
    pub cli_status: String,
}

/// `named` is the speaker Windows' output is named after. When the output
/// is missing there is no device to rename.
pub fn view(scene: Scene, named: Option<&SpeakerId>, output_missing: bool, version: u64) -> View {
    let lang = scene.lang;
    let panel = panel(&scene);
    let place = &panel.place;
    let heading = match &place.output {
        Some(output) => lang.join_parts(&[place.name.clone(), output.clone()]),
        None => place.name.clone(),
    };
    let status = t!(
        lang,
        PlaceStatus,
        place = &heading,
        state = &place.state.text
    );
    let mut parts = vec![status.clone()];
    if let About::Delay(controls) = &panel.about {
        match &controls.head {
            DelayHead::One(words) => {
                parts.push([words.before.as_str(), &words.figure, &words.after].concat())
            }
            DelayHead::Ways { lines, .. } => {
                if let Some(chosen) = lines.iter().find(|line| line.chosen) {
                    parts.push(chosen.spoken.clone());
                }
            }
        }
    }
    if let Some(v) = &place.volume {
        parts.push(if v.muted {
            t!(lang, CliPartMuted)
        } else {
            t!(lang, CliPartVolume, level = v.level)
        });
    }
    View {
        version,
        lang,
        panel,
        tray: by_clock::tray(&scene),
        popup: by_clock::popup(&scene),
        confirmation: by_clock::confirmation(&scene),
        window: by_clock::window_words(&scene),
        device_name: (!output_missing).then(|| match named {
            Some(id) => t!(lang, DeviceForPlace, place = &scene.speaker_name(id)),
            None => t!(lang, AppName),
        }),
        status,
        cli_status: lang.join_parts(&parts),
        scene,
    }
}

/// Windows' name for the sound output while the app isn't running (set at
/// quit and by the crash handler).
pub fn device_name_not_running(lang: Lang) -> String {
    t!(lang, DeviceNotRunning)
}

/// Never a signed number.
pub fn change_text(lang: Lang, change_ms: i32) -> Option<String> {
    match change_ms {
        0 => None,
        c if c < 0 => Some(t!(lang, ChangeRemoved, ms = c.unsigned_abs())),
        c => Some(t!(lang, ChangeAdded, ms = c)),
    }
}

use super::command::{DelayStep, Effect, Outcome, Refusal};
use super::facts::Progress;
/// A reply printed by the command line, with its exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliReply {
    pub text: String,
    /// 0 done, 3 refused (1, 2 and 4 belong to the console program: not
    /// running, bad arguments, no reply).
    pub exit: u8,
}

pub const EXIT_DONE: u8 = 0;
pub const EXIT_NOT_RUNNING: u8 = 1;
pub const EXIT_BAD_ARGUMENTS: u8 = 2;
pub const EXIT_REFUSED: u8 = 3;
pub const EXIT_NO_REPLY: u8 = 4;

/// The command line's reply to a command's outcome.
pub fn cli_reply(lang: Lang, outcome: &Outcome) -> CliReply {
    let snap = &outcome.snapshot;
    let refused = |text: String| CliReply { text, exit: EXIT_REFUSED };
    match &outcome.result {
        Ok(Effect::Done) => CliReply { text: lang.sentence(&snap.status), exit: EXIT_DONE },
        Ok(Effect::Delay(d)) => {
            let name = &snap.scene.speaker_name(&d.speaker);
            let view = snap.scene.speaker(&d.speaker).and_then(|s| s.ways.iter().find(|w| w.way == s.way));
            let moved = (d.after_ms - d.before_ms).unsigned_abs();
            if moved == 0 && !matches!(d.step, DelayStep::Reset | DelayStep::SetTotal(_)) {
                let max_removed = view.map(|v| v.range.max_removed_ms).unwrap_or(0);
                let max_added = view.map(|v| v.range.max_added_ms).unwrap_or(0);
                let text = match d.step {
                    DelayStep::Remove(_) if max_removed == 0 => t!(lang, CliNoLess, speaker = name),
                    DelayStep::Remove(_) => t!(lang, CliCantRemove, speaker = name, max = max_removed),
                    _ => t!(lang, CliCantAdd, speaker = name, max = max_added),
                };
                return refused(text);
            }
            let total = change_text(lang, d.after_ms);
            let head = match (d.step, total) {
                (DelayStep::Reset, _) | (DelayStep::SetTotal(_), None) => {
                    t!(lang, CliBackToOwn, speaker = name)
                }
                (DelayStep::SetTotal(_), Some(total)) if d.stopped_at_limit => {
                    t!(lang, CliSetToMost, speaker = name, total = &total)
                }
                (DelayStep::SetTotal(_), Some(total)) => t!(lang, CliSetTo, speaker = name, total = &total),
                (DelayStep::Remove(_), None) => t!(lang, CliRemovedToOwn, speaker = name, ms = moved),
                (DelayStep::Add(_), None) => t!(lang, CliAddedToOwn, speaker = name, ms = moved),
                (DelayStep::Remove(_), Some(total)) if d.stopped_at_limit => {
                    t!(lang, CliRemovedMost, speaker = name, ms = moved, total = &total)
                }
                (DelayStep::Add(_), Some(total)) if d.stopped_at_limit => {
                    t!(lang, CliAddedMost, speaker = name, ms = moved, total = &total)
                }
                (DelayStep::Remove(_), Some(total)) => {
                    t!(lang, CliRemoved, speaker = name, ms = moved, total = &total)
                }
                (DelayStep::Add(_), Some(total)) => {
                    t!(lang, CliAdded, speaker = name, ms = moved, total = &total)
                }
            };
            // After the dash, where the speaker is now: still applying. A
            // reply that ends at a limit, or back at the speaker's own
            // delay, already says where it is.
            let says_where = d.stopped_at_limit || d.step == DelayStep::Reset || d.after_ms == 0;
            let suffix = view.filter(|_| !says_where && snap.scene.situation.place_speaker() == Some(&d.speaker)).and_then(|_| match snap.scene.progress {
                Progress::Live { to_go_ms } if to_go_ms < 0 => Some(t!(lang, CliRemovingSuffix)),
                Progress::Live { to_go_ms } if to_go_ms > 0 => Some(t!(lang, CliAddingSuffix)),
                Progress::ReconnectPending | Progress::Reconnecting => {
                    Some(t!(lang, CliReconnectingSuffix))
                }
                _ => None,
            });
            let text = match suffix {
                Some(s) => format!("{head} {s}"),
                None => head,
            };
            CliReply { text, exit: EXIT_DONE }
        }
        Err(refusal) => refused(refusal_text(lang, refusal, snap)),
    }
}

/// A busy speaker's reply to the terminal or phone that requested a switch.
pub(crate) fn busy_reply(lang: Lang, speaker: &str, pc: Option<&str>) -> String {
    match pc {
        Some(pc) => t!(lang, BusyOtherPcTitle, speaker = speaker, pc = pc),
        None => t!(lang, BusyTitle, speaker = speaker),
    }
}

/// The words for a refused command (the command line, and the API's
/// `message`).
pub fn refusal_text(lang: Lang, refusal: &Refusal, snap: &View) -> String {
    match refusal {
        Refusal::NoDelayHere => t!(lang, RefuseNoDelay, status = &snap.status),
        Refusal::MusicAssistantDelay => t!(lang, RefuseMaDelay, status = &snap.status),
        Refusal::FixedOnSteady { speaker } => {
            let name = snap.scene.speaker_name(speaker);
            match snap.scene.speaker(speaker).and_then(|s| s.adjustable_way().map(|way| s.way_name(lang, way, false))) {
                Some(way) => t!(lang, RefuseSteady, speaker = &name, way = &way),
                None => t!(lang, FixedSpeakerDelay, speaker = &name),
            }
        }
        Refusal::UnknownPlace { name } => t!(lang, RefuseUnknownPlace, name = name.as_str()),
        Refusal::OnlyFromWindow { speaker } => {
            t!(lang, RefuseOnlyFromWindow, speaker = &snap.scene.speaker_name(speaker))
        }
        // Only the app's own windows send these commands; no other surface
        // has words for refusing them yet.
        Refusal::WindowOnly => String::new(),
    }
}
