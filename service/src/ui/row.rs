//! A place in the list: This PC, a speaker, a group, Music Assistant.
//!
//! ```text
//! ▌ [icon]  Kitchen                                  ⋯
//!           Sonos · about 0.15 s delay
//! ```
//!
//! At least 52 points tall, for a finger; a long name wraps rather than
//! being cut. The chosen place has an accent bar and a tint; ⋯ shows while
//! the pointer or the keyboard is on the row, and a right-click or the menu
//! key asks for the same menu.

use egui::{Color32, Key, Rect, Sense, Ui, Vec2, WidgetInfo, WidgetType};

use super::button::{Button, IconButton};
use super::icon::{paint_spinner, Icon};
use super::look::look_as;
use super::text::Tone;
use super::theme::{focus_ring, radius, size, space, Theme};

/// What sits at the right end of a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trailing<'a> {
    None,
    /// A chevron that opens or closes rows under this one (This PC's
    /// outputs, the folded speakers).
    Expand { open: bool },
    /// Connecting: a turning ring and a × that stops it.
    Busy,
    /// One button on the row (*Try again*).
    Button(&'a str),
    /// A tick: the chosen one of a set of sub-rows.
    Check,
}

/// Everything a row shows, as plain values.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaceRow<'a> {
    pub icon: Icon,
    pub name: &'a str,
    /// The second line: kind and delay, or what is true now.
    pub detail: &'a str,
    pub tone: Tone,
    /// The place the sound plays now: accent bar and tint.
    pub current: bool,
    /// Remembered but not found, or can't be played: drawn faint.
    pub dimmed: bool,
    pub trailing: Trailing<'a>,
    /// Offer ⋯ (on pointer or keyboard focus) and the right-click menu.
    pub menu: bool,
    /// A sub-row (This PC's outputs): set in under its parent.
    pub indent: bool,
    /// What a screen reader says; the name and second line by default.
    pub spoken: Option<String>,
}

impl<'a> PlaceRow<'a> {
    pub fn new(icon: Icon, name: &'a str, detail: &'a str) -> Self {
        PlaceRow {
            icon,
            name,
            detail,
            tone: Tone::Secondary,
            current: false,
            dimmed: false,
            trailing: Trailing::None,
            menu: false,
            indent: false,
            spoken: None,
        }
    }
}

/// What the person did with a row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowAction {
    /// Clicked it, or Enter on it.
    Choose,
    /// Asked for its menu: ⋯, right-click, the menu key or Shift+F10. The
    /// menu opens at this point.
    Menu(egui::Pos2),
    /// The chevron.
    Expand,
    /// The × of a connecting row.
    Cancel,
    /// The row's button.
    Button,
}

const MIN_H: f32 = space::ROW_HEIGHT;
const PAD_L: f32 = 14.0;
const PAD_R: f32 = 10.0;
const GAP: f32 = 12.0;
const INDENT: f32 = 30.0;

