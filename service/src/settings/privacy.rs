//! Settings → Privacy & remote: who can listen, devices recently refused,
//! the phone remote, and the key for scripts.

use egui::Ui;

use crate::ui::button::Button;
use crate::ui::icon::Icon;
use crate::ui::inputs::{Choice, Switch};
use crate::ui::text::{Mark, Tone};

use super::port::{Act, Api, Phone, Refused};
use super::widgets::{body, boxed, buttons, copy_button, divider, heading, marked, mono, note, page_title, row};
use super::words::{self, w};
use super::{Cx, PageState};

pub(super) fn page(ui: &mut Ui, cx: &mut Cx, st: &mut PageState) {
    let lang = cx.lang;
    let v = cx.port.privacy();
    page_title(ui, &w!(lang, SetNavPrivacy));

    boxed(ui, |ui| {
        body(ui, &w!(lang, SetWhoCanListen));
        if Choice::new(v.only_playing, &w!(lang, SetOnlyPlaying)).description(&w!(lang, SetOnlyPlayingLine)).show(ui) {
            cx.act(Act::OnlyPlaying(true));
        }
        if Choice::new(!v.only_playing, &w!(lang, SetAnyDevice)).description(&w!(lang, SetAnyDeviceLine)).show(ui) {
            cx.act(Act::OnlyPlaying(false));
        }
    });

    if let Some(refused) = v.refused.as_ref().filter(|r| !r.is_empty()) {
        boxed(ui, |ui| {
            body(ui, &w!(lang, SetRecentlyRefused));
            for r in refused {
                refused_row(ui, cx, r);
            }
        });
    }

    if v.phone.is_some() || v.api.is_some() {
        heading(ui, &w!(lang, SetFromElsewhere));
    }
    if let Some(p) = &v.phone {
        phone(ui, cx, st, p);
    }
    if let Some(a) = &v.api {
        api(ui, cx, st, a);
    }
}

fn refused_row(ui: &mut Ui, cx: &mut Cx, r: &Refused) {
    let lang = cx.lang;
    let at = words::time(lang, r.at);
    let who = match &r.name {
        Some(n) => w!(lang, SetRefusedNamed, name = n, address = &r.address, time = &at),
        None => w!(lang, SetRefusedUnknown, address = &r.address, time = &at),
    };
    let icon = if r.group_of.is_some() { Icon::Speaker } else { Icon::Phone };
    row(ui, Some(icon), &who, None, |ui| match &r.group_of {
        Some((id, speaker)) => {
            let long = w!(lang, SetAllowFor, speaker = speaker);
            let short = w!(lang, SetAllowIt);
            if Button::standard(super::widgets::fitting(ui, &long, &short)).accessible_name(long.clone()).show(ui).clicked() {
                cx.act(Act::Allow { address: r.address.clone(), speaker: id.clone() });
            }
        }
        None => super::widgets::side_note(ui, &w!(lang, SetNotPartOfSpeaker), 160.0),
    });
}

fn phone(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, p: &Phone) {
    let lang = cx.lang;
    boxed(ui, |ui| {
        let title = w!(lang, SetPhone);
        if let Some(on) = row(ui, Some(Icon::Phone), &title, Some(&w!(lang, SetPhoneLine)), |ui| Switch::new(p.on, &title).show(ui)) {
            cx.act(Act::Phone(on));
        }
        if let Some(why) = &p.problem {
            marked(ui, Mark::Error, Tone::Red, why);
        }
        let Some(link) = p.link.as_ref().filter(|_| p.on) else { return };
        divider(ui);
        ui.horizontal_top(|ui| {
            ui.add_space(46.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                body(ui, &w!(lang, SetScanOrOpen));
                mono(ui, link);
                buttons(ui, |ui| {
                    copy_button(ui, st, "phone-link", &w!(lang, SetCopyLink), &w!(lang, SetCopied), link);
                    if Button::standard(&w!(lang, SetNewLink)).show(ui).clicked() {
                        cx.act(Act::NewLink);
                    }
                });
                note(ui, &w!(lang, SetLinkLine));
            });
        });
    });
}

fn api(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, a: &Api) {
    let lang = cx.lang;
    boxed(ui, |ui| {
        let title = w!(lang, SetApi);
        if let Some(on) = row(ui, Some(Icon::Wrench), &title, Some(&w!(lang, SetApiLine)), |ui| Switch::new(a.on, &title).show(ui)) {
            cx.act(Act::Api(on));
        }
        ui.horizontal(|ui| {
            ui.add_space(46.0);
            if Button::link(&w!(lang, SetApiHow)).icon(Icon::External).show(ui).clicked() {
                cx.act(Act::OpenApiHelp);
            }
        });
        if let (true, Some(key)) = (a.on, &a.key) {
            ui.horizontal(|ui| {
                ui.add_space(46.0);
                buttons(ui, |ui| {
                    copy_button(ui, st, "api-key", &w!(lang, SetCopyKey), &w!(lang, SetCopied), key);
                    if Button::standard(&w!(lang, SetNewKey)).show(ui).clicked() {
                        cx.act(Act::NewKey);
                    }
                });
            });
        }
    });
}
