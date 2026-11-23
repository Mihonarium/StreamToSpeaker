//! Cards, headings and the setting row that Settings pages and the panel
//! are made of.

use egui::{InnerResponse, Margin, Rect, Sense, Ui, Vec2, WidgetInfo, WidgetType};

use super::icon::Icon;
use super::look::look_as;
use super::theme::{focus_ring, radius, size, space, Theme};

/// A white (dark: lighter grey) card with a thin border: `pad` inside.
pub fn card<R>(ui: &mut Ui, pad: Margin, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    let t = Theme::of(ui.ctx());
    egui::Frame::none().fill(t.c.card).stroke(t.card_stroke()).rounding(radius::CARD).inner_margin(pad).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui)
    })
}

/// The padding of the panel's cards.
pub const PANEL_CARD_PAD: Margin = Margin { left: space::CARD_PAD_X, right: space::CARD_PAD_X, top: space::CARD_PAD_Y, bottom: space::CARD_PAD_Y };
/// The padding of a Settings card.
pub const SETTINGS_CARD_PAD: Margin = Margin { left: 16.0, right: 16.0, top: 12.0, bottom: 12.0 };

/// A Settings section's heading (*Your speakers*), with its space above.
pub fn section_heading(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        ui.label(egui::RichText::new(text).font(t.strong(size::BODY)).color(t.c.text));
    });
    ui.add_space(6.0);
}

/// A list's heading in the panel (*Play on*): small, grey, semibold.
pub fn list_heading(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.label(egui::RichText::new(text).font(t.strong(size::SMALL)).color(t.c.text_secondary));
}

/// A thin line across the width.
pub fn divider(ui: &mut Ui) {
    let t = Theme::of(ui.ctx());
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, t.c.divider);
}

/// A setting as a card of its own: an icon, a title, a grey description,
/// and a control at the right (a switch, a drop-down, a button), or a ›
/// when the card opens a page of its own.
#[must_use = "call .show(ui, control)"]
pub struct SettingRow<'a> {
    title: &'a str,
    description: Option<&'a str>,
    icon: Option<Icon>,
    opens_page: bool,
}

/// What happened on a setting row.
pub struct SettingResponse<R> {
    /// The row was clicked (only rows that open a page can be).
    pub opened: bool,
    /// What the control returned.
    pub inner: R,
}

impl<'a> SettingRow<'a> {
    pub fn new(title: &'a str) -> Self {
        SettingRow { title, description: None, icon: None, opens_page: false }
    }

    pub fn description(mut self, d: &'a str) -> Self {
        self.description = Some(d);
        self
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// The whole card is a link to another page; a › shows at its end.
    pub fn opens_page(mut self) -> Self {
        self.opens_page = true;
        self
    }

    /// Draw the card. `control` draws what sits at the right, right to
    /// left; give it nothing for a card that only opens a page.
    pub fn show<R>(self, ui: &mut Ui, control: impl FnOnce(&mut Ui) -> R) -> SettingResponse<R> {
        let t = Theme::of(ui.ctx());
        let width = ui.available_width();
        let pad = SETTINGS_CARD_PAD;
        let icon_w = if self.icon.is_some() { 32.0 + 14.0 } else { 0.0 };
        // The id the card's own space will get, known before it is taken,
        // to look up its control's width.
        let id = ui.next_auto_id().with("setting-row");
        // The control's width, measured the frame before (it rarely changes).
        let control_w: f32 = ui.ctx().data(|d| d.get_temp(id.with("control-w"))).unwrap_or(0.0);
        let chevron_w = if self.opens_page { 28.0 } else { 0.0 };
        let text_w = (width - pad.left - pad.right - icon_w - control_w - chevron_w - if control_w > 0.0 { 14.0 } else { 0.0 }).max(120.0);
        let title = ui.painter().layout(self.title.to_owned(), t.font(size::BODY), t.c.text, text_w);
        let desc = self.description.map(|d| ui.painter().layout(d.to_owned(), t.font(size::CAPTION), t.c.text_secondary, text_w));
        let text_h = title.size().y + desc.as_ref().map_or(0.0, |d| d.size().y + 2.0);
        let inner_h = text_h.max(space::BUTTON_HEIGHT);
        let height = inner_h + pad.top + pad.bottom;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let resp = if self.opens_page { Some(ui.interact(rect, id, Sense::click())) } else { None };
        if let Some(r) = &resp {
            r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, self.title));
        }
        let l = resp.as_ref().map(|r| look_as(ui, r, self.title)).unwrap_or_default();
        let p = ui.painter().clone();
        let fill = if l.pressed {
            t.c.control_pressed
        } else if l.hovered {
            t.c.control_hover
        } else {
            t.c.card
        };
        p.rect_filled(rect, radius::CARD, fill);
        p.rect_stroke(rect.shrink(0.5), radius::CARD, t.card_stroke());
        let inner = Rect::from_min_max(rect.min + Vec2::new(pad.left, pad.top), rect.max - Vec2::new(pad.right, pad.bottom));
        if let Some(icon) = self.icon {
            let r = Rect::from_center_size(egui::pos2(inner.left() + 16.0, inner.center().y), Vec2::splat(20.0));
            icon.paint(&p, r, t.c.text);
        }
        let tx = inner.left() + icon_w;
        let ty = inner.center().y - text_h / 2.0;
        let title_h = title.size().y;
        p.galley(egui::pos2(tx, ty), title, t.c.text);
        if let Some(d) = desc {
            p.galley(egui::pos2(tx, ty + title_h + 2.0), d, t.c.text_secondary);
        }
        if self.opens_page {
            let r = Rect::from_center_size(egui::pos2(inner.right() - 10.0, inner.center().y), Vec2::splat(16.0));
            Icon::ChevronRight.paint(&p, r, t.c.text_secondary);
        }
        // The control lays out at its own width, right to left, in up to
        // three fifths of the row; the words get the rest (from the next
        // pass on, when the width is new).
        let control_room = ((inner.width() - icon_w - chevron_w) * 0.6).max(0.0);
        let control_rect =
            Rect::from_min_max(egui::pos2(inner.right() - chevron_w - control_room, inner.top()), egui::pos2(inner.right() - chevron_w, inner.bottom()));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(control_rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
        let out = control(&mut child);
        let used = child.min_rect().width();
        let used = if used.is_finite() && child.min_rect().is_positive() { used } else { 0.0 };
        if (used - control_w).abs() > 0.5 {
            ui.ctx().data_mut(|d| d.insert_temp(id.with("control-w"), used));
            ui.ctx().request_discard("a setting's control changed width");
        }
        if l.focused {
            focus_ring(&p, rect, radius::CARD, true, &t);
        }
        SettingResponse { opened: resp.is_some_and(|r| r.clicked()), inner: out }
    }
}
