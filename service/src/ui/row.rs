//! A place in the list: This PC, a speaker, a group, Music Assistant.
//!
//! ```text
//! ▌ [icon]  Kitchen                                  Not found
//! ```
//!
//! One line, 40 points tall: the kind icon, the name (cut with … when it is
//! too long; the tooltip has all of it) and a short status at the right
//! end. A second line appears only while something needs the person: the
//! error after a click, with *Try again*. The kind and delay of the place
//! (*Sonos · about 0.2 s delay*) are its tooltip and what a reader of the
//! display hears after the name.
//!
//! [`ROW_STYLE`] selects the form: two lines, 52 points, with the kind and
//! delay always on the second line (the panel's), or one line of 40.

use egui::text::{LayoutJob, TextWrapping};
use egui::{Color32, Key, Rect, Sense, Ui, Vec2, WidgetInfo, WidgetType};

use super::button::{Button, IconButton};
use super::icon::{paint_spinner, Icon};
use super::look::look_as;
use super::text::Tone;
use super::theme::{focus_ring, radius, size, space, Theme};

/// How rows are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStyle {
    /// One line, 40 points; a second line only for a problem.
    OneLine,
    /// Two lines, 52 points; the second line is [`PlaceRow::about`] unless
    /// a problem replaces it.
    TwoLines,
}

/// The style every row is drawn in: the name, and under it the kind and
/// the delay, which tell two ways to one speaker apart.
pub const ROW_STYLE: RowStyle = RowStyle::TwoLines;

/// What sits at the right end of a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status<'a> {
    None,
    /// A few words: *Not found*, *In use*, *1 of 2 found*.
    Words(&'a str),
    /// Connecting: a turning ring.
    Busy,
    /// Something about this place needs a look: the amber icon. The words
    /// are elsewhere (the top of the panel, or the row's tooltip).
    Warning,
    /// A chevron that shows or hides rows under this one (the folded
    /// speakers).
    Expand { open: bool },
    /// A tick: the chosen one of a set of sub-rows.
    Check,
}

/// A second line, only while something needs the person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Problem<'a> {
    pub text: &'a str,
    pub tone: Tone,
    /// One button for it (*Try again*).
    pub button: Option<&'a str>,
}

/// Everything a row shows, as plain values.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaceRow<'a> {
    pub icon: Icon,
    pub name: &'a str,
    /// Kind and delay (*Sonos · about 0.2 s delay*): the tooltip, and what
    /// a reader of the display hears after the name.
    pub about: &'a str,
    pub status: Status<'a>,
    pub problem: Option<Problem<'a>>,
    /// The place the sound plays now: accent bar and tint.
    pub current: bool,
    /// Remembered but not found: drawn faint.
    pub dimmed: bool,
    /// Offer ⋯ (on pointer or keyboard focus) and the right-click menu.
    pub menu: bool,
    /// A sub-row, set in under its parent.
    pub indent: bool,
    /// What a reader of the display says; the name, the status and
    /// [`PlaceRow::about`] by default.
    pub spoken: Option<String>,
    /// Whether Tab stops on the row. A list makes one row the stop and
    /// moves between rows with the arrow keys.
    pub tab_stop: bool,
    /// The *›* at the right end that opens the place's own view: its
    /// tooltip (*Options for Kitchen*) and what a reader of the display
    /// says (*Options for Kitchen, Sonos*). The Right arrow on the row
    /// presses it too.
    pub options: Option<(&'a str, &'a str)>,
}

impl<'a> PlaceRow<'a> {
    pub fn new(icon: Icon, name: &'a str, about: &'a str) -> Self {
        PlaceRow {
            icon,
            name,
            about,
            status: Status::None,
            problem: None,
            current: false,
            dimmed: false,
            menu: false,
            indent: false,
            spoken: None,
            tab_stop: true,
            options: None,
        }
    }
}

/// What the person did with a row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowAction {
    /// Clicked it, or Enter on it.
    Choose,
    /// Asked for its menu: ⋯, right-click or Shift+F10. The menu opens at
    /// this point.
    Menu(egui::Pos2),
    /// The chevron.
    Expand,
    /// The problem's button.
    Button,
    /// The *›*, or the Right arrow on the row: the place's own view.
    Options,
}

const PAD_L: f32 = 14.0;
const PAD_R: f32 = 12.0;
const GAP: f32 = 12.0;
const INDENT: f32 = 30.0;

