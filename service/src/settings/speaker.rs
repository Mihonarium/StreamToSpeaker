//! A speaker's page: its delay first, then whatever applies to it (the way
//! it connects, its group, a volume limit, tone, coming back by itself,
//! song titles, pairing, its name here), its details, and hiding or
//! forgetting it.

use egui::{Rect, Sense, Ui, Vec2};

use crate::model::text::Lang;
use crate::ui::button::Button;
use crate::ui::card::{card, SettingRow, SETTINGS_CARD_PAD};
use crate::ui::delay::{DelayAction, DelayControl, DelayFigure, DelayNote, DelayView, Limits as UiLimits, Reconnect};
use crate::ui::icon::Icon;
use crate::ui::inputs::{Choice, DropDown, Switch};
use crate::ui::notice::BarKind;
use crate::ui::theme::{radius, size, Theme};
use crate::ui::volume::Slider;

use super::dialogs::{Dialog, PasswordDialog, ReportDialog, ToneDialog};
use super::port::*;
use super::speakers::kind_icon;
use super::widgets::{body, boxed, buttons, crumb, divider, fold, heading, kept_field, mono, note, row};
use super::words::{self, w};
use super::{Cx, Page, PageState};

pub(super) fn page(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, id: &str) {
    let lang = cx.lang;
    let Some(v) = cx.port.speaker(id) else {
        // Forgotten or removed: back to the list.
        cx.go(Page::Speakers);
        return;
    };
    let name = v.row.name.clone();
    if crumb(ui, &w!(lang, SetNavSpeakers), &name) {
        cx.back = true;
    }
    header(ui, cx, &v);

    if let Some(CantPlay::AirPlayOff) = v.row.cant_play {
        let _ = super::widgets::bar(ui, BarKind::Error, &w!(lang, SetCantPlayTitle, speaker = &name), &w!(lang, SetCantPlayFix), None);
    }
    if v.row.untested {
        let title = w!(lang, SetUntestedPairTitle);
        let text = w!(lang, SetUntestedPairLine, speaker = &name);
        let action = w!(lang, SetReportProblem);
        if super::widgets::bar(ui, BarKind::Warning, &title, &text, Some(&action)) {
            cx.dialog(Dialog::Report(ReportDialog::default()));
        }
    }

    if let Some(d) = &v.delay {
        heading(ui, &w!(lang, SetDelayHeading));
        delay_card(ui, cx, st, &v, d);
    }

    if let Some(c) = &v.connects_with {
        heading(ui, &w!(lang, SetConnectsWith));
        boxed(ui, |ui| {
            let own = w!(lang, SetWaySonos, delay = &words::about(lang, c.own_delay_ms));
            let airplay = w!(lang, SetWayAirPlay, delay = &words::about(lang, c.airplay_delay_ms));
            if Choice::new(c.own_way, &own).show(ui) {
                cx.act(Act::ConnectWith { id: id.to_string(), own_way: true });
            }
            if Choice::new(!c.own_way, &airplay).description(&w!(lang, SetWayAirPlayLine)).show(ui) {
                cx.act(Act::ConnectWith { id: id.to_string(), own_way: false });
            }
            note(ui, &w!(lang, SetWaysKeptApart));
        });
    }

    if let Some(g) = &v.group {
        heading(ui, &w!(lang, SetGroupHeading));
        group_card(ui, lang, &name, g);
    }

    if let Some(limit) = v.volume_limit {
        heading(ui, &w!(lang, SetVolumeLimit));
        boxed(ui, |ui| {
            let label = w!(lang, SetNeverLouder);
            let value = if limit >= 100 { w!(lang, SetNoLimit) } else { limit.to_string() };
            if let Some(n) = slider_row(ui, &label, &value, limit as f32, 0.0..=100.0, 1.0) {
                cx.act(Act::VolumeLimit(id.to_string(), n.round() as u8));
            }
            note(ui, &w!(lang, SetVolumeLimitLine));
        });
    }

    if let Some((bass, treble)) = v.tone {
        heading(ui, &w!(lang, SetToneHeading));
        let can_undo = cx.port.tone(id).is_some_and(|t| t.can_undo);
        boxed(ui, |ui| {
            if let Some(b) = slider_row(ui, &w!(lang, SetBass), &db(lang, bass), bass, -12.0..=12.0, 0.5) {
                cx.act(Act::Tone(id.to_string(), ToneAct::Bass(b)));
            }
            if let Some(tr) = slider_row(ui, &w!(lang, SetTreble), &db(lang, treble), treble, -12.0..=12.0, 0.5) {
                cx.act(Act::Tone(id.to_string(), ToneAct::Treble(tr)));
            }
            ui.add_space(4.0);
            buttons(ui, |ui| {
                if Button::standard(&w!(lang, SetAllBands)).icon(Icon::Sliders).show(ui).clicked() {
                    cx.dialog(Dialog::Tone(ToneDialog::new(id)));
                }
                if Button::standard(&w!(lang, SetUndo)).enabled(can_undo).show(ui).clicked() {
                    cx.act(Act::Tone(id.to_string(), ToneAct::Undo));
                }
                if Button::standard(&w!(lang, SetResetFlat)).show(ui).clicked() {
                    cx.act(Act::Tone(id.to_string(), ToneAct::Flat));
                }
            });
            note(ui, &w!(lang, SetToneLine, speaker = &name));
        });
    }

    let has_connection = v.pairing.is_some() || v.comes_back.is_some() || v.song_titles.is_some() || v.show_as.is_some();
    if has_connection {
        heading(ui, &w!(lang, SetConnectionHeading));
    }
    match &v.pairing {
        Some(Pairing::Paired { on }) => {
            let title = match on {
                Some(d) => w!(lang, SetPairedOn, date = &words::date(lang, *d)),
                None => w!(lang, SetPaired),
            };
            let r = SettingRow::new(&title).icon(Icon::Lock).description(&w!(lang, SetPairedLine, speaker = &name)).show(ui, |ui| {
                let forget = Button::standard(&w!(lang, SetForgetPairing)).show(ui).clicked();
                let again = Button::standard(&w!(lang, SetPairAgain)).show(ui).clicked();
                (again, forget)
            });
            if r.inner.0 {
                cx.act(Act::PairAgain(id.to_string()));
            }
            if r.inner.1 {
                cx.act(Act::ForgetPairing(id.to_string()));
            }
        }
        Some(Pairing::PasswordSaved) => {
            let r = SettingRow::new(&w!(lang, SetPasswordTitle)).icon(Icon::Lock).description(&w!(lang, SetPasswordSaved)).show(ui, |ui| {
                let remove = Button::standard(&w!(lang, SetRemove)).accessible_name(w!(lang, SetRemovePassword)).show(ui).clicked();
                let change = Button::standard(&w!(lang, SetChange)).accessible_name(w!(lang, SetChangePassword)).show(ui).clicked();
                (change, remove)
            });
            if r.inner.0 {
                cx.dialog(Dialog::Password(PasswordDialog::change(id, &name)));
            }
            if r.inner.1 {
                cx.act(Act::RemovePassword(id.to_string()));
            }
        }
        None => {}
    }
    if let Some(cb) = v.comes_back {
        let options = [w!(lang, SetComesBackLast), w!(lang, SetComesBackAppears), w!(lang, SetComesBackNever)];
        let refs: Vec<&str> = options.iter().map(String::as_str).collect();
        let chosen = match cb {
            ComesBack::WhenLastChosen => 0,
            ComesBack::WhenItAppears => 1,
            ComesBack::Never => 2,
        };
        let line = match cb {
            ComesBack::WhenLastChosen => w!(lang, SetComesBackLastLine, speaker = &name),
            ComesBack::WhenItAppears => w!(lang, SetComesBackAppearsLine, speaker = &name),
            ComesBack::Never => w!(lang, SetComesBackNeverLine, speaker = &name),
        };
        let title = w!(lang, SetComesBack);
        let r = SettingRow::new(&title)
            .icon(Icon::Refresh)
            .description(&line)
            .show(ui, |ui| DropDown::new(("comes-back", id), &title, &refs, chosen).min_width(220.0).show(ui));
        if let Some(i) = r.inner {
            let to = [ComesBack::WhenLastChosen, ComesBack::WhenItAppears, ComesBack::Never][i];
            cx.act(Act::ComesBack(id.to_string(), to));
        }
    }
    if let Some(on) = v.song_titles {
        let title = w!(lang, SetSongTitles, speaker = &name);
        let r = SettingRow::new(&title).icon(Icon::Music).description(&w!(lang, SetSongTitlesLine, speaker = &name)).show(ui, |ui| Switch::new(on, &title).show(ui));
        if let Some(on) = r.inner {
            cx.act(Act::SongTitles(id.to_string(), on));
        }
    }
    if let Some(shown) = &v.show_as {
        let title = w!(lang, SetShowAs);
        let line = match v.row.kind {
            SpeakerKind::Sonos | SpeakerKind::SonosGroup => w!(lang, SetShowAsSonos),
            SpeakerKind::HomePod | SpeakerKind::HomePodPair | SpeakerKind::AppleTv => w!(lang, SetShowAsHome),
            _ => w!(lang, SetShowAsLine),
        };
        let r = SettingRow::new(&title).icon(Icon::User).description(&line).show(ui, |ui| kept_field(ui, st, &format!("show-as:{id}"), shown, &title, 220.0));
        if let Some(n) = r.inner {
            cx.act(Act::ShowAs(id.to_string(), n));
        }
    }

    ui.add_space(4.0);
    details(ui, cx, st, &v);

    ui.add_space(8.0);
    buttons(ui, |ui| {
        if v.row.added_by_address {
            if Button::standard(&w!(lang, SetRemove)).accessible_name(w!(lang, SetRemoveNamed, speaker = &name)).show(ui).clicked() {
                cx.act(Act::Remove(id.to_string()));
            }
        } else if Button::standard(&w!(lang, SetForgetNamed, speaker = &name)).show(ui).clicked() {
            cx.act(Act::Forget(id.to_string()));
        }
        if Button::standard(&w!(lang, SetHideFromList)).show(ui).clicked() {
            cx.act(Act::HideSpeaker(id.to_string()));
        }
    });
    note(ui, &if v.row.added_by_address { w!(lang, SetRemoveLine, speaker = &name) } else { w!(lang, SetForgetLine, speaker = &name) });
}

