//! Settings → Lab (shown only when turned on in About): settings for
//! testing speakers that aren't fully supported yet, values carried over
//! from an earlier version, and live counters.

use egui::Ui;

use crate::ui::button::Button;
use crate::ui::icon::Icon;
use crate::ui::inputs::{DropDown, Switch};
use crate::ui::notice::BarKind;

use super::port::{Act, LabAct, LabView, LAB_CATCH_UP_DEFAULT, LAB_SILENCE_PACE_DEFAULT};
use super::widgets::{body, boxed, divider, fold, mono, note, page_title, row, strong};
use super::words::w;
use super::{Cx, PageState};
use crate::ui::theme::size;

pub(super) fn page(ui: &mut Ui, cx: &mut Cx, st: &mut PageState) {
    let lang = cx.lang;
    let v = cx.port.lab();
    page_title(ui, &w!(lang, SetNavLab));
    let title = w!(lang, SetLabTitle);
    let text = w!(lang, SetLabLine);
    let reset = w!(lang, SetLabReset);
    if super::widgets::bar(ui, BarKind::Warning, &title, &text, v.differs().then_some(reset.as_str())) {
        cx.act(Act::Lab(LabAct::ResetAll));
    }

    if let Some(c) = &v.carried_over {
        boxed(ui, |ui| {
            strong(ui, &w!(lang, SetCarriedTitle), size::BODY);
            if let Some(ms) = c.airplay_ms {
                body(ui, &w!(lang, SetCarriedAirPlay, ms = ms));
            }
            if let Some(ms) = c.homepod_ms {
                body(ui, &w!(lang, SetCarriedHomePod, ms = ms));
            }
            if c.homepod_buffered {
                body(ui, &w!(lang, SetCarriedBuffered));
            }
            let line = if c.speakers.is_empty() {
                w!(lang, SetCarriedNoneKnown)
            } else {
                w!(lang, SetCarriedSpeakers, speakers = &c.speakers.join(", "))
            };
            note(ui, &line);
            if Button::standard(&w!(lang, SetCarriedReset)).show(ui).clicked() {
                cx.act(Act::Lab(LabAct::ResetCarriedOver));
            }
        });
    }

    let delay = v.at_once.is_some() || v.catch_up_frames.is_some() || v.drift_ppm.is_some() || v.silence_pace_ms.is_some();
    if delay {
        boxed(ui, |ui| {
            strong(ui, &w!(lang, SetDelayHeading), size::BODY + 1.0);
            delay_rows(ui, cx, &v);
        });
    }
    if v.realtime.is_some() || v.mfi.is_some() || v.uncompressed_alac.is_some() {
        boxed(ui, |ui| {
            strong(ui, &w!(lang, SetLabConnections), size::BODY + 1.0);
            connection_rows(ui, cx, &v);
        });
    }
    if let Some(counters) = &v.counters {
        let text = counters.iter().map(|(k, val)| format!("{k}: {val}")).collect::<Vec<_>>().join("\n");
        fold(ui, st, "lab-counters", Icon::List, &w!(lang, SetLabCounters), Some(&w!(lang, SetLabCountersLine)), |ui, _| mono(ui, &text));
    }
}