impl PlaceRow<'_> {
    pub fn show(&self, ui: &mut Ui) -> Option<RowAction> {
        self.show_in(ui, ROW_STYLE)
    }

    /// Draw the row in a given style (pictures show both).
    pub fn show_in(&self, ui: &mut Ui, style: RowStyle) -> Option<RowAction> {
        self.show_full(ui, style).0
    }

    /// Draw the row; what the person did, and the row's own response (for
    /// moving the keyboard focus between rows).
    pub fn show_full(&self, ui: &mut Ui, style: RowStyle) -> (Option<RowAction>, egui::Response) {
        let t = Theme::of(ui.ctx());
        let two = style == RowStyle::TwoLines;
        let full_w = ui.available_width();
        let indent = if self.indent { INDENT } else { 0.0 };
        let width = full_w - indent;
        let (icon_box, min_h) = if two { (24.0, space::ROW_HEIGHT) } else { (20.0, 40.0) };
        let fade = if self.dimmed { 0.5 } else { 1.0 };
        let name_color = t.c.text.gamma_multiply(fade);

        // The right end: its width first, so the words know their room.
        let status_galley = match self.status {
            Status::Words(w) => Some(ui.painter().layout_no_wrap(w.to_owned(), t.font(size::CAPTION), t.c.text_secondary)),
            _ => None,
        };
        let button_w = self.problem.and_then(|p| p.button).map(|b| {
            ui.painter().layout_no_wrap(b.to_owned(), t.font(size::SECONDARY), Color32::PLACEHOLDER).size().x + 20.0
        });
        let more_w = if self.menu && self.options.is_none() { 28.0 } else { 0.0 };
        // The › sits at the very end, the status just before it.
        let options_w = if self.options.is_some() { 32.0 } else { 0.0 };
        let status_w = match self.status {
            Status::None => 0.0,
            Status::Words(_) => status_galley.as_ref().map_or(0.0, |g| g.size().x),
            Status::Busy | Status::Warning | Status::Check => 20.0,
            Status::Expand { .. } => 28.0,
        };
        let right_w = button_w.unwrap_or(0.0).max(status_w).max(more_w) + options_w;
        let text_x = PAD_L + icon_box + GAP;
        let text_w = (width - text_x - PAD_R - if right_w > 0.0 { right_w + 8.0 } else { 0.0 }).max(40.0);

        // The name on one line, cut with … in the one-line style.
        let mut job = LayoutJob::simple_singleline(self.name.to_owned(), t.font(size::BODY), name_color);
        job.wrap = if two { TextWrapping::wrap_at_width(text_w) } else { TextWrapping::truncate_at_width(text_w) };
        let name = ui.fonts(|f| f.layout_job(job));
        let elided = name.elided;
        let second: Option<(String, Color32)> = match self.problem {
            Some(p) => Some((p.text.to_owned(), p.tone.color(&t))),
            None if two => Some((self.about.to_owned(), t.c.text_secondary.gamma_multiply(if self.dimmed { 0.8 } else { 1.0 }))),
            None => None,
        };
        let second = second.map(|(s, c)| (ui.painter().layout(s, t.font(size::CAPTION), c, text_w), c));
        let text_h = name.size().y + second.as_ref().map_or(0.0, |(g, _)| g.size().y);
        let height = (text_h + if second.is_some() { 16.0 } else { 12.0 }).max(min_h);

        let (outer, alloc) = ui.allocate_exact_size(Vec2::new(full_w, height), Sense::hover());
        let rect = Rect::from_min_max(outer.min + Vec2::new(indent, 0.0), outer.max);
        let sense = if self.tab_stop { Sense::click() } else { Sense { click: true, drag: false, focusable: false } };
        let resp = ui.interact(rect, alloc.id.with("place-row"), sense);
        let spoken = self.spoken.clone().unwrap_or_else(|| {
            let mut s = format!("{}.", self.name);
            if let Status::Words(w) = self.status {
                s.push_str(&format!(" {w}."));
            }
            if let Some(p) = self.problem {
                s.push_str(&format!(" {}", p.text));
            }
            if !self.about.is_empty() {
                s.push_str(&format!(" {}.", self.about));
            }
            s
        });
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
        } else if l.hovered {
            Some(t.c.hover)
        } else {
            None
        };
        if let Some(f) = fill {
            p.rect_filled(rect, radius::ROW, f);
        }
        if self.current {
            let bar = Rect::from_center_size(egui::pos2(rect.left() + 1.5, rect.center().y), Vec2::new(3.0, (height - 22.0).clamp(16.0, 24.0)));
            p.rect_filled(bar, 1.5, t.c.accent);
        }
        let icon_center = egui::pos2(rect.left() + PAD_L + icon_box / 2.0, rect.center().y);
        self.icon.paint(&p, Rect::from_center_size(icon_center, Vec2::splat(20.0)), name_color);
        let text_top = rect.center().y - text_h / 2.0;
        let name_h = name.size().y;
        p.galley(egui::pos2(rect.left() + text_x, text_top), name, name_color);
        if let Some((g, c)) = second {
            p.galley(egui::pos2(rect.left() + text_x, text_top + name_h), g, c);
        }

        // The right end: the ›, then what sits before it.
        let end = rect.right() - PAD_R + if self.options.is_some() { 6.0 } else { 0.0 };
        if let Some((tip, said)) = self.options {
            let r = Rect::from_min_size(egui::pos2(end - 28.0, rect.center().y - 14.0), Vec2::splat(28.0));
            let chevron = super::inside(ui, r, |ui: &mut Ui| {
                IconButton::new(Icon::ChevronRight, tip).spoken(said).tab_stop(self.tab_stop).size(28.0, 16.0).color(t.c.text_secondary).show(ui)
            });
            if chevron.clicked() {
                action = Some(RowAction::Options);
            }
        }
        let right = end - options_w;
        let slot = |w: f32, h: f32| Rect::from_min_size(egui::pos2(right - w, rect.center().y - h / 2.0), Vec2::new(w, h));
        let show_more = self.menu && self.options.is_none() && (l.hovered || l.focused || ui.rect_contains_pointer(rect));
        if let Some(words) = self.problem.and_then(|p| p.button) {
            let r = slot(button_w.unwrap_or(0.0), space::SMALL_BUTTON_HEIGHT);
            if super::inside(ui, r, |ui: &mut Ui| Button::standard(words).small().show(ui)).clicked() {
                action = Some(RowAction::Button);
            }
        } else if show_more {
            let r = slot(28.0, 28.0);
            let more = super::inside(ui, r, |ui: &mut Ui| IconButton::new(Icon::More, "More actions").size(28.0, 18.0).color(t.c.text_secondary).show(ui));
            if more.clicked() {
                action = Some(RowAction::Menu(r.left_bottom()));
            }
        } else {
            match self.status {
                Status::None => {}
                Status::Words(_) => {
                    if let Some(g) = status_galley {
                        p.galley(egui::pos2(right - g.size().x, rect.center().y - g.size().y / 2.0), g, t.c.text_secondary);
                    }
                }
                Status::Busy => paint_spinner(&p, slot(18.0, 18.0), t.c.accent, ui.input(|i| i.time)),
                Status::Warning => Icon::Warning.paint_weight(&p, slot(18.0, 18.0), t.c.amber, 1.6),
                Status::Check => Icon::Check.paint_weight(&p, slot(16.0, 16.0), t.c.accent, 1.8),
                Status::Expand { open } => {
                    let chevron = super::inside(ui, slot(28.0, 28.0), |ui: &mut Ui| {
                        IconButton::new(if open { Icon::ChevronUp } else { Icon::ChevronDown }, if open { "Hide" } else { "Show more" })
                            .size(28.0, 16.0)
                            .color(t.c.text_secondary)
                            .show(ui)
                    });
                    if chevron.clicked() {
                        action = Some(RowAction::Expand);
                    }
                }
            }
        }
        if l.focused {
            focus_ring(&p, rect, radius::ROW, false, &t);
        }

        if action.is_none() {
            let (right_key, left_key) = if resp.has_focus() { ui.input(|i| (i.key_pressed(Key::ArrowRight), i.key_pressed(Key::ArrowLeft))) } else { (false, false) };
            if self.options.is_some() && right_key {
                action = Some(RowAction::Options);
            } else if matches!(self.status, Status::Expand { open: false }) && right_key || matches!(self.status, Status::Expand { open: true }) && left_key {
                action = Some(RowAction::Expand);
            } else if resp.clicked() {
                action = Some(RowAction::Choose);
            } else if self.menu && resp.secondary_clicked() {
                action = Some(RowAction::Menu(resp.interact_pointer_pos().unwrap_or(rect.center())));
            } else if self.menu && resp.has_focus() && ui.input(|i| i.key_pressed(Key::F10) && i.modifiers.shift) {
                action = Some(RowAction::Menu(rect.left_bottom() + Vec2::new(PAD_L, 0.0)));
            }
        }
        // The tooltip: the whole name when it was cut, and what the row
        // used to say under it.
        let tip = match (elided, self.about.is_empty()) {
            (true, false) => Some(format!("{}\n{}", self.name, self.about)),
            (true, true) => Some(self.name.to_owned()),
            (false, false) if !two => Some(self.about.to_owned()),
            _ => None,
        };
        let resp = match tip {
            Some(tip) => resp.on_hover_text(tip),
            None => resp,
        };
        (action, resp)
    }
}
