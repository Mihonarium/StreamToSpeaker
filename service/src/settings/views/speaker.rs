//! The page for one panel row. Playback state and delay commands come from the same snapshot as the panel.
use std::sync::Arc;

use egui::Ui;
use crate::model::{Command, Snapshot, SpeakerId};
use crate::model::command::{DelayStep, Open};
use crate::model::facts::Applies;
use crate::model::panel::{self, About, Row};
use crate::model::situation::{Confirm, Doing, RowState, WayChoice};
use crate::model::state::Secret;
use crate::model::text::Lang;
use crate::settings::{exact_delay::ExactDelay, port::*, words::w, widgets, PageState};
use crate::shell::{Ask, ViewBar, ViewContent, WindowContent};
use crate::ui::theme::{size, Theme};

use super::parts::{page_button, page_link, buttons, edit_field, gap, plain, sentence, slider_line, switch_line, thin};

pub struct SpeakerOptions {
    port: Arc<dyn SettingsPort>,
    id: String,
    exact_delay: Option<(WayChoice, ExactDelay)>,
    st: PageState,
    saved_at: Option<f64>,
    gone: bool,
}

impl SpeakerOptions {
    pub fn new(port: Arc<dyn SettingsPort>, id: &str) -> Self {
        Self { port, id: id.into(), exact_delay: None, st: PageState::default(), saved_at: None, gone: false }
    }

    pub(super) fn open_exact_delay(&mut self, way: &crate::model::scene::WaySeen) {
        self.exact_delay = Some((way.way, ExactDelay::new(way.delay)));
    }

    pub fn with_open(mut self, fold: &str) -> Self {
        self.st.open.insert(fold.into());
        self
    }

    fn row<'a>(&self, snap: &'a Snapshot) -> Option<&'a Row> {
        let list = &snap.panel.list;
        list.rows.iter().chain(list.more.iter().flat_map(|fold| &fold.rows))
            .find(|r| r.place.speaker().is_some_and(|id| id.as_str() == self.id))
    }

    fn playing(&self, snap: &Snapshot) -> bool {
        snap.scene.situation.place_speaker() == Some(&SpeakerId::new(&self.id)) && snap.scene.situation.doing == Doing::Playing
    }
}

impl ViewContent for SpeakerOptions {
    fn bar(&self, snap: &Snapshot) -> ViewBar {
        let row = self.row(snap);
        let lang = snap.lang;
        let v = self.port.speaker(&self.id);
        let title = v.as_ref().map(|v| v.row.name.clone()).unwrap_or_else(|| snap.scene.speaker_name(&SpeakerId::new(&self.id)));
        let line = if self.playing(snap) { Some(w!(lang, Playing)) }
            else if let Some(row) = row { row.text.clone() }
            else if v.is_some_and(|v| matches!(v.row.presence, Presence::NotFound { .. })) { Some(w!(lang, RowNotFoundShort)) }
            else { None };
        ViewBar { title, line, button: None }
    }

    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = snap.lang;
        ui.spacing_mut().item_spacing.y = 8.0;
        let Some(v) = self.port.speaker(&self.id) else {
            sentence(ui, &w!(lang, SetSpeakerGone));
            if !self.gone { self.gone = true; ask(Ask::Back); }
            return;
        };
        let id = SpeakerId::new(&self.id);
        let name = v.row.name.as_str();
        let row = self.row(snap);
        let missing = row.map_or(matches!(v.row.presence, Presence::NotFound { .. }), |r| r.state == RowState::NotFound);
        let here = snap.scene.situation.place_speaker() == Some(&id);
        let chime = w!(lang, PlayTestChime);
        buttons(ui, |ui| {
            if missing {
                if page_button(&w!(lang, SetSearchAgain)).show(ui).clicked() { self.port.act(Act::SearchAgain); }
            } else {
                if !here {
                    let command = row.and_then(|r| r.command.as_ref());
                    let label = w!(lang, SwitchToSpeaker, speaker = name);
                    let short = w!(lang, SetSwitch);
                    let font = Theme::of(ui.ctx()).font(size::BODY);
                    let pair_width = ui.fonts(|fonts| [&label, &chime].into_iter().map(|s|
                        fonts.layout_no_wrap(s.clone(), font.clone(), egui::Color32::PLACEHOLDER).size().x
                    ).sum::<f32>()) + 4.0 * crate::ui::theme::space::M + 8.0;
                    let label_fit = if pair_width <= ui.available_width() { &label } else { &short };
                    if page_button(label_fit).accessible_name(&label).enabled(command.is_some()).show(ui).clicked() {
                        ask(Ask::Command(command.unwrap().clone()));
                    }
                }
                if page_button(&chime).show(ui).on_hover_text(w!(lang, SetChimeTip, speaker = name)).clicked() {
                    ask(Ask::Command(Command::PlayChime { speaker: id.clone(), confirmed: false }));
                }
            }
        });

