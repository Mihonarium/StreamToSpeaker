//! General settings, opened from the panel gear.
use std::sync::Arc;

use egui::Ui;

use crate::model::command::Open;
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
}

impl SettingsView {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        SettingsView { port, st: PageState::default() }
    }
}

fn setting(ui: &mut Ui, label: &str, help: Option<String>, value: bool) -> Option<bool> {
    let changed = switch_line(ui, label, value);
    if let Some(help) = help { crate::settings::widgets::note(ui, &help); }
    changed
}

impl ViewContent for SettingsView {
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
            if control_line(ui, &w!(lang, NoticeUpdate, version = version), None, |ui| page_button(&w!(lang, SetUpdateNow)).show(ui).clicked()) {
                self.port.act(if about.updates.installs_here { Act::UpdateNow } else { Act::WhatsNew });
            }
            gap(ui);
        }
        if let Some(k) = &g.shortcuts {
            if let Some(on) = setting(ui, &w!(lang, SetShortcutsSwitch), Some(w!(lang, SetKeysHelp)), k.on) { self.port.act(Act::Shortcuts(on)); }
            if k.on { shortcuts(ui, lang, k, &mut self.st, self.port.as_ref()); }
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
        if let Some(on) = g.play_calls {
            if let Some(on) = setting(ui, &w!(lang, SetPlayCalls), Some(w!(lang, SetCallsHelp)), on) { self.port.act(Act::PlayCalls(on)); }
            gap(ui);
        }
        if let Some(mode) = g.what_plays {
            let some = mode == WhatPlays::OnlySentApps;
            if let Some(on) = setting(ui, &w!(lang, SetOnlySomeApps), Some(w!(lang, SetSomeAppsHelp)), some) {
                self.port.act(Act::WhatPlays(if on { WhatPlays::OnlySentApps } else { WhatPlays::Everything }));
            }
            if some && page_button(&w!(lang, SetOpenSoundSettings)).show(ui).clicked() { ask(Ask::Open(Open::VolumeMixer)); }
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
            if page_link(&w!(lang, SetNavAbout)).show(ui).clicked() { ask(Ask::View(Box::new(AboutView::new(self.port.clone())))); }
        });
        let label = if snap.scene.situation.place_speaker().is_some() { w!(lang, QuitAndPlayOnPc) } else { w!(lang, Quit) };
        if page_button(&label).show(ui).clicked() { ask(Ask::Command(crate::model::Command::Quit)); }
    }
}

pub(super) fn refused(ui: &mut Ui, port: &dyn SettingsPort, r: &Refused) {
    let lang = port.lang();
    let at = words::time(lang, r.at);
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

/// The delay shortcuts while they are on: each combination and whether
/// it is free, and *Change…*.
fn shortcuts(ui: &mut Ui, lang: crate::model::text::Lang, k: &Shortcuts, st: &mut PageState, port: &dyn SettingsPort) {
    let lines =
        [(w!(lang, SetShortcutRemove, ms = 25u32), k.remove.join("+"), k.taken[0]), (w!(lang, SetShortcutAdd, ms = 25u32), k.add.join("+"), k.taken[1])];
    for (i, (what, keys, taken)) in lines.iter().enumerate() {
        if st.recording == Some(i) {
            sentence(ui, &w!(lang, SetPressKeysFor, what = what));
            record(ui, st, port, i);
            continue;
        }
        let line = w!(lang, SetShortcutLine, what = what, keys = keys);
        if *taken {
            marked(ui, Mark::Warning, Tone::Amber, &format!("{line} {}", w!(lang, SetShortcutTaken)), &[]);
        } else {
            marked(ui, Mark::Done, Tone::Secondary, &line, &[]);
        }
    }
    sentence(ui, &w!(lang, SetHoldForLarge, key = &k.larger, ms = 100u32));
    if page_button(&w!(lang, SetChangeKeys)).show(ui).clicked() {
        st.recording = Some(0);
    }
}

/// While recording: the next key pressed with Ctrl or Alt is the new
/// combination; Esc stops recording.
fn record(ui: &mut Ui, st: &mut PageState, port: &dyn SettingsPort, i: usize) {
    let pressed = ui.input(|inp| {
        inp.events.iter().find_map(|e| match e {
            egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
            _ => None,
        })
    });
    if let Some((key, m)) = pressed {
        if key == egui::Key::Escape {
            st.recording = None;
        } else if m.ctrl || m.alt {
            let mut keys = Vec::new();
            if m.ctrl {
                keys.push("Ctrl".to_string());
            }
            if m.alt {
                keys.push("Alt".to_string());
            }
            if m.shift {
                keys.push("Shift".to_string());
            }
            keys.push(key.symbol_or_name().to_string());
            port.act(Act::ShortcutKeys(i, keys));
            st.recording = if i == 0 { Some(1) } else { None };
        }
    }
}
