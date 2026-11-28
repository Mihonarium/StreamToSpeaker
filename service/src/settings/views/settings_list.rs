//! General settings, opened from the panel gear.
use std::sync::Arc;

use egui::Ui;

use crate::model::command::Open;
use crate::model::situation::RoutingQuestion;
use crate::model::Snapshot;
use crate::shell::{Ask, ViewBar, ViewContent};
use crate::ui::icon::Icon;
use crate::ui::text::{Mark, Tone};

use super::parts::{page_button, page_link, control_line, gap, marked, sentence, switch_line, thin};
use super::AboutView;
use crate::settings::port::*;
use crate::settings::words::{self, w};
use crate::settings::{Dialog, ReportDialog};
use crate::settings::{PageState, SettingsPort};

pub struct SettingsView {
    port: Arc<dyn SettingsPort>,
    st: PageState,
    recording: Option<Recording>,
    rejected_keys: Option<RejectedKeys>,
}

enum RejectedKeys {
    Shift,
    Windows(String),
    OtherShortcut(String),
}

enum Recording {
    Remove,
    Add { remove: Vec<String> },
}

impl SettingsView {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        SettingsView { port, st: PageState::default(), recording: None, rejected_keys: None }
    }

    fn end_recording(&mut self) {
        self.rejected_keys = None;
        if self.recording.take().is_some() {
            self.port.act(Act::RecordShortcuts(false));
        }
    }
}

impl Drop for SettingsView {
    fn drop(&mut self) {
        self.end_recording();
    }
}

fn setting(ui: &mut Ui, label: &str, help: Option<String>, value: bool) -> Option<bool> {
    let changed = switch_line(ui, label, value);
    if let Some(help) = help { crate::settings::widgets::note(ui, &help); }
    changed
}

impl ViewContent for SettingsView {
    fn hidden(&mut self) {
        self.end_recording();
    }

