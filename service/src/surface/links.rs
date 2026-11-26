use super::{
    grid::*,
    panel::{hit, name, Drawn},
};
use crate::model::panel::Act;
use crate::ui::{
    look::look_as,
    theme::{focus_ring, Theme},
};
use egui::{Rect, Stroke, Ui, WidgetInfo, WidgetType};

pub(super) fn link(
    ui: &mut Ui,
    words: Rect,
    pad: f32,
    a: &Act,
    em: f32,
    t: &Theme,
    out: &mut Drawn,
) {
    let r = words.expand2(egui::vec2(pad, 0.0));
    let resp = hit(ui, r, true);
    draw_link(ui, resp, words, a, em, t, out);
}

pub(super) fn draw_link(
    ui: &mut Ui,
    resp: egui::Response,
    words: Rect,
    a: &Act,
    em: f32,
    t: &Theme,
    out: &mut Drawn,
) {
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Link, true, name(a)));
    let l = look_as(ui, &resp, name(a));
    let color = if l.hovered || l.pressed {
        t.c.accent_strong
    } else {
        t.c.accent
    };
    let g = one(ui, job(&a.label, t.font(em), color), words.width());
    let bounds = line(words.min, 0.0, g.size().x, words.height() / 2.0, g.size().y);
    paint(ui, words, g);
    if l.hovered {
        ui.painter().hline(
            bounds.x_range(),
            bounds.bottom() - NEAR,
            Stroke::new(RULE, color),
        );
    }
    if l.focused {
        focus_ring(ui.painter(), bounds, ROUND, true, t);
    }
    out.record(ui, &resp, name(a), true, Some(&a.command));
}

pub(super) fn footer(ui: &mut Ui, r: Rect, acts: &[Act; 3], t: &Theme, out: &mut Drawn) {
    let [w0, w1, w2] = acts.each_ref().map(|a| width(ui, &a.label, t.font(SMALL)));
    let w2 = w2.min((RIGHT - LEFT) / 2.0);
    let end = RIGHT - w2;
    let room = end - LEFT - BODY_LINE - 2.0 * GAP;
    let w0 = w0.min((room / 2.0).floor().max(room - w1));
    let x1 = LEFT + w0 + BODY_LINE;
    // Keep the third action anchored even with translated labels.
    for (a, (x, w)) in acts
        .iter()
        .zip([(LEFT, w0), (x1, w1.min(end - x1 - 2.0 * GAP)), (end, w2)])
    {
        link(
            ui,
            line(r.min, x, x + w, FOOT / 2.0, HIT),
            GAP,
            a,
            SMALL,
            t,
            out,
        );
    }
}
