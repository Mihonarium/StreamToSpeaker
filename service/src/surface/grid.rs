//! All dimensions are in logical points, relative to the inner border.
use crate::ui::theme::{radius, size, space};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{pos2, vec2, Color32, FontId, Galley, Pos2, Rect, Sense, Ui};
use std::sync::Arc;

/// Inside the flyout’s one-point border (384 points across the frame).
pub const WIDTH: f32 = 382.0;
pub const LEFT: f32 = space::L;
pub const ICON: f32 = space::XL;
pub const TEXT: f32 = 46.0;
pub const END: f32 = 334.0;
pub const ACTION: f32 = 358.0;
pub const RIGHT: f32 = WIDTH - LEFT;
pub const INSET: f32 = space::XS;
pub const EDGE: f32 = WIDTH - INSET;
pub const PLACE: f32 = 116.0;
pub const ABOUT: f32 = 192.0;
pub const FOOT: f32 = 44.0;
pub const RULE: f32 = 1.0;
pub const ROUND: f32 = radius::CONTROL;
pub const GAP: f32 = space::S;
pub const GUTTER: f32 = space::M;
pub const TIGHT: f32 = 6.0;
pub const NEAR: f32 = 2.0;
pub const MEDIUM: f32 = 10.0;
pub const BODY: f32 = size::BODY;
pub const SMALL: f32 = size::SECONDARY;
pub const CAPTION: f32 = size::CAPTION;
pub const TITLE: f32 = size::DIALOG;
pub const BODY_LINE: f32 = 20.0;
pub const SMALL_LINE: f32 = 18.0;
pub const CAP_LINE: f32 = 16.0;
pub const TITLE_LINE: f32 = 26.0;
pub const TITLE_TOP: f32 = LEFT;
pub const STATE_TOP: f32 = TITLE_TOP + TITLE_LINE + NEAR;
pub const VOLUME_TOP: f32 = STATE_TOP + BODY_LINE + GAP;
pub const OUTPUT_MIN: f32 = 80.0;
pub const HIT: f32 = space::BUTTON_HEIGHT;
pub const LINK_HIT: f32 = space::XL;
pub const GLYPH: f32 = BODY_LINE;
pub const MARK: f32 = BODY;
pub const GEAR: f32 = 15.0;
pub const RADIO: f32 = CAP_LINE;
pub const ROW: f32 = 40.0;
pub const SHORT_ROW: f32 = HIT;
pub const NAME_MIN: f32 = 72.0;
pub const BAR: f32 = 3.0;
pub const MIN_SCROLL: f32 = 2.5 * ROW;
pub const METER_BAR: f32 = 2.5;
pub const WAY: f32 = 22.0;
pub const WAY_NAME_MIN: f32 = 50.0;
pub const VERB_END: f32 = 236.0;
pub const AMOUNT: f32 = 56.0;
pub const LARGE: f32 = RIGHT - AMOUNT;
pub const SMALL_AMOUNT: f32 = LARGE - TIGHT - AMOUNT;
pub const AMOUNT_H: f32 = space::SMALL_BUTTON_HEIGHT;
pub const NOTE_PAD: f32 = 13.0;
pub const NOTE_END: f32 = RIGHT - RULE;
pub const LINE_TOP: f32 = 14.0;
pub const MARGIN: f32 = size::DIALOG;

