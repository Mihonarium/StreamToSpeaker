//! The volume row (a mute button, a slider, and the number) and the slider
//! itself, which Settings uses for tone and the volume limit.
//!
//! The volume is the speaker's own level (or this PC's output's, on This
//! PC); a volume limit shows as a tick on the track.

use egui::{Key, Rect, Sense, Stroke, Ui, Vec2, WidgetInfo};

use super::button::IconButton;
use super::icon::Icon;
use super::look::look_as;
use super::theme::{focus_ring, size, Theme};

/// A horizontal slider: a track, the filled part up to the thumb, and the
/// thumb. Arrow keys move one step, Page Up and Page Down ten.
#[must_use = "call .show(ui)"]
pub struct Slider<'a> {
    value: f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    name: &'a str,
    tick: Option<f32>,
    max_allowed: Option<f32>,
    faint: bool,
    flyout: bool,
}

impl<'a> Slider<'a> {
    /// `name` is what it sets, for readers of the display (*Kitchen's
    /// volume*, *Bass*).
    pub fn new(value: f32, range: std::ops::RangeInclusive<f32>, name: &'a str) -> Self {
        Slider { value, range, step: 1.0, name, tick: None, max_allowed: None, faint: false, flyout: false }
    }

    /// The unit a key press moves (1 by default); values snap to it.
    pub fn step(mut self, step: f32) -> Self {
        self.step = step;
        self
    }

    /// A tick on the track at this value (a volume limit).
    pub fn tick(mut self, at: f32) -> Self {
        self.tick = Some(at);
        self
    }

    /// Nothing above this value can be chosen (a volume limit).
    pub fn max_allowed(mut self, max: f32) -> Self {
        self.max_allowed = Some(max);
        self
    }

    /// Draw the filled part grey (a muted volume).
    pub fn faint(mut self, faint: bool) -> Self {
        self.faint = faint;
        self
    }

    /// A track press seeks immediately, while grabbing the thumb preserves
    /// the pointer’s offset. Positions land on whole pixels.
    pub(crate) fn flyout(mut self) -> Self {
        self.flyout = true;
        self
    }

    /// Draw it across the available width, 20 points tall; the new value
    /// while the person drags, clicks or uses the keys.
    pub fn show(self, ui: &mut Ui) -> Option<f32> {
        let t = Theme::of(ui.ctx());
        let sense = if self.flyout && !ui.is_enabled() { Sense::hover() } else { Sense::click_and_drag() };
        let (track, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), sense);
        resp.widget_info(|| WidgetInfo::slider(!self.flyout || ui.is_enabled(), self.value as f64, self.name));
        let (lo, hi) = (*self.range.start(), *self.range.end());
        let top = self.max_allowed.unwrap_or(hi).min(hi);
        let snap = |v: f32| ((v / self.step).round() * self.step).clamp(lo, top);
        // The thumb's centre runs from 10 points in at each end, so the thumb
        // never reaches past the track.
        let (x0, x1) = (track.left() + 10.0, track.right() - 10.0);
        let frac = |v: f32| if hi > lo { (v - lo) / (hi - lo) } else { 0.0 };
        let x_of = |v: f32| x0 + (x1 - x0) * frac(v).clamp(0.0, 1.0);
        let mut out = None;
        let pointer_x = if self.flyout {
            let center = egui::pos2(ui.painter().round_to_pixel(x_of(self.value)), track.center().y);
            flyout_pointer_x(ui, &resp, center)
        } else {
            resp.interact_pointer_pos()
                .filter(|_| resp.dragged() || resp.clicked() || resp.is_pointer_button_down_on())
                .map(|p| p.x)
        };
        if let Some(x) = pointer_x {
            let f = ((x - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0);
            let to = snap(lo + f * (hi - lo));
            if to != self.value {
                out = Some(to);
            }
        }
        if resp.has_focus() {
            ui.memory_mut(|m| m.set_focus_lock_filter(resp.id, egui::EventFilter { horizontal_arrows: true, ..Default::default() }));
            let steps: f32 = ui.input(|i| {
                let mut s = 0.0;
                if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::ArrowDown) {
                    s -= 1.0;
                }
                if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::ArrowUp) {
                    s += 1.0;
                }
                if i.key_pressed(Key::PageDown) {
                    s -= 10.0;
                }
                if i.key_pressed(Key::PageUp) {
                    s += 10.0;
                }
                s
            });
            if steps != 0.0 {
                let to = snap(self.value + steps * self.step);
                if to != self.value {
                    out = Some(to);
                }
            }
        }

        let l = look_as(ui, &resp, self.name);
        let p = ui.painter();
        let cy = track.center().y;
        let bar = |from: f32, to: f32| {
            let r = Rect::from_min_max(egui::pos2(from, cy - 2.0), egui::pos2(to, cy + 2.0));
            if self.flyout { p.round_rect_to_pixels(r) } else { r }
        };
        let thumb_x = if self.flyout { p.round_to_pixel(x_of(out.unwrap_or(self.value))) } else { x_of(self.value) };
        let fill = if self.faint { t.c.text_disabled } else { t.c.accent };
        let (left, right) = (track.left(), track.right());
        p.rect_filled(bar(left, right), 2.0, t.c.track);
        p.rect_filled(bar(left, thumb_x), 2.0, fill);
        if let Some(at) = self.tick {
            let mut tick = Rect::from_center_size(egui::pos2(x_of(at), cy), Vec2::new(2.0, 14.0));
            if self.flyout {
                // Remains visible below the thumb when the level is at its limit.
                tick.max.y = cy + 14.0;
                tick = p.round_rect_to_pixels(tick);
            }
            p.rect_filled(tick, 1.0, t.c.text_secondary);
        }
        let thumb = egui::pos2(thumb_x, cy);
        p.circle_filled(thumb + Vec2::new(0.0, 0.5), 10.5, t.c.card_border);
        p.circle(thumb, 10.0, if self.flyout { t.c.card } else { t.c.control }, Stroke::new(1.0_f32, t.c.control_border));
        let dot = if l.pressed {
            6.5
        } else if l.hovered {
            10.0
        } else {
            8.0
        };
        p.circle_filled(thumb, dot / 2.0, fill);
        if l.focused {
            focus_ring(p, track.expand2(Vec2::new(2.0, 4.0)), 4.0, false, &t);
        }
        out
    }
}