impl PlaceRow<'_> {
    pub fn show(&self, ui: &mut Ui) -> Option<RowAction> {
        let t = Theme::of(ui.ctx());
        let full_w = ui.available_width();
        let indent = if self.indent { INDENT } else { 0.0 };
        let width = full_w - indent;
        let trailing_w = match self.trailing {
            Trailing::None if self.menu => 28.0,
            Trailing::None => 0.0,
            Trailing::Expand { .. } | Trailing::Check => 28.0,
            Trailing::Busy => 20.0 + 8.0 + 28.0,
            Trailing::Button(words) => {
                let g = ui.painter().layout_no_wrap(words.to_owned(), t.font(size::CONTROL), Color32::PLACEHOLDER);
                g.size().x + 2.0 * space::M
            }
        };
        let text_x = PAD_L + space::ROW_ICON_TILE + GAP;
        let text_w = (width - text_x - PAD_R - if trailing_w > 0.0 { trailing_w + 8.0 } else { 0.0 }).max(40.0);
        let (fade_name, fade_detail) = if self.dimmed { (0.5, 0.8) } else { (1.0, 1.0) };
        let name_color = t.c.text.gamma_multiply(fade_name);
        let detail_color = self.tone.color(&t).gamma_multiply(fade_detail);
        let name = ui.painter().layout(self.name.to_owned(), t.font(size::BODY), name_color, text_w);
        let detail = ui.painter().layout(self.detail.to_owned(), t.font(size::CAPTION), detail_color, text_w);
        let text_h = name.size().y + detail.size().y;
        let height = (text_h + 16.0).max(MIN_H);

        let (outer, alloc) = ui.allocate_exact_size(Vec2::new(full_w, height), Sense::hover());
        let rect = Rect::from_min_max(outer.min + Vec2::new(indent, 0.0), outer.max);
        let id = alloc.id.with("place-row");
        let resp = ui.interact(rect, id, Sense::click());
        let spoken = self.spoken.clone().unwrap_or_else(|| format!("{}. {}.", self.name, self.detail));
        resp.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, self.current, &spoken));
        let l = look_as(ui, &resp, &spoken);
        let mut action = None;

        let p = ui.painter().clone();
        if self.indent {
            let line = Rect::from_min_size(egui::pos2(outer.left() + INDENT - 4.0, outer.top()), Vec2::new(2.0, height));
            p.rect_filled(line, 0.0, t.c.divider);
        }
        let fill = if self.current {
            Some(t.c.selected)
        } else if l.pressed {
            Some(t.c.pressed)
        } else if l.hovered || self.trailing == Trailing::Busy {
            Some(t.c.hover)
        } else {
            None
        };
        if let Some(f) = fill {
            p.rect_filled(rect, radius::ROW, f);
        }
        if self.current {
            let bar_h = (height - 28.0).clamp(16.0, 24.0);
            let bar = Rect::from_center_size(egui::pos2(rect.left() + 1.5, rect.center().y), Vec2::new(3.0, bar_h));
            p.rect_filled(bar, 1.5, t.c.accent);
        }
        let tile = Rect::from_min_size(egui::pos2(rect.left() + PAD_L, rect.center().y - 16.0), Vec2::splat(space::ROW_ICON_TILE));
        p.rect_filled(tile, 6.0, t.c.hover.gamma_multiply(fade_name));
        self.icon.paint(&p, Rect::from_center_size(tile.center(), Vec2::splat(18.0)), name_color);
        let text_top = rect.center().y - text_h / 2.0;
        let detail_top = text_top + name.size().y;
        p.galley(egui::pos2(rect.left() + text_x, text_top), name, name_color);
        p.galley(egui::pos2(rect.left() + text_x, detail_top), detail, detail_color);

        // The right end.
        let right = rect.right() - PAD_R;
        let slot = |w: f32, h: f32| Rect::from_min_size(egui::pos2(right - w, rect.center().y - h / 2.0), Vec2::new(w, h));
        match self.trailing {
            Trailing::None => {}
            Trailing::Expand { open } => {
                let r = slot(28.0, 28.0);
                let chevron = super::inside(ui, r, |ui: &mut Ui| {
                    IconButton::new(if open { Icon::ChevronUp } else { Icon::ChevronDown }, if open { "Hide" } else { "Show more" })
                        .size(28.0, 16.0)
                        .color(t.c.text_secondary)
                        .show(ui)
                });
                if chevron.clicked() {
                    action = Some(RowAction::Expand);
                }
            }
            Trailing::Check => {
                Icon::Check.paint_weight(&p, slot(28.0, 16.0).shrink2(Vec2::new(6.0, 0.0)), t.c.accent, 1.8);
            }
            Trailing::Busy => {
                let cancel = super::inside(ui, slot(28.0, 28.0), |ui: &mut Ui| {
                    IconButton::new(Icon::Close, "Stop connecting").size(28.0, 14.0).color(t.c.text_secondary).show(ui)
                });
                if cancel.clicked() {
                    action = Some(RowAction::Cancel);
                }
                let spin = Rect::from_center_size(egui::pos2(right - 28.0 - 8.0 - 10.0, rect.center().y), Vec2::splat(20.0));
                paint_spinner(&p, spin, t.c.accent, ui.input(|i| i.time));
            }
            Trailing::Button(words) => {
                let r = slot(trailing_w, space::BUTTON_HEIGHT);
                if super::inside(ui, r, |ui: &mut Ui| Button::standard(words).show(ui)).clicked() {
                    action = Some(RowAction::Button);
                }
            }
        }
        if self.menu && self.trailing == Trailing::None && (l.hovered || l.focused || ui.rect_contains_pointer(rect)) {
            let r = slot(28.0, 28.0);
            let more = super::inside(ui, r, |ui: &mut Ui| IconButton::new(Icon::More, "More actions").size(28.0, 18.0).color(t.c.text_secondary).show(ui));
            if more.clicked() {
                action = Some(RowAction::Menu(r.left_bottom()));
            }
        }
        if l.focused {
            focus_ring(&p, rect, radius::ROW, false, &t);
        }

        if action.is_none() {
            if resp.clicked() {
                action = Some(RowAction::Choose);
            } else if self.menu && resp.secondary_clicked() {
                let at = resp.interact_pointer_pos().unwrap_or(rect.center());
                action = Some(RowAction::Menu(at));
            } else if self.menu && resp.has_focus() {
                let menu_key = ui.input(|i| i.key_pressed(Key::F10) && i.modifiers.shift);
                if menu_key {
                    action = Some(RowAction::Menu(rect.left_bottom() + Vec2::new(PAD_L, 0.0)));
                }
            }
        }
        action
    }
}