    fn bar(&self, _snap: &Snapshot) -> ViewBar {
        ViewBar { title: w!(self.port.lang(), Settings), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 6.0;
        let g = self.port.general();
        let p = self.port.privacy();
        let ma = self.port.music_assistant();
        let about = self.port.about();
        if let UpdateState::Available { version, .. } = &about.updates.state {
            // The line and its button say what happens: the app installs
            // the update, or opens the page that has it.
            let (line, label, act) = if about.updates.installs_here {
                (w!(lang, NoticeUpdate, version = version), w!(lang, SetUpdateNow), Act::UpdateNow)
            } else {
                (w!(lang, SetUpdateAvailable, version = version), w!(lang, SetOpenDownloadPage), Act::WhatsNew)
            };
            if control_line(ui, &line, None, |ui| page_button(&label).show(ui).clicked()) {
                self.port.act(act);
            }
            gap(ui);
        }
        if !g.shortcuts.as_ref().is_some_and(|k| k.on) || !ui.input(|i| i.focused) {
            self.end_recording();
        }
        if let Some(k) = &g.shortcuts {
            if let Some(on) = setting(ui, &w!(lang, SetShortcutsSwitch), Some(w!(lang, SetKeysHelp)), k.on) {
                self.port.act(Act::Shortcuts(on));
                if !on { self.end_recording(); }
            }
            if k.on { self.shortcuts(ui, lang, k); }
            gap(ui);
        }
        if let Some(phone) = &p.phone {
            if let Some(on) = setting(ui, &w!(lang, SetPhone), Some(w!(lang, SetPhoneHelp)), phone.on) { self.port.act(Act::Phone(on)); }
            if let Some(why) = &phone.problem { marked(ui, Mark::Error, Tone::Red, why, &[]); }
            if let (true, Some(link)) = (phone.on, &phone.link) {
                crate::settings::phone_qr::show(ui, link);
                ui.add(egui::Hyperlink::from_label_and_url(link, link));
                crate::settings::widgets::note(ui, &w!(lang, SetPhoneLinkLine));
                super::parts::buttons(ui, |ui| {
                    crate::settings::widgets::copy_button(ui, &mut self.st, "phone-link", &w!(lang, SetCopyLink), &w!(lang, SetCopied), link);
                    if page_button(&w!(lang, SetNewLink)).show(ui).clicked() { self.port.act(Act::NewLink); }
                });
            }
            gap(ui);
        }
        if let Some(on) = g.play_calls.filter(|_| g.what_plays != Some(WhatPlays::OnlySentApps)) {
            if let Some(on) = setting(ui, &w!(lang, SetPlayCalls), Some(w!(lang, SetCallsHelp)), on) { self.port.act(Act::PlayCalls(on)); }
            gap(ui);
        }
        if let Some(mode) = g.what_plays {
            let some = mode == WhatPlays::OnlySentApps;
            // What the switch does by itself, then what the person does in Windows.
            let help = format!("{} {}", w!(lang, SetSomeAppsRest), w!(lang, SetSomeAppsHelp));
            if let Some(on) = setting(ui, &w!(lang, SetOnlySomeApps), Some(help), some) {
                self.port.act(Act::WhatPlays(if on { WhatPlays::OnlySentApps } else { WhatPlays::Everything }));
            }
            if let Some(routing) = snap.scene.situation.confirm_under_routing.filter(|_| !some) {
                if let crate::model::panel::About::Question(q) = &snap.panel.about {
                    if routing == RoutingQuestion::TakePlayback {
                        let theme = crate::ui::theme::Theme::of(ui.ctx());
                        ui.label(crate::surface::grid::box_title(ui, &q.title, ui.available_width(), &theme));
                    } else {
                        sentence(ui, &q.title);
                    }
                    sentence(ui, &q.sentence);
                    super::parts::buttons(ui, |ui| {
                        for answer in [&q.yes, &q.no] {
                            if page_button(&answer.label).show(ui).clicked() {
                                ask(Ask::Command(answer.command.clone()));
                            }
                        }
                    });
                }
            }
            if some && page_button(&w!(lang, ChooseApps)).show(ui).clicked() { ask(Ask::Open(Open::VolumeMixer)); }
            gap(ui);
        }
        if let Some(start) = &g.start_at_sign_in {
            if let Some(on) = switch_line(ui, &w!(lang, SetStartAtSignIn), *start.as_ref().unwrap_or(&false)) { self.port.act(Act::StartAtSignIn(on)); }
            if let Err(why) = start { marked(ui, Mark::Error, Tone::Red, &w!(lang, SetSignInFailed, why = why), &[]); }
            gap(ui);
        }
        if let Some(on) = switch_line(ui, &w!(lang, SetQuietSwitch), g.switch_back_after_quiet) { self.port.act(Act::SwitchBackAfterQuiet(on)); }
        gap(ui);
        if let Some(on) = setting(ui, &w!(lang, SetNotesSwitch), Some(w!(lang, SetNotesHelp)), g.notes_by_clock) { self.port.act(Act::NotesByClock(on)); }
        gap(ui);
        if let Some(offer) = ma.offer {
            if let Some(on) = setting(ui, &w!(lang, SetMaOffer), Some(w!(lang, SetMaHelp)), offer) {
                self.port.act(Act::OfferToMa(on));
                if on { ask(Ask::Open(Open::MusicAssistantSettings)); }
            }
            gap(ui);
        }
        if let Some(api) = &p.api {
            if let Some(on) = switch_line(ui, &w!(lang, SetApi), api.on) { self.port.act(Act::Api(on)); }
            if let (true, Some(key)) = (api.on, &api.key) {
                super::parts::buttons(ui, |ui| {
                    crate::settings::widgets::copy_button(ui, &mut self.st, "api-key", &w!(lang, SetCopyKey), &w!(lang, SetCopied), key);
                    if page_button(&w!(lang, SetNewKey)).show(ui).clicked() { self.port.act(Act::NewKey); }
                });
                if page_link(&w!(lang, SetApiHow)).icon(Icon::External).show(ui).clicked() { self.port.act(Act::OpenApiHelp); }
            }
            gap(ui);
        }
        if let Some(chosen) = &g.language {
            let label = w!(lang, SetLanguage);
            let options = [w!(lang, SetLanguageWindows, language = &w!(lang, SetEnglish)), w!(lang, SetEnglish)];
            let refs: Vec<&str> = options.iter().map(String::as_str).collect();
            let i = if chosen.is_some() { 1 } else { 0 };
            if let Some(i) = super::parts::drop_line(ui, "settings-language", &label, None, &refs, i) {
                self.port.act(Act::Language(if i == 0 { None } else { Some("en".into()) }));
            }
        }

        thin(ui);
        super::parts::buttons(ui, |ui| {
            if page_link(&w!(lang, SetReportProblem)).show(ui).clicked() { super::open_dialog(&self.port, Dialog::Report(ReportDialog::default()), ask); }
            // Room between two links, which otherwise read as one phrase.
            ui.add_space(12.0);
            if page_link(&w!(lang, SetNavAbout)).show(ui).clicked() { ask(Ask::View(Box::new(AboutView::new(self.port.clone())))); }
        });
        let some = g.what_plays.map_or(snap.scene.situation.only_some_apps, |mode| mode == WhatPlays::OnlySentApps);
        let label = if snap.scene.situation.place_speaker().is_some() && !some { w!(lang, QuitAndPlayOnPc) } else { w!(lang, Quit) };
        if page_button(&label).show(ui).clicked() { ask(Ask::Command(crate::model::Command::Quit)); }
        if some { crate::settings::widgets::note(ui, &w!(lang, SetQuitChosenApps)); }
    }
}

pub(super) fn refused(ui: &mut Ui, port: &dyn SettingsPort, r: &Refused) {
    let lang = port.lang();
    // Today's by its time alone; an earlier one says which day.
    let at = match r.at {
        Seen::Today(t) => words::time(lang, t),
        earlier => words::seen(lang, earlier),
    };
    let who = match &r.name {
        Some(n) => w!(lang, SetRefusedNamed, name = n, address = &r.address, time = &at),
        None => w!(lang, SetRefusedUnknown, address = &r.address, time = &at),
    };
    let line = w!(lang, SetRefusedLine, who = &who);
    let allow = w!(lang, SetAllow);
    let clicked = control_line(ui, &line, None, |ui| page_button(&allow).accessible_name(w!(lang, SetAllowNamed, who = &who)).show(ui).clicked());
    if clicked {
        let speaker = r.group_of.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
        port.act(Act::Allow { address: r.address.clone(), speaker });
    }
}

impl SettingsView {
    fn shortcuts(&mut self, ui: &mut Ui, lang: crate::model::text::Lang, k: &Shortcuts) {
        if self.recording.is_some() { self.record(ui); }
        let remove = match &self.recording {
            Some(Recording::Add { remove }) => remove,
            _ => &k.remove,
        };
        let lines =
            [(w!(lang, SetShortcutRemove, ms = 25u32), words::shortcut_name(remove), k.status[0]), (w!(lang, SetShortcutAdd, ms = 25u32), words::shortcut_name(&k.add), k.status[1])];
        let failure = |status| match status {
            crate::platform::KeyStatus::Registered => None,
            crate::platform::KeyStatus::Taken => Some(w!(lang, SetShortcutTaken)),
            crate::platform::KeyStatus::Rejected => Some(w!(lang, SetShortcutRejected)),
        };
        let recording = self.recording.as_ref().map(|r| usize::from(matches!(r, Recording::Add { .. })));
        for (i, (what, keys, status)) in lines.iter().enumerate() {
            if recording == Some(i) {
                let prompt = match &self.rejected_keys {
                    Some(RejectedKeys::Shift) => w!(lang, SetShiftKeptForLargeStep, what = what),
                    Some(RejectedKeys::Windows(keys)) => w!(lang, SetPressOtherKeysFor, keys = keys, what = what),
                    Some(RejectedKeys::OtherShortcut(keys)) => w!(lang, SetKeysAlreadyFor, keys = keys, other = &lines[1 - i].0, what = what),
                    None => w!(lang, SetPressKeysFor, what = what),
                };
                sentence(ui, &prompt);
                continue;
            }
            let line = w!(lang, SetShortcutLine, what = what, keys = keys);
            if i == 0 && recording == Some(1) {
                sentence(ui, &line);
            } else if let Some(message) = failure(*status) {
                marked(ui, Mark::Warning, Tone::Amber, &format!("{line}. {message}"), &[]);
            } else {
                marked(ui, Mark::Done, Tone::Secondary, &line, &[]);
            }
        }
        let large = w!(lang, SetHoldForLarge, key = &k.larger, ms = 100u32);
        if let Some(message) = failure(k.status[2]) {
            marked(ui, Mark::Warning, Tone::Amber, &format!("{large} {message}"), &[]);
        } else {
            sentence(ui, &large);
        }
        if page_button(&w!(lang, SetChangeKeys)).show(ui).clicked() {
            self.start_recording();
        }
    }

