//! A speaker's options, opened from the *›* at the end of its row: its
//! delay (the panel's own block), its tone, its loudest level, song titles,
//! when it gets the sound by itself, its name in this app, and at the
//! bottom its details, hiding it and forgetting it.

use std::sync::Arc;

use egui::Ui;

use crate::model::command::{Command, Open};
use crate::model::delay::PanelDelay;
use crate::model::snapshot::Snapshot;
use crate::model::state::{NowState, Place, Secret, SpeakerId};
use crate::model::text::Lang;
use crate::shell::{Ask, ViewBar, ViewContent};
use crate::ui::button::Button;
use crate::ui::icon::Icon;
use crate::ui::inputs::{Choice, DropDown};
use crate::ui::text::{Mark, Tone};

use super::parts::{choices, field_line, gap, marked, sentence, sentence_links, slider_line, switch_line, thin};
use crate::settings::port::*;
use crate::settings::words::{self, w};
use crate::settings::{Dialog, PasswordDialog, ReportDialog};
use crate::settings::{PageState, SettingsPort};

/// A speaker's options: one view per row, so a speaker reached two ways
/// has two.
pub struct SpeakerOptions {
    port: Arc<dyn SettingsPort>,
    id: String,
    delay: crate::ui::delay::DelayState,
    st: PageState,
    /// *Forget* was clicked: the view asks first.
    forget_asked: bool,
    /// The speaker left the list (removed): back once.
    gone: bool,
}

/// What the bar's one button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BarButton {
    TestSound,
    PlayHere,
    SearchAgain,
}

impl SpeakerOptions {
    pub fn new(port: Arc<dyn SettingsPort>, id: &str) -> Self {
        SpeakerOptions { port, id: id.to_string(), delay: crate::ui::delay::DelayState::opened(), st: PageState::default(), forget_asked: false, gone: false }
    }

    /// With the amount field of the delay open (pictures).
    pub fn with_amount(mut self, digits: &str) -> Self {
        self.delay = crate::ui::delay::DelayState::with_amount(digits, false);
        self.delay.set_open(true);
        self
    }

    /// Asking whether to forget it (pictures).
    pub fn asking_to_forget(mut self) -> Self {
        self.forget_asked = true;
        self
    }

    /// With all bands, or the details, open (pictures and tests).
    pub fn with_open(mut self, fold: &str) -> Self {
        self.st.open.insert(fold.to_string());
        self
    }

    fn sid(&self) -> SpeakerId {
        SpeakerId::new(&self.id)
    }

    fn button(&self, v: &SpeakerView) -> Option<BarButton> {
        match v.row.presence {
            // While this speaker is the place: hear it.
            Presence::PlayingNow | Presence::Ready => v.test_sound.map(|_| BarButton::TestSound),
            Presence::NotFound { .. } => Some(BarButton::SearchAgain),
            Presence::Connecting => None,
            Presence::Available => v.row.cant_play.is_none().then_some(BarButton::PlayHere),
        }
    }
}

/// *Sonos · playing now*: the kind and the state.
fn kind_and_state(lang: Lang, row: &SpeakerRow) -> String {
    let kind = words::kind_short(lang, &row.kind);
    let state = match &row.presence {
        Presence::PlayingNow => Some(w!(lang, SetStatePlaying)),
        Presence::Ready => Some(w!(lang, SetStateReady)),
        Presence::Connecting => Some(w!(lang, SetStateConnecting)),
        Presence::NotFound { .. } => Some(w!(lang, SetStateNotFound)),
        Presence::Available => None,
    };
    match state {
        Some(s) => w!(lang, SetKindStatus, kind = &kind, status = &s),
        None => kind,
    }
}

impl ViewContent for SpeakerOptions {
    fn bar(&self, snap: &Snapshot) -> ViewBar {
        let lang = self.port.lang();
        let Some(v) = self.port.speaker(&self.id) else {
            let name = snap.names.iter().find(|(i, _)| i.as_str() == self.id).map(|(_, n)| n.clone()).unwrap_or_else(|| self.id.clone());
            return ViewBar { title: name, ..Default::default() };
        };
        let button = self.button(&v).map(|b| match b {
            BarButton::TestSound => w!(lang, SetTestSoundButton),
            BarButton::PlayHere => w!(lang, SetPlayHere),
            BarButton::SearchAgain => w!(lang, SetSearchAgain),
        });
        ViewBar { title: v.row.name.clone(), line: Some(kind_and_state(lang, &v.row)), button }
    }

