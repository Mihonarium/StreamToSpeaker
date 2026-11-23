//! Settings → General: what plays on the speaker, start-up, what happens
//! when a speaker stops responding, notifications, the keyboard shortcuts
//! for delay, and the language.

use egui::Ui;

use crate::ui::button::Button;
use crate::ui::card::SettingRow;
use crate::ui::icon::Icon;
use crate::ui::inputs::{Choice, DropDown, Switch};
use crate::ui::text::{Mark, Tone};
use crate::ui::theme::Theme;

use super::port::{Act, Shortcuts, WhatPlays, WhenStops};
use super::widgets::{boxed, divider, heading, keycap, keycaps, marked, page_title, row};
use super::words::w;
use super::{Cx, PageState};

pub(super) fn page(ui: &mut Ui, cx: &mut Cx, st: &mut PageState) {
    let lang = cx.lang;
    let v = cx.port.general();
    page_title(ui, &w!(lang, SetNavGeneral));

    if let Some(mode) = v.what_plays {
        boxed(ui, |ui| {
            super::widgets::body(ui, &w!(lang, SetWhatPlays));
            if Choice::new(mode == WhatPlays::Everything, &w!(lang, SetEverything)).description(&w!(lang, SetEverythingLine)).show(ui) {
                cx.act(Act::WhatPlays(WhatPlays::Everything));
            }
            if let Some(calls) = v.play_calls {
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    ui.vertical(|ui| {
                        let title = w!(lang, SetPlayCalls);
                        let on = row(ui, None, &title, Some(&w!(lang, SetPlayCallsLine)), |ui| {
                            Switch::new(calls, &title).enabled(mode == WhatPlays::Everything).show(ui)
                        });
                        if let Some(on) = on {
                            cx.act(Act::PlayCalls(on));
                        }
                    });
                });
            }
            if Choice::new(mode == WhatPlays::OnlySentApps, &w!(lang, SetOnlySent)).description(&w!(lang, SetOnlySentLine)).show(ui) {
                cx.act(Act::WhatPlays(WhatPlays::OnlySentApps));
            }
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                if Button::link(&w!(lang, SetOpenVolumeMixer)).show(ui).clicked() {
                    cx.act(Act::OpenVolumeMixer);
                }
            });
        });
    }

    if v.start_at_sign_in.is_some() || v.go_back.is_some() {
        heading(ui, &w!(lang, SetStartUp));
    }
    if let Some(start) = &v.start_at_sign_in {
        let title = w!(lang, SetStartAtSignIn);
        let on = *start.as_ref().unwrap_or(&false);
        let r = SettingRow::new(&title).icon(Icon::Play).description(&w!(lang, SetStartAtSignInLine)).show(ui, |ui| Switch::new(on, &title).show(ui));
        if let Some(on) = r.inner {
            cx.act(Act::StartAtSignIn(on));
        }
        if let Err(why) = start {
            marked(ui, Mark::Error, Tone::Red, &w!(lang, SetSignInFailed, why = why));
        }
    }
    if let Some(back) = v.go_back {
        let title = w!(lang, SetGoBack);
        let r = SettingRow::new(&title).icon(Icon::Refresh).description(&w!(lang, SetGoBackLine)).show(ui, |ui| Switch::new(back, &title).show(ui));
        if let Some(on) = r.inner {
            cx.act(Act::GoBack(on));
        }
    }

    if v.when_stops.is_some() || v.notifications.is_some() {
        heading(ui, &w!(lang, SetWhenWrong));
    }
    if let Some(when) = v.when_stops {
        boxed(ui, |ui| {
            super::widgets::body(ui, &w!(lang, SetWhenStops));
            if Choice::new(when == WhenStops::BringBack, &w!(lang, SetBringBack)).description(&w!(lang, SetBringBackLine)).show(ui) {
                cx.act(Act::WhenStops(WhenStops::BringBack));
            }
            if Choice::new(when == WhenStops::StaySilent, &w!(lang, SetStaySilent)).description(&w!(lang, SetStaySilentLine)).show(ui) {
                cx.act(Act::WhenStops(WhenStops::StaySilent));
            }
        });
    }
    if let Some(on) = v.notifications {
        let title = w!(lang, SetNotify);
        let r = SettingRow::new(&title).icon(Icon::Bell).description(&w!(lang, SetNotifyLine)).show(ui, |ui| Switch::new(on, &title).show(ui));
        if let Some(on) = r.inner {
            cx.act(Act::Notifications(on));
        }
    }

    if let Some(k) = &v.shortcuts {
        heading(ui, &w!(lang, SetShortcutsHeading));
        shortcuts(ui, cx, st, k);
    }

    if let Some(chosen) = &v.language {
        heading(ui, &w!(lang, SetLanguage));
        let title = w!(lang, SetLanguage);
        let options = [w!(lang, SetLanguageWindows, language = &w!(lang, SetEnglish)), w!(lang, SetEnglish)];
        let refs: Vec<&str> = options.iter().map(String::as_str).collect();
        let i = if chosen.is_some() { 1 } else { 0 };
        let r = SettingRow::new(&title).icon(Icon::List).show(ui, |ui| DropDown::new("settings-language", &title, &refs, i).min_width(220.0).show(ui));
        if let Some(i) = r.inner {
            cx.act(Act::Language(if i == 0 { None } else { Some("en".into()) }));
        }
    }
}