/// The header card: the kind's icon in a tile, the name, the model and
/// where it is; *Play on Kitchen* and *Play a test sound*.
fn header(ui: &mut Ui, cx: &mut Cx, v: &SpeakerView) {
    let lang = cx.lang;
    let t = Theme::of(ui.ctx());
    let name = &v.row.name;
    let presence = match &v.row.presence {
        Presence::PlayingNow => w!(lang, SetPlayingNowCap),
        Presence::Ready => w!(lang, SetReadyCap),
        Presence::Connecting => w!(lang, SetConnectingCap),
        Presence::Available => w!(lang, SetAvailableCap),
        Presence::NotFound { .. } => w!(lang, SetNotFoundCap),
    };
    let what = v.model.clone().unwrap_or_else(|| words::kind(lang, &v.row.kind));
    let line = w!(lang, SetKindStatus, kind = &what, status = &presence);
    let _ = card(ui, SETTINGS_CARD_PAD, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(14.0, 8.0);
            let (tile, _) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::hover());
            ui.painter().rect_filled(tile, radius::CARD, t.c.hover);
            kind_icon(&v.row.kind).paint(ui.painter(), Rect::from_center_size(tile.center(), Vec2::splat(28.0)), t.c.text);
            let text_w = (ui.available_width() - 330.0).max(180.0);
            ui.allocate_ui(Vec2::new(text_w, 56.0), |ui| {
                ui.vertical(|ui| {
                    ui.add_space(8.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add(egui::Label::new(egui::RichText::new(name).font(t.strong(size::TITLE + 2.0)).color(t.c.text)).wrap());
                    ui.add(egui::Label::new(egui::RichText::new(&line).font(t.font(size::CAPTION)).color(t.c.text_secondary)).wrap());
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.set_min_height(56.0);
                if let Some(()) = v.test_sound {
                    if Button::standard(&w!(lang, SetTestSound)).icon(Icon::Music).show(ui).clicked() {
                        cx.act(Act::TestSound(v.row.id.clone()));
                    }
                }
                if v.row.cant_play.is_none() {
                    match v.row.presence {
                        Presence::PlayingNow | Presence::Ready => {
                            let _ = Button::standard(&w!(lang, SetPlayingButton)).enabled(false).show(ui);
                        }
                        Presence::Connecting => {
                            let _ = Button::standard(&w!(lang, SetConnectingButton)).enabled(false).show(ui);
                        }
                        _ => {
                            if Button::primary(&w!(lang, SetPlayOn, speaker = name)).show(ui).clicked() {
                                cx.act(Act::PlayOn(v.row.id.clone()));
                            }
                        }
                    }
                }
            });
        });
    });
}

