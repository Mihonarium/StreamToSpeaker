//! A paragraph that can start with a mark (an icon), lead with a few
//! semibold words and end in links that continue the sentence, wrapping
//! together like the words of a sentence do: *Office is about 2 s late on
//! Steady. For video, use Short delay (about 0.3 s). Use Short delay*.

use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Galley, Rect, Response, Sense, Stroke, Ui, Vec2, WidgetInfo, WidgetType};

use super::icon::{paint_spinner, Icon};
use super::look::look_as;
use super::theme::{focus_ring, size, Theme};

pub(crate) const PAGE_TEXT: &str = "panel-page-text";

pub(crate) fn on_page(ui: &Ui) -> bool {
    ui.stack().iter().any(|s| s.tags().contains(PAGE_TEXT))
}

/// Reflow a lone final word within three quarters of the original width
/// and at most one extra line. Keep the original if no such wrap fits.
pub(crate) fn page_wrap(ui: &Ui, original: Arc<Galley>) -> Arc<Galley> {
    let [.., previous, last] = original.rows.as_slice() else { return original };
    if previous.ends_with_newline || !on_page(ui) || last.text().split_whitespace().count() != 1 {
        return original;
    }

    let lines = original.rows.len();
    // egui caches wrap widths in whole points. Round up to respect the lower bound.
    let min_width = (original.job.wrap.max_width * 0.75).ceil();
    let mut candidate = original.clone();
    while candidate.job.wrap.max_width > min_width {
        let mut job = (*original.job).clone();
        // Skip the unused space to the right of the longest line.
        let used = candidate.size().x.min(candidate.job.wrap.max_width);
        job.wrap.max_width = (used.ceil() - 1.0).max(min_width);
        candidate = ui.fonts(|f| f.layout_job(job));
        if candidate.rows.len() > lines + 1 {
            break;
        }
        if candidate.rows.len() >= lines && candidate.rows.last().unwrap().text().split_whitespace().count() >= 2 {
            return candidate;
        }
    }
    original
}

/// Keep native label and link interaction when reflowing their text.
pub(crate) fn page_widget_text(ui: &Ui, text: egui::RichText) -> egui::WidgetText {
    let text = egui::WidgetText::from(text);
    if !on_page(ui) { return text; }
    let galley = text.into_galley(ui, Some(egui::TextWrapMode::Wrap), ui.available_width(), egui::FontSelection::Default);
    page_wrap(ui, galley).into()
}

/// The icon before a line, which says what kind of line it is by its shape
/// as well as its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    None,
    /// Something worth knowing (accent circle with "i").
    Info,
    /// Needs a look (amber triangle with "!").
    Warning,
    /// Not working (red circle with "×").
    Error,
    /// Done (green tick).
    Done,
    /// Something is happening now (a turning ring).
    Busy,
    /// Lab settings (amber flask).
    Lab,
    /// A request from the people who make the app (a person).
    Person,
}

impl Mark {
    /// Paint the mark in the square `rect`.
    pub fn paint(self, ui: &Ui, rect: Rect) {
        let t = Theme::of(ui.ctx());
        let p = ui.painter();
        match self {
            Mark::None => {}
            Mark::Info => Icon::Info.paint_weight(p, rect, t.c.accent, 1.6),
            Mark::Warning => Icon::Warning.paint_weight(p, rect, t.c.amber, 1.6),
            Mark::Error => Icon::ErrorCross.paint_weight(p, rect, t.c.red, 1.7),
            Mark::Done => Icon::Check.paint_weight(p, rect, t.c.green, 2.0),
            Mark::Busy => paint_spinner(p, rect.shrink(rect.width() * 0.08), t.c.accent, ui.input(|i| i.time)),
            Mark::Lab => Icon::Flask.paint_weight(p, rect, t.c.amber, 1.6),
            Mark::Person => Icon::User.paint(p, rect, t.c.text_secondary),
        }
    }
}

/// The colour of a paragraph's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Second-line grey: notes, descriptions.
    Secondary,
    /// Main text colour.
    Primary,
    /// A warning that is true now.
    Amber,
    /// An error.
    Red,
}

impl Tone {
    pub fn color(self, t: &Theme) -> Color32 {
        match self {
            Tone::Secondary => t.c.text_secondary,
            Tone::Primary => t.c.text,
            Tone::Amber => t.c.amber,
            Tone::Red => t.c.red,
        }
    }
}

/// What was clicked in a paragraph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParaClick {
    /// The link at this index, in the order they were added.
    Link(usize),
    /// The closing ×.
    Dismiss,
}

#[must_use = "call .show(ui)"]
pub struct Para<'a> {
    mark: Mark,
    lead: Option<&'a str>,
    text: &'a str,
    links: Vec<&'a str>,
    dismiss: Option<&'a str>,
    size: f32,
    tone: Tone,
}

impl<'a> Para<'a> {
    pub fn new(text: &'a str) -> Self {
        Para { mark: Mark::None, lead: None, text, links: Vec::new(), dismiss: None, size: size::CAPTION, tone: Tone::Secondary }
    }

    pub fn mark(mut self, mark: Mark) -> Self {
        self.mark = mark;
        self
    }

    /// Semibold words before the text, in the main text colour.
    pub fn lead(mut self, lead: &'a str) -> Self {
        self.lead = Some(lead);
        self
    }

