//! About, update checks, and the optional Lab page.

use std::sync::Arc;

use egui::Ui;

use crate::model::Snapshot;
use crate::shell::{Ask, ViewBar, ViewContent};
use crate::ui::button::Button;
use crate::ui::text::{Mark, Tone};
use crate::ui::theme::size;

use super::parts::{page_button, page_link, gap, marked, plain, sentence, switch_line, thin};
use crate::settings::port::*;
use crate::settings::words::{self, w};
use crate::settings::{PageState, SettingsPort};

pub struct AboutView {
    port: Arc<dyn SettingsPort>,
}

impl AboutView {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        AboutView { port }
    }
}

impl ViewContent for AboutView {
    fn bar(&self, _snap: &Snapshot) -> ViewBar {
        ViewBar { title: w!(self.port.lang(), SetNavAbout), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 8.0;
        let v = self.port.about();
        crate::settings::widgets::strong(ui, &w!(lang, SetAppVersion, version = &v.version), size::TITLE);
        updates(ui, lang, &v.updates, self.port.as_ref());
        if let Some(daily) = v.updates.daily {
            // The switch sits level with its words, as on the Settings page.
            if let Some(on) = switch_line(ui, &w!(lang, SetDailyCheck), daily) {
                self.port.act(Act::DailyUpdateCheck(on));
            }
            crate::settings::widgets::note(ui, &w!(lang, SetDailyCheckLine));
        }
        gap(ui);
        if page_link(&w!(lang, SetLicences)).show(ui).clicked() { self.port.act(Act::LicenceAndSource); }
        if page_button(&w!(lang, SetShowWelcome)).show(ui).clicked() {
            ask(Ask::Command(crate::model::Command::FirstRunAction(crate::model::command::FirstRunAction::ShowAgain)));
        }
        if let Some(on) = super::parts::switch_line(ui, &w!(lang, SetShowLab), v.show_lab) { self.port.act(Act::ShowLab(on)); }
        if v.show_lab && page_link(&w!(lang, SetNavLab)).show(ui).clicked() { ask(Ask::View(Box::new(LabView::new(self.port.clone())))); }
    }
}

/// Where the update check stands and what can be done.
fn updates(ui: &mut Ui, lang: crate::model::text::Lang, u: &Updates, port: &dyn SettingsPort) {
    let checked = |at: &Option<Seen>| at.map(|s| w!(lang, SetCheckedAt, when = &words::seen(lang, s)));
    let (title, line): (String, Option<String>) = match &u.state {
        UpdateState::DevBuild => (w!(lang, SetDevBuild), None),
        UpdateState::NeverChecked => (w!(lang, SetNotChecked), None),
        UpdateState::Checking => (w!(lang, SetChecking), None),
        UpdateState::UpToDate { at } => (w!(lang, SetUpToDate), checked(&Some(*at))),
        UpdateState::Available { version, at } if u.installs_here => {
            let how = w!(lang, SetUpdateHow);
            (w!(lang, NoticeUpdate, version = version), Some(checked(at).map_or(how.clone(), |c| format!("{c} {how}"))))
        }
        // The app opens the page that has the installer: its button says so.
        UpdateState::Available { version, at } => (w!(lang, SetUpdateAvailable, version = version), checked(at)),
        UpdateState::Failed { why } => (w!(lang, SetUpdateFailed), Some(why.clone())),
        UpdateState::Downloading => (w!(lang, SetDownloading), None),
        UpdateState::ReadyToInstall { version, speaker } => (
            w!(lang, NoticeUpdate, version = version),
            Some(match speaker {
                Some(s) => w!(lang, SetClosesForSetup, speaker = s),
                None => w!(lang, SetClosesForSetupPc),
            }),
        ),
        UpdateState::NotSigned => (w!(lang, SetNotSigned), None),
    };
    plain(ui, &title);
    if let Some(l) = &line {
        sentence(ui, l);
    }
    super::parts::buttons(ui, |ui| match &u.state {
        UpdateState::Available { .. } => {
            if u.installs_here {
                if page_button(&w!(lang, SetUpdateNow)).show(ui).clicked() {
                    port.act(Act::UpdateNow);
                }
                if page_button(&w!(lang, SetWhatsNew)).show(ui).clicked() {
                    port.act(Act::WhatsNew);
                }
            } else if page_button(&w!(lang, SetOpenDownloadPage)).show(ui).clicked() {
                port.act(Act::WhatsNew);
            }
            if page_button(&w!(lang, SetSkipVersion)).show(ui).clicked() {
                port.act(Act::SkipVersion);
            }
        }
        UpdateState::ReadyToInstall { .. } => {
            if page_button(&w!(lang, SetInstall)).show(ui).clicked() {
                port.act(Act::Install);
            }
            if page_button(&w!(lang, SetCancel)).show(ui).clicked() {
                port.act(Act::CancelInstall);
            }
        }
        UpdateState::DevBuild | UpdateState::Checking | UpdateState::Downloading => {}
        _ => {
            if page_button(&w!(lang, SetCheckNow)).accessible_name(w!(lang, SetCheckUpdatesNow)).show(ui).clicked() {
                port.act(Act::CheckNow);
            }
        }
    });
}

pub struct LabView {
    port: Arc<dyn SettingsPort>,
    st: PageState,
}

impl LabView {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        LabView { port, st: PageState::default() }
    }