/// The delay card: what the speaker's own delay is, its choice where it
/// has one, the delay control, the symptom line, and the details behind a
/// fold.
fn delay_card(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, v: &SpeakerView, d: &DelayCard) {
    let lang = cx.lang;
    let id = v.row.id.as_str();
    let name = v.row.name.as_str();
    let s = &d.status;
    let mut action = None;
    boxed(ui, |ui| {
        // What the delay is and who sets it.
        match v.row.kind {
            SpeakerKind::HomePodPair => {
                body(ui, &w!(lang, SetPairAlwaysShort, delay = &words::about(lang, s.own_delay_ms)));
                note(ui, &w!(lang, SetKeepsAsked, speaker = name));
            }
            SpeakerKind::AirPlay | SpeakerKind::HomePod | SpeakerKind::AppleTv if s.choice.is_some() => {
                body(ui, &w!(lang, SetKeepsAsked, speaker = name));
            }
            SpeakerKind::Sendspin => {
                body(ui, &w!(lang, SetOwnDelay, speaker = name, delay = &words::about(lang, s.own_delay_ms)));
                note(ui, &w!(lang, SetSendspinOwn));
            }
            _ => {
                body(ui, &w!(lang, SetOwnDelay, speaker = name, delay = &words::about(lang, s.own_delay_ms)));
                note(ui, &w!(lang, SetSonosOwn));
            }
        }
        if let Some((ms, on)) = d.measured {
            note(ui, &w!(lang, SetMeasuredOn, speaker = name, seconds = &measured_seconds(lang, ms), date = &words::date(lang, on)));
        }
        // Short delay or Steady.
        if let Some(choice) = s.choice {
            ui.add_space(6.0);
            let short = w!(lang, SetShortDelayAbout, delay = &words::about(lang, s.short_ms));
            let steady = w!(lang, SetSteadyAbout, delay = &words::about(lang, s.steady_ms));
            if Choice::new(choice == DelayChoice::Short, &short).description(&w!(lang, SetShortDelayLine)).show(ui) {
                action = Some(DelayAct::Choose(DelayChoice::Short));
            }
            if Choice::new(choice == DelayChoice::Steady, &steady).description(&w!(lang, SetSteadyLine)).show(ui) {
                action = Some(DelayAct::Choose(DelayChoice::Steady));
            }
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                ui.vertical(|ui| note(ui, &w!(lang, SetChoiceReconnects, speaker = name)));
            });
        }
        ui.add_space(8.0);
        // The control.
        let offer_key = format!("steady-offer:{id}");
        let view = delay_view(lang, name, d, st.is_open(&offer_key));
        let state = st.delay.entry(id.to_string()).or_default();
        if let Some(a) = DelayControl::new(&view, state).max_width(470.0).show(ui) {
            action = Some(match a {
                DelayAction::Remove(n) => DelayAct::Remove(n),
                DelayAction::Add(n) => DelayAct::Add(n),
                DelayAction::BackToOwn => DelayAct::BackToOwn,
                DelayAction::UseShortDelay => DelayAct::Choose(DelayChoice::Short),
                DelayAction::UseSteady => DelayAct::Choose(DelayChoice::Steady),
            });
        }
        // The symptoms and their cure, where delay can be changed.
        if s.applies.is_ok() && s.limits.is_some_and(|l| l.remove_ms > 0 || l.add_ms > 0) {
            ui.add_space(8.0);
            note(ui, &w!(lang, SetSymptoms));
        }
        // How a change applies and the range, behind a fold.
        let fold_key = format!("delay-details:{id}");
        let open = st.is_open(&fold_key);
        let label = w!(lang, SetDetails);
        let (rect, resp) = {
            let g = ui.painter().layout_no_wrap(label.clone(), Theme::of(ui.ctx()).font(size::SECONDARY), egui::Color32::PLACEHOLDER);
            ui.allocate_exact_size(Vec2::new(g.size().x + 24.0, 22.0), Sense::click())
        };
        fold_link(ui, rect, &resp, &label, open);
        if resp.clicked() {
            st.toggle(&fold_key);
        }
        if open {
            for line in delay_details(lang, v, d, cx.port.general().shortcuts.as_ref()) {
                note(ui, &line);
            }
        }
        // The offer of Short delay follows a Remove press on Steady, until
        // the next change of another kind.
        let steady = s.choice == Some(DelayChoice::Steady) && s.applies.is_ok();
        match action {
            Some(DelayAct::Remove(_)) if steady => {
                st.open.insert(offer_key.clone());
            }
            Some(_) => {
                st.open.remove(&offer_key);
            }
            None => {}
        }
    });
    if let Some(a) = action {
        cx.act(Act::Delay(id.to_string(), a));
    }
}