    fn bar_button(&mut self, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let Some(v) = self.port.speaker(&self.id) else { return };
        match self.button(&v) {
            Some(BarButton::TestSound) => ask(Ask::Command(Command::PlayTestSound(self.sid()))),
            // What a click on the speaker's row does.
            Some(BarButton::PlayHere) => {
                let place = Place::Speaker(self.sid());
                let command = snap.list.rows().find(|r| r.place == place).and_then(|r| r.command.clone()).unwrap_or(Command::Play(place));
                ask(Ask::Command(command));
            }
            Some(BarButton::SearchAgain) => self.port.act(Act::SearchAgain),
            None => {}
        }
    }

    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 6.0;
        let Some(v) = self.port.speaker(&self.id) else {
            sentence(ui, &w!(lang, SetSpeakerGone));
            if !self.gone {
                self.gone = true;
                ask(Ask::Back);
            }
            return;
        };
        let id = self.id.clone();
        let name = v.row.name.clone();

        // A code or a password, while the speaker asks for one.
        if let NowState::NeedsSecret { speaker, secret } = &snap.state {
            if speaker.as_str() == id {
                let label = match secret {
                    Secret::Code => w!(lang, SetEnterCode),
                    Secret::Password => w!(lang, SetEnterPassword),
                };
                if Button::primary(&label).show(ui).clicked() {
                    ask(Ask::Open(Open::Pair(self.sid())));
                }
                gap(ui);
            }
        }
        if v.row.untested
            && marked(ui, Mark::Warning, Tone::Primary, &w!(lang, SetUntestedPairShort, speaker = &name), &[&w!(lang, SetReportProblem)]).is_some()
        {
            super::open_dialog(&self.port, Dialog::Report(ReportDialog::default()), ask);
        }

        // The delay: the panel's block, the same value.
        if let Some(d) = &v.delay {
            self.delay_part(ui, lang, &v, d, ask);
        }
        if let Some(g) = &v.group {
            gap(ui);
            sentence(ui, &group_words(lang, &name, g));
        }

        // Tone and the loudest level.
        if v.tone.is_some() || v.volume_limit.is_some() {
            thin(ui);
        }
        if let Some((bass, treble)) = v.tone {
            self.tone_part(ui, lang, bass, treble);
        }
        if let Some(limit) = v.volume_limit {
            let shown = if limit >= 100 { w!(lang, SetNoLimit) } else { limit.to_string() };
            let label = w!(lang, SetNeverLouder);
            let spoken = if limit >= 100 { w!(lang, SetSpokenNoLimit, what = &label) } else { w!(lang, SetSpokenLevel, what = &label, level = limit) };
            if let Some(n) = slider_line(ui, &label, &shown, limit as f32, 0.0..=100.0, 1.0, &spoken) {
                self.port.act(Act::VolumeLimit(id.clone(), n.round() as u8));
            }
        }

