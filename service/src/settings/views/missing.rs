//! *Speaker missing?*: when a speaker shows up, where the app is looking,
//! adding a speaker by its address (the result in place), the networks to
//! look on, Sendspin speakers, the speakers hidden from the list and the
//! ones found that this app can't play to; at the bottom *Check step by
//! step*.

use std::sync::Arc;

use egui::Ui;

use crate::model::Snapshot;
use crate::shell::{Ask, ViewBar, ViewContent};
use crate::ui::button::Button;
use crate::ui::inputs::{Choice, TextField};
use crate::ui::text::{Mark, Tone};

use super::parts::{control_line, gap, marked, plain, sentence, thin};
use crate::settings::port::*;
use crate::settings::widgets::{check_line, Found as Mark2};
use crate::settings::words::{self, w};
use crate::settings::SettingsPort;

pub struct MissingView {
    port: Arc<dyn SettingsPort>,
    address: String,
    /// *Add* was pressed: a speaker found one way is added at once.
    adding: bool,
    /// The way chosen when two answered.
    way: Option<Way>,
    /// The details typed by hand: shown, AirPlay or UPnP, port,
    /// description address.
    by_hand: bool,
    airplay: bool,
    port_typed: String,
    description: String,
    /// The name of the speaker last added, for the line that says so.
    added: Option<String>,
    /// Put the keyboard in the address field (opened to add a speaker).
    focus_address: bool,
}

impl MissingView {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        port.act(Act::ResetProbe);
        MissingView {
            port,
            address: String::new(),
            adding: false,
            way: None,
            by_hand: false,
            airplay: true,
            port_typed: String::new(),
            description: String::new(),
            added: None,
            focus_address: false,
        }
    }

    /// Opened to add a speaker by its address: the keyboard in its field.
    pub fn to_add(mut self) -> Self {
        self.focus_address = true;
        self
    }

    /// With an address typed (pictures and tests).
    pub fn with_address(mut self, address: &str) -> Self {
        self.address = address.into();
        self
    }

    /// With the fields for the details by hand open (pictures).
    pub fn by_hand(mut self) -> Self {
        self.by_hand = true;
        self
    }

    /// With the fields for a UPnP speaker by hand open (pictures).
    pub fn by_hand_upnp(mut self) -> Self {
        self.by_hand = true;
        self.airplay = false;
        self
    }
}