    /// A link after the text (several follow each other).
    pub fn link(mut self, link: &'a str) -> Self {
        self.links.push(link);
        self
    }

    /// A small × at the end that closes the line; `name` is what a screen
    /// reader says (*Close this message*).
    pub fn dismiss(mut self, name: &'a str) -> Self {
        self.dismiss = Some(name);
        self
    }

    /// The words' em size in pixels at 100 % (12 by default).
    pub fn size(mut self, em_px: f32) -> Self {
        self.size = em_px;
        self
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Option<ParaClick> {
        let t = Theme::of(ui.ctx());
        let font = t.font(self.size);
        let row_h = font.size;
        let mark_side = (self.size + 2.0).round();
        let gap = if self.size >= size::SMALL { 8.0 } else { 6.0 };
        let width = ui.available_width();
        let indent = if self.mark == Mark::None { 0.0 } else { mark_side + gap };

        let mut job = LayoutJob { wrap: egui::text::TextWrapping { max_width: (width - indent).max(20.0), ..Default::default() }, ..Default::default() };
        let plain = TextFormat { font_id: font.clone(), color: self.tone.color(&t), ..Default::default() };
        let mut sections_of_links: Vec<usize> = Vec::new();
        if let Some(lead) = self.lead.filter(|l| !l.is_empty()) {
            job.append(lead, 0.0, TextFormat { font_id: t.strong(self.size), color: t.c.text, ..Default::default() });
            if !self.text.is_empty() {
                job.append(" ", 0.0, plain.clone());
            }
        }
        job.append(self.text, 0.0, plain.clone());
        // After a full sentence a link follows with a space; after a few
        // words (*50 ms removed*), and between links, with a middle dot.
        let words_before = if self.text.is_empty() { self.lead.unwrap_or("") } else { self.text };
        let ends_sentence = words_before.is_empty() || words_before.trim_end().ends_with(['.', '?', '!', ':']);
        for (i, link) in self.links.iter().enumerate() {
            let sep = if i == 0 && ends_sentence { " " } else { " · " };
            job.append(sep, 0.0, plain.clone());
            // The link never breaks inside: it moves to the next line whole.
            sections_of_links.push(job.sections.len());
            job.append(&link.replace(' ', "\u{a0}"), 0.0, TextFormat { font_id: font.clone(), color: t.c.accent, ..Default::default() });
        }
        let dismiss_section = self.dismiss.map(|_| {
            job.append("\u{a0}\u{a0}", 0.0, plain.clone());
            let i = job.sections.len();
            job.append("×", 0.0, TextFormat { font_id: font.clone(), color: t.c.text_secondary, ..Default::default() });
            i
        });
        let galley = page_wrap(ui, ui.fonts(|f| f.layout_job(job)));
        let text_h = galley.size().y.max(row_h);
        let (rect, alloc) = ui.allocate_exact_size(Vec2::new(width, text_h), Sense::hover());
        let origin = rect.min + Vec2::new(indent, 0.0);
        if self.mark != Mark::None {
            let m = Rect::from_center_size(rect.min + Vec2::new(mark_side / 2.0, row_h / 2.0), Vec2::splat(mark_side));
            self.mark.paint(ui, m);
        }

        // Each link's area, row by row, to click and underline.
        let mut clicked = None;
        let mut underlines: Vec<(Rect, Color32)> = Vec::new();
        let mut targets: Vec<(usize, Option<ParaClick>)> = sections_of_links
            .iter()
            .enumerate()
            .map(|(i, s)| (*s, Some(ParaClick::Link(i))))
            .collect();
        if let Some(s) = dismiss_section {
            targets.push((s, Some(ParaClick::Dismiss)));
        }
        for (section, what) in targets {
            let mut area = Rect::NOTHING;
            for row in &galley.rows {
                for g in &row.glyphs {
                    if g.section_index as usize == section {
                        let r = Rect::from_min_size(origin + Vec2::new(g.pos.x, row.rect.min.y), Vec2::new(g.advance_width, row.rect.height()));
                        area = area.union(r);
                    }
                }
            }
            if !area.is_positive() {
                continue;
            }
            let id = alloc.id.with(("link", section));
            let target = if what == Some(ParaClick::Dismiss) { area.expand2(Vec2::new(4.0, 0.0)) } else { area };
            let resp: Response = ui.interact(target, id, Sense::click());
            let name = match what {
                Some(ParaClick::Dismiss) => self.dismiss.unwrap_or("Close").to_owned(),
                Some(ParaClick::Link(i)) => self.links[i].to_owned(),
                None => String::new(),
            };
            resp.widget_info(|| WidgetInfo::labeled(if what == Some(ParaClick::Dismiss) { WidgetType::Button } else { WidgetType::Link }, true, &name));
            let l = look_as(ui, &resp, &name);
            if l.hovered || l.pressed {
                let color = if what == Some(ParaClick::Dismiss) { t.c.text } else { t.c.accent_strong };
                underlines.push((area, color));
            }
            if l.focused {
                focus_ring(ui.painter(), area.expand2(Vec2::new(2.0, 0.0)), 2.0, true, &t);
            }
            if resp.clicked() {
                clicked = what;
            }
        }
        ui.painter().galley(origin, galley, self.tone.color(&t));
        for (area, color) in underlines {
            let y = area.max.y - 1.5;
            ui.painter().hline(area.x_range(), y, Stroke::new(1.0_f32, color));
        }
        clicked
    }
}
