//! Menus: the panel's ⋯ menu, a row's menu, a drop-down's list.
//!
//! Items are 32 points tall with room for a tick at the left and a ›
//! at the right; a greyed first line can say the state (*Playing on
//! Kitchen*).

use egui::{Id, Pos2, Rect, Sense, Ui, Vec2, WidgetInfo, WidgetType};

use super::icon::Icon;
use super::look::look_as;
use super::theme::{focus_ring, radius, size, space, Theme};

/// One item of a menu.
#[must_use = "call .show(ui)"]
pub struct MenuItem<'a> {
    label: &'a str,
    checked: bool,
    selected: bool,
    enabled: bool,
    submenu: bool,
}

impl<'a> MenuItem<'a> {
    pub fn new(label: &'a str) -> Self {
        MenuItem { label, checked: false, selected: false, enabled: true, submenu: false }
    }

    /// A tick before the words (the place sound plays on now).
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    /// Marked as the current choice of a drop-down: an accent bar and a
    /// tint.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// A › at the right: it opens more items.
    pub fn submenu(mut self, submenu: bool) -> Self {
        self.submenu = submenu;
        self
    }

    /// Draw it; true when chosen (click, or Enter while focused).
    pub fn show(self, ui: &mut Ui) -> bool {
        let t = Theme::of(ui.ctx());
        let width = ui.available_width();
        let text_w = width - 34.0 - if self.submenu { 24.0 } else { 12.0 };
        let color = if self.enabled { t.c.text } else { t.c.text_disabled };
        let galley = ui.painter().layout(self.label.to_owned(), t.font(size::CONTROL), color, text_w);
        let height = (galley.size().y + 13.0).max(space::MENU_ITEM_HEIGHT);
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), sense);
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, self.enabled, self.checked || self.selected, self.label));
        let l = look_as(ui, &resp, self.label);
        let p = ui.painter();
        if self.enabled && (l.hovered || l.pressed || self.selected) {
            p.rect_filled(rect, radius::CONTROL, if l.pressed { t.c.pressed } else { t.c.hover });
        }
        if self.selected {
            let bar = Rect::from_center_size(egui::pos2(rect.left() + 1.5, rect.center().y), Vec2::new(3.0, 16.0));
            p.rect_filled(bar, 1.5, t.c.accent);
        }
        if self.checked {
            Icon::Check.paint(p, Rect::from_center_size(egui::pos2(rect.left() + 18.0, rect.center().y), Vec2::splat(16.0)), color);
        }
        p.galley(egui::pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0), galley, color);
        if self.submenu {
            let r = Rect::from_center_size(egui::pos2(rect.right() - 14.0, rect.center().y), Vec2::splat(14.0));
            Icon::ChevronRight.paint(p, r, t.c.text_secondary);
        }
        if l.focused {
            focus_ring(p, rect, radius::CONTROL, false, &t);
        }
        self.enabled && resp.clicked()
    }
}

/// A greyed line that says something rather than doing it.
pub fn menu_header(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    let galley = ui.painter().layout(text.to_owned(), t.font(size::SMALL), t.c.text_secondary, ui.available_width() - 46.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), (galley.size().y + 13.0).max(32.0)), Sense::hover());
    ui.painter().galley(egui::pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0), galley, t.c.text_secondary);
}

pub fn menu_separator(ui: &mut Ui) {
    let t = Theme::of(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
    let line = Rect::from_min_max(egui::pos2(rect.left() + 2.0, rect.center().y - 0.5), egui::pos2(rect.right() - 2.0, rect.center().y + 0.5));
    ui.painter().rect_filled(line, 0.0, t.c.divider);
}

/// The frame of a menu: the window colour, a thin border, rounded corners
/// and the flyout shadow.
pub fn menu_frame(ui_ctx: &egui::Context) -> egui::Frame {
    let t = Theme::of(ui_ctx);
    egui::Frame::none()
        .fill(t.c.surface)
        .stroke(t.card_stroke())
        .rounding(radius::WINDOW)
        .shadow(t.shadow())
        .inner_margin(egui::Margin::same(4.0))
}

/// Open the menu `id` at `at` (it shows from the next frame on).
pub fn open_menu(ctx: &egui::Context, id: Id, at: Pos2) {
    ctx.data_mut(|d| d.insert_temp(id, MenuOpen(at)));
}

pub fn close_menu(ctx: &egui::Context, id: Id) {
    ctx.data_mut(|d| d.remove::<MenuOpen>(id));
}

pub fn menu_is_open(ctx: &egui::Context, id: Id) -> bool {
    ctx.data(|d| d.get_temp::<MenuOpen>(id)).is_some()
}

#[derive(Clone, Copy, Debug)]
struct MenuOpen(Pos2);

/// Show the menu `id` if it is open, `width` points wide, with `items`
/// returning true when one was chosen. It closes when an item is chosen,
/// on Esc, and on a click outside it. Returns what `items` returned.
pub fn show_menu<R>(ctx: &egui::Context, id: Id, width: f32, items: impl FnOnce(&mut Ui) -> (bool, R)) -> Option<R> {
    let at = ctx.data(|d| d.get_temp::<MenuOpen>(id))?.0;
    let area = egui::Area::new(id.with("area")).order(egui::Order::Foreground).fixed_pos(at).constrain(true);
    let inner = area.show(ctx, |ui| {
        menu_frame(ctx).show(ui, |ui| {
            ui.set_width(width - 8.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            items(ui)
        })
    });
    let rect = inner.response.rect;
    let (chosen, r) = inner.inner.inner;
    let outside = ctx.input(|i| i.pointer.any_pressed() && i.pointer.press_origin().is_some_and(|p| !rect.contains(p)));
    let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    if chosen || outside || escape {
        close_menu(ctx, id);
    }
    Some(r)
}