/// The delay shortcuts: the switch, then each combination with whether
/// it is free, and *Change…*.
fn shortcuts(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, k: &Shortcuts) {
    let lang = cx.lang;
    let t = Theme::of(ui.ctx());
    boxed(ui, |ui| {
        let title = w!(lang, SetShortcutsSwitch);
        if let Some(on) = row(ui, Some(Icon::Keyboard), &title, Some(&w!(lang, SetShortcutsLine)), |ui| Switch::new(k.on, &title).show(ui)) {
            cx.act(Act::Shortcuts(on));
        }
        if !k.on {
            return;
        }
        divider(ui);
        let lines = [(w!(lang, SetShortcutRemove, ms = 25u32), &k.remove), (w!(lang, SetShortcutAdd, ms = 25u32), &k.add)];
        for (i, (label, keys)) in lines.iter().enumerate() {
            shortcut_line(ui, cx, st, i, label, |ui| keycaps(ui, keys), k.taken[i]);
        }
        let larger = [k.larger.clone()];
        shortcut_line(
            ui,
            cx,
            st,
            2,
            "",
            |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(egui::RichText::new(w!(lang, SetHold)).font(t.font(crate::ui::theme::size::BODY)).color(t.c.text));
                    keycap(ui, &larger[0]);
                    ui.label(egui::RichText::new(w!(lang, SetForLarge, ms = 100u32)).font(t.font(crate::ui::theme::size::BODY)).color(t.c.text));
                });
            },
            k.taken[2],
        );
        ui.horizontal(|ui| {
            ui.add_space(46.0);
            if Button::standard(&w!(lang, SetChangeKeys)).show(ui).clicked() {
                st.recording = Some(0);
            }
        });
    });
}

fn shortcut_line(ui: &mut Ui, cx: &mut Cx, st: &mut PageState, i: usize, label: &str, keys: impl FnOnce(&mut Ui), taken: bool) {
    let lang = cx.lang;
    let t = Theme::of(ui.ctx());
    ui.horizontal(|ui| {
        ui.add_space(46.0);
        let w = ui.available_width();
        if !label.is_empty() {
            ui.allocate_ui(egui::vec2(160.0_f32.min(w * 0.35), 24.0), |ui| {
                ui.set_min_width(160.0_f32.min(w * 0.35));
                ui.label(egui::RichText::new(label).font(t.font(crate::ui::theme::size::BODY)).color(t.c.text));
            });
        }
        if st.recording == Some(i) {
            ui.label(egui::RichText::new(w!(lang, SetPressKeys)).font(t.font(crate::ui::theme::size::BODY)).color(t.c.accent));
            // The next key pressed with Ctrl or Alt is the new combination;
            // Esc stops recording.
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
                    cx.act(Act::ShortcutKeys(i, keys));
                    st.recording = if i == 0 { Some(1) } else { None };
                }
            }
        } else {
            keys(ui);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if taken {
                super::widgets::side_text(ui, &w!(lang, SetShortcutTaken), t.c.amber, 200.0);
            } else {
                super::widgets::side_text(ui, &w!(lang, SetShortcutReady), t.c.green, 200.0);
            }
        });
    });
}