/// The fold's link: *Details* and a chevron.
fn fold_link(ui: &mut Ui, rect: Rect, resp: &egui::Response, label: &str, open: bool) {
    let t = Theme::of(ui.ctx());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::CollapsingHeader, true, open, label));
    let l = crate::ui::look::look_as(ui, resp, label);
    let color = if l.hovered { t.c.accent_strong } else { t.c.accent };
    let g = ui.painter().layout_no_wrap(label.to_owned(), t.font(size::SECONDARY), color);
    let gy = rect.center().y - g.size().y / 2.0;
    let gw = g.size().x;
    ui.painter().galley(egui::pos2(rect.left(), gy), g, color);
    let chev = if open { Icon::ChevronUp } else { Icon::ChevronDown };
    chev.paint(ui.painter(), Rect::from_center_size(egui::pos2(rect.left() + gw + 12.0, rect.center().y), Vec2::splat(14.0)), color);
    if l.focused {
        crate::ui::theme::focus_ring(ui.painter(), rect, radius::CONTROL, true, &t);
    }
}

/// The lines behind the delay card's fold: how a change applies, the
/// range, the shortcuts.
fn delay_details(lang: Lang, v: &SpeakerView, d: &DelayCard, shortcuts: Option<&Shortcuts>) -> Vec<String> {
    let name = v.row.name.as_str();
    let s = &d.status;
    let mut out = Vec::new();
    match (v.row.kind.clone(), s.applies) {
        (SpeakerKind::HomePodPair, _) => out.push(w!(lang, SetAppliesPair, speaker = name)),
        (_, Ok(Applies::Live)) if d.at_once => out.push(w!(lang, SetAppliesAtOnce, speaker = name)),
        (_, Ok(Applies::Live)) => out.push(w!(lang, SetAppliesLive, speaker = name)),
        (_, Ok(Applies::Reconnect)) => out.push(w!(lang, SetAppliesReconnect, speaker = name)),
        (_, Err(_)) => out.push(w!(lang, SetRemembered, speaker = name)),
    }
    if let Some(l) = s.limits {
        let line = match (s.choice, l.remove_ms) {
            (_, 0) => w!(lang, SetRangeAddOnly, speaker = name, add = l.add_ms),
            (Some(DelayChoice::Steady), _) => w!(lang, SetRangeSteady, speaker = name, remove = l.remove_ms, add = l.add_ms),
            (Some(DelayChoice::Short), _) => w!(lang, SetRangeShort, speaker = name, remove = l.remove_ms, add = l.add_ms),
            (None, _) => w!(lang, SetRange, speaker = name, remove = l.remove_ms, add = l.add_ms),
        };
        out.push(line);
    }
    if let Some(k) = shortcuts {
        let remove = k.remove.join("+");
        let add = k.add.join("+");
        out.push(if k.on { w!(lang, SetShortcutsOnLine, remove = &remove, add = &add) } else { w!(lang, SetShortcutsOffLine, remove = &remove, add = &add) });
    }
    out
}

