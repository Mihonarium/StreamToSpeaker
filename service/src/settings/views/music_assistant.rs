//! Music Assistant's view, from the *›* at the end of its row or from
//! Settings: whether this PC is offered to it, the three steps that tick
//! themselves (or, once set up, that it isn't connected), its code, the
//! name this PC appears as.

use std::sync::Arc;

use egui::Ui;

use crate::model::Snapshot;
use crate::shell::{Ask, ViewBar, ViewContent};
use crate::ui::button::Button;
use crate::ui::text::{Mark, Tone};

use super::parts::{field_line, gap, marked, sentence, sentence_links, switch_line, thin};
use crate::settings::port::*;
use crate::settings::widgets::{step, StepMark};
use crate::settings::words::{self, w};
use crate::settings::{PageState, SettingsPort};

pub struct MusicAssistantView {
    port: Arc<dyn SettingsPort>,
    st: PageState,
}

impl MusicAssistantView {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        MusicAssistantView { port, st: PageState::default() }
    }
}

/// Where the set-up stands, in a few words for the bar.
fn state_words(lang: crate::model::text::Lang, v: &MaView) -> String {
    match v.offer {
        None | Some(false) => w!(lang, SetMaStateOff),
        Some(true) if v.paired_with.is_empty() => w!(lang, SetMaStateSettingUp),
        Some(true) if v.found_by.is_none() => w!(lang, SetMaStateNotConnected),
        Some(true) => w!(lang, SetMaStateReady),
    }
}

impl ViewContent for MusicAssistantView {
    fn bar(&self, _snap: &Snapshot) -> ViewBar {
        let lang = self.port.lang();
        let v = self.port.music_assistant();
        ViewBar { title: w!(lang, MaName), line: Some(state_words(lang, &v)), button: None }
    }

    fn show(&mut self, ui: &mut Ui, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 8.0;
        let v = self.port.music_assistant();
        sentence(ui, &w!(lang, SetMaLeadExperimental));
        let Some(offer) = v.offer else {
            marked(ui, Mark::Info, Tone::Secondary, &w!(lang, SetMaOffForRun), &[]);
            return;
        };
        if let Some(on) = switch_line(ui, &w!(lang, SetMaOffer), offer) {
            self.port.act(Act::OfferToMa(on));
        }
        if let Some(why) = &v.failed {
            marked(ui, Mark::Error, Tone::Red, &w!(lang, SetMaFailed, why = why), &[]);
        }
        if !offer {
            sentence(ui, &w!(lang, SetMaOfferLine));
            return;
        }
        if v.paused && marked(ui, Mark::Warning, Tone::Primary, &w!(lang, SetMaPausedBoth), &[&w!(lang, SetMaAllowAgain)]).is_some() {
            self.port.act(Act::AllowPairingAgain);
        }
        thin(ui);
        let pc = v.appears_as.as_str();
        let connected = v.found_by.is_some();
        let paired = !v.paired_with.is_empty();
        let played = v.played.is_some();
        let port = self.port.clone();
        let paired_lines = |ui: &mut Ui| {
            for server in &v.paired_with {
                if sentence_links(ui, &w!(lang, SetMaPaired, server = server), &[&w!(lang, SetMaUnpair)]).is_some() {
                    port.act(Act::Unpair(server.clone()));
                }
            }
        };
        if paired && !connected {
            // Set up earlier and not connected now. The steps would show a
            // later one done under an earlier one that isn't, so the view
            // says what holds in plain lines. A failure to start is said
            // above and is the reason then.
            if v.failed.is_none() {
                sentence(ui, &w!(lang, SetMaNotConnected));
            }
            public_network(ui, lang, &v, ask);
            paired_lines(ui);
        } else {
            let mark = |done: bool, current: bool| {
                if done {
                    StepMark::Done
                } else if current {
                    StepMark::Current
                } else {
                    StepMark::Later
                }
            };
            step(ui, 1, mark(connected, !connected), &w!(lang, SetMaStep1), |ui| match &v.found_by {
                Some((server, Some(at))) => {
                    sentence(ui, &w!(lang, SetMaFoundAt, server = server, address = at, pc = pc));
                    if Button::standard(&w!(lang, SetOpenMa)).icon(crate::ui::icon::Icon::External).show(ui).clicked() {
                        port.act(Act::OpenMusicAssistant);
                    }
                }
                Some((server, None)) => sentence(ui, &w!(lang, SetMaFound, server = server, pc = pc)),
                None => {
                    sentence(ui, &w!(lang, SetMaStep1Line, pc = pc));
                    public_network(ui, lang, &v, ask);
                }
            });
            step(ui, 2, mark(paired, connected && !paired), &w!(lang, SetMaStep2), |ui| {
                if paired {
                    paired_lines(ui);
                } else {
                    sentence(ui, &w!(lang, SetMaStep2Line, pc = pc));
                    if let Some((code, left)) = &v.code {
                        code_box(ui, lang, &mut self.st, code, *left);
                    }
                }
            });
            step(ui, 3, mark(played, connected && paired && !played), &w!(lang, SetMaStep3), |ui| match v.played {
                Some(Some(d)) => sentence(ui, &w!(lang, SetMaPlayedOn, date = &words::date(lang, d))),
                Some(None) => sentence(ui, &w!(lang, SetMaPlayed)),
                None => sentence(ui, &w!(lang, SetMaStep3Line, pc = pc)),
            });
            if connected && paired && played {
                sentence(ui, &w!(lang, SetMaAllDone));
            }
        }
        thin(ui);
        if let Some(n) = field_line(ui, &mut self.st, "appears-as", &w!(lang, SetMaAppearsAs), &v.appears_as) {
            if !n.is_empty() {
                self.port.act(Act::AppearsAs(n));
            }
        }
        sentence(ui, &w!(lang, SetMaDelayLine));
        if let Some(token) = &v.token {
            gap(ui);
            sentence(ui, &w!(lang, SetMaTokenLine));
            let shown = if self.st.token_shown { token.clone() } else { mask(token) };
            crate::settings::widgets::mono(ui, &shown);
            super::parts::buttons(ui, |ui| {
                crate::settings::widgets::copy_button(ui, &mut self.st, "ma-token", &w!(lang, SetCopyToken), &w!(lang, SetCopied), token);
                let label = if self.st.token_shown { w!(lang, SetHide) } else { w!(lang, SetShow) };
                if Button::standard(&label).accessible_name(w!(lang, SetShowToken)).show(ui).clicked() {
                    self.st.token_shown = !self.st.token_shown;
                }
            });
        }
    }

