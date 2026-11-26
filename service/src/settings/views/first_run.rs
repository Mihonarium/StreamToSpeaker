//! The root page of the ordinary window while the engine is in first run.
use egui::Ui;

use super::parts::{gap, page_button, page_link, slider_line, switch_line};
use crate::model::{
    command::FirstRunAction,
    panel::About,
    situation::{Confirm, FirstRunStep},
    text::t,
    Command, Open, Snapshot, SpeakerId,
};
use crate::shell::{Ask, ViewBar, ViewContent};
use crate::surface::ChimePicker;
use crate::ui::{button::Button, theme::size};

pub(crate) struct FirstRunPage {
    pub step: FirstRunStep,
    pub tray: bool,
    picker: ChimePicker,
}

impl FirstRunPage {
    pub fn new(step: FirstRunStep) -> Self {
        Self {
            step,
            tray: true,
            picker: ChimePicker::default(),
        }
    }

    pub fn same_page(&self, new: &FirstRunStep) -> bool {
        match (&self.step, new) {
            (FirstRunStep::Two { speaker: a, .. }, FirstRunStep::Two { speaker: b, .. }) => a == b,
            (a, b) => a == b,
        }
    }
}

fn action(ask: &mut dyn FnMut(Ask), act: FirstRunAction) {
    ask(Ask::Command(Command::FirstRunAction(act)));
}

fn plain(ui: &mut Ui, words: &str) {
    let t = crate::ui::theme::Theme::of(ui.ctx());
    crate::surface::grid::page_paragraph(ui, words, t.font(size::BODY), t.c.text);
}

fn question(ui: &mut Ui, snap: &Snapshot, speaker: &SpeakerId, ask: &mut dyn FnMut(Ask)) {
    if let Some(failure) = snap.scene.situation.pick_failed.as_ref().filter(|p|
        &p.speaker == speaker && (p.by == crate::model::situation::SwitchBy::FirstRun
            || matches!(snap.scene.situation.first_run, Some(FirstRunStep::Two { .. })))) {
        let (title, _) = crate::model::notes::didnt_get_ready(&snap.scene, speaker, failure.why);
        plain(ui, &title);
        return;
    }
    let Some(q) = snap
        .scene
        .situation
        .confirm_in_panel
        .as_ref()
        .filter(|q| q.speaker() == speaker)
    else {
        return;
    };
    let About::Question(words) = &snap.panel.about else {
        return;
    };
    let line = if matches!(q, Confirm::ChimeBusy { .. }) {
        t!(
            snap.lang,
            SetChimeBusy,
            speaker = &snap.scene.speaker_name(speaker)
        )
    } else {
        format!("{} {}", words.title, words.sentence)
    };
    plain(ui, &line);
    ui.horizontal(|ui| {
        for a in [&words.yes, &words.no] {
            if page_button(&a.label).show(ui).clicked() {
                ask(Ask::Command(a.command.clone()));
            }
        }
    });
    gap(ui);
}

pub(super) fn chime_volume(
    ui: &mut Ui,
    snap: &Snapshot,
    name: &str,
    level: u8,
    limit: u8,
    ask: &mut dyn FnMut(Ask),
) {
    let lang = snap.lang;
    let label = t!(lang, FirstVolume, speaker = name);
    let value = t!(lang, VolumeNumber, level = level);
    if let Some(level) = slider_line(
        ui,
        &label,
        &value,
        f32::from(level),
        0.0..=f32::from(limit),
        1.0,
        &label,
    ) {
        action(ask, FirstRunAction::ChimeVolume(level as u8));
    }
}

impl ViewContent for FirstRunPage {
    fn has_back(&self) -> bool {
        matches!(self.step, FirstRunStep::Two { .. })
    }