/// The delay control's values, from the delay engine's status.
pub(super) fn delay_view<'a>(lang: Lang, name: &'a str, d: &DelayCard, steady_offer: bool) -> DelayView<'a> {
    let s = &d.status;
    let limits = s.limits.map_or(UiLimits { max_removed_ms: 0, max_added_ms: 0 }, |l| UiLimits { max_removed_ms: l.remove_ms, max_added_ms: l.add_ms });
    let steady = s.choice == Some(DelayChoice::Steady);
    let note = match (s.notice, s.progress) {
        (Some(DelayNotice::NotAccepted { tried_ms, back_to_ms }), _) => Some(DelayNote::NotAccepted { asked_ms: tried_ms, now_ms: back_to_ms }),
        (Some(DelayNotice::DroppedOut { from_ms, to_ms }), _) => Some(DelayNote::DroppedOut { had_ms: from_ms, now_ms: to_ms }),
        _ if steady_offer && steady && s.applies.is_ok() => Some(DelayNote::SteadyOffer { steady_ms: s.delay_ms, short_ms: s.short_ms }),
        _ if s.limits.is_some() && s.limits.unwrap().remove_ms == 0 && s.change_ms == 0 => Some(DelayNote::AsLittleAsItCan),
        _ if s.limits.is_some_and(|l| s.change_ms <= -(l.remove_ms as i32)) => {
            Some(if steady { DelayNote::SteadyMostRemoved } else { DelayNote::MostRemoved })
        }
        _ if s.limits.is_some_and(|l| s.change_ms >= l.add_ms as i32) => Some(DelayNote::MostAdded),
        (_, Progress::CatchingUp { ms_to_go }) if ms_to_go < 0 => Some(DelayNote::Removing { to_go_ms: ms_to_go.unsigned_abs() }),
        (_, Progress::CatchingUp { ms_to_go }) => Some(DelayNote::Adding { to_go_ms: ms_to_go as u32 }),
        (_, Progress::WillReconnect) => Some(DelayNote::WillReconnect),
        (_, Progress::Reconnecting { .. }) => Some(DelayNote::Reconnecting),
        (Some(DelayNotice::ChoiceSwitched { to, now_ms, other_ms }), _) => {
            Some(DelayNote::ChoiceSwitched { to_short: to == DelayChoice::Short, now_ms, other_ms })
        }
        _ => None,
    };
    let reconnecting = match s.progress {
        Progress::Reconnecting { from_ms } => Some(if s.change_ms == 0 {
            Reconnect::BackToOwn
        } else if s.change_ms < from_ms {
            Reconnect::Remove((from_ms - s.change_ms) as u32)
        } else {
            Reconnect::Add((s.change_ms - from_ms) as u32)
        }),
        _ => None,
    };
    let _ = lang;
    DelayView {
        speaker: name,
        delay: match d.measured {
            Some((ms, _)) => DelayFigure::measured((ms as i64 + s.change_ms as i64).max(0) as u32),
            None => DelayFigure::about(s.delay_ms),
        },
        change_ms: s.change_ms,
        limits,
        small_step_ms: d.small_step_ms,
        large_step_ms: d.large_step_ms,
        note,
        fixed_with_short_ms: (s.applies == Err(Unchangeable::KeptBySpeakerOnSteady)).then_some(s.short_ms),
        other_amount_link: true,
        reconnecting,
    }
}

