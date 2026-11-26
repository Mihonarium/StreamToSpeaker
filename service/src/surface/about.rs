use super::{
    grid::*,
    links::link,
    panel::{hit, Drawn},
};
use crate::{
    model::panel::*,
    ui::{
        icon::Icon,
        look::look_as,
        theme::{focus_ring, Theme},
    },
};
use egui::{vec2, Color32, Galley, Pos2, Rect, Stroke, Ui, WidgetInfo, WidgetType};
use std::sync::Arc;

/// Measure wrapped paragraphs once, using the same galleys for paint and height.
pub(super) enum Layout<'a> {
    Delay {
        d: &'a DelayControls,
        head: Arc<Galley>,
        table_y: f32,
        tail: Option<Arc<Galley>>,
    },
    Box {
        mark: Mark,
        title: Arc<Galley>,
        sentence: Arc<Galley>,
        first: Option<&'a Act>,
        second: Option<&'a Act>,
        close: Option<&'a Act>,
        h: f32,
    },
    Line(Arc<Galley>, Option<&'a Act>),
    Empty,
}
fn ink(dim: bool, normal: Color32, t: &Theme) -> Color32 {
    if dim {
        t.c.text_disabled
    } else {
        normal
    }
}
const TABLE_H: f32 = AMOUNT_H * 2.0 + GUTTER;