        // Song titles, coming back, the name here.
        thin(ui);
        if let Some(on) = v.song_titles {
            if let Some(on) = switch_line(ui, &w!(lang, SetSongTitles, speaker = &name), on) {
                self.port.act(Act::SongTitles(id.clone(), on));
            }
            gap(ui);
        }
        if let Some(cb) = v.comes_back {
            let answers = [w!(lang, SetMoveResume, speaker = &name), w!(lang, SetMoveWhenOn, speaker = &name), w!(lang, SetMoveNever)];
            let chosen = match cb {
                ComesBack::WhenLastChosen => 0,
                ComesBack::WhenItAppears => 1,
                ComesBack::Never => 2,
            };
            if let Some(i) = choices(ui, &w!(lang, SetMoveAutomatically, speaker = &name), &answers, chosen) {
                self.port.act(Act::ComesBack(id.clone(), [ComesBack::WhenLastChosen, ComesBack::WhenItAppears, ComesBack::Never][i]));
            }
            gap(ui);
        }
        if let Some(shown) = &v.show_as {
            if let Some(n) = field_line(ui, &mut self.st, "name-here", &w!(lang, SetNameInApp), shown) {
                self.port.act(Act::ShowAs(id.clone(), n));
            }
            gap(ui);
        }
        match &v.pairing {
            Some(Pairing::Paired { on }) => {
                let text = match on {
                    Some(d) => w!(lang, SetPairedSentenceOn, date = &words::date(lang, *d)),
                    None => w!(lang, SetPairedSentence),
                };
                match sentence_links(ui, &text, &[&w!(lang, SetPairAgain), &w!(lang, SetForgetPairing)]) {
                    Some(0) => self.port.act(Act::PairAgain(id.clone())),
                    Some(_) => self.port.act(Act::ForgetPairing(id.clone())),
                    None => {}
                }
            }
            Some(Pairing::PasswordSaved) => match sentence_links(ui, &w!(lang, SetPasswordSavedSentence), &[&w!(lang, SetChange), &w!(lang, SetRemove)]) {
                Some(0) => super::open_dialog(&self.port, Dialog::Password(PasswordDialog::change(&id, &name)), ask),
                Some(_) => self.port.act(Act::RemovePassword(id.clone())),
                None => {}
            },
            None => {}
        }

        // Details and forgetting, opened from the links at the bottom.
        if self.st.is_open("details") {
            thin(ui);
            self.details_part(ui, lang, &v);
        }
        if self.forget_asked {
            thin(ui);
            let (question, confirm) = if v.row.added_by_address {
                (w!(lang, SetRemoveAsk, speaker = &name), w!(lang, SetRemoveNamed, speaker = &name))
            } else {
                (w!(lang, SetForgetAsk, speaker = &name), w!(lang, SetForgetNamed, speaker = &name))
            };
            marked(ui, Mark::Warning, Tone::Primary, &question, &[]);
            let confirm_short = w!(lang, SetForgetThis);
            super::parts::buttons(ui, |ui| {
                let label = crate::settings::widgets::fitting(ui, &confirm, &confirm_short);
                let r = Button::danger(label).accessible_name(confirm.clone()).show(ui);
                if r.clicked() {
                    self.port.act(if v.row.added_by_address { Act::Remove(id.clone()) } else { Act::Forget(id.clone()) });
                    self.forget_asked = false;
                }
                if Button::standard(&w!(lang, SetCancel)).show(ui).clicked() {
                    self.forget_asked = false;
                }
                r.scroll_to_me(None);
            });
        }
    }

    fn has_links(&self, _snap: &Snapshot) -> bool {
        self.port.speaker(&self.id).is_some()
    }

    fn links(&mut self, ui: &mut Ui, _snap: &Snapshot, _ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        let Some(v) = self.port.speaker(&self.id) else { return };
        let hidden = self.port.speakers().hidden.iter().any(|r| r.id == self.id);
        if Button::link(&w!(lang, SetDetails)).show(ui).clicked() {
            self.st.toggle("details");
        }
        let hide = if hidden { w!(lang, SetShowInList) } else { w!(lang, SetHideFromList) };
        if Button::link(&hide).show(ui).clicked() {
            self.port.act(if hidden { Act::ShowSpeaker(self.id.clone()) } else { Act::HideSpeaker(self.id.clone()) });
        }
        let forget = if v.row.added_by_address { w!(lang, SetRemove) } else { w!(lang, SetForget) };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if super::parts::red_link(ui, &forget).clicked() {
                self.forget_asked = true;
            }
        });
    }

    fn holds_unused_text(&self) -> bool {
        self.port.speaker(&self.id).and_then(|v| v.show_as).is_some_and(|shown| self.st.typing.get("name-here").is_some_and(|t| t.trim() != shown))
    }
}

