//! Paint a model panel at the cursor. Only the list changes height under a cap.
pub use super::grid::WIDTH;
use super::{about, grid::*, links, list, place};
use crate::model::{
    panel::{Act, Panel},
    Command, VolumeOwner,
};
use crate::ui::{
    button::Button,
    icon::Icon,
    inside,
    look::look_as_exact,
    theme::{focus_ring, Theme},
};
use egui::{vec2, Color32, Rect, Response, Sense, Ui, WidgetInfo, WidgetType};

/// Presentation state retained by the open panel.
#[derive(Clone, Debug, Default)]
pub struct PanelUi {
    /// Show the speakers inside the fold.
    pub more_open: bool,
    pub(super) keyboard: list::Keyboard,
    /// Last volume requested while waiting for the model to acknowledge it.
    pub(super) pending: Option<(VolumeOwner, u8)>,
}

impl PanelUi {
    /// On the next frame, focus the current place (or This PC when none is marked).
    pub fn focus_place(&mut self) {
        self.keyboard.focus_place();
    }
}

/// An action in reading order. Its rectangle is clipped to the visible list.
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub rect: Rect,
    pub name: String,
    /// Disabled actions, the fold and the slider have no fixed command.
    pub command: Option<Command>,
    pub enabled: bool,
}
/// The commands, measured sections and visible hit targets of this frame.
pub struct Drawn {
    pub commands: Vec<Command>,
    pub natural_height: f32,
    /// Place, list, detail and footer, in screen coordinates.
    pub parts: [Rect; 4],
    pub controls: Vec<Control>,
}
pub(super) fn name(a: &Act) -> &str {
    a.spoken.as_deref().unwrap_or(&a.label)
}
impl Drawn {
    pub(super) fn record(
        &mut self,
        ui: &Ui,
        r: &Response,
        name: &str,
        enabled: bool,
        command: Option<&Command>,
    ) {
        if r.gained_focus() && !ui.clip_rect().contains_rect(r.rect) {
            r.scroll_to_me_animation(None, egui::style::ScrollAnimation::none());
        }
        let rect = r.rect.intersect(ui.clip_rect());
        if rect.is_positive() {
            self.controls.push(Control {
                rect,
                name: name.into(),
                enabled,
                command: command.filter(|_| enabled).cloned(),
            });
        }
        if enabled && r.clicked() {
            self.commands.extend(command.cloned());
        }
    }
    pub(super) fn button(&mut self, ui: &mut Ui, r: Rect, a: &Act, enabled: bool, small: bool) {
        let text_room = r.width();
        let r = ui.painter().round_rect_to_pixels(r);
        let mut b = Button::standard(&a.label)
            .width(r.width())
            .max_width(text_room)
            .enabled(enabled)
            .accessible_name(name(a));
        b = if small {
            b.small().grid_padding(INSET)
        } else {
            b.text_size(BODY).grid_padding(GUTTER)
        };
        let resp = inside(ui, r, |u| b.show(u));
        self.record(ui, &resp, name(a), enabled, Some(&a.command));
    }
    pub(super) fn icon(
        &mut self,
        ui: &mut Ui,
        resp: Response,
        a: &Act,
        glyph: Icon,
        side: f32,
        ink: Color32,
        t: &Theme,
    ) {
        let r = ui.painter().round_rect_to_pixels(resp.rect);
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &a.label));
        let l = look_as_exact(ui, &resp, &a.label);
        if l.hovered || l.pressed {
            ui.painter()
                .rect_filled(r, ROUND, if l.pressed { t.c.pressed } else { t.c.hover });
        }
        let glyph_rect = square(r.center(), side);
        if glyph == Icon::Gear {
            Icon::paint_cog(
                ui.painter(),
                glyph_rect,
                if l.hovered { t.c.text_secondary } else { ink },
            );
        } else {
            glyph.paint(ui.painter(), glyph_rect, ink);
        }
        if l.focused {
            focus_ring(ui.painter(), r, ROUND, true, t);
        }
        self.record(ui, &resp, &a.label, true, Some(&a.command));
        resp.on_hover_text(&a.label);
    }
}
pub(super) fn hit(ui: &mut Ui, r: Rect, enabled: bool) -> Response {
    let id = ui.next_auto_id();
    ui.skip_ahead_auto_ids(1);
    ui.interact(
        r,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    )
}

