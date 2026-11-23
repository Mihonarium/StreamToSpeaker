//! Settings → Music Assistant: offering this PC's sound to Music
//! Assistant, three steps that tick themselves, the token for other
//! Sendspin servers, the name this PC appears as.

use egui::{Sense, Ui, Vec2};

use crate::ui::button::Button;
use crate::ui::card::SettingRow;
use crate::ui::icon::Icon;
use crate::ui::inputs::Switch;
use crate::ui::notice::{info_bar, BarKind, InfoBar};
use crate::ui::text::{Mark, Tone};
use crate::ui::theme::{size, Theme};

use super::port::{Act, MaView};
use super::widgets::{boxed, buttons, copy_button, divider, fold, heading, kept_field, marked, mono, note, page_title_with, pill, step, tint, StepMark};
use super::words::{self, w};
use super::{Cx, PageState, Request};

pub(super) fn page(ui: &mut Ui, cx: &mut Cx, st: &mut PageState) {
    let lang = cx.lang;
    let v = cx.port.music_assistant();
    let experimental = w!(lang, SetExperimental);
    page_title_with(ui, &w!(lang, MaName), |ui| {
        ui.vertical(|ui| {
            ui.add_space(8.0);
            pill(ui, &experimental);
        });
    });
    ui.add_space(-6.0);
    super::widgets::body(ui, &w!(lang, SetMaLead));
    ui.add_space(6.0);

    let Some(offer) = v.offer else {
        marked(ui, Mark::Info, Tone::Secondary, &w!(lang, SetMaOffForRun));
        return;
    };
    let title = w!(lang, SetMaOffer);
    let r = SettingRow::new(&title).icon(Icon::Hub).description(&w!(lang, SetMaOfferLine)).show(ui, |ui| Switch::new(offer, &title).show(ui));
    if let Some(on) = r.inner {
        cx.act(Act::OfferToMa(on));
    }
    if let Some(why) = &v.failed {
        marked(ui, Mark::Error, Tone::Red, &w!(lang, SetMaFailed, why = why));
    }
    if offer {
        if v.paused {
            let t = w!(lang, SetMaPausedTitle);
            let text = w!(lang, SetMaPausedLine);
            let a = w!(lang, SetMaAllowAgain);
            if info_bar(ui, &InfoBar { kind: BarKind::Warning, title: Some(&t), text: &text, action: Some(&a) }) {
                cx.act(Act::AllowPairingAgain);
            }
        }
        heading(ui, &w!(lang, SetMaSettingUp));
        steps(ui, cx, st, &v);
        token(ui, cx, st, &v);
    }

    let title = w!(lang, SetMaAppearsAs);
    let r = SettingRow::new(&title)
        .icon(Icon::Laptop)
        .description(&w!(lang, SetMaAppearsAsLine))
        .show(ui, |ui| kept_field(ui, st, "ma-appears-as", &v.appears_as, &title, 220.0));
    if let Some(n) = r.inner {
        if !n.is_empty() {
            cx.act(Act::AppearsAs(n));
        }
    }
    let _ = SettingRow::new(&w!(lang, SetDelayHeading)).icon(Icon::Timer).description(&w!(lang, SetMaDelayLine)).show(ui, |_| ());
}

