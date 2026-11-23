//! Settings → Speakers: every speaker this PC knows, each opening its
//! page; hidden ones; finding speakers; more kinds of speakers.

use egui::Ui;

use crate::model::text::Lang;
use crate::ui::button::Button;
use crate::ui::card::SettingRow;
use crate::ui::icon::Icon;
use crate::ui::inputs::{DropDown, Switch};

use super::dialogs::{AddDialog, Dialog};
use super::port::{Act, CantPlay, Looking, Networks, Presence, SpeakerKind, SpeakerRow};
use super::widgets::{buttons, heading, link, page_title};
use super::words::{self, w};
use super::{Cx, Page, PageState};

pub(super) fn page(ui: &mut Ui, cx: &mut Cx, _st: &mut PageState) {
    let lang = cx.lang;
    let v = cx.port.speakers();
    page_title(ui, &w!(lang, SetNavSpeakers));

    heading(ui, &w!(lang, SetYourSpeakers));
    if v.speakers.is_empty() {
        let _ = SettingRow::new(&w!(lang, SetNoSpeakersYet)).icon(Icon::Search).description(&w!(lang, SetNoSpeakersYetLine)).show(ui, |_| ());
    }
    for s in &v.speakers {
        let r = SettingRow::new(&s.name).icon(kind_icon(&s.kind)).description(&row_line(lang, s)).opens_page().show(ui, |_| ());
        if r.opened {
            cx.go(Page::Speaker(s.id.clone()));
        }
    }

    if !v.hidden.is_empty() {
        heading(ui, &w!(lang, SetHiddenHeading, count = v.hidden.len()));
        for s in &v.hidden {
            let line = w!(lang, SetHiddenLine, kind = &words::kind(lang, &s.kind));
            let show = w!(lang, SetShow);
            let r = SettingRow::new(&s.name)
                .icon(kind_icon(&s.kind))
                .description(&line)
                .show(ui, |ui| Button::standard(&show).accessible_name(w!(lang, SetShowNamed, speaker = &s.name)).show(ui).clicked());
            if r.inner {
                cx.act(Act::ShowSpeaker(s.id.clone()));
            }
        }
    }

    heading(ui, &w!(lang, SetFindingSpeakers));
    if let Some(n) = &v.networks {
        look_for(ui, cx, n);
    }
    ui.add_space(8.0);
    buttons(ui, |ui| {
        if Button::standard(&w!(lang, SetAddByAddress)).icon(Icon::Plus).show(ui).clicked() {
            cx.dialog(Dialog::AddAddress(AddDialog::default()));
        }
        if v.networks.is_some() && Button::standard(&w!(lang, SetSearchAgain)).icon(Icon::Refresh).show(ui).clicked() {
            cx.act(Act::SearchAgain);
        }
        if link(ui, &w!(lang, SetCantFindLink)) {
            cx.go(Page::CantFind);
        }
    });

    if let Some(on) = v.sendspin {
        heading(ui, &w!(lang, SetMoreKinds));
        let title = w!(lang, SetSendspinSwitch);
        let r = SettingRow::new(&title).icon(Icon::Flask).description(&w!(lang, SetSendspinLine)).show(ui, |ui| Switch::new(on, &title).show(ui));
        if let Some(on) = r.inner {
            cx.act(Act::SendspinSpeakers(on));
        }
    }
}

/// *Look for speakers on* ▾ with what it is doing under it.
fn look_for(ui: &mut Ui, cx: &mut Cx, n: &Networks) {
    let lang = cx.lang;
    let title = w!(lang, SetLookOn);
    let line = looking_line(lang, n);
    let mut options = vec![w!(lang, SetAllNetworks)];
    options.extend(n.adapters.iter().map(|a| a.name.clone()));
    let refs: Vec<&str> = options.iter().map(String::as_str).collect();
    let chosen = n.chosen.as_ref().and_then(|k| n.adapters.iter().position(|a| &a.key == k)).map_or(0, |i| i + 1);
    let pinned = n.pinned.is_some();
    let r = SettingRow::new(&title)
        .icon(Icon::Wifi)
        .description(&line)
        .show(ui, |ui| DropDown::new("settings-look-on", &title, &refs, chosen).min_width(200.0).enabled(!pinned).show(ui));
    if let Some(i) = r.inner {
        cx.act(Act::LookOn(if i == 0 { None } else { n.adapters.get(i - 1).map(|a| a.key.clone()) }));
    }
}

/// *Looking on Wi-Fi “HomeNet” (192.168.1.23) and Ethernet 2 (10.8.0.4).*
pub(super) fn looking_line(lang: Lang, n: &Networks) -> String {
    if let Some(addr) = &n.pinned {
        return w!(lang, SetLookingPinned, address = addr);
    }
    match &n.looking {
        Looking::On(places) if !places.is_empty() => {
            let names: Vec<String> = places.iter().map(|p| place_words(lang, p)).collect();
            let list = match names.as_slice() {
                [one] => one.clone(),
                [rest @ .., last] => w!(lang, SetListAnd, list = &rest.join(", "), last = last),
                [] => String::new(),
            };
            w!(lang, SetLookingOn, networks = &list)
        }
        Looking::AdapterOff { adapter } => w!(lang, SetNotLookingOff, adapter = adapter),
        _ => w!(lang, SetLookingNowhere),
    }
}

/// *Wi-Fi “HomeNet” (192.168.1.23)*.
pub(super) fn place_words(lang: Lang, p: &super::port::NetworkPlace) -> String {
    match &p.network {
        Some(net) => w!(lang, SetNetworkNamed, adapter = &p.adapter, network = net, address = &p.address),
        None => w!(lang, SetNetworkPlain, adapter = &p.adapter, address = &p.address),
    }
}

/// A row's second line: the kind and one status.
pub(super) fn row_line(lang: Lang, s: &SpeakerRow) -> String {
    let kind = words::kind(lang, &s.kind);
    let status = if let Some(c) = s.cant_play {
        Some(match c {
            CantPlay::AirPlayOff => w!(lang, SetCantPlayRow),
        })
    } else {
        match &s.presence {
            Presence::PlayingNow => Some(w!(lang, SetPlayingNow)),
            Presence::Ready => Some(w!(lang, SetReadyNow)),
            Presence::Connecting => Some(w!(lang, SetConnectingNow)),
            Presence::NotFound { last_seen: Some(at) } => Some(w!(lang, SetNotFoundSeen, seen = &words::seen(lang, *at))),
            Presence::NotFound { last_seen: None } => Some(w!(lang, SetNotFoundRow)),
            Presence::Available if s.added_by_address => Some(w!(lang, SetAddedByAddress)),
            Presence::Available => None,
        }
    };
    match status {
        Some(st) => w!(lang, SetKindStatus, kind = &kind, status = &st),
        None => kind,
    }
}

/// The icon of a kind of speaker.
pub(super) fn kind_icon(k: &SpeakerKind) -> Icon {
    match k {
        SpeakerKind::SonosGroup => Icon::Group,
        SpeakerKind::HomePodPair => Icon::Pair,
        SpeakerKind::AppleTv => Icon::Tv,
        _ => Icon::Speaker,
    }
}