impl ViewContent for MissingView {
    fn bar(&self, _snap: &Snapshot) -> ViewBar {
        ViewBar { title: w!(self.port.lang(), SpeakerMissing), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, _snap: &Snapshot, _ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 8.0;
        let v = self.port.speakers();
        sentence(ui, &w!(lang, SetShowsUpWhen));
        // Where the app looks.
        if let Some(n) = &v.networks {
            let looking = looking_words(lang, n);
            let again = w!(lang, SetSearchAgain);
            if control_line(ui, &looking, None, |ui| Button::standard(&again).show(ui).clicked()) {
                self.port.act(Act::SearchAgain);
            }
        }
        self.address_part(ui, lang);
        // The networks to look on.
        if let Some(n) = &v.networks {
            if n.pinned.is_none() {
                let label = w!(lang, SetLookOn);
                let mut options = vec![w!(lang, SetAllNetworks)];
                options.extend(n.adapters.iter().map(|a| a.name.clone()));
                let refs: Vec<&str> = options.iter().map(String::as_str).collect();
                let chosen = n.chosen.as_ref().and_then(|k| n.adapters.iter().position(|a| &a.key == k)).map_or(0, |i| i + 1);
                if let Some(i) = super::parts::drop_line(ui, "look-on", &label, None, &refs, chosen) {
                    self.port.act(Act::LookOn(if i == 0 { None } else { n.adapters.get(i - 1).map(|a| a.key.clone()) }));
                }
            }
        }
        if let Some(on) = v.sendspin {
            let label = w!(lang, SetLookSendspin);
            let under = format!("{} {}", w!(lang, SetExperimentalCap), w!(lang, SetSendspinLine));
            let switched = control_line(ui, &label, Some(&under), |ui| crate::ui::inputs::Switch::new(on, &label).without_words().show(ui));
            if let Some(on) = switched {
                self.port.act(Act::SendspinSpeakers(on));
            }
        }
        // Hidden ones, and ones this app can't play to.
        if !v.hidden.is_empty() {
            thin(ui);
            sentence(ui, &w!(lang, SetHiddenFromList));
            for r in &v.hidden {
                let show = w!(lang, SetShow);
                let kind = words::kind_short(lang, &r.kind);
                let clicked = control_line(ui, &r.name, Some(&kind), |ui| {
                    Button::standard(&show).accessible_name(w!(lang, SetShowNamed, speaker = &r.name)).show(ui).clicked()
                });
                if clicked {
                    self.port.act(Act::ShowSpeaker(r.id.clone()));
                }
            }
        }
        let cant: Vec<&SpeakerRow> = v.speakers.iter().chain(v.hidden.iter()).filter(|r| r.cant_play.is_some()).collect();
        if !cant.is_empty() {
            thin(ui);
            sentence(ui, &w!(lang, SetCantPlayHeading));
            for r in cant {
                let why = match r.cant_play {
                    Some(CantPlay::AirPlayOff) | None => w!(lang, SetCantPlayFixShort),
                };
                let line = w!(lang, SetKindStatus, kind = &words::kind_short(lang, &r.kind), status = &why);
                control_line(ui, &r.name, Some(&line), |_| ());
            }
        }
        gap(ui);
        let _ = plain;
    }

    fn has_links(&self, _snap: &Snapshot) -> bool {
        true
    }

    fn links(&mut self, ui: &mut Ui, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        if Button::link(&w!(self.port.lang(), SetCheckStepByStep)).show(ui).clicked() {
            ask(Ask::View(Box::new(CheckView::new(self.port.clone()))));
        }
    }

    fn holds_unused_text(&self) -> bool {
        !self.address.trim().is_empty() && self.added.is_none()
    }
}

impl MissingView {
    /// *Add a speaker by its address*: the field and *Add*; the result in
    /// place.
    fn address_part(&mut self, ui: &mut Ui, lang: crate::model::text::Lang) {
        let probe = self.port.probe();
        plain(ui, &w!(lang, SetAddByItsAddress));
        let add = w!(lang, SetAddButton);
        let mut go = false;
        ui.horizontal(|ui| {
            let button_w = ui
                .painter()
                .layout_no_wrap(add.clone(), crate::ui::theme::Theme::of(ui.ctx()).font(crate::ui::theme::size::CONTROL), egui::Color32::PLACEHOLDER)
                .size()
                .x
                + 34.0;
            let field_w = (ui.available_width() - button_w - 8.0).max(80.0);
            let r = TextField::new(&mut self.address, &w!(lang, SetAddress)).hint(&w!(lang, SetAddressHint)).width(field_w).show(ui);
            if std::mem::take(&mut self.focus_address) {
                r.request_focus();
                r.scroll_to_me(Some(egui::Align::Center));
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            let checking = matches!(probe, Probe::Checking { .. });
            // At the right edge, whatever its width turns out to be.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Button::standard(&add).enabled(!self.address.trim().is_empty() && !checking).show(ui).clicked() {
                    go = true;
                }
            });
        });
        if go && !self.address.trim().is_empty() {
            self.adding = true;
            self.way = None;
            self.added = None;
            self.port.act(Act::Find { address: self.address.trim().to_string(), name: String::new() });
        }
        match &probe {
            Probe::Checking { address } => {
                marked(ui, Mark::Busy, Tone::Secondary, &w!(lang, SetCheckingAddress, address = address), &[]);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            Probe::Nothing { address } => {
                marked(ui, Mark::Error, Tone::Red, &w!(lang, SetNothingAt, address = address), &[]);
            }
            Probe::Invalid(why) => {
                marked(ui, Mark::Error, Tone::Red, why, &[]);
            }
            Probe::Found { address, ways } => self.found(ui, lang, address, ways),
            Probe::Added => {
                if let Some(name) = &self.added {
                    marked(ui, Mark::Done, Tone::Secondary, &w!(lang, SetAddedNamed, speaker = name), &[]);
                }
            }
            Probe::Idle => {}
        }
        if !self.by_hand {
            if Button::link(&w!(lang, SetByHandLink)).text_size(crate::ui::theme::size::SECONDARY).show(ui).clicked() {
                self.by_hand = true;
            }
        } else {
            self.by_hand_fields(ui, lang);
        }
    }