/// The panel at full height and with its list scrolled down to its minimum.
pub fn heights(ui: &Ui, panel: &Panel, state: &PanelUi) -> (f32, f32) {
    let full = list::height(&panel.list, state.more_open);
    let fixed = PLACE + about::Layout::new(ui, &panel.about, &Theme::of(ui.ctx())).height() + FOOT + 3.0 * RULE;
    (fixed + full, fixed + full.min(ROW + MIN_SCROLL + 2.0 * TIGHT))
}

/// Draw the same delay rows inside a page that already supplies the side margins.
pub(crate) fn page_delay(ui: &mut Ui, about: &crate::model::panel::About) -> Vec<Command> {
    let theme = Theme::of(ui.ctx());
    let layout = about::Layout::new(ui, about, &theme);
    let (inner, _) = ui.allocate_exact_size(vec2(ui.available_width(), layout.content_height()), Sense::hover());
    let mut out = Drawn { commands: Vec::new(), controls: Vec::new(), parts: [Rect::NOTHING; 4], natural_height: inner.height() };
    layout.draw(ui, inner.expand2(vec2(LEFT, 0.0)), &theme, &mut out);
    out.commands
}

/// Draw at the cursor. Only the speaker list scrolls under a height limit.
pub fn show(ui: &mut Ui, panel: &Panel, state: &mut PanelUi, max_height: f32) -> Drawn {
    crate::ui::look::follow_input(ui.ctx());
    let t = Theme::of(ui.ctx());
    let detail = about::Layout::new(ui, &panel.about, &t);
    let full_list = list::height(&panel.list, state.more_open);
    let fixed = PLACE + detail.height() + FOOT + 3.0 * RULE;
    let shown = full_list.min(
        (max_height - fixed)
            .floor()
            .max(ROW + MIN_SCROLL + 2.0 * TIGHT),
    );
    let heights = [PLACE, shown, detail.height(), FOOT];
    let (outer, _) = ui.allocate_exact_size(vec2(WIDTH, fixed + shown), Sense::hover());
    let mut out = Drawn {
        commands: vec![],
        natural_height: fixed + full_list,
        parts: [Rect::NOTHING; 4],
        controls: vec![],
    };
    let mut y = 0.0;
    for (i, h) in heights.into_iter().enumerate() {
        let r = rect(outer.min, 0.0, y, WIDTH, h);
        let mut part = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("flyout", i))
                .max_rect(r)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        let (r, _) = part.allocate_exact_size(r.size(), Sense::hover());
        out.parts[i] = r;
        let mut corners = egui::Rounding::ZERO;
        if i == 0 {
            corners.nw = GAP;
            corners.ne = GAP;
        }
        if i == 3 {
            corners.sw = GAP;
            corners.se = GAP;
        }
        part.painter().rect_filled(
            part.painter().round_rect_to_pixels(r),
            corners,
            if i < 2 { t.c.surface } else { t.c.chrome },
        );
        match i {
            0 => place::draw(&mut part, r, &panel.place, state, &t, &mut out),
            1 => list::draw(&mut part, r, &panel.list, state, &t, &mut out),
            2 => detail.draw(&mut part, r, &t, &mut out),
            _ => links::footer(&mut part, r, &panel.links, &t, &mut out),
        }
        y += h;
        if i < 3 {
            ui.painter().rect_filled(
                ui.painter()
                    .round_rect_to_pixels(rect(outer.min, 0.0, y, WIDTH, RULE)),
                0.0,
                t.c.divider,
            );
            y += RULE;
        }
    }
    out
}
