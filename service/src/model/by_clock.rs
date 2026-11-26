//! The views that share the panel's place and words outside the panel.

use std::cmp::Reverse;

use super::command::{Command, Open};
use super::notes;
use super::panel::{place_part, switch, Act, QuestionView};
use super::scene::Scene;
use super::situation::{Cause, Confirm, Popup, Shown};
use super::state::Place;
use super::text::t;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupView {
    pub title: String,
    pub sentence: Option<String>,
    pub button: Option<Act>,
    pub link: Option<Act>,
    /// The ×, with its name.
    pub close: Act,
}

pub fn popup(scene: &Scene) -> Option<Shown<PopupView>> {
    let lang = scene.lang;
    let shown = scene.situation.popups.popup.as_ref()?;
    let mut link = None;
    let (title, sentence, button) = match &shown.what {
        Popup::WindowClosed => {
            let words = window_words(scene);
            link = Some(words.quit);
            (words.title, Some(words.sentence), None)
        }
        Popup::Note(n) => {
            let v = notes::note(scene, n);
            let own_sentence = n.available_again
                || matches!(
                    n.cause,
                    Cause::HeadphonesGone { .. }
                        | Cause::HeadphonesConnected { .. }
                        | Cause::PlayingAtStart { .. }
                        | Cause::SwitchedAfterQuiet { .. }
                        | Cause::DidntGetReady { .. }
                );
            (
                v.title,
                Some(if own_sentence {
                    v.sentence
                } else {
                    t!(lang, SoundIsOnPc)
                }),
                v.button,
            )
        }
        Popup::PlayingOn { speaker } => (
            t!(lang, PlayingTitle, speaker = &scene.speaker_name(speaker)),
            None,
            Some(notes::switch_to_pc(lang)),
        ),
        Popup::AlsoPlayingIn { room } => (
            t!(lang, NowAlsoPlaying, room = room),
            Some(t!(
                lang,
                GroupedInSonos,
                room = room,
                place = &scene.place_name()
            )),
            None,
        ),
        Popup::DidntGetReady { speaker, why } => {
            let (title, sentence) = notes::didnt_get_ready(scene, speaker, *why);
            (title, Some(sentence), None)
        }
        Popup::NoSpeakerChosen => (
            t!(lang, NoSpeakerChosen),
            Some(t!(lang, ChooseByClock, place = &scene.place_name())),
            None,
        ),
        Popup::CallOn { speaker } => {
            let v = notes::call_on(scene, speaker);
            (v.title, Some(v.sentence), v.button)
        }
    };
    Some(Shown {
        seq: shown.seq,
        since_ms: shown.since_ms,
        what: PopupView {
            title,
            sentence,
            button,
            link,
            close: Act::new(t!(lang, Close), Command::ClosePopup),
        },
    })
}

pub fn confirmation(scene: &Scene) -> Option<Shown<QuestionView>> {
    let shown = scene.situation.popups.confirmation.as_ref()?;
    let mut question = notes::question(scene, &shown.what);
    if !matches!(shown.what, Confirm::ChimeBusy { .. }) {
        question.sentence = t!(
            scene.lang,
            UntilYouSwitch,
            sentence = &question.sentence,
            place = &scene.place_name()
        );
    }
    Some(Shown {
        seq: shown.seq,
        since_ms: shown.since_ms,
        what: question,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tray {
    pub tooltip: String,
    pub accent: bool,
    pub amber: bool,
    pub menu: Vec<MenuEntry>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntry {
    pub label: String,
    pub checked: bool,
    pub command: Option<Command>,
}

impl MenuEntry {
    fn new(label: String, command: Option<Command>) -> Self {
        Self {
            label,
            checked: false,
            command,
        }
    }
    fn place(label: String, place: Place, checked: bool) -> Self {
        Self {
            label,
            checked,
            command: (!checked).then(|| switch(place)),
        }
    }
}

pub fn tray(scene: &Scene) -> Tray {
    let lang = scene.lang;
    let place = place_part(scene);
    let accent = scene.situation.place_speaker().is_some();
    let mut menu = vec![
        MenuEntry::new(t!(lang, AppName), None),
        MenuEntry::new(t!(lang, OpenShort), Some(Command::Open(Open::Panel))),
        MenuEntry::place(t!(lang, ThisPc), Place::ThisPc, !accent),
    ];
    let mut speakers: Vec<_> = scene
        .speakers
        .iter()
        .filter(|s| !s.row_state(scene).grey())
        .collect();
    speakers.sort_by_key(|s| Reverse((s.used_here, s.last_used)));
    let more = speakers.len() > 8;
    menu.extend(speakers.into_iter().take(8).map(|s| {
        MenuEntry::place(
            s.row_name(scene),
            Place::of_speaker(&s.id),
            scene.situation.place_speaker() == Some(&s.id),
        )
    }));
    if more {
        menu.push(MenuEntry::new(
            t!(lang, MoreSpeakersEllipsis),
            Some(Command::Open(Open::Panel)),
        ));
    }
    menu.push(MenuEntry::new(
        t!(lang, Settings),
        Some(Command::Open(Open::Settings)),
    ));
    menu.push(MenuEntry::new(
        if accent {
            t!(lang, QuitAndPlayOnPc)
        } else {
            t!(lang, Quit)
        },
        Some(Command::Quit),
    ));
    Tray {
        tooltip: format!("{} – {}", t!(lang, AppName), lang.join_parts(&[place.name, place.state.text])),
        amber: scene.situation.amber,
        accent,
        menu,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowWords {
    pub strip: String,
    pub title: String,
    pub sentence: String,
    pub quit: Act,
}

pub fn window_words(scene: &Scene) -> WindowWords {
    let lang = scene.lang;
    let quit = if scene.situation.place_speaker().is_some() {
        t!(lang, QuitAndPlayOnPc)
    } else {
        t!(lang, QuitTheApp)
    };
    WindowWords {
        strip: t!(lang, WindowStrip),
        title: t!(lang, AppStillRunning),
        sentence: t!(lang, IconByClock),
        quit: Act::new(quit, Command::Quit),
    }
}