    /// What answered: one way is added at once; two ways ask which.
    fn found(&mut self, ui: &mut Ui, lang: crate::model::text::Lang, address: &str, ways: &[Found]) {
        let Some(first) = ways.first() else { return };
        if ways.len() == 1 {
            if self.adding {
                self.adding = false;
                self.added = Some(first.name.clone());
                self.port.act(Act::AddFound { way: first.way, name: first.name.clone() });
            }
            let line = w!(lang, SetFoundLine, kind = &words::kind_short(lang, &first.kind), address = address, delay = &words::about(lang, first.delay_ms));
            marked(ui, Mark::Done, Tone::Secondary, &w!(lang, SetFoundNamed, speaker = &first.name), &[]);
            sentence(ui, &line);
            return;
        }
        plain(ui, &w!(lang, SetFoundTwoWaysNamed, speaker = &first.name));
        let way = self.way.unwrap_or(first.way);
        for f in ways {
            let label = match f.way {
                Way::Upnp => w!(lang, SetWayRecommended, way = &words::kind_short(lang, &f.kind)),
                Way::AirPlay => w!(lang, SetWayPlain, way = &w!(lang, KindAirPlay)),
            };
            if Choice::new(f.way == way, &label).description(&words::about(lang, f.delay_ms)).show(ui) {
                self.way = Some(f.way);
            }
        }
        let chosen = ways.iter().find(|f| f.way == way).unwrap_or(first);
        let add = w!(lang, SetAddNamed, speaker = &chosen.name);
        let short = w!(lang, SetAddButton);
        let label = crate::settings::widgets::fitting(ui, &add, &short);
        if Button::primary(label).accessible_name(add.clone()).show(ui).clicked() {
            self.added = Some(chosen.name.clone());
            self.port.act(Act::AddFound { way, name: chosen.name.clone() });
        }
    }

    /// The details by hand: AirPlay or UPnP, the port or the description
    /// address, *Add*.
    fn by_hand_fields(&mut self, ui: &mut Ui, lang: crate::model::text::Lang) {
        gap(ui);
        let label = w!(lang, SetConnectsBy);
        let options = [w!(lang, KindAirPlay), w!(lang, SetUpnpKind)];
        let refs: Vec<&str> = options.iter().map(String::as_str).collect();
        if let Some(i) = super::parts::drop_line(ui, "by-hand-kind", &label, None, &refs, if self.airplay { 0 } else { 1 }) {
            self.airplay = i == 0;
        }
        if self.airplay {
            let p = w!(lang, SetPort);
            plain(ui, &p);
            let _ = TextField::new(&mut self.port_typed, &p).hint("7000").show(ui);
        } else {
            let d = w!(lang, SetDescriptionAddress);
            plain(ui, &d);
            let _ = TextField::new(&mut self.description, &d).show(ui);
        }
        let ready = if self.airplay { !self.address.trim().is_empty() } else { !self.description.trim().is_empty() };
        if Button::standard(&w!(lang, SetAddButton)).enabled(ready).show(ui).clicked() {
            self.added = Some(self.address.trim().to_string());
            self.port.act(Act::AddByHand {
                airplay: self.airplay,
                host: self.address.trim().to_string(),
                port: self.port_typed.trim().parse().ok(),
                description: self.description.trim().to_string(),
                name: String::new(),
            });
        }
    }
}