        // The engine can discover that a speaker is busy after the initial click.
        if let Some(q) = snap.scene.situation.confirm_in_panel.as_ref().filter(|q| q.speaker() == &id) {
            if let About::Question(question) = &snap.panel.about {
                let text = if matches!(q, Confirm::ChimeBusy { .. }) { w!(lang, SetChimeBusy, speaker = name) }
                    else { format!("{} {}", question.title, question.sentence) };
                egui::Frame::none().fill(widgets::tint(ui)).inner_margin(12.0).show(ui, |ui| {
                    plain(ui, &text);
                    buttons(ui, |ui| {
                        for a in [&question.yes, &question.no] {
                            if page_button(&a.label).show(ui).clicked() { ask(Ask::Command(a.command.clone())); }
                        }
                    });
                });
            }
        }
        if let Some(secret) = snap.scene.situation.secret_for(&id) {
            let label = match secret { Secret::Code => w!(lang, SetEnterCode), Secret::Password => w!(lang, SetEnterPassword) };
            if page_button(&label).show(ui).clicked() { ask(Ask::Open(Open::Pair(id.clone()))); }
        }
        let about = panel::delay_about(&snap.scene, &id);
        thin(ui);
        widgets::strong(ui, &w!(lang, DelayLabel), size::BODY);
        if let About::Delay(d) = &about {
            for c in crate::surface::panel::page_delay(ui, &about) { ask(Ask::Command(c)); }
            let way = snap.scene.speaker(&id).and_then(|s| s.ways.iter().find(|w| w.way == s.way && w.applies != Applies::Fixed));
            if let Some(way) = way {
                if self.exact_delay.as_ref().is_some_and(|(opened, _)| *opened != way.way) {
                    self.exact_delay = None;
                }
                ui.add_enabled_ui(!d.dimmed, |ui| {
                    if let Some((_, editor)) = &mut self.exact_delay {
                        let (close, step) = editor.show(ui, lang, way.delay.own_ms);
                        if let Some(step) = step { ask(Ask::Command(Command::ChangeDelay { speaker: id.clone(), step })); }
                        if close { self.exact_delay = None; }
                    } else if page_link(&w!(lang, SetExactDelay)).show(ui).clicked() {
                        self.open_exact_delay(way);
                    }
                    if way.delay.change_ms != 0 && page_link(&w!(lang, SetDefaultDelay, ms = way.delay.own_ms)).show(ui).clicked() {
                        ask(Ask::Command(Command::ChangeDelay { speaker: id.clone(), step: DelayStep::Reset }));
                        self.exact_delay = None;
                    }
                });
            } else {
                self.exact_delay = None;
            }
        } else {
            self.exact_delay = None;
        }
        if let Some(k) = self.port.general().shortcuts {
            let label = if k.on { w!(lang, SetDelayKeysOn, remove = &k.remove.join("+"), add = &k.add.join("+")) }
                else { w!(lang, SetDelayKeysLink) };
            if super::parts::wrapped_link(ui, &label).clicked() { ask(Ask::Open(Open::Settings)); }
        }
        if !self.playing(snap) { widgets::note(ui, &w!(lang, SetInactiveDelay, speaker = name)); }
        if let Some((bass, treble)) = v.tone {
            thin(ui);
            widgets::strong(ui, &w!(lang, SetTone), size::BODY);
            self.tone_part(ui, lang, name, bass, treble);
        }