impl SpeakerOptions {
    /// The choice of an AirPlay speaker, the panel's delay block, what the
    /// speaker's own delay is, and the hint that tells which way helps.
    fn delay_part(&mut self, ui: &mut Ui, lang: Lang, v: &SpeakerView, d: &DelayCard, ask: &mut dyn FnMut(Ask)) {
        let s = &d.status;
        let name = v.row.name.as_str();
        if let Some(choice) = s.choice {
            let steady = w!(lang, SetChoiceSteady);
            let short = w!(lang, SetChoiceShort);
            let steady_line = w!(lang, SetChoiceSteadyLine, seconds = words::seconds(s.steady_ms));
            let short_line = w!(lang, SetChoiceShortLine, seconds = words::seconds(s.short_ms));
            let mut pick = None;
            if Choice::new(choice == DelayChoice::Steady, &steady).description(&steady_line).show(ui) {
                pick = Some(crate::model::facts::DelayChoice::Steady);
            }
            if Choice::new(choice == DelayChoice::Short, &short).description(&short_line).show(ui) {
                pick = Some(crate::model::facts::DelayChoice::Short);
            }
            if let Some(c) = pick {
                ask(Ask::Command(Command::UseDelayChoice { speaker: self.sid(), choice: c }));
            }
            gap(ui);
        }
        if let Some(block) = panel_delay_for(lang, &self.id, name, d) {
            for c in crate::panel::delay_block(ui, &block, &mut self.delay) {
                ask(Ask::Command(c));
            }
        }
        match v.row.kind {
            SpeakerKind::Sonos | SpeakerKind::SonosGroup | SpeakerKind::Other { .. } | SpeakerKind::Sendspin => {
                gap(ui);
                sentence(ui, &w!(lang, SetOwnDelay, speaker = name, delay = &words::about(lang, s.own_delay_ms)));
            }
            SpeakerKind::HomePodPair => {
                gap(ui);
                sentence(ui, &w!(lang, SetPairAlwaysShort, delay = &words::about(lang, s.own_delay_ms)));
            }
            _ => {}
        }
        let changeable = s.applies.is_ok() && s.limits.is_some_and(|l| l.remove_ms > 0 || l.add_ms > 0);
        if changeable {
            sentence(ui, &w!(lang, SetSymptoms));
        }
    }