    /// With the live counters shown (pictures).
    pub fn with_counters(mut self) -> Self {
        self.st.open.insert("counters".into());
        self
    }
}

impl ViewContent for LabView {
    fn bar(&self, _snap: &Snapshot) -> ViewBar {
        ViewBar { title: w!(self.port.lang(), SetNavLab), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, _snap: &Snapshot, _ask: &mut dyn FnMut(Ask)) {
        let lang = self.port.lang();
        ui.spacing_mut().item_spacing.y = 8.0;
        let v = self.port.lab();
        // Its warning first.
        let warning = format!("{} {}", w!(lang, SetLabTitle), w!(lang, SetLabLine));
        let reset = w!(lang, SetLabReset);
        let links: Vec<&str> = if v.differs() { vec![reset.as_str()] } else { vec![] };
        if marked(ui, Mark::Lab, Tone::Primary, &warning, &links).is_some() {
            self.port.act(Act::Lab(LabAct::ResetAll));
        }
        if let Some(c) = &v.carried_over {
            thin(ui);
            plain(ui, &w!(lang, SetCarriedTitle));
            if let Some(ms) = c.airplay_ms {
                sentence(ui, &w!(lang, SetCarriedAirPlay, ms = ms));
            }
            if let Some(ms) = c.homepod_ms {
                sentence(ui, &w!(lang, SetCarriedHomePod, ms = ms));
            }
            if c.homepod_buffered {
                sentence(ui, &w!(lang, SetCarriedBuffered));
            }
            let line = if c.speakers.is_empty() { w!(lang, SetCarriedNoneKnown) } else { w!(lang, SetCarriedSpeakers, speakers = &c.speakers.join(", ")) };
            sentence(ui, &line);
            if Button::standard(&w!(lang, SetCarriedReset)).show(ui).clicked() {
                self.port.act(Act::Lab(LabAct::ResetCarriedOver));
            }
        }
        thin(ui);
        if let Some(at_once) = v.at_once {
            let label = w!(lang, SetLabApply);
            let options = [w!(lang, SetLabGradually), w!(lang, SetLabAtOnce)];
            if let Some(i) = pick_line(ui, "lab-apply", &label, Some(&w!(lang, SetLabApplyLine)), &options, at_once as usize) {
                self.port.act(Act::Lab(LabAct::AtOnce(i == 1)));
            }
        }
        if let Some(n) = v.catch_up_frames {
            let values = with(&[1, 2, 4, 8, 16, 32], n as i64, LAB_CATCH_UP_DEFAULT as i64);
            let labels: Vec<String> = values.iter().map(|f| w!(lang, SetLabFrames, count = *f)).collect();
            let chosen = values.iter().position(|v| *v == n as i64).unwrap_or(0);
            if let Some(i) = pick_line(ui, "lab-catch-up", &w!(lang, SetLabCatchUp), Some(&w!(lang, SetLabCatchUpLine)), &labels, chosen) {
                self.port.act(Act::Lab(LabAct::CatchUpFrames(values[i] as u32)));
            }
        }
        if let Some(ppm) = v.drift_ppm {
            let values = with(&[-200, -100, -50, -20, 0, 20, 50, 100, 200], ppm as i64, 0);
            let labels: Vec<String> = values.iter().map(|p| w!(lang, SetLabPpm, ppm = &signed(*p))).collect();
            let chosen = values.iter().position(|v| *v == ppm as i64).unwrap_or(0);
            if let Some(i) = pick_line(ui, "lab-drift", &w!(lang, SetLabDrift), Some(&w!(lang, SetLabDriftLine)), &labels, chosen) {
                self.port.act(Act::Lab(LabAct::DriftPpm(values[i] as i32)));
            }
        }
        if let Some(ms) = v.silence_pace_ms {
            let values = with(&[5, 8, 10, 12, 15, 20], ms as i64, LAB_SILENCE_PACE_DEFAULT as i64);
            let labels: Vec<String> = values.iter().map(|m| w!(lang, SetLabMs, ms = *m)).collect();
            let chosen = values.iter().position(|v| *v == ms as i64).unwrap_or(0);
            if let Some(i) = pick_line(ui, "lab-pacing", &w!(lang, SetLabPacing), Some(&w!(lang, SetLabPacingLine)), &labels, chosen) {
                self.port.act(Act::Lab(LabAct::SilencePace(values[i] as u32)));
            }
        }
        if v.catch_up_frames.is_some() || v.drift_ppm.is_some() || v.silence_pace_ms.is_some() {
            sentence(ui, &w!(lang, SetLabThisRun));
        }
        if let Some(rt) = v.realtime {
            let options = [w!(lang, SetLabAutomatic), w!(lang, SetLabRealtime)];
            if let Some(i) = pick_line(ui, "lab-ap2", &w!(lang, SetLabAp2), Some(&w!(lang, SetLabAp2Line)), &options, rt as usize) {
                self.port.act(Act::Lab(LabAct::Realtime(i == 1)));
            }
        }
        if let Some(on) = v.mfi {
            if let Some(on) = switch_line(ui, &w!(lang, SetLabMfi), on) {
                self.port.act(Act::Lab(LabAct::Mfi(on)));
            }
            crate::settings::widgets::note(ui, &w!(lang, SetLabMfiLine));
        }
        if let Some(on) = v.uncompressed_alac {
            if let Some(on) = switch_line(ui, &w!(lang, SetLabAlac), on) {
                self.port.act(Act::Lab(LabAct::UncompressedAlac(on)));
            }
            crate::settings::widgets::note(ui, &w!(lang, SetLabAlacLine));
        }
        let p = self.port.privacy();
        if let Some(on) = super::parts::switch_line(ui, &w!(lang, SetAnyoneListens), !p.only_playing) { self.port.act(Act::OnlyPlaying(!on)); }
        if !p.only_playing { marked(ui, Mark::Warning, Tone::Primary, &w!(lang, SetAnyDeviceLine), &[]); }
        for r in p.refused.iter().flatten().filter(|r| r.group_of.is_none()) { super::settings_list::refused(ui, self.port.as_ref(), r); }
        if let Some(counters) = &v.counters {
            thin(ui);
            let open = self.st.is_open("counters");
            let label = if open { w!(lang, SetHideCounters) } else { w!(lang, SetShowCounters) };
            if Button::link(&label).show(ui).clicked() {
                self.st.toggle("counters");
            }
            if open {
                let text = counters.iter().map(|(k, val)| format!("{k}: {val}")).collect::<Vec<_>>().join("\n");
                crate::settings::widgets::mono(ui, &text);
            }
        }
    }
}

/// A sentence with a drop-down of choices.
fn pick_line(ui: &mut Ui, id: &str, label: &str, under: Option<&str>, options: &[String], chosen: usize) -> Option<usize> {
    let refs: Vec<&str> = options.iter().map(String::as_str).collect();
    super::parts::drop_line(ui, id, label, under, &refs, chosen)
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
