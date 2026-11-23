//! Pictures of the tray: the icon in each state on a light and a dark
//! taskbar, with the tooltip the model gives it, and the menu's items. The
//! menu itself is Windows' own; the picture shows its items in the app's
//! menu part.

use egui::{Color32, ColorImage, Id, Rect, RichText, Sense, TextureHandle, TextureOptions, Ui, Vec2};

use super::icons::{tray_icon, SIDE};
use crate::model::port::Port;
use crate::model::scripted::{Names, Scenario, ScriptedEngine};
use crate::model::state::{Arcs, Badge};
use crate::model::surfaces::MenuEntry;
use crate::ui::menu::{menu_frame, menu_header, menu_separator, MenuItem};
use crate::ui::preview::{Scene, Stage};
use crate::ui::theme::{size, Theme};

/// Every picture of the tray.
pub fn scenes() -> Vec<Scene> {
    vec![
        Scene { name: "tray-icons", size: [900.0, 0.0], draw: icons },
        Scene { name: "tray-menu", size: [900.0, 0.0], draw: menus },
    ]
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

/// The first scenario whose tray shows `arcs` with `badge`.
fn example(arcs: Arcs, badge: Badge) -> Option<(Scenario, String)> {
    Scenario::ALL.iter().find_map(|s| {
        let snap = ScriptedEngine::new(*s).snapshot();
        (snap.tray.arcs == arcs && snap.tray.badge == badge).then(|| (*s, snap.tray.tooltip.clone()))
    })
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
            // From nothing to report to not working, then any other pair a
            // scenario shows.
            let mut pairs = vec![
                (Arcs::Ink, Badge::None),
                (Arcs::GreenFaint, Badge::None),
                (Arcs::Green, Badge::None),
                (Arcs::Green, Badge::Attention),
                (Arcs::Ink, Badge::Attention),
                (Arcs::Ink, Badge::Failure),
            ];
            for arcs in [Arcs::Ink, Arcs::GreenFaint, Arcs::Green] {
                for badge in [Badge::None, Badge::Attention, Badge::Failure] {
                    if !pairs.contains(&(arcs, badge)) {
                        pairs.push((arcs, badge));
                    }
                }
            }
            for (arcs, badge) in pairs {
                let Some((scenario, tooltip)) = example(arcs, badge) else { continue };
                let what = match (arcs, badge) {
                    (Arcs::Ink, Badge::None) => "Sound on this PC",
                    (Arcs::GreenFaint, Badge::None) => "Connecting",
                    (Arcs::Green, Badge::None) => "On a speaker",
                    (_, Badge::Attention) => "Needs a look",
                    (_, Badge::Failure) => "Not working",
                };
                ui.label(RichText::new(what).font(t.strong(size::BODY)).color(t.c.text));
                on_taskbar(ui, arcs, badge, false);
                on_taskbar(ui, arcs, badge, true);
                ui.allocate_ui(Vec2::new(400.0, 40.0), |ui| {
                    ui.set_width(400.0);
                    ui.add(egui::Label::new(RichText::new(tooltip).font(t.font(size::BODY)).color(t.c.text)).wrap());
                });
                ui.label(RichText::new(scenario.name()).font(t.font(size::SMALL)).color(t.c.text_secondary));
                ui.end_row();
            }
        });
    });
    ui.add_space(20.0);
}

fn menu_of(ui: &mut Ui, caption: &str, scenario: Scenario, names: Names) {
    let t = Theme::of(ui.ctx());
    let snap = ScriptedEngine::with_names(scenario, names).snapshot();
    ui.vertical(|ui| {
        ui.label(RichText::new(caption.to_uppercase()).font(t.strong(11.5)).color(t.c.text_secondary));
        ui.add_space(6.0);
        menu_frame(ui.ctx()).show(ui, |ui| {
            ui.set_width(260.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            for e in &snap.tray.menu {
                match e {
                    MenuEntry::Label(text) => menu_header(ui, text),
                    MenuEntry::Separator => menu_separator(ui),
                    MenuEntry::Item(item) => {
                        let _ = MenuItem::new(&item.label).checked(item.checked).enabled(item.enabled).show(ui);
                    }
                }
            }
        });
    });
}

fn menus(ui: &mut Ui, _st: &mut Stage) {
    ui.add_space(20.0);
    ui.horizontal_top(|ui| {
        ui.add_space(20.0);
        ui.spacing_mut().item_spacing.x = 28.0;
        menu_of(ui, "Playing on a speaker", Scenario::Playing, Names::Usual);
        menu_of(ui, "Long names", Scenario::Playing, Names::Long);
        menu_of(ui, "Many speakers", Scenario::RowCatalogue, Names::Usual);
    });
    ui.add_space(20.0);
}