fn group_card(ui: &mut Ui, lang: Lang, name: &str, g: &Group) {
    let (icon, title, line, members) = match g {
        Group::Sonos { members } => {
            let list = and_list(lang, members);
            (Icon::Group, w!(lang, KindSonosGroup), w!(lang, SetSonosGroupLine, rooms = &list), members.clone())
        }
        Group::HomePodPair { left, right } => {
            (Icon::Pair, w!(lang, KindHomePodPair), w!(lang, SetPairLine, left = left, right = right), vec![left.clone(), right.clone()])
        }
        Group::AppleTv { members } => {
            let list = and_list(lang, members);
            (Icon::Tv, w!(lang, SetAppleTvGroup), w!(lang, SetAppleTvGroupLine, speaker = name, members = &list), members.clone())
        }
    };
    let _ = card(ui, SETTINGS_CARD_PAD, |ui| {
        row(ui, Some(icon), &title, Some(&line), |_| ());
        if members.len() > 2 {
            ui.horizontal(|ui| {
                ui.add_space(46.0);
                ui.vertical(|ui| {
                    for m in &members {
                        note(ui, m);
                    }
                });
            });
        }
    });
}

/// *Living Room and Dining Room*; *A, B and C*.
fn and_list(lang: Lang, items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => w!(lang, SetListAnd, list = &rest.join(", "), last = last),
    }
}

