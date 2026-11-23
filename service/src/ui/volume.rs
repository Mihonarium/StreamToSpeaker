//! The volume row: a mute button, a slider, and the number.
//!
//! The volume is the speaker's own level (or this PC's output's, on This
//! PC); a volume limit shows as a tick on the track.

use egui::{Key, Rect, Sense, Stroke, Ui, Vec2, WidgetInfo};

use super::button::IconButton;
use super::icon::Icon;
use super::look::look_as;
use super::theme::{focus_ring, size, Theme};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeView<'a> {
    /// 0–100.
    pub level: u8,
    pub muted: bool,
    /// The highest level allowed, shown as a tick on the track.
    pub limit: Option<u8>,
    /// A line under the slider (*Level sent to Music Assistant*).
    pub label: Option<&'a str>,
    /// What the slider is the volume of, for screen readers
    /// (*Kitchen's volume*).
    pub name: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeAction {
    Set(u8),
    ToggleMute,
}

const SLIDER_H: f32 = 20.0;

/// Draw the row; returns what the person changed.
pub fn volume_row(ui: &mut Ui, v: &VolumeView) -> Option<VolumeAction> {
    let t = Theme::of(ui.ctx());
    let mut action = None;
    let width = ui.available_width();
    let number = if v.muted { "Muted".to_string() } else { v.level.to_string() };
    let num = ui.painter().layout_no_wrap(number, t.font(size::BODY), t.c.text);
    let num_w = num.size().x.max(28.0);
    let (row, alloc) = ui.allocate_exact_size(Vec2::new(width, 32.0), Sense::hover());

    let mute_name = if v.muted { "Unmute" } else { "Mute" };
    let mute = super::inside(ui, Rect::from_min_size(row.min, Vec2::splat(32.0)), |ui: &mut Ui| {
        IconButton::new(if v.muted { Icon::Mute } else { Icon::Volume }, mute_name).size(32.0, 18.0).show(ui)
    });
    if mute.clicked() {
        action = Some(VolumeAction::ToggleMute);
    }

    let track = Rect::from_min_max(
        egui::pos2(row.left() + 32.0 + 10.0, row.center().y - SLIDER_H / 2.0),
        egui::pos2(row.right() - num_w - 10.0, row.center().y + SLIDER_H / 2.0),
    );
    let id = alloc.id.with("slider");
    let resp = ui.interact(track, id, Sense::click_and_drag());
    resp.widget_info(|| WidgetInfo::slider(true, v.level as f64, v.name));
    // The thumb's centre runs from 10 points in at each end.
    let span = (track.left() + 10.0)..=(track.right() - 10.0);
    let level_at = |x: f32| -> u8 {
        let f = (x - span.start()) / (span.end() - span.start());
        (f.clamp(0.0, 1.0) * 100.0).round() as u8
    };
    if let Some(p) = resp.interact_pointer_pos() {
        if resp.dragged() || resp.clicked() || resp.is_pointer_button_down_on() {
            let to = level_at(p.x);
            let to = v.limit.map_or(to, |l| to.min(l));
            if to != v.level {
                action = Some(VolumeAction::Set(to));
            }
        }
    }
    if resp.has_focus() {
        ui.memory_mut(|m| m.set_focus_lock_filter(id, egui::EventFilter { horizontal_arrows: true, ..Default::default() }));
        let step: i32 = ui.input(|i| {
            let mut s = 0;
            if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::ArrowDown) {
                s -= 1;
            }
            if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::ArrowUp) {
                s += 1;
            }
            if i.key_pressed(Key::PageDown) {
                s -= 10;
            }
            if i.key_pressed(Key::PageUp) {
                s += 10;
            }
            s
        });
        if step != 0 {
            let max = v.limit.unwrap_or(100) as i32;
            let to = (v.level as i32 + step).clamp(0, max) as u8;
            if to != v.level {
                action = Some(VolumeAction::Set(to));
            }
        }
    }

    let l = look_as(ui, &resp, v.name);
    let p = ui.painter();
    let x_of = |level: u8| span.start() + (span.end() - span.start()) * level as f32 / 100.0;
    let cy = track.center().y;
    let bar = |from: f32, to: f32| Rect::from_min_max(egui::pos2(from, cy - 2.0), egui::pos2(to, cy + 2.0));
    let thumb_x = x_of(v.level);
    p.rect_filled(bar(track.left(), track.right()), 2.0, t.c.track);
    p.rect_filled(bar(track.left(), thumb_x), 2.0, if v.muted { t.c.text_disabled } else { t.c.accent });
    if let Some(limit) = v.limit {
        let x = x_of(limit);
        p.rect_filled(Rect::from_center_size(egui::pos2(x, cy), Vec2::new(2.0, 14.0)), 1.0, t.c.text_secondary);
    }
    let thumb = egui::pos2(thumb_x, cy);
    p.circle_filled(thumb + Vec2::new(0.0, 0.5), 10.5, t.c.card_border);
    p.circle(thumb, 10.0, t.c.control, Stroke::new(1.0_f32, t.c.control_border));
    let dot = if l.pressed {
        6.5
    } else if l.hovered {
        10.0
    } else {
        8.0
    };
    p.circle_filled(thumb, dot / 2.0, if v.muted { t.c.text_disabled } else { t.c.accent });
    if l.focused {
        focus_ring(p, track.expand2(Vec2::new(2.0, 4.0)), 4.0, false, &t);
    }
    p.galley(egui::pos2(row.right() - num.size().x, row.center().y - num.size().y / 2.0), num, t.c.text);

    if let Some(label) = v.label {
        ui.add_space(2.0);
        ui.label(egui::RichText::new(label).font(t.font(size::CAPTION)).color(t.c.text_secondary));
    }
    action
}