    fn holds_unused_text(&self) -> bool {
        let shown = self.port.music_assistant().appears_as;
        self.st.typing.get("appears-as").is_some_and(|t| t.trim() != shown)
    }
}

/// While Music Assistant isn't connected: that Windows treats the network
/// as public, where it does. Music Assistant has to reach this PC.
fn public_network(ui: &mut Ui, lang: crate::model::text::Lang, v: &MaView, ask: &mut dyn FnMut(Ask)) {
    let Some(network) = &v.public_network else { return };
    if marked(ui, Mark::Warning, Tone::Primary, &w!(lang, SetMaPublicLine, network = network), &[&w!(lang, SetOpenNetworkSettings)]).is_some() {
        ask(Ask::Open(crate::model::command::Open::NetworkSettings));
    }
}

/// The code to type in Music Assistant, large, with *Copy* and the time it
/// has left.
fn code_box(ui: &mut Ui, lang: crate::model::text::Lang, st: &mut PageState, code: &str, left: Option<u32>) {
    let t = crate::ui::theme::Theme::of(ui.ctx());
    egui::Frame::none().fill(t.c.accent_soft).rounding(6.0).inner_margin(egui::Margin::symmetric(12.0, 8.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let g = ui.painter().layout_no_wrap(crate::settings::common::spaced_code(code), t.strong(26.0), t.c.text);
            let (r, _) = ui.allocate_exact_size(g.size(), egui::Sense::hover());
            ui.painter().galley(r.min, g, t.c.text);
            crate::settings::widgets::copy_button(ui, st, "ma-code", &w!(lang, SetCopy), &w!(lang, SetCopied), code);
            if let Some(s) = left {
                ui.add(
                    egui::Label::new(egui::RichText::new(words::left(lang, s)).font(t.font(crate::ui::theme::size::CAPTION)).color(t.c.text_secondary))
                        .extend(),
                );
            }
        });
    });
}

/// `SP:0 •••• •••• ••••`.
fn mask(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("{head} •••• •••• ••••")
}
