//! Pictures of the tray: the icon in each state on a light and a dark
//! taskbar, with the tooltip the model gives it, and the menu's items. The
//! menu itself is Windows' own; the picture shows its items in the app's
//! menu part.

use egui::{Color32, ColorImage, Id, Rect, RichText, Sense, TextureHandle, TextureOptions, Ui, Vec2};

use super::icons::{tray_icon, SIDE};
use crate::model::port::Port;
use crate::model::{samples, fake::ScriptedEngine};
use crate::model::state::{Arcs, Badge};
use crate::ui::menu::{menu_frame, MenuItem};
use crate::ui::preview::{Scene, Stage};
use crate::ui::theme::{size, Theme};

/// Every picture of the tray.
pub fn scenes() -> Vec<Scene> {
    let mut scenes = vec![
        Scene { name: "tray-icons", size: [1000.0, 0.0], draw: icons },
        Scene { name: "tray-menu", size: [900.0, 0.0], draw: menus },
    ];
    macro_rules! popups {
        ($($sample:literal),*) => {
            $(if super::popup::Contents::from_view(&ScriptedEngine::new($sample).snapshot()).is_some() {
                scenes.push(Scene { name: concat!("popup-", $sample), size: [368.0, 0.0],
                    draw: |ui, _| popup(ui, $sample) });
            })*
        };
    }
    samples::with_sample_names!(popups);
    macro_rules! windows {
        ($($sample:literal),*) => { $(scenes.push(Scene { name: concat!("window-", $sample),
            size: [super::main_window::WIDTH + 42.0, 0.0], draw: |ui, _| ordinary(ui, $sample, None) });)* };
    }
    windows!("kitchen-playing", "pc-plain", "pc-kitchen-didnt-respond", "many-speakers", "long-names",
        "pc-welcome-pick", "pc-welcome-looking", "pc-welcome-none-found", "pc-welcome-busy", "pc-welcome-chime", "kitchen-welcome-done");
    macro_rules! first_run {
        ($($name:literal),*) => { [$(Scene {
            name: $name, size: [super::main_window::WIDTH + 42.0, 0.0],
            draw: |ui, _| ordinary(ui, $name, None),
        }),*] };
    }
    scenes.extend(samples::with_sample_names!(apps_first_run, first_run));
    scenes.extend([
        Scene { name: "apps-window-playing", size: [super::main_window::WIDTH + 42.0, 0.0], draw: |ui, _| ordinary(ui, "apps-panel-playing", None) },
        Scene { name: "apps-popup-moved", size: [368.0, 0.0], draw: |ui, _| popup(ui, "apps-moved") },
        Scene { name: "window-speaker", size: [424.0, 0.0], draw: |ui, _| ordinary(ui, "kitchen-playing",
            Some(crate::model::Open::SpeakerSettings(crate::model::SpeakerId::new("kitchen")))) },
        Scene { name: "window-settings", size: [424.0, 0.0], draw: |ui, _| ordinary(ui, "kitchen-playing", Some(crate::model::Open::Settings)) },
        Scene { name: "popup-window-closed-speaker", size: [368.0, 0.0], draw: |ui, _| closed(ui, "kitchen-playing") },
        Scene { name: "popup-window-closed-pc", size: [368.0, 0.0], draw: |ui, _| closed(ui, "pc-plain") },
    ]);
    scenes
}

fn popup(ui: &mut Ui, sample: &str) {
    popup_card(ui, &ScriptedEngine::new(sample).snapshot());
}

fn popup_card(ui: &mut Ui, view: &crate::model::Snapshot) {
    ui.add_space(20.0);
    ui.horizontal_top(|ui| {
        ui.add_space(20.0);
        ui.vertical(|ui| { super::popup::card(ui, super::popup::Contents::from_view(view).unwrap(), view.lang); });
    });
    ui.add_space(20.0);
}