impl<'a> Layout<'a> {
    pub(super) fn new(ui: &Ui, about: &'a About, t: &Theme) -> Self {
        match about {
            About::Empty => Self::Empty,
            About::Line { text, link } => Self::Line(
                wrapped(
                    ui,
                    job(text, t.font(SMALL), t.c.text_secondary),
                    END - TEXT,
                    SMALL_LINE,
                    false,
                ),
                link.as_ref(),
            ),
            About::Note(n) => Self::box_layout(
                ui,
                n.mark,
                &n.title,
                &n.sentence,
                n.button.as_ref(),
                None,
                n.close.as_ref(),
                t,
            ),
            About::Question(q) => Self::box_layout(
                ui,
                Mark::Info,
                &q.title,
                &q.sentence,
                Some(&q.yes),
                Some(&q.no),
                None,
                t,
            ),
            About::Delay(d) => {
                let (head, head_end) = match &d.head {
                    DelayHead::One(f) => {
                        let c = ink(d.dimmed, t.c.text, t);
                        let g = wrapped(
                            ui,
                            figured(f, t.font(SMALL), t.strong(SMALL), c, c),
                            RIGHT - LEFT,
                            SMALL_LINE,
                            true,
                        );
                        let end = GUTTER + g.size().y;
                        (g, end)
                    }
                    DelayHead::Ways { label, lines } => {
                        let head = wrapped(ui, job(label, t.font(SMALL), ink(d.dimmed, t.c.text_secondary, t)), RIGHT - LEFT, SMALL_LINE, false);
                        let end = GUTTER + head.size().y + INSET + WAY * lines.len() as f32;
                        (head, end)
                    }
                };
                let tail = d.line.as_ref().map(|s| {
                    wrapped(
                        ui,
                        job(s, t.font(CAPTION), ink(d.dimmed, t.c.text_secondary, t)),
                        RIGHT - LEFT,
                        CAP_LINE,
                        false,
                    )
                });
                Self::Delay {
                    d,
                    head,
                    table_y: head_end + MEDIUM,
                    tail,
                }
            }
        }
    }
    fn box_layout(
        ui: &Ui,
        mark: Mark,
        title: &str,
        sentence: &str,
        first: Option<&'a Act>,
        second: Option<&'a Act>,
        close: Option<&'a Act>,
        t: &Theme,
    ) -> Self {
        let title = wrapped(
            ui,
            job(title, t.strong(BODY), t.c.text),
            if close.is_some() {
                END - TEXT - GAP
            } else {
                NOTE_END - TEXT
            },
            BODY_LINE,
            true,
        );
        let sentence = wrapped(
            ui,
            job(sentence, t.font(BODY), t.c.text_secondary),
            NOTE_END - TEXT,
            BODY_LINE,
            false,
        );
        let h = 2.0 * NOTE_PAD
            + title.size().y
            + NEAR
            + sentence.size().y
            + if first.is_some() { MEDIUM + HIT } else { 0.0 };
        Self::Box {
            mark,
            title,
            sentence,
            first,
            second,
            close,
            h,
        }
    }
    pub(super) fn height(&self) -> f32 {
        ABOUT.max(self.content_height())
    }
    pub(super) fn content_height(&self) -> f32 {
        (match self {
                Self::Box { h, .. } => h + 2.0 * GAP,
                Self::Line(g, a) => {
                    LINE_TOP + g.size().y + if a.is_some() { GAP + LINK_HIT } else { 0.0 } + GUTTER
                }
                Self::Delay { table_y, tail, .. } => {
                    table_y + TABLE_H + tail.as_ref().map_or(0.0, |g| GAP + g.size().y) + GUTTER
                }
                Self::Empty => 0.0,
            })
            .ceil()
    }
    pub(super) fn draw(&self, ui: &mut Ui, r: Rect, t: &Theme, out: &mut Drawn) {
        let o = r.min;
        match self {
            Self::Empty => {}
            Self::Line(g, a) => {
                paragraph(ui, o, TEXT, LINE_TOP, g);
                if let Some(a) = a {
                    let w = width(ui, &a.label, t.font(SMALL)).min(END - TEXT);
                    link(
                        ui,
                        line(
                            o,
                            TEXT,
                            TEXT + w,
                            LINE_TOP + g.size().y + GAP + SMALL_LINE / 2.0,
                            LINK_HIT,
                        ),
                        0.0,
                        a,
                        SMALL,
                        t,
                        out,
                    );
                }
            }
            Self::Box {
                mark: m,
                title,
                sentence,
                first,
                second,
                close,
                h,
            } => {
                // A note is a self-contained box: it stands in the middle of
                // its part.
                let top = ((r.height() - h) / 2.0).round();
                let b = ui
                    .painter()
                    .round_rect_to_pixels(rect(o, INSET, top, EDGE - INSET, *h));
                let (fill, edge) = if *m == Mark::Warn {
                    (t.c.amber_bg, t.c.amber.gamma_multiply(0.16))
                } else {
                    (t.c.card, t.c.card_border)
                };
                let stroke = ui.painter().round_to_pixel(RULE);
                ui.painter().rect(
                    b.shrink(stroke / 2.0),
                    ROUND,
                    fill,
                    Stroke::new(stroke, edge),
                );
                let y = top + NOTE_PAD;
                let cy = y + BODY_LINE / 2.0;
                mark(ui, *m, o + vec2(ICON, cy), RADIO, t);
                paragraph(ui, o, TEXT, y, title);
                let y = y + title.size().y + NEAR;
                paragraph(ui, o, TEXT, y, sentence);
                if let Some(a) = first {
                    let w = |a: &Act| width(ui, &a.label, t.font(BODY)) + 2.0 * GUTTER;
                    let room = NOTE_END - TEXT;
                    let w_a = w(a);
                    let y = y + sentence.size().y + MEDIUM;
                    if let Some(b) = second {
                        let w_b = w(b);
                        let budget = room - GAP;
                        // Short labels keep their natural width; longer labels
                        // share the remaining room on this one line.
                        let first = w_a.min((budget / 2.0).floor().max(budget - w_b));
                        out.button(ui, rect(o, TEXT, y, first, HIT), a, true, false);
                        out.button(
                            ui,
                            rect(o, TEXT + first + GAP, y, w_b.min(budget - first), HIT),
                            b,
                            true,
                            false,
                        );
                    } else {
                        out.button(ui, rect(o, TEXT, y, w_a.min(room), HIT), a, true, false);
                    }
                }
                if let Some(a) = close {
                    let close = hit(ui, square(o + vec2(ACTION, cy), HIT), true);
                    out.icon(ui, close, a, Icon::Close, MARK, t.c.text_secondary, t);
                }
            }
            Self::Delay {
                d,
                head,
                table_y,
                tail,
            } => {
                match &d.head {
                    DelayHead::One(_) => paragraph(ui, o, LEFT, GUTTER, head),
                    DelayHead::Ways { lines, .. } => {
                        paint(
                            ui,
                            rect(o, LEFT, GUTTER, RIGHT - LEFT, head.size().y),
                            head.clone(),
                        );
                        let widest = lines
                            .iter()
                            .map(|l| width(ui, &l.name, t.font(SMALL)))
                            .fold(WAY_NAME_MIN, f32::max);
                        let x = (TEXT + widest + GAP).min(RIGHT - NAME_MIN);
                        for (i, w) in lines.iter().enumerate() {
                            let y = GUTTER + head.size().y + INSET + i as f32 * WAY;
                            way_line(ui, o, y, x, w, d.dimmed, t, out);
                        }
                    }
                }
                for (i, row) in d.rows.iter().enumerate() {
                    let y = table_y + i as f32 * (AMOUNT_H + GUTTER);
                    let cy = y + AMOUNT_H / 2.0;
                    let verb_w = width(ui, &row.verb, t.font(SMALL)).min(VERB_END - LEFT - GAP);
                    label(
                        ui,
                        line(o, LEFT, VERB_END - verb_w - GAP, cy, SMALL_LINE),
                        &row.question,
                        t.font(SMALL),
                        ink(d.dimmed, t.c.text_secondary, t),
                    );
                    label(
                        ui,
                        line(o, VERB_END - verb_w, VERB_END, cy, SMALL_LINE),
                        &row.verb,
                        t.font(SMALL),
                        ink(d.dimmed || !row.enabled, t.c.text, t),
                    );
                    for (x, a) in [(SMALL_AMOUNT, &row.small), (LARGE, &row.large)] {
                        out.button(
                            ui,
                            rect(o, x, y, AMOUNT, AMOUNT_H),
                            a,
                            row.enabled && !d.dimmed,
                            true,
                        );
                    }
                }
                if let Some(g) = tail {
                    paragraph(ui, o, LEFT, table_y + TABLE_H + GAP, g);
                }
            }
        }
    }
}
fn paragraph(ui: &mut Ui, o: Pos2, x: f32, y: f32, g: &Arc<Galley>) {
    paint(ui, rect(o, x, y, g.size().x, g.size().y), g.clone());
}
fn way_line(
    ui: &mut Ui,
    o: Pos2,
    y: f32,
    x: f32,
    w: &WayLine,
    dim: bool,
    t: &Theme,
    out: &mut Drawn,
) {
    let cy = y + WAY / 2.0;
    let link_w = w
        .connecting
        .as_ref()
        .map_or(0.0, |a| width(ui, &a.label, t.font(SMALL)) + GUTTER);
    let text = ink(dim, t.c.text_secondary, t);
    let (font, emph) = if w.chosen {
        (t.strong(SMALL), ink(dim, t.c.text, t))
    } else {
        (t.font(SMALL), text)
    };
    let g = one(
        ui,
        figured(&w.gives, t.font(SMALL), font, text, emph),
        RIGHT - x - link_w,
    );
    let end = x + g.size().x.ceil();
    // A pending way has an independent Cancel link. Its disabled radio
    // ends before that link, so the two hit targets remain unambiguous.
    let end_of_radio = if w.connecting.is_some() { end } else { RIGHT };
    let r = rect(o, LEFT, y, end_of_radio - LEFT, WAY);
    let enabled = w.command.is_some() && !dim;
    let resp = hit(ui, r, enabled);
    resp.widget_info(|| {
        WidgetInfo::selected(WidgetType::RadioButton, enabled, w.chosen, &w.spoken)
    });
    let l = look_as(ui, &resp, &w.spoken);
    out.record(ui, &resp, &w.spoken, enabled, w.command.as_ref());
    if l.focused {
        focus_ring(ui.painter(), r, ROUND, false, t);
    }
    let c = o + vec2(ICON, cy);
    if w.connecting.is_some() {
        spinner(ui, c, t.c.accent);
    } else if w.chosen {
        ui.painter()
            .circle_filled(c, RADIO / 2.0, ink(dim, t.c.accent, t));
        ui.painter().circle_filled(c, TIGHT / 2.0, t.c.on_accent);
    } else {
        let color = if enabled && (l.hovered || l.pressed) {
            t.c.text
        } else {
            ink(dim, t.c.control_strong, t)
        };
        ui.painter()
            .circle_stroke(c, (RADIO - RULE) / 2.0, Stroke::new(RULE, color));
    }
    label(
        ui,
        line(o, TEXT, x - GAP, cy, SMALL_LINE),
        &w.name,
        t.font(SMALL),
        ink(dim, t.c.text, t),
    );
    paint(ui, line(o, x, RIGHT - link_w, cy, SMALL_LINE), g);
    if let Some(a) = &w.connecting {
        link(
            ui,
            line(o, end + GUTTER, end + link_w, cy, LINK_HIT),
            0.0,
            a,
            SMALL,
            t,
            out,
        );
    }
}