    fn record(&mut self, ui: &mut Ui) {
        let mut presses = Vec::new();
        ui.input_mut(|input| input.events.retain(|event| {
            if let egui::Event::Key { key, pressed: true, repeat, modifiers, .. } = event {
                if *key == egui::Key::Escape || modifiers.ctrl || modifiers.alt || modifiers.shift {
                    if !repeat { presses.push((*key, *modifiers)); }
                    return false;
                }
            }
            true
        }));
        for (key, modifiers) in presses {
            if self.recording.is_none() { break; }
            self.record_press(key, modifiers);
        }
    }

    pub(super) fn start_recording(&mut self) {
        self.port.act(Act::RecordShortcuts(true));
        self.rejected_keys = None;
        self.recording = Some(Recording::Remove);
    }

    pub(super) fn record_press(&mut self, key: egui::Key, m: egui::Modifiers) {
        if key == egui::Key::Escape {
            self.end_recording();
            return;
        }
        if m.shift {
            self.rejected_keys = Some(RejectedKeys::Shift);
            return;
        }
        let mut keys = Vec::new();
        if m.ctrl {
            keys.push("Ctrl".to_string());
        }
        if m.alt {
            keys.push("Alt".to_string());
        }
        if self.port.windows_key_down() {
            keys.push("Win".to_string());
        }
        let name = match key {
            egui::Key::ArrowLeft => "Left",
            egui::Key::ArrowRight => "Right",
            egui::Key::ArrowDown => "Down",
            egui::Key::ArrowUp => "Up",
            egui::Key::PageDown => "Page Down",
            egui::Key::PageUp => "Page Up",
            egui::Key::Backtick => "`",
            egui::Key::Minus => "-",
            egui::Key::Quote => "'",
            egui::Key::Copy | egui::Key::Cut | egui::Key::Paste => return,
            _ => key.symbol_or_name(),
        };
        keys.push(name.to_string());
        let candidate = match &self.recording {
            Some(Recording::Remove) => [keys.clone(), Vec::new()],
            Some(Recording::Add { remove }) => [remove.clone(), keys.clone()],
            None => return,
        };
        if candidate[0] == candidate[1] {
            self.rejected_keys = Some(RejectedKeys::OtherShortcut(words::shortcut_name(&keys)));
            return;
        }
        if !self.port.accepts_shortcuts(&crate::platform::DelayKeys(candidate)) {
            self.rejected_keys = Some(RejectedKeys::Windows(words::shortcut_name(&keys)));
            return;
        }
        self.rejected_keys = None;
        match &mut self.recording {
            Some(Recording::Remove) => self.recording = Some(Recording::Add { remove: keys }),
            Some(Recording::Add { remove }) => {
                self.port.act(Act::ShortcutKeys(crate::platform::DelayKeys([std::mem::take(remove), keys])));
                self.end_recording();
            }
            None => {}
        }
    }
}