fn closed(ui: &mut Ui, sample: &str) {
    let engine = ScriptedEngine::new(sample);
    engine.send(crate::model::Surface::Panel, crate::model::Command::PanelShown(true));
    engine.send(crate::model::Surface::Panel, crate::model::Command::PanelShown(false));
    engine.send(crate::model::Surface::Panel, crate::model::Command::WindowClosed);
    popup_card(ui, &engine.snapshot());
}

fn ordinary(ui: &mut Ui, sample: &str, page: Option<crate::model::Open>) {
    use super::main_window::{MainWindow, TITLE_HEIGHT, WIDTH};
    use super::Views;
    let t = Theme::of(ui.ctx());
    let snap = ScriptedEngine::new(sample).snapshot();
    crate::ui::look::set_focus_visible(ui.ctx(), false);
    let mut window = MainWindow::new(None, None);
    let page_state = egui::Id::new(("window-page", sample));
    window.sync_first_run(snap.scene.situation.first_run.as_ref());
    if let Some((_, state)) = &mut window.welcome {
        *state = ui.ctx().data(|d| d.get_temp(page_state)).unwrap_or_default();
    }
    if let Some(open) = page {
        let views = crate::settings::views::SettingsViews::new(std::sync::Arc::new(crate::settings::scripted::ScriptedSettings::new()));
        window.page(views.view(&open, &snap).unwrap(), false);
        window.pages[0].1 = ui.ctx().data(|d| d.get_temp(page_state)).unwrap_or_default();
    }
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    ui.add_space(20.0);
    ui.horizontal_top(|ui| {
        ui.add_space(20.0);
        egui::Frame::none().stroke(t.card_stroke()).shadow(t.shadow()).fill(t.c.surface).rounding(6.0).show(ui, |ui| {
            ui.set_width(WIDTH);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                let (bar, _) = ui.allocate_exact_size(egui::vec2(WIDTH, TITLE_HEIGHT), Sense::hover());
                let cy = bar.center().y;
                let p = ui.painter();
                p.rect_filled(bar, 0.0, t.c.chrome);
                let icon = Rect::from_center_size(egui::pos2(bar.left()+24.0,cy), Vec2::splat(16.0));
                p.image(super::icons::app_mark(ui).id(), icon, Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0,1.0)), t.c.text);
                p.text(egui::pos2(bar.left()+42.0,cy),egui::Align2::LEFT_CENTER,crate::PRODUCT_NAME,t.font(12.0),t.c.text);
                let x = bar.right()-115.0;
                p.hline(x-5.0..=x+5.0,cy,egui::Stroke::new(1.0_f32,t.c.text));
                p.rect_stroke(Rect::from_center_size(egui::pos2(x+46.0,cy),Vec2::splat(9.0)),0.0,egui::Stroke::new(1.0_f32,t.c.text_disabled));
                crate::ui::icon::Icon::Close.paint(p,Rect::from_center_size(egui::pos2(x+92.0,cy),Vec2::splat(16.0)),t.c.text);
                let cap = if sample == "many-speakers" { 620.0 } else { 1024.0 };
                let body = if window.welcome.is_some() {
                    ui.ctx().data(|d| d.get_temp::<f32>(page_state.with("height"))).unwrap_or(500.0)
                } else { window.natural_height(ui, &snap, true) }.min(cap);
                ui.allocate_ui_with_layout(egui::vec2(WIDTH, body), egui::Layout::top_down(egui::Align::Min), |ui| {
                    if window.welcome.is_some() { ui.set_min_height(body); }
                    let drawn = window.draw(ui, &snap, true, body);
                    if let Some((_, state)) = &window.welcome {
                        let natural = drawn.natural.max(super::main_window::MIN_HEIGHT);
                        ui.ctx().data_mut(|d| {
                            d.insert_temp(page_state, state.clone());
                            d.insert_temp(page_state.with("height"), natural);
                        });
                        if (natural - body).abs() > 0.5 { ui.ctx().request_discard("welcome page height"); }
                    }
                });
                if let Some((_, state)) = window.pages.last() { ui.ctx().data_mut(|d| d.insert_temp(page_state,state.clone())); }
            });
        });
    });
    ui.add_space(20.0);
}