/// A labelled slider with its value at the right: *Never louder than*
/// ━━━━●━━ *70*.
fn slider_row(ui: &mut Ui, label: &str, value: &str, at: f32, range: std::ops::RangeInclusive<f32>, step: f32) -> Option<f32> {
    let t = Theme::of(ui.ctx());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let w = ui.available_width();
        let label_w = (w * 0.25).clamp(110.0, 160.0);
        let value_w = 70.0;
        ui.allocate_ui(Vec2::new(label_w, 28.0), |ui| {
            ui.set_min_width(label_w);
            ui.add(egui::Label::new(egui::RichText::new(label).font(t.font(size::BODY)).color(t.c.text)).truncate());
        });
        let mut out = None;
        ui.allocate_ui(Vec2::new((w - label_w - value_w - 24.0).max(80.0), 28.0), |ui| {
            out = Slider::new(at, range, label).step(step).show(ui);
        });
        ui.label(egui::RichText::new(value).font(t.font(size::BODY)).color(t.c.text));
        out
    })
    .inner
}

/// *−2 dB*, *+1 dB*, *0 dB*.
pub(super) fn db(lang: Lang, v: f32) -> String {
    let n = (v * 10.0).round() as i64;
    let num = crate::model::text::Number { value: n.abs(), scale: 1 }.normalized();
    let digits = lang.number(num);
    if n > 0 {
        w!(lang, SetDbPlus, n = &digits)
    } else if n < 0 {
        w!(lang, SetDbMinus, n = &digits)
    } else {
        w!(lang, SetDbZero)
    }
}

/// *0.21 s*.
fn measured_seconds(lang: Lang, ms: u32) -> String {
    let n = crate::model::text::Number { value: ((ms + 5) / 10) as i64, scale: 2 };
    lang.number(n)
}

/// The *Details* card: what support needs, with *Copy details*.
fn details(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, v: &SpeakerView) {
    let lang = cx.lang;
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
    let key = format!("details:{}", v.row.id);
    fold(ui, st, &key, Icon::List, &w!(lang, SetDetails), Some(&w!(lang, SetDetailsLine)), |ui, st| {
        mono(ui, &text);
        ui.add_space(4.0);
        super::widgets::copy_button(ui, st, &key, &w!(lang, SetCopyDetails), &w!(lang, SetCopied), &text);
    });
    let _ = divider;
}