fn delay_rows(ui: &mut Ui, cx: &mut Cx, v: &LabView) {
    let lang = cx.lang;
    let mut first = true;
    let mut sep = |ui: &mut Ui| {
        if !first {
            divider(ui);
        }
        first = false;
    };
    if let Some(at_once) = v.at_once {
        sep(ui);
        let title = w!(lang, SetLabApply);
        let options = [w!(lang, SetLabGradually), w!(lang, SetLabAtOnce)];
        let refs: Vec<&str> = options.iter().map(String::as_str).collect();
        if let Some(i) = row(ui, None, &title, Some(&w!(lang, SetLabApplyLine)), |ui| DropDown::new("lab-apply", &title, &refs, at_once as usize).min_width(150.0).show(ui)) {
            cx.act(Act::Lab(LabAct::AtOnce(i == 1)));
        }
    }
    if let Some(n) = v.catch_up_frames {
        sep(ui);
        let title = w!(lang, SetLabCatchUp);
        let values = with(&[1, 2, 4, 8, 16, 32], n as i64, LAB_CATCH_UP_DEFAULT as i64);
        let labels: Vec<String> = values.iter().map(|f| w!(lang, SetLabFrames, count = *f)).collect();
        if let Some(f) = pick(ui, cx, "lab-catch-up", &title, &w!(lang, SetLabCatchUpLine), &values, &labels, n as i64) {
            cx.act(Act::Lab(LabAct::CatchUpFrames(f as u32)));
        }
    }
    if let Some(ppm) = v.drift_ppm {
        sep(ui);
        let title = w!(lang, SetLabDrift);
        let values = with(&[-200, -100, -50, -20, 0, 20, 50, 100, 200], ppm as i64, 0);
        let labels: Vec<String> = values.iter().map(|p| w!(lang, SetLabPpm, ppm = &signed(*p))).collect();
        if let Some(p) = pick(ui, cx, "lab-drift", &title, &w!(lang, SetLabDriftLine), &values, &labels, ppm as i64) {
            cx.act(Act::Lab(LabAct::DriftPpm(p as i32)));
        }
    }
    if let Some(ms) = v.silence_pace_ms {
        sep(ui);
        let title = w!(lang, SetLabPacing);
        let values = with(&[5, 8, 10, 12, 15, 20], ms as i64, LAB_SILENCE_PACE_DEFAULT as i64);
        let labels: Vec<String> = values.iter().map(|m| w!(lang, SetLabMs, ms = *m)).collect();
        if let Some(m) = pick(ui, cx, "lab-pacing", &title, &w!(lang, SetLabPacingLine), &values, &labels, ms as i64) {
            cx.act(Act::Lab(LabAct::SilencePace(m as u32)));
        }
    }
    if v.catch_up_frames.is_some() || v.drift_ppm.is_some() || v.silence_pace_ms.is_some() {
        ui.add_space(2.0);
        note(ui, &w!(lang, SetLabThisRun));
    }
}

fn connection_rows(ui: &mut Ui, cx: &mut Cx, v: &LabView) {
    let lang = cx.lang;
    let mut first = true;
    let mut sep = |ui: &mut Ui| {
        if !first {
            divider(ui);
        }
        first = false;
    };
    if let Some(rt) = v.realtime {
        sep(ui);
        let title = w!(lang, SetLabAp2);
        let options = [w!(lang, SetLabAutomatic), w!(lang, SetLabRealtime)];
        let refs: Vec<&str> = options.iter().map(String::as_str).collect();
        if let Some(i) = row(ui, None, &title, Some(&w!(lang, SetLabAp2Line)), |ui| DropDown::new("lab-ap2", &title, &refs, rt as usize).min_width(150.0).show(ui)) {
            cx.act(Act::Lab(LabAct::Realtime(i == 1)));
        }
    }
    if let Some(on) = v.mfi {
        sep(ui);
        let title = w!(lang, SetLabMfi);
        if let Some(on) = row(ui, None, &title, Some(&w!(lang, SetLabMfiLine)), |ui| Switch::new(on, &title).show(ui)) {
            cx.act(Act::Lab(LabAct::Mfi(on)));
        }
    }
    if let Some(on) = v.uncompressed_alac {
        sep(ui);
        let title = w!(lang, SetLabAlac);
        if let Some(on) = row(ui, None, &title, Some(&w!(lang, SetLabAlacLine)), |ui| Switch::new(on, &title).show(ui)) {
            cx.act(Act::Lab(LabAct::UncompressedAlac(on)));
        }
    }
}

/// A row with a drop-down of numbers; the number picked.
fn pick(ui: &mut Ui, _cx: &mut Cx, id: &str, title: &str, line: &str, values: &[i64], labels: &[String], current: i64) -> Option<i64> {
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let chosen = values.iter().position(|v| *v == current).unwrap_or(0);
    row(ui, None, title, Some(line), |ui| DropDown::new(id, title, &refs, chosen).min_width(150.0).show(ui)).map(|i| values[i])
}

/// `presets` with `current` and `default` in it, in order.
fn with(presets: &[i64], current: i64, default: i64) -> Vec<i64> {
    let mut v: Vec<i64> = presets.to_vec();
    for x in [current, default] {
        if !v.contains(&x) {
            v.push(x);
        }
    }
    v.sort_unstable();
    v
}

fn signed(n: i64) -> String {
    if n > 0 {
        format!("+{n}")
    } else if n < 0 {
        format!("−{}", n.unsigned_abs())
    } else {
        "0".into()
    }
}
