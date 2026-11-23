//! Settings, from the gear: one list of plain sentences, each with its
//! switch at the right; a switch that needs more shows it right under
//! itself while it is on. Thin lines separate three groups. At the bottom:
//! *Guides*, *Report a problem*, *Lab* and *About*.

use std::sync::Arc;

use egui::Ui;

use crate::model::command::Open;
use crate::model::snapshot::Snapshot;
use crate::shell::{Ask, ViewBar, ViewContent};
use crate::ui::button::Button;
use crate::ui::icon::Icon;
use crate::ui::text::{Mark, Tone};

use super::parts::{control_line, gap, marked, sentence, switch_line, thin};
use super::{AboutView, GuidesView, LabView};
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

impl ViewContent for SettingsView {
    fn bar(&self, _snap: &Snapshot) -> ViewBar {
        ViewBar { title: w!(self.port.lang(), Settings), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 8.0;
        let g = self.port.general();
        let p = self.port.privacy();
        let ma = self.port.music_assistant();
        let about = self.port.about();

        // An update that is ready comes first.
        if let UpdateState::Available { version, .. } = &about.updates.state {
            let label = if about.updates.installs_here { w!(lang, SetUpdateNow) } else { w!(lang, SetGetUpdate) };
            let clicked = control_line(ui, &w!(lang, NoticeUpdate, version = version), None, |ui| Button::primary(&label).show(ui).clicked());
            if clicked {
                self.port.act(if about.updates.installs_here { Act::UpdateNow } else { Act::WhatsNew });
            }
            thin(ui);
        }

        // Delay from the keyboard, the phone, calls, some apps.
        let mut first = true;
        if let Some(k) = &g.shortcuts {
            first = false;
            if let Some(on) = switch_line(ui, &w!(lang, SetShortcutsSwitch), k.on) {
                self.port.act(Act::Shortcuts(on));
            }
            if k.on {
                shortcuts(ui, lang, k, &mut self.st, self.port.as_ref());
            }
        }
        if let Some(phone) = &p.phone {
            first = false;
            if let Some(on) = switch_line(ui, &w!(lang, SetPhone), phone.on) {
                self.port.act(Act::Phone(on));
            }
            if let Some(why) = &phone.problem {
                marked(ui, Mark::Error, Tone::Red, why, &[]);
            }
            if let (true, Some(link)) = (phone.on, &phone.link) {
                crate::settings::widgets::mono(ui, link);
                super::parts::buttons(ui, |ui| {
                    crate::settings::widgets::copy_button(ui, &mut self.st, "phone-link", &w!(lang, SetCopyLink), &w!(lang, SetCopied), link);
                    if Button::standard(&w!(lang, SetNewLink)).show(ui).clicked() {
                        self.port.act(Act::NewLink);
                    }
                });
                sentence(ui, &w!(lang, SetPhoneLinkLine));
            }
        }
        if let Some(on) = g.play_calls {
            first = false;
            if let Some(on) = switch_line(ui, &w!(lang, SetPlayCalls), on) {
                self.port.act(Act::PlayCalls(on));
            }
        }
        if let Some(mode) = g.what_plays {
            first = false;
            let some = mode == WhatPlays::OnlySentApps;
            if let Some(on) = switch_line(ui, &w!(lang, SetOnlySomeApps), some) {
                self.port.act(Act::WhatPlays(if on { WhatPlays::OnlySentApps } else { WhatPlays::Everything }));
            }
            if some && sentence_with_link(ui, &w!(lang, SetOnlySentLine), &w!(lang, SetOpenVolumeMixer)) {
                self.port.act(Act::OpenVolumeMixer);
            }
        }

        // Start-up and when something goes wrong.
        let second = g.start_at_sign_in.is_some() || g.go_back.is_some() || g.when_stops.is_some() || g.notifications.is_some();
        if second && !first {
            thin(ui);
        }
        if let Some(start) = &g.start_at_sign_in {
            let on = *start.as_ref().unwrap_or(&false);
            if let Some(on) = switch_line(ui, &w!(lang, SetStartAtSignIn), on) {
                self.port.act(Act::StartAtSignIn(on));
            }
            if let Err(why) = start {
                marked(ui, Mark::Error, Tone::Red, &w!(lang, SetSignInFailed, why = why), &[]);
            }
        }
        if let Some(back) = g.go_back {
            let label = w!(lang, SetGoBack);
            let under = w!(lang, SetGoBackLine);
            if let Some(on) = super::parts::control_line(ui, &label, Some(&under), |ui| crate::ui::inputs::Switch::new(back, &label).without_words().show(ui)) {
                self.port.act(Act::GoBack(on));
            }
        }
        if let Some(when) = g.when_stops {
            let on = when == WhenStops::BringBack;
            if let Some(on) = switch_line(ui, &w!(lang, SetBringBackSwitch), on) {
                self.port.act(Act::WhenStops(if on { WhenStops::BringBack } else { WhenStops::StaySilent }));
            }
            if !on {
                sentence(ui, &w!(lang, SetStaySilentOff));
            }
        }
        if let Some(on) = g.notifications {
            if let Some(on) = switch_line(ui, &w!(lang, SetNotify), on) {
                self.port.act(Act::Notifications(on));
            }
        }

        // Music Assistant, scripts, who can listen, the language.
        thin(ui);
        if let Some(offer) = ma.offer {
            if let Some(on) = switch_line(ui, &w!(lang, SetMaOffer), offer) {
                self.port.act(Act::OfferToMa(on));
                // Turning it on opens its set-up.
                if on {
                    ask(Ask::Open(Open::MusicAssistantSettings));
                }
            }
        }
        if let Some(api) = &p.api {
            if let Some(on) = switch_line(ui, &w!(lang, SetApi), api.on) {
                self.port.act(Act::Api(on));
            }
            if let (true, Some(key)) = (api.on, &api.key) {
                sentence(ui, &w!(lang, SetApiLine));
                super::parts::buttons(ui, |ui| {
                    crate::settings::widgets::copy_button(ui, &mut self.st, "api-key", &w!(lang, SetCopyKey), &w!(lang, SetCopied), key);
                    if Button::standard(&w!(lang, SetNewKey)).show(ui).clicked() {
                        self.port.act(Act::NewKey);
                    }
                });
                if Button::link(&w!(lang, SetApiHow)).icon(Icon::External).show(ui).clicked() {
                    self.port.act(Act::OpenApiHelp);
                }
            }
        }
        let anyone = !p.only_playing;
        if let Some(on) = switch_line(ui, &w!(lang, SetAnyoneListens), anyone) {
            self.port.act(Act::OnlyPlaying(!on));
        }
        if anyone {
            marked(ui, Mark::Warning, Tone::Primary, &w!(lang, SetAnyDeviceLine), &[]);
        }
        for r in p.refused.iter().flatten() {
            let at = words::time(lang, r.at);
            let who = match &r.name {
                Some(n) => w!(lang, SetRefusedNamed, name = n, address = &r.address, time = &at),
                None => w!(lang, SetRefusedUnknown, address = &r.address, time = &at),
            };
            let line = w!(lang, SetRefusedLine, who = &who);
            let allow = w!(lang, SetAllow);
            let clicked = control_line(ui, &line, None, |ui| Button::standard(&allow).accessible_name(w!(lang, SetAllowNamed, who = &who)).show(ui).clicked());
            if clicked {
                let speaker = r.group_of.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
                self.port.act(Act::Allow { address: r.address.clone(), speaker });
            }
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
        gap(ui);
    }

    fn has_links(&self, _snap: &Snapshot) -> bool {
        true
    }

    fn links(&mut self, ui: &mut Ui, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        if Button::link(&w!(lang, SetGuides)).show(ui).clicked() {
            ask(Ask::View(Box::new(GuidesView::new(self.port.clone()))));
        }
        if Button::link(&w!(lang, SetReportProblem)).show(ui).clicked() {
            super::open_dialog(&self.port, Dialog::Report(ReportDialog::default()), ask);
        }
        if Button::link(&w!(lang, SetNavLab)).show(ui).clicked() {
            ask(Ask::View(Box::new(LabView::new(self.port.clone()))));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if Button::link(&w!(lang, SetNavAbout)).show(ui).clicked() {
                ask(Ask::View(Box::new(AboutView::new(self.port.clone()))));
            }
        });
    }
}

/// A grey sentence and a link that continues it; true when the link was
/// clicked.
fn sentence_with_link(ui: &mut Ui, text: &str, link: &str) -> bool {
    super::parts::sentence_links(ui, text, &[link]).is_some()
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
    if Button::standard(&w!(lang, SetChangeKeys)).show(ui).clicked() {
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