    /// *Bass* and *Treble*, *Reset to flat* while the tone isn't flat, and
    /// *All bands…*, which opens the ten bands here.
    fn tone_part(&mut self, ui: &mut Ui, lang: Lang, bass: f32, treble: f32) {
        let id = self.id.clone();
        let tone = self.port.tone(&id);
        for (label, value, act) in [(w!(lang, SetBass), bass, ToneAct::Bass as fn(f32) -> ToneAct), (w!(lang, SetTreble), treble, ToneAct::Treble)] {
            let spoken = spoken_db(lang, &label, value);
            if let Some(db) = slider_line(ui, &label, &crate::settings::common::speaker_db(lang, value), value, -12.0..=12.0, 0.5, &spoken) {
                self.port.act(Act::Tone(id.clone(), act(db)));
            }
        }
        let flat = tone.as_ref().map_or(bass == 0.0 && treble == 0.0, |t| t.eq.bands_db.iter().all(|b| *b == 0.0) && t.eq.preamp_db == 0.0);
        let open = self.st.is_open("bands");
        let bands_link = if open { w!(lang, SetFewerBands) } else { w!(lang, SetAllBands) };
        ui.horizontal(|ui| {
            if !flat && Button::link(&w!(lang, SetResetFlat)).show(ui).clicked() {
                self.port.act(Act::Tone(id.clone(), ToneAct::Flat));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Button::link(&bands_link).show(ui).clicked() {
                    self.st.toggle("bands");
                }
            });
        });
        if open {
            if let Some(t) = tone {
                self.bands(ui, lang, &t);
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
        let picked = super::parts::control_line(ui, &label, None, |ui| DropDown::new(("tone-preset", &id), &label, &refs, current).min_width(140.0).show(ui));
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

    /// Model, address, the way it connects, the values behind the delay,
    /// the last problem; *Copy*.
    fn details_part(&mut self, ui: &mut Ui, lang: Lang, v: &SpeakerView) {
        let d = &v.details;
        let mut lines: Vec<String> = Vec::new();
        if let Some(m) = &d.model {
            lines.push(w!(lang, SetDetailModel, value = m));
        }
        if let Some(a) = &d.address {
            lines.push(w!(lang, SetDetailAddress, value = a));
        }
        if let Some(c) = &d.connection {
            lines.push(w!(lang, SetDetailConnection, value = c));
        }
        if let Some(dv) = &d.delay_values {
            lines.push(w!(lang, SetDetailDelay, value = dv));
        }
        let none = w!(lang, SetDetailNone);
        lines.push(w!(lang, SetDetailProblem, value = d.last_problem.as_deref().unwrap_or(&none)));
        lines.push(w!(lang, SetDetailId, value = &d.id));
        let text = lines.join("\n");
        crate::settings::widgets::mono(ui, &text);
        if let Some(card) = &v.delay {
            for line in crate::settings::common::delay_details(lang, v, card) {
                sentence(ui, &line);
            }
        }
        let key = format!("details:{}", self.id);
        crate::settings::widgets::copy_button(ui, &mut self.st, &key, &w!(lang, SetCopy), &w!(lang, SetCopied), &text);
        let _ = Icon::Copy;
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

/// The group's members, as a sentence.
fn group_words(lang: Lang, name: &str, g: &Group) -> String {
    let and = |items: &[String]| match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => w!(lang, SetListAnd, list = &rest.join(", "), last = last),
    };
    match g {
        Group::Sonos { members } => w!(lang, SetSonosGroupLine, rooms = &and(members)),
        Group::HomePodPair { left, right } => w!(lang, SetPairLine, left = left, right = right),
        Group::AppleTv { members } => w!(lang, SetAppleTvGroupLine, speaker = name, members = &and(members)),
    }
}

/// The panel's delay block for this speaker, from the delay engine's
/// status: the same block, words and commands the panel has for the
/// speaker that plays.
pub fn panel_delay_for(lang: Lang, id: &str, name: &str, d: &DelayCard) -> Option<PanelDelay> {
    use crate::model::facts::{Applies as A, ChoiceFacts, DelayChoice as C, DelayFacts, DelayNote as N, DelayRange, DelaySteps, Progress as P, SpeakerDelay};
    let s = &d.status;
    let applies = match s.applies {
        Ok(Applies::Live) => A::Live,
        Ok(Applies::Reconnect) => A::Reconnect,
        Err(Unchangeable::KeptBySpeakerOnSteady) => A::Fixed,
        Err(_) => return None,
    };
    let range = s.limits.map_or(DelayRange { max_removed_ms: 0, max_added_ms: 0 }, |l| DelayRange { max_removed_ms: l.remove_ms, max_added_ms: l.add_ms });
    let choice = s.choice.map(|c| ChoiceFacts {
        current: match c {
            DelayChoice::Short => C::Short,
            DelayChoice::Steady => C::Steady,
        },
        steady_ms: s.steady_ms,
        short_ms: s.short_ms,
    });
    let progress = match s.progress {
        Progress::Done => P::Settled,
        Progress::CatchingUp { ms_to_go } => P::Live { to_go_ms: ms_to_go },
        Progress::WillReconnect => P::ReconnectPending,
        Progress::Reconnecting { .. } => P::Reconnecting,
    };
    let note = s.notice.map(|n| match n {
        DelayNotice::NotAccepted { tried_ms, back_to_ms } => N::NotAccepted { tried_ms, back_to_ms },
        DelayNotice::DroppedOut { from_ms, to_ms } => N::DroppedOut { with_ms: from_ms, back_to_ms: to_ms },
        DelayNotice::ChoiceSwitched { other_ms, .. } => N::ChoiceSwitched { other_kept_ms: other_ms },
    });
    let speaker = SpeakerId::new(id);
    let steps = DelaySteps { small_ms: d.small_step_ms, large_ms: d.large_step_ms };
    let facts = DelayFacts { speaker: speaker.clone(), range, applies, choice, progress, note };
    let own = d.measured.map_or(s.own_delay_ms, |(ms, _)| ms);
    let view = crate::model::delay::DelayView {
        speaker,
        name: name.to_string(),
        delay: SpeakerDelay { own_ms: own, measured: d.measured.is_some(), change_ms: s.change_ms },
        range,
        applies,
        progress,
        steps,
        choice,
    };
    let reconnecting = match s.progress {
        Progress::Reconnecting { from_ms } => Some((from_ms, s.change_ms)),
        _ => None,
    };
    Some(crate::model::delay::panel_delay(lang, &view, &facts, reconnecting))
}