/// The three steps, each ticking itself when the app sees it happen.
fn steps(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, v: &MaView) {
    let lang = cx.lang;
    let pc = v.appears_as.as_str();
    let found = v.found_by.is_some() || !v.paired_with.is_empty();
    let paired = !v.paired_with.is_empty();
    let played = v.played.is_some();
    let mark = |done: bool, current: bool| if done { StepMark::Done } else if current { StepMark::Current } else { StepMark::Later };
    boxed(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        step(ui, 1, mark(found, !found), &w!(lang, SetMaStep1), |ui| match &v.found_by {
            Some((server, Some(at))) => note(ui, &w!(lang, SetMaFoundAt, server = server, address = at, pc = pc)),
            Some((server, None)) => note(ui, &w!(lang, SetMaFound, server = server, pc = pc)),
            None if found => note(ui, &w!(lang, SetMaFoundBefore, pc = pc)),
            None => note(ui, &w!(lang, SetMaStep1Line, pc = pc)),
        });
        divider(ui);
        step(ui, 2, mark(paired, found && !paired), &w!(lang, SetMaStep2), |ui| {
            if paired {
                for server in &v.paired_with {
                    ui.horizontal_wrapped(|ui| {
                        note(ui, &w!(lang, SetMaPaired, server = server));
                        if Button::link(&w!(lang, SetMaUnpair)).accessible_name(w!(lang, SetMaUnpairNamed, server = server)).show(ui).clicked() {
                            cx.act(Act::Unpair(server.clone()));
                        }
                    });
                }
            } else {
                note(ui, &w!(lang, SetMaStep2Line, pc = pc));
                if let Some((code, left)) = &v.code {
                    ui.add_space(4.0);
                    code_box(ui, cx, st, code, *left);
                }
            }
        });
        divider(ui);
        step(ui, 3, mark(played, paired && !played), &w!(lang, SetMaStep3), |ui| match v.played {
            Some(Some(d)) => note(ui, &w!(lang, SetMaPlayedOn, date = &words::date(lang, d))),
            Some(None) => note(ui, &w!(lang, SetMaPlayed)),
            None => note(ui, &w!(lang, SetMaStep3Line, pc = pc)),
        });
        divider(ui);
        if found && paired && played {
            note(ui, &w!(lang, SetMaAllDone));
            ui.add_space(2.0);
            buttons(ui, |ui| {
                if Button::standard(&w!(lang, SetOpenPanel)).show(ui).clicked() {
                    cx.requests.push(Request::OpenPanel);
                }
                if v.found_by.as_ref().is_some_and(|(_, a)| a.is_some()) && Button::standard(&w!(lang, SetOpenMa)).icon(Icon::External).show(ui).clicked() {
                    cx.act(Act::OpenMusicAssistant);
                }
            });
        } else {
            note(ui, &w!(lang, SetMaTicks));
        }
    });
}

/// The code, large, with *Copy* and the time it has left.
pub(super) fn code_box(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, code: &str, left: Option<u32>) {
    let lang = cx.lang;
    let t = Theme::of(ui.ctx());
    egui::Frame::none().fill(tint(ui)).rounding(6.0).inner_margin(egui::Margin::symmetric(16.0, 10.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            let spaced = spaced_code(code);
            let g = ui.painter().layout_no_wrap(spaced, t.strong(30.0), t.c.text);
            let (r, _) = ui.allocate_exact_size(g.size() + Vec2::new(0.0, 4.0), Sense::hover());
            ui.painter().galley(r.min, g, t.c.text);
            copy_button(ui, st, "ma-code", &w!(lang, SetCopy), &w!(lang, SetCopied), code);
            if let Some(s) = left {
                ui.add(egui::Label::new(egui::RichText::new(words::left(lang, s)).font(t.font(size::CAPTION)).color(t.c.text_secondary)).extend());
            }
        });
    });
}

/// *482 917*: the digits in two groups of three.
pub(super) fn spaced_code(code: &str) -> String {
    let digits: Vec<char> = code.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() == 6 {
        let a: String = digits[..3].iter().collect();
        let b: String = digits[3..].iter().collect();
        format!("{a}\u{2002}{b}")
    } else {
        code.to_string()
    }
}

/// *Pairing with another Sendspin server?*: the token, masked until
/// *Show*.
fn token(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, v: &MaView) {
    let lang = cx.lang;
    let Some(token) = &v.token else { return };
    let line = w!(lang, SetMaTokenLine);
    fold(ui, st, "ma-token", Icon::Lock, &w!(lang, SetMaToken), Some(&line), |ui, st| {
        let shown = if st.token_shown { token.clone() } else { mask(token) };
        mono(ui, &shown);
        buttons(ui, |ui| {
            copy_button(ui, st, "ma-token", &w!(lang, SetCopyToken), &w!(lang, SetCopied), token);
            let label = if st.token_shown { w!(lang, SetHide) } else { w!(lang, SetShow) };
            if Button::standard(&label).show(ui).clicked() {
                st.token_shown = !st.token_shown;
            }
        });
    });
    let _ = cx;
}

/// `SP:0 •••• •••• ••••`: the start and dots.
fn mask(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("{head} •••• •••• ••••")
}