pub(super) fn rect(origin: Pos2, x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::from_min_size(
        pos2((origin.x + x).round(), (origin.y + y).round()),
        vec2(w.max(0.0), h),
    )
}
pub(super) fn square(center: Pos2, side: f32) -> Rect {
    rect(center, -side / 2.0, -side / 2.0, side, side)
}
pub(super) fn job(words: &str, font: FontId, color: Color32) -> LayoutJob {
    LayoutJob::simple_singleline(words.into(), font, color)
}
pub(super) fn one(ui: &Ui, mut text: LayoutJob, width: f32) -> Arc<Galley> {
    text.wrap = TextWrapping::truncate_at_width(width.max(0.0));
    ui.fonts(|f| f.layout_job(text))
}
pub(super) fn width(ui: &Ui, words: &str, font: FontId) -> f32 {
    ui.fonts(|f| {
        f.layout_no_wrap(words.to_owned(), font, Color32::PLACEHOLDER)
            .size()
            .x
            .ceil()
    })
}
pub(super) fn wrapped(
    ui: &Ui,
    mut job: LayoutJob,
    width: f32,
    height: f32,
    balanced: bool,
) -> Arc<Galley> {
    job.break_on_newline = true;
    for section in &mut job.sections {
        section.format.line_height = Some(height);
    }
    let layout = |mut job: LayoutJob, w| {
        job.wrap.max_width = w;
        ui.fonts(|f| f.layout_job(job))
    };
    let first = layout(job.clone(), width);
    let count = first.rows.len();
    if count <= 1 {
        return first;
    }
    let orphan = |g: &Galley| {
        g.rows
            .last()
            .is_some_and(|r| r.text().split_whitespace().count() == 1)
    };
    if orphan(&first) && crate::ui::text::on_page(ui) {
        return crate::ui::text::page_wrap(ui, first);
    }
    if !balanced && !orphan(&first) {
        return first;
    }
    // A balanced paragraph uses the narrowest measure with the original row count.
    if balanced {
        let (mut lo, mut hi) = (1_u32, width.ceil() as u32);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if layout(job.clone(), mid as f32).rows.len() > count {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // At a word boundary, a slightly wider measure can finish the
        // same number of lines without leaving a word alone.
        for w in lo..=width.ceil() as u32 {
            let candidate = layout(job.clone(), w as f32);
            if !orphan(&candidate) {
                return candidate;
            }
        }
    }
    for w in ((width * 0.6).ceil() as u32..width.ceil() as u32).rev() {
        let candidate = layout(job.clone(), w as f32);
        if !orphan(&candidate) {
            return candidate;
        }
    }
    first
}
/// Page text uses the same word wrapping as the panel's paragraphs.
pub(crate) fn page_paragraph(ui: &mut Ui, words: &str, font: FontId, color: Color32) {
    let height = ui.fonts(|f| f.row_height(&font));
    let galley = wrapped(ui, job(words, font, color), ui.available_width(), height, false);
    ui.label(galley);
}
pub(super) fn figured(
    f: &crate::model::panel::Figured,
    regular: FontId,
    emph: FontId,
    color: Color32,
    emphasis: Color32,
) -> LayoutJob {
    let mut j = LayoutJob::default();
    for (s, font, ink) in [
        (&f.before, &regular, color),
        (&f.figure, &emph, emphasis),
        (&f.after, &regular, color),
    ] {
        j.append(s, 0.0, TextFormat::simple(font.clone(), ink));
    }
    j
}
/// Paint at a left edge, vertically aligned inside the line box.
pub(super) fn paint(ui: &mut Ui, line: Rect, g: Arc<Galley>) {
    let pos = ui
        .painter()
        .round_pos_to_pixels(pos2(line.left(), line.center().y - g.size().y / 2.0));
    if g.elided {
        let id = ui.next_auto_id();
        ui.skip_ahead_auto_ids(1);
        ui.interact(Rect::from_min_size(pos, g.size()), id, Sense::hover())
            .on_hover_text(g.text());
    }
    ui.painter().galley(pos, g, Color32::PLACEHOLDER);
}
pub(super) fn label(ui: &mut Ui, line: Rect, words: &str, font: FontId, color: Color32) -> f32 {
    let g = one(ui, job(words, font, color), line.width());
    let w = g.size().x.ceil();
    paint(ui, line, g);
    w
}
pub(super) fn line(origin: Pos2, x: f32, end: f32, cy: f32, h: f32) -> Rect {
    rect(origin, x, cy - h / 2.0, end - x, h)
}
pub(super) fn animate(ui: &Ui) -> f64 {
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(50));
    ui.input(|i| i.time)
}
pub(super) fn spinner(ui: &Ui, center: Pos2, color: Color32) {
    let r = square(center, MARK);
    if ui.is_rect_visible(r) {
        let track = crate::ui::theme::Theme::of(ui.ctx()).c.control_border;
        crate::ui::icon::paint_spinner_on_track(ui.painter(), r, color, track, animate(ui));
    }
}
pub(super) fn mark(
    ui: &Ui,
    m: crate::model::panel::Mark,
    center: Pos2,
    side: f32,
    t: &crate::ui::theme::Theme,
) {
    use crate::model::panel::Mark;
    use crate::ui::icon::Icon;
    match m {
        Mark::Busy => spinner(ui, center, t.c.accent),
        Mark::Warn => {
            Icon::Warning.paint_badge(ui.painter(), square(center, side), t.c.amber, t.c.card)
        }
        Mark::Info => {
            Icon::Info.paint_badge(ui.painter(), square(center, side), t.c.accent, t.c.card)
        }
    }
}