fn texture(ui: &Ui, arcs: Arcs, badge: Badge, dark: bool) -> TextureHandle {
    let id = Id::new(("tray-icon-texture", format!("{arcs:?}{badge:?}"), dark));
    if let Some(t) = ui.ctx().data(|d| d.get_temp::<TextureHandle>(id)) {
        return t;
    }
    let image = ColorImage::from_rgba_unmultiplied([SIDE as usize, SIDE as usize], tray_icon(arcs, badge, dark));
    let t = ui.ctx().load_texture(format!("tray-{arcs:?}-{badge:?}-{dark}"), image, TextureOptions::LINEAR);
    ui.ctx().data_mut(|d| d.insert_temp(id, t.clone()));
    t
}

/// A strip of taskbar with the icon at its size there (16 points).
fn on_taskbar(ui: &mut Ui, arcs: Arcs, badge: Badge, dark: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(64.0, 40.0), Sense::hover());
    let bg = if dark { Color32::from_rgb(0x20, 0x20, 0x20) } else { Color32::from_rgb(0xee, 0xee, 0xee) };
    ui.painter().rect_filled(rect, 6.0, bg);
    let tex = texture(ui, arcs, badge, dark);
    let icon = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
    ui.painter().image(tex.id(), icon, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
}

fn icons(ui: &mut Ui, _st: &mut Stage) {
    let t = Theme::of(ui.ctx());
    ui.add_space(20.0);
    ui.horizontal_top(|ui| {
        ui.add_space(20.0);
        egui::Grid::new("tray-icons").num_columns(5).spacing(Vec2::new(18.0, 10.0)).min_row_height(40.0).show(ui, |ui| {
            for h in ["", "Light taskbar", "Dark taskbar", "Tooltip", "Scenario"] {
                ui.label(RichText::new(h.to_uppercase()).font(t.strong(11.5)).color(t.c.text_secondary));
            }
            ui.end_row();
            for sample in ["pc-plain", "kitchen-playing", "pc-kitchen-stopped-responding", "kitchen-not-responding-trying"] {
                let view = ScriptedEngine::new(sample).snapshot();
                let (arcs, badge) = super::icons::appearance(&view.tray);
                let tooltip = &view.tray.tooltip;
                let what = if view.tray.amber { "Needs a look" } else if view.tray.accent { "On a speaker" } else { "Sound on this PC" };
                ui.label(RichText::new(what).font(t.strong(size::BODY)).color(t.c.text));
                on_taskbar(ui, arcs, badge, false);
                on_taskbar(ui, arcs, badge, true);
                ui.allocate_ui(Vec2::new(400.0, 40.0), |ui| {
                    ui.set_width(400.0);
                    ui.add(egui::Label::new(RichText::new(tooltip).font(t.font(size::BODY)).color(t.c.text)).wrap());
                });
                ui.label(RichText::new(sample).font(t.font(size::SMALL)).color(t.c.text_secondary));
                ui.end_row();
            }
        });
    });
    ui.add_space(20.0);
}

fn menu_of(ui: &mut Ui, caption: &str, sample: &str) {
    let t = Theme::of(ui.ctx());
    let snap = ScriptedEngine::new(sample).snapshot();
    ui.vertical(|ui| {
        ui.label(RichText::new(caption.to_uppercase()).font(t.strong(11.5)).color(t.c.text_secondary));
        ui.add_space(6.0);
        menu_frame(ui.ctx()).show(ui, |ui| {
            ui.set_width(260.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            for e in &snap.tray.menu {
                MenuItem::new(&e.label).checked(e.checked).enabled(e.command.is_some() || e.checked).show(ui);
            }
        });
    });
}

fn menus(ui: &mut Ui, _st: &mut Stage) {
    ui.add_space(20.0);
    ui.horizontal_top(|ui| {
        ui.add_space(20.0);
        ui.spacing_mut().item_spacing.x = 28.0;
        menu_of(ui, "Playing on a speaker", "kitchen-playing");
        menu_of(ui, "Long names", "long-names");
        menu_of(ui, "Many speakers", "many-speakers");
    });
    ui.add_space(20.0);
}