/// *Looking on Wi-Fi “HomeNet”*: where the app looks, without a count.
fn looking_words(lang: crate::model::text::Lang, n: &Networks) -> String {
    if let Some(addr) = &n.pinned {
        return w!(lang, SetLookingPinned, address = addr);
    }
    match &n.looking {
        Looking::On(places) if !places.is_empty() => {
            let names: Vec<String> = places
                .iter()
                .map(|p| match &p.network {
                    Some(net) => w!(lang, SetNetworkNamedShort, adapter = &p.adapter, network = net),
                    None => p.adapter.clone(),
                })
                .collect();
            let list = match names.as_slice() {
                [one] => one.clone(),
                [rest @ .., last] => w!(lang, SetListAnd, list = &rest.join(", "), last = last),
                [] => String::new(),
            };
            w!(lang, SetLookingOnShort, networks = &list)
        }
        Looking::AdapterOff { adapter } => w!(lang, SetNotLookingOff, adapter = adapter),
        _ => w!(lang, SetLookingNowhere),
    }
}

/// *Check step by step*: each line says only what was checked.
pub struct CheckView {
    port: Arc<dyn SettingsPort>,
}

impl CheckView {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        CheckView { port }
    }
}

impl ViewContent for CheckView {
    fn bar(&self, _snap: &Snapshot) -> ViewBar {
        ViewBar { title: w!(self.port.lang(), SetCheckStepByStep), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 8.0;
        if snap.scene.search.phase == crate::model::facts::SearchPhase::Off {
            check_line(ui, Mark2::Info, &w!(lang, SetFindSearchOff), None, |_| ());
            return;
        }
        let c = self.port.discovery_check();
        let place = |p: &NetworkPlace| match &p.network {
            Some(net) => w!(lang, SetNetworkNamed, adapter = &p.adapter, network = net, address = &p.address),
            None => w!(lang, SetNetworkPlain, adapter = &p.adapter, address = &p.address),
        };
        for line in &c.lines {
            match line {
                FindLine::LookingOn { place: p, .. } => check_line(ui, Mark2::Good, &w!(lang, SetFindLooking, network = &place(p)), None, |_| ()),
                FindLine::Vpn { vpn, only } => {
                    check_line(ui, Mark2::Warning, &w!(lang, SetFindVpn, network = &place(vpn)), Some(&w!(lang, SetFindVpnLine)), |ui| {
                        if Button::standard(&w!(lang, SetLookOnlyOn, adapter = &only.name)).show(ui).clicked() {
                            self.port.act(Act::LookOnlyOn(only.key.clone()));
                        }
                    })
                }
                FindLine::Profile { network, private: true } => check_line(ui, Mark2::Good, &w!(lang, SetFindPrivate, network = network), None, |_| ()),
                // A caution: the app doesn't read Windows Firewall, which
                // is stricter on a public network.
                FindLine::Profile { network, private: false } => {
                    check_line(ui, Mark2::Warning, &w!(lang, SetFindPublic, network = network), Some(&w!(lang, SetFindPublicLine)), |ui| {
                        if Button::standard(&w!(lang, SetOpenNetworkSettings)).show(ui).clicked() {
                            ask(Ask::Open(crate::model::command::Open::NetworkSettings));
                        }
                    })
                }
                FindLine::AirPlaySearchFailed => check_line(ui, Mark2::Warning, &w!(lang, SetFindAirPlayFailed), None, |_| ()),
                FindLine::AdapterOff { adapter } => check_line(ui, Mark2::Bad, &w!(lang, SetNotLookingOff, adapter = adapter), None, |ui| {
                    if Button::standard(&w!(lang, SetLookEverywhere)).show(ui).clicked() {
                        self.port.act(Act::LookOn(None));
                    }
                }),
                FindLine::Failed { adapter, why } => check_line(ui, Mark2::Bad, &w!(lang, SetFindFailed, adapter = adapter), Some(why), |_| ()),
            }
        }
        check_line(ui, Mark2::Info, &w!(lang, SetFindSameNetwork), Some(&w!(lang, SetFindSameNetworkLine)), |_| ());
        if Button::standard(&w!(lang, SetSearchAgain)).show(ui).clicked() {
            self.port.act(Act::SearchAgain);
        }
    }
}