/// Return a new thumb position only when a press seeks or its pointer moves.
fn flyout_pointer_x(ui: &Ui, resp: &egui::Response, center: egui::Pos2) -> Option<f32> {
    let grab = resp.id.with("thumb grab");
    let (press, down) = ui.input(|i| {
        let press = i.events.iter().find_map(|e| match e {
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, .. } => Some(*pos),
            _ => None,
        });
        (press, i.pointer.primary_down())
    });
    let pointer = resp.interact_pointer_pos();
    ui.data_mut(|data| {
        // Take and reinsert only for the lifetime of this primary gesture.
        let previous = data.remove_temp::<(f32, Option<f32>)>(grab);
        let (offset, last_x) = match press {
            Some(p) if pointer.is_some() && resp.enabled() => {
                if p.distance(center) <= 10.0 { (p.x - center.x, Some(p.x)) } else { (0.0, None) }
            }
            Some(_) => return None,
            None => previous?,
        };
        let x = pointer?.x;
        if down {
            data.insert_temp(grab, (offset, Some(x)));
        }
        (Some(x) != last_x).then_some(x - offset)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeView<'a> {
    /// 0–100.
    pub level: u8,
    pub muted: bool,
    /// The highest level allowed, shown as a tick on the track.
    pub limit: Option<u8>,
    /// A line under the slider (*Level sent to Music Assistant*).
    pub label: Option<&'a str>,
    /// What the slider is the volume of, for readers of the display
    /// (*Kitchen's volume*, *Level sent to Music Assistant*).
    pub name: &'a str,
    /// Show `name` as the slider's tooltip (where it isn't obvious, as for
    /// the level sent to Music Assistant).
    pub name_as_tooltip: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeAction {
    Set(u8),
    ToggleMute,
}

/// Draw the row; returns what the person changed.
pub fn volume_row(ui: &mut Ui, v: &VolumeView) -> Option<VolumeAction> {
    let t = Theme::of(ui.ctx());
    let mut action = None;
    let width = ui.available_width();
    let number = if v.muted { "Muted".to_string() } else { v.level.to_string() };
    let num = ui.painter().layout_no_wrap(number, t.font(size::BODY), t.c.text);
    let num_w = num.size().x.max(28.0);
    let (row, _) = ui.allocate_exact_size(Vec2::new(width, 32.0), Sense::hover());

    let mute_name = if v.muted { "Unmute" } else { "Mute" };
    let mute = super::inside(ui, Rect::from_min_size(row.min, Vec2::splat(32.0)), |ui: &mut Ui| {
        IconButton::new(if v.muted { Icon::Mute } else { Icon::Volume }, mute_name).size(32.0, 18.0).show(ui)
    });
    if mute.clicked() {
        action = Some(VolumeAction::ToggleMute);
    }

    let track = Rect::from_min_max(egui::pos2(row.left() + 32.0 + 10.0, row.center().y - 10.0), egui::pos2(row.right() - num_w - 10.0, row.center().y + 10.0));
    let mut slider = Slider::new(v.level as f32, 0.0..=100.0, v.name).faint(v.muted);
    if let Some(l) = v.limit {
        slider = slider.tick(l as f32).max_allowed(l as f32);
    }
    let set = super::inside(ui, track, |ui: &mut Ui| {
        let r = ui.scope(|ui| slider.show(ui));
        if v.name_as_tooltip {
            let _ = r.response.on_hover_text(v.name);
        }
        r.inner
    });
    if let Some(to) = set {
        action = Some(VolumeAction::Set(to.round() as u8));
    }
    ui.painter().galley(egui::pos2(row.right() - num.size().x, row.center().y - num.size().y / 2.0), num, t.c.text);

    if let Some(label) = v.label {
        ui.add_space(2.0);
        ui.label(egui::RichText::new(label).font(t.font(size::CAPTION)).color(t.c.text_secondary));
    }
    action
}