        thin(ui);
        if let Some(limit) = v.volume_limit {
            let label = w!(lang, SetVolumeLimit);
            plain(ui, &label);
            let spoken = w!(lang, SetSpokenLevel, what = &label, level = limit);
            if let Some(n) = slider_line(ui, "", &limit.to_string(), limit as f32, 0.0..=100.0, 1.0, &spoken) {
                self.port.act(Act::VolumeLimit(self.id.clone(), n.round() as u8));
            }
            widgets::note(ui, &w!(lang, SetVolumeLimitLine, speaker = name));
            gap(ui);
        }
        if let Some(on) = v.song_titles {
            if let Some(on) = switch_line(ui, &w!(lang, SetSongTitles, speaker = name), on) { self.port.act(Act::SongTitles(self.id.clone(), on)); }
            widgets::note(ui, &w!(lang, SetTitlesLine, speaker = name));
            gap(ui);
        }
        for r in self.port.privacy().refused.iter().flatten().filter(|r| r.group_of.as_ref().is_some_and(|(id, _)| id == &self.id)) {
            super::settings_list::refused(ui, self.port.as_ref(), r);
        }
        if let Some(value) = &v.show_as {
            let label = w!(lang, SetNameInApp);
            ui.horizontal(|ui| {
                plain(ui, &label);
                if self.saved_at.is_some_and(|at| ui.input(|i| i.time) - at < 2.0) {
                    widgets::note(ui, &w!(lang, SetNameSaved));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                }
            });
            if let Some(n) = edit_field(ui, &mut self.st, "name-here", &label, value) {
                self.port.act(Act::ShowAs(self.id.clone(), n));
                self.saved_at = Some(ui.input(|i| i.time));
            }
        }
        thin(ui);
        let remove = missing || v.row.added_by_address;
        let hidden = self.port.speakers().hidden.iter().any(|r| r.id == self.id);
        buttons(ui, |ui| {
            if !remove && page_button(&w!(lang, SetResetSettings)).show(ui).clicked() {
                ask(Ask::Dialog(Box::new(ConfirmSettings { port: self.port.clone(), id: id.clone(), name: name.into(), remove: false })));
            }
            let hide = if hidden { w!(lang, SetShowInList) } else { w!(lang, SetHideFromList) };
            if page_button(&hide).show(ui).clicked() {
                self.port.act(if hidden { Act::ShowSpeaker(self.id.clone()) } else { Act::HideSpeaker(self.id.clone()) });
            }
            if remove {
                let label = w!(lang, SetRemoveNamed, speaker = name);
                let short = w!(lang, SetRemove);
                if page_button(widgets::fitting(ui, &label, &short)).accessible_name(&label).show(ui).clicked() {
                    ask(Ask::Dialog(Box::new(ConfirmSettings { port: self.port.clone(), id: id.clone(), name: name.into(), remove: true })));
                }
            }
        });
        widgets::note(ui, &w!(lang, SetHiddenHelp));
        if page_link(&w!(lang, SetTechnicalDetails)).show(ui).clicked() { self.st.toggle("details"); }
        if self.st.is_open("details") {
            let mut lines = Vec::new();
            if let Some(m) = &v.details.model { lines.push(w!(lang, SetDetailModel, value = m)); }
            if let Some(a) = &v.details.address { lines.push(w!(lang, SetDetailAddress, value = a)); }
            if let Some(c) = &v.details.connection { lines.push(w!(lang, SetDetailConnection, value = c)); }
            let text = lines.join("\n");
            widgets::mono(ui, &text);
            widgets::copy_button(ui, &mut self.st, "details", &w!(lang, SetCopy), &w!(lang, SetCopied), &text);
        }
    }

    fn holds_unused_text(&self) -> bool {
        self.exact_delay.as_ref().is_some_and(|(_, editor)| editor.has_edit()) || self.port.speaker(&self.id).and_then(|v| v.show_as)
            .is_some_and(|shown| self.st.typing.get("name-here").is_some_and(|t| t.trim() != shown))
    }
}

pub(super) struct ConfirmSettings {
    pub port: Arc<dyn SettingsPort>,
    pub id: SpeakerId,
    pub name: String,
    pub remove: bool,
}

impl WindowContent for ConfirmSettings {
    fn title(&self, snap: &Snapshot) -> String { w!(snap.lang, AppName) }
    fn sizes(&self) -> (egui::Vec2, egui::Vec2) { (egui::vec2(420.0, 260.0), egui::vec2(320.0, 220.0)) }
    fn show(&mut self, ctx: &egui::Context, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) -> bool {
        let lang = snap.lang;
        egui::CentralPanel::default().frame(egui::Frame::none().fill(Theme::of(ctx).c.surface)).show(ctx, |ui| {
            let mut footer = crate::settings::dialogs::frame(ui, |ui| {
                let text = if self.remove { w!(lang, SetRemoveQuestion, speaker = &self.name) } else { w!(lang, SetResetQuestion, speaker = &self.name) };
                plain(ui, &text);
                if self.remove && snap.scene.situation.place_speaker() == Some(&self.id) { plain(ui, &w!(lang, SetRemovalToPc)); }
            });
            let label = if self.remove { w!(lang, SetRemove) } else { w!(lang, Reset) };
            if page_button(&label).show(&mut footer).clicked() {
                self.port.act(Act::ClearSpeakerCredentials(self.id.as_str().into()));
                ask(Ask::Command(if self.remove { Command::RemoveSpeaker(self.id.clone()) } else { Command::ResetSpeaker(self.id.clone()) }));
                return true;
            }
            page_button(&w!(lang, SetCancel)).show(&mut footer).clicked() || ctx.input(|i| i.key_pressed(egui::Key::Escape))
        }).inner
    }
}