    fn bar(&self, snap: &Snapshot) -> ViewBar {
        let title = match &self.step {
            FirstRunStep::One => t!(snap.lang, FirstPickTitle),
            FirstRunStep::Two { speaker, level: None, .. } => t!(
                snap.lang, FirstSwitchTitle, speaker = &snap.scene.speaker_name(speaker)
            ),
            FirstRunStep::Two { speaker, .. } => t!(
                snap.lang,
                FirstHearTitle,
                speaker = &snap.scene.speaker_name(speaker)
            ),
            FirstRunStep::Three { speaker } if snap.scene.situation.place_speaker() != Some(speaker)
                || matches!(snap.panel.about, About::Note(_)) => snap.panel.place.name.clone(),
            FirstRunStep::Three { speaker } => t!(
                snap.lang,
                FirstPlayingTitle,
                speaker = &snap.scene.speaker_name(speaker)
            ),
        };
        ViewBar {
            title,
            ..Default::default()
        }
    }

    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = snap.lang;
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        match &self.step {
            FirstRunStep::One => {
                let can_chime = snap.scene.speakers.iter().any(|s| snap.scene.situation.speaker(&s.id)
                    .is_some_and(|r| r.can_chime && !r.state.grey()));
                if !snap.scene.speakers.is_empty() {
                    plain(ui, &if can_chime { t!(lang, FirstPickChime) } else { t!(lang, ClickSpeaker) });
                }
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let commands = self.picker.show(ui, snap, |ui, id| {
                        ui.scope(|ui| {
                            ui.spacing_mut().item_spacing.y = 6.0;
                            question(ui, snap, id, ask);
                        });
                    });
                    for c in commands {
                        match c {
                            Command::Open(page) => ask(Ask::Open(page)),
                            c => ask(Ask::Command(c)),
                        }
                    }
                });
            }
            // Nothing to hear yet, so no volume to choose: the speaker
            // starts at its own level.
            FirstRunStep::Two { speaker, level: None, .. } => {
                let name = snap.scene.speaker_name(speaker);
                plain(ui, &t!(lang, FirstNoChime, speaker = &name));
                if Button::primary(&t!(lang, SwitchToSpeaker, speaker = &name)).text_size(size::BODY).show(ui).clicked() {
                    action(ask, FirstRunAction::UseWithoutChecking);
                }
                question(ui, snap, speaker, ask);
            }
            FirstRunStep::Two {
                speaker,
                level: Some(level),
                limit,
            } => {
                let name = snap.scene.speaker_name(speaker);
                plain(ui, &t!(lang, FirstHearLine, speaker = &name));
                ui.horizontal(|ui| {
                    if Button::primary(&t!(lang, HeardIt))
                        .text_size(size::BODY)
                        .show(ui)
                        .clicked()
                    {
                        action(ask, FirstRunAction::Heard);
                    }
                    if page_button(&t!(lang, DidntHear)).show(ui).clicked() {
                        ask(Ask::Open(Open::CantHear(Some(speaker.clone()))));
                    }
                });
                question(ui, snap, speaker, ask);
                gap(ui);
                chime_volume(ui, snap, &name, *level, *limit, ask);
                ui.horizontal(|ui| {
                    if page_button(&t!(lang, PlayAgain)).show(ui).clicked() {
                        action(ask, FirstRunAction::ChimeAgain);
                    }
                    let remaining = ui.available_width();
                    if page_button(&t!(lang, UseWithoutChecking, speaker = &name))
                        .max_width(remaining)
                        .show(ui)
                        .clicked()
                    {
                        action(ask, FirstRunAction::UseWithoutChecking);
                    }
                });
            }
            FirstRunStep::Three { speaker } => {
                if snap.scene.situation.place_speaker() == Some(speaker) {
                    plain(ui, &t!(lang, FirstStays, speaker = &snap.scene.speaker_name(speaker)));
                }
                if let About::Note(note) = &snap.panel.about {
                    plain(ui, &note.title);
                    plain(ui, &note.sentence);
                    if let Some(a) = &note.button {
                        if page_button(&a.label).show(ui).clicked() {
                            match &a.command {
                                Command::Open(page) => ask(Ask::Open(page.clone())),
                                cmd => ask(Ask::Command(cmd.clone())),
                            }
                        }
                    }
                }
                if self.tray { plain(ui, &t!(lang, FirstIcon)); }
                gap(ui);
                if let Some(on) =
                    switch_line(ui, &t!(lang, SetStartAtSignIn), snap.scene.start_at_sign_in)
                {
                    action(ask, FirstRunAction::StartAtSignIn(on));
                }
                ui.horizontal(|ui| {
                    if Button::primary(&t!(lang, Done))
                        .text_size(size::BODY)
                        .show(ui)
                        .clicked()
                    {
                        action(ask, FirstRunAction::Done);
                    }
                    if snap.scene.situation.place_speaker().is_some() && page_button(&t!(lang, SwitchToPc)).show(ui).clicked() {
                        ask(Ask::Command(Command::Switch { place: crate::model::Place::ThisPc, way: None }));
                        action(ask, FirstRunAction::Done);
                    }
                });
            }
        }
    }

    fn has_links(&self, _: &Snapshot) -> bool {
        !matches!(self.step, FirstRunStep::Three { .. })
    }

    fn links(&mut self, ui: &mut Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        match self.step {
            FirstRunStep::One => {
                for (label, page) in [
                    (t!(snap.lang, SpeakerMissing), Open::SpeakerMissing),
                    (t!(snap.lang, Settings), Open::Settings),
                ] {
                    if page_link(&label).show(ui).clicked() {
                        ask(Ask::Open(page));
                    }
                }
            }
            FirstRunStep::Two { .. } => {
                if page_link(&t!(snap.lang, ChooseAnotherSpeaker))
                    .show(ui)
                    .clicked()
                {
                    action(ask, FirstRunAction::ChooseAnother);
                }
            }
            FirstRunStep::Three { .. } => {}
        }
    }
}
