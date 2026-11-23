//! Settings → About: the version, the sound output, updates, support,
//! privacy, the licence, and the switch that shows Lab.

use egui::{Rect, Sense, Ui, Vec2};

use crate::ui::button::Button;
use crate::ui::card::SettingRow;
use crate::ui::icon::{paint_mark, Icon};
use crate::ui::inputs::Switch;
use crate::ui::notice::BarKind;
use crate::ui::theme::{size, Theme};

use super::port::{Act, Driver, UpdateState, Updates};
use super::widgets::{boxed, buttons, heading, note, page_title, strong};
use super::words::{self, w};
use super::{Cx, PageState};

pub(super) fn page(ui: &mut Ui, cx: &mut Cx, _st: &mut PageState) {
    let lang = cx.lang;
    let v = cx.port.about();
    page_title(ui, &w!(lang, SetNavAbout));
    let t = Theme::of(ui.ctx());
    boxed(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let (r, _) = ui.allocate_exact_size(Vec2::splat(52.0), Sense::hover());
            paint_mark(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(46.0)), t.c.mark_ink, t.c.mark_arcs, false);
            ui.vertical(|ui| {
                ui.add_space(6.0);
                strong(ui, &w!(lang, SetAppVersion, version = &v.version), size::TITLE + 2.0);
                note(ui, &w!(lang, SetAppLine));
            });
        });
    });

    if let Some(d) = &v.driver {
        heading(ui, &w!(lang, SetDriverHeading));
        match d {
            Driver::Running(ver) => {
                let _ = SettingRow::new(&w!(lang, SetDriverRunning, version = ver)).icon(Icon::Volume).show(ui, |_| ());
            }
            Driver::Waiting { installed, running } => {
                let title = w!(lang, SetDriverWaiting, version = installed);
                let text = w!(lang, SetDriverUntil, version = running);
                let action = w!(lang, SetRestartNow);
                if super::widgets::bar(ui, BarKind::Warning, &title, &text, Some(&action)) {
                    cx.act(Act::RestartWindows);
                }
            }
        }
    }

    heading(ui, &w!(lang, SetUpdatesHeading));
    updates(ui, cx, &v.updates);
    if let Some(daily) = v.updates.daily {
        let title = w!(lang, SetDailyCheck);
        let r = SettingRow::new(&title).icon(Icon::Clock).description(&w!(lang, SetDailyCheckLine)).show(ui, |ui| Switch::new(daily, &title).show(ui));
        if let Some(on) = r.inner {
            cx.act(Act::DailyUpdateCheck(on));
        }
    }

    heading(ui, &w!(lang, SetMoreHeading));
    let r = SettingRow::new(&w!(lang, SetSupport))
        .icon(Icon::User)
        .description(&w!(lang, SetSupportLine))
        .show(ui, |ui| Button::standard(&w!(lang, SetDonate)).icon(Icon::External).show(ui).clicked());
    if r.inner {
        cx.act(Act::Donate);
    }
    let _ = SettingRow::new(&w!(lang, SetPrivacyTitle)).icon(Icon::Shield).description(&w!(lang, SetPrivacyLine)).show(ui, |_| ());
    let r = SettingRow::new(&w!(lang, SetLicence))
        .icon(Icon::List)
        .description(&w!(lang, SetLicenceLine))
        .show(ui, |ui| Button::standard(&w!(lang, SetViewSource)).icon(Icon::External).show(ui).clicked());
    if r.inner {
        cx.act(Act::LicenceAndSource);
    }
    let title = w!(lang, SetShowLab);
    let r = SettingRow::new(&title).icon(Icon::Flask).description(&w!(lang, SetShowLabLine)).show(ui, |ui| Switch::new(v.lab, &title).show(ui));
    if let Some(on) = r.inner {
        cx.act(Act::ShowLab(on));
    }
}

/// The update card: where the check stands and what can be done.
fn updates(ui: &mut Ui, cx: &mut Cx, u: &Updates) {
    let lang = cx.lang;
    let checked = |at: &Option<super::port::Seen>| at.map(|s| w!(lang, SetCheckedAt, when = &words::seen(lang, s)));
    let (title, line): (String, Option<String>) = match &u.state {
        UpdateState::DevBuild => (w!(lang, SetDevBuild), None),
        UpdateState::NeverChecked => (w!(lang, SetNotChecked), None),
        UpdateState::Checking => (w!(lang, SetChecking), None),
        UpdateState::UpToDate { at } => (w!(lang, SetUpToDate), checked(&Some(*at))),
        UpdateState::Available { version, at } => {
            let how = if u.installs_here { w!(lang, SetUpdateHow) } else { w!(lang, SetUpdateHowPage) };
            (w!(lang, NoticeUpdate, version = version), Some(match checked(at) {
                Some(c) => format!("{c} {how}"),
                None => how,
            }))
        }
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
    boxed(ui, |ui| {
        super::widgets::row(ui, Some(Icon::Refresh), &title, line.as_deref(), |_| ());
        let actions = !matches!(u.state, UpdateState::DevBuild | UpdateState::Checking | UpdateState::Downloading);
        if !actions {
            return;
        }
        ui.horizontal(|ui| {
            ui.add_space(46.0);
            buttons(ui, |ui| match &u.state {
                UpdateState::Available { .. } => {
                    if u.installs_here {
                        if Button::primary(&w!(lang, SetUpdateNow)).show(ui).clicked() {
                            cx.act(Act::UpdateNow);
                        }
                    } else if Button::primary(&w!(lang, SetGetUpdate)).icon(Icon::External).show(ui).clicked() {
                        cx.act(Act::WhatsNew);
                    }
                    if u.installs_here && Button::standard(&w!(lang, SetWhatsNew)).show(ui).clicked() {
                        cx.act(Act::WhatsNew);
                    }
                    if Button::standard(&w!(lang, SetSkipVersion)).show(ui).clicked() {
                        cx.act(Act::SkipVersion);
                    }
                }
                UpdateState::ReadyToInstall { .. } => {
                    if Button::primary(&w!(lang, SetInstall)).show(ui).clicked() {
                        cx.act(Act::Install);
                    }
                    if Button::standard(&w!(lang, SetCancel)).show(ui).clicked() {
                        cx.act(Act::CancelInstall);
                    }
                }
                _ => {
                    if Button::standard(&w!(lang, SetCheckNow)).accessible_name(w!(lang, SetCheckUpdatesNow)).show(ui).clicked() {
                        cx.act(Act::CheckNow);
                    }
                }
            });
        });
    });
}