impl SpeakerOptions {
    fn tone_part(&mut self, ui: &mut Ui, lang: Lang, name: &str, bass: f32, treble: f32) {
        for (label, value, act) in [(w!(lang, SetBass), bass, ToneAct::Bass as fn(f32) -> ToneAct), (w!(lang, SetTreble), treble, ToneAct::Treble)] {
            if let Some(db) = slider_line(ui, &label, &crate::settings::common::speaker_db(lang, value), value, -12.0..=12.0, 0.5, &spoken_db(lang, &label, value)) {
                self.port.act(Act::Tone(self.id.clone(), act(db)));
            }
        }
        widgets::note(ui, &w!(lang, SetToneLine, speaker = name));
        let open = self.st.is_open("bands");
        let label = if open { w!(lang, SetFewerBands) } else { w!(lang, SetMoreTone) };
        if page_link(&label).show(ui).clicked() { self.st.toggle("bands"); }
        if let Some(t) = self.port.tone(&self.id) {
            if open { self.bands(ui, lang, &t); }
            if (t.eq.bands_db.iter().any(|n| *n != 0.0) || t.eq.preamp_db != 0.0)
                && page_button(&w!(lang, SetResetTone)).show(ui).clicked() {
                self.port.act(Act::Tone(self.id.clone(), ToneAct::Flat));
            }
        }
    }
    /// The ten bands, the preamp and the presets, in place.
    fn bands(&mut self, ui: &mut Ui, lang: Lang, t: &ToneView) {
        use crate::equalizer::{EQ_BAND_HZ, EQ_PRESETS};
        let id = self.id.clone();
        gap(ui);
        let label = w!(lang, SetPreset);
        let mut names: Vec<String> = EQ_PRESETS.iter().map(|p| crate::settings::common::preset_name(lang, p.name)).collect();
        names.push(w!(lang, SetPresetCustom));
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let current = crate::equalizer::matching_preset(&t.eq.bands_db).and_then(|n| EQ_PRESETS.iter().position(|p| p.name == n)).unwrap_or(EQ_PRESETS.len());
        let picked = super::parts::drop_line(ui, ("tone-preset", &id), &label, None, &refs, current);
        if let Some(i) = picked.filter(|i| *i < EQ_PRESETS.len()) {
            self.port.act(Act::Tone(id.clone(), ToneAct::Preset(i)));
        }
        let pre = w!(lang, SetPreamp);
        if let Some(db) = slider_line(
            ui,
            &pre,
            &crate::settings::common::speaker_db(lang, t.eq.preamp_db),
            t.eq.preamp_db,
            -12.0..=0.0,
            0.5,
            &spoken_db(lang, &pre, t.eq.preamp_db),
        ) {
            self.port.act(Act::Tone(id.clone(), ToneAct::Preamp(db)));
        }
        for (i, hz) in EQ_BAND_HZ.iter().enumerate() {
            let band = w!(lang, SetBandName, band = &crate::settings::common::hz_label(*hz));
            let v = t.eq.bands_db[i];
            if let Some(db) = slider_line(ui, &band, &crate::settings::common::speaker_db(lang, v), v, -12.0..=12.0, 0.5, &spoken_db(lang, &band, v)) {
                self.port.act(Act::Tone(id.clone(), ToneAct::Band(i, db)));
            }
        }
        if let Some(h) = t.headroom_db {
            sentence(ui, &w!(lang, SetHeadroom, db = &crate::settings::common::speaker_db(lang, h)));
        }
    }

}

/// *Bass, minus 2 decibels*.
fn spoken_db(lang: Lang, what: &str, v: f32) -> String {
    let n = (v * 10.0).round() as i64;
    let num = crate::model::text::Number { value: n.abs(), scale: 1 }.normalized();
    if n > 0 {
        w!(lang, SetSpokenDbPlus, what = what, n = num)
    } else if n < 0 {
        w!(lang, SetSpokenDbMinus, what = what, n = num)
    } else {
        w!(lang, SetSpokenDbZero, what = what)
    }
}
