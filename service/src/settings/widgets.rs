//! The few shapes Settings pages share beyond the common parts: titles,
//! cards with several rows, folds, numbered steps, check lines, key caps.

use egui::{Color32, Id, Rect, Sense, Ui, Vec2, WidgetInfo, WidgetType};

use crate::ui::button::Button;
use crate::ui::card::{card, SETTINGS_CARD_PAD};
use crate::ui::icon::Icon;
use crate::ui::look::look_as;
use crate::ui::text::{Mark, Para, Tone};
use crate::ui::theme::{focus_ring, radius, size, Theme};

use super::PageState;

/// A page's title: 26 points, semibold.
pub fn page_title(ui: &mut Ui, text: &str) {
    page_title_with(ui, text, |_| ());
}

/// A page's title with something after it (the *experimental* pill).
pub fn page_title_with(ui: &mut Ui, text: &str, after: impl FnOnce(&mut Ui)) {
    let t = Theme::of(ui.ctx());
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        ui.label(egui::RichText::new(text).font(t.strong(size::PAGE)).color(t.c.text));
        after(ui);
    });
    ui.add_space(10.0);
}

/// A sub-page's title: *Speakers › Kitchen*. The first part goes back
/// (returns true when clicked).
pub fn crumb(ui: &mut Ui, parent: &str, title: &str) -> bool {
    let t = Theme::of(ui.ctx());
    ui.add_space(6.0);
    let mut back = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let g = ui.painter().layout_no_wrap(parent.to_owned(), t.font(size::PAGE), t.c.text_secondary);
        let (rect, resp) = ui.allocate_exact_size(g.size(), Sense::click());
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Link, true, parent));
        let l = look_as(ui, &resp, parent);
        let color = if l.hovered { t.c.text } else { t.c.text_secondary };
        ui.painter().galley(rect.min, g, color);
        if l.focused {
            focus_ring(ui.painter(), rect, radius::CONTROL, true, &t);
        }
        back = resp.clicked();
        let (chev, _) = ui.allocate_exact_size(Vec2::new(18.0, rect.height()), Sense::hover());
        Icon::ChevronRight.paint_weight(ui.painter(), Rect::from_center_size(chev.center() + Vec2::new(0.0, 2.0), Vec2::splat(18.0)), t.c.text, 2.2);
        ui.label(egui::RichText::new(title).font(t.strong(size::PAGE)).color(t.c.text));
    });
    ui.add_space(10.0);
    back
}

/// A section heading over cards (*Your speakers*).
pub fn heading(ui: &mut Ui, text: &str) {
    crate::ui::card::section_heading(ui, text);
}

/// A card holding several rows or free content.
pub fn boxed<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    card(ui, SETTINGS_CARD_PAD, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        add(ui)
    })
    .inner
}

/// Words in the main colour, wrapping (a card's title or statement).
pub fn body(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.add(egui::Label::new(egui::RichText::new(text).font(t.font(size::BODY)).color(t.c.text)).wrap());
}

/// Words in semibold, wrapping.
pub fn strong(ui: &mut Ui, text: &str, em: f32) {
    let t = Theme::of(ui.ctx());
    ui.add(egui::Label::new(egui::RichText::new(text).font(t.strong(em)).color(t.c.text)).wrap());
}

/// A grey line of help (12 points), wrapping.
pub fn note(ui: &mut Ui, text: &str) {
    let _ = Para::new(text).show(ui);
}

/// A grey line of help with links after it; the link clicked.
pub fn note_links(ui: &mut Ui, text: &str, links: &[&str]) -> Option<usize> {
    let mut p = Para::new(text);
    for l in links {
        p = p.link(l);
    }
    match p.show(ui) {
        Some(crate::ui::text::ParaClick::Link(i)) => Some(i),
        _ => None,
    }
}

/// A line with a mark: amber, red or an info line.
pub fn marked(ui: &mut Ui, mark: Mark, tone: Tone, text: &str) {
    let _ = Para::new(text).mark(mark).tone(tone).size(size::SECONDARY).show(ui);
}

/// A row of buttons, wrapping when the window is narrow.
pub fn buttons<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        add(ui)
    })
    .inner
}

/// A link that stands alone (*Show the welcome again*).
pub fn link(ui: &mut Ui, text: &str) -> bool {
    Button::link(text).show(ui).clicked()
}

/// A row inside a card: an optional icon, a title and grey description on
/// the left, a control on the right (right to left, at its own width).
pub fn row<R>(ui: &mut Ui, icon: Option<Icon>, title: &str, description: Option<&str>, control: impl FnOnce(&mut Ui) -> R) -> R {
    let t = Theme::of(ui.ctx());
    let id = ui.next_auto_id().with("settings-row");
    let width = ui.available_width();
    let icon_w = if icon.is_some() { 46.0 } else { 0.0 };
    let (control_w, control_h): (f32, f32) = ui.ctx().data(|d| d.get_temp(id)).unwrap_or((0.0, 0.0));
    let gap = if control_w > 0.0 { 14.0 } else { 0.0 };
    let text_w = (width - icon_w - control_w - gap).max(140.0);
    let title_g = ui.painter().layout(title.to_owned(), t.font(size::BODY), t.c.text, text_w);
    let desc_g = description.map(|d| ui.painter().layout(d.to_owned(), t.font(size::CAPTION), t.c.text_secondary, text_w));
    let text_h = title_g.size().y + desc_g.as_ref().map_or(0.0, |g| g.size().y + 2.0);
    let height = text_h.max(control_h).max(24.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let p = ui.painter();
    if let Some(i) = icon {
        i.paint(p, Rect::from_center_size(egui::pos2(rect.left() + 16.0, rect.center().y), Vec2::splat(20.0)), t.c.text);
    }
    let tx = rect.left() + icon_w;
    let ty = rect.center().y - text_h / 2.0;
    let title_h = title_g.size().y;
    p.galley(egui::pos2(tx, ty), title_g, t.c.text);
    if let Some(g) = desc_g {
        p.galley(egui::pos2(tx, ty + title_h + 2.0), g, t.c.text_secondary);
    }
    let room = (width - icon_w - 140.0 - 14.0).max(0.0);
    let c_rect = Rect::from_min_max(egui::pos2(rect.right() - room, rect.top()), rect.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(c_rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
    let out = control(&mut child);
    let used = child.min_rect();
    let size = if used.is_positive() { (used.width(), used.height()) } else { (0.0, 0.0) };
    if (size.0 - control_w).abs() > 0.5 || (size.1 - control_h).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(id, size));
        ui.ctx().request_discard("a settings row's control changed size");
    }
    out
}

/// A thin line between rows of a card.
pub fn divider(ui: &mut Ui) {
    ui.add_space(4.0);
    crate::ui::card::divider(ui);
    ui.add_space(4.0);
}

/// A card that folds open (*Details*): an icon, the title, a description,
/// and a chevron; `inside` draws what it holds while open.
pub fn fold(ui: &mut Ui, st: &mut PageState, key: &str, icon: Icon, title: &str, description: Option<&str>, inside: impl FnOnce(&mut Ui, &mut PageState)) {
    let open = st.is_open(key);
    let t = Theme::of(ui.ctx());
    let mut toggle = false;
    boxed(ui, |ui| {
        let id = Id::new(("fold", key));
        let width = ui.available_width();
        let text_w = width - 46.0 - 30.0;
        let title_g = ui.painter().layout(title.to_owned(), t.font(size::BODY), t.c.text, text_w);
        let desc_g = description.map(|d| ui.painter().layout(d.to_owned(), t.font(size::CAPTION), t.c.text_secondary, text_w));
        let text_h = title_g.size().y + desc_g.as_ref().map_or(0.0, |g| g.size().y + 2.0);
        let h = text_h.max(32.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, h), Sense::hover());
        let resp = ui.interact(rect.expand2(Vec2::new(16.0, 12.0)), id, Sense::click());
        let name = title.to_string();
        resp.widget_info(|| WidgetInfo::selected(WidgetType::CollapsingHeader, true, open, &name));
        let l = look_as(ui, &resp, title);
        let p = ui.painter();
        if l.hovered {
            p.rect_filled(resp.rect, radius::CARD, t.c.hover);
        }
        icon.paint(p, Rect::from_center_size(egui::pos2(rect.left() + 16.0, rect.center().y), Vec2::splat(20.0)), t.c.text);
        let ty = rect.center().y - text_h / 2.0;
        let th = title_g.size().y;
        p.galley(egui::pos2(rect.left() + 46.0, ty), title_g, t.c.text);
        if let Some(g) = desc_g {
            p.galley(egui::pos2(rect.left() + 46.0, ty + th + 2.0), g, t.c.text_secondary);
        }
        let chev = if open { Icon::ChevronUp } else { Icon::ChevronDown };
        chev.paint(p, Rect::from_center_size(egui::pos2(rect.right() - 10.0, rect.center().y), Vec2::splat(16.0)), t.c.text_secondary);
        if l.focused {
            focus_ring(p, resp.rect, radius::CARD, false, &t);
        }
        toggle = resp.clicked();
        if open {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(46.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    inside(ui, st)
                });
            });
        }
    });
    if toggle {
        st.toggle(key);
    }
}

/// How a step of a set-up or check stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepMark {
    Done,
    Current,
    Later,
}

/// A numbered step (Music Assistant's set-up): a circle with the number or
/// a tick, the title, then whatever `body` draws under it.
pub fn step(ui: &mut Ui, n: u8, mark: StepMark, title: &str, body: impl FnOnce(&mut Ui)) {
    let t = Theme::of(ui.ctx());
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(36.0, 30.0), Sense::hover());
        let c = Rect::from_center_size(egui::pos2(r.left() + 14.0, r.top() + 13.0), Vec2::splat(26.0));
        let p = ui.painter();
        match mark {
            StepMark::Done => {
                p.circle_filled(c.center(), 13.0, t.c.green);
                Icon::Check.paint_weight(p, c.shrink(6.0), t.c.card, 2.4);
            }
            StepMark::Current => {
                p.circle_stroke(c.center(), 12.0, egui::Stroke::new(1.6_f32, t.c.accent));
                let g = p.layout_no_wrap(n.to_string(), t.strong(size::SECONDARY), t.c.accent);
                p.galley(c.center() - g.size() / 2.0, g, t.c.accent);
            }
            StepMark::Later => {
                p.circle_stroke(c.center(), 12.0, egui::Stroke::new(1.0_f32, t.c.text_disabled));
                let g = p.layout_no_wrap(n.to_string(), t.font(size::CAPTION), t.c.text_disabled);
                p.galley(c.center() - g.size() / 2.0, g, t.c.text_disabled);
            }
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            let color = if mark == StepMark::Later { t.c.text_disabled } else { t.c.text };
            ui.add(egui::Label::new(egui::RichText::new(title).font(t.strong(size::BODY)).color(color)).wrap());
            body(ui);
        });
    });
}

/// What a line of a check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Good,
    Bad,
    Warning,
    /// The PC can't tell.
    Unknown,
    Info,
}

/// One line of a check: the mark, a statement, a grey detail, then
/// `actions` (buttons) under it. Lines sit in one card, divided.
pub fn check_line(ui: &mut Ui, found: Found, title: &str, detail: Option<&str>, actions: impl FnOnce(&mut Ui)) {
    let t = Theme::of(ui.ctx());
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(30.0, 20.0), Sense::hover());
        let m = Rect::from_center_size(egui::pos2(r.left() + 10.0, r.top() + 10.0), Vec2::splat(18.0));
        let p = ui.painter();
        match found {
            Found::Good => Icon::Check.paint_weight(p, m, t.c.green, 2.0),
            Found::Bad => Icon::ErrorCross.paint_weight(p, m, t.c.red, 1.7),
            Found::Warning => Icon::Warning.paint_weight(p, m, t.c.amber, 1.6),
            Found::Unknown => Icon::Help.paint_weight(p, m, t.c.text_secondary, 1.6),
            Found::Info => Icon::Info.paint_weight(p, m, t.c.accent, 1.6),
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.add(egui::Label::new(egui::RichText::new(title).font(t.font(size::BODY)).color(t.c.text)).wrap());
            if let Some(d) = detail {
                let _ = Para::new(d).size(size::SECONDARY).show(ui);
            }
            ui.add_space(2.0);
            actions(ui);
        });
    });
}

/// Keys as small caps: [Ctrl] + [Win] + [,].
pub fn keycaps(ui: &mut Ui, keys: &[String]) {
    let t = Theme::of(ui.ctx());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                ui.label(egui::RichText::new("+").font(t.font(size::SECONDARY)).color(t.c.text_secondary));
            }
            keycap(ui, k);
        }
    });
}

pub fn keycap(ui: &mut Ui, key: &str) {
    let t = Theme::of(ui.ctx());
    let g = ui.painter().layout_no_wrap(key.to_owned(), t.font(size::SECONDARY), t.c.text);
    let (r, _) = ui.allocate_exact_size(Vec2::new(g.size().x.max(10.0) + 12.0, 22.0), Sense::hover());
    ui.painter().rect_filled(r, 4.0, t.c.control);
    ui.painter().rect_stroke(r.shrink(0.5), 4.0, egui::Stroke::new(1.0_f32, t.c.control_border));
    ui.painter().galley(r.center() - g.size() / 2.0, g, t.c.text);
}

/// A small pill after a title (*experimental*).
pub fn pill(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    let g = ui.painter().layout_no_wrap(text.to_owned(), t.strong(size::CAPTION), t.c.text_secondary);
    let (r, _) = ui.allocate_exact_size(Vec2::new(g.size().x + 16.0, 22.0), Sense::hover());
    ui.painter().rect_filled(r, 11.0, t.c.hover);
    ui.painter().galley(r.center() - g.size() / 2.0, g, t.c.text_secondary);
}

/// A copy button: *Copy* until clicked, then *Copied* for two seconds.
/// Copies `text` itself.
pub fn copy_button(ui: &mut Ui, st: &mut PageState, key: &str, label: &str, copied: &str, text: &str) {
    let now = ui.input(|i| i.time);
    let just = st.copied.as_ref().is_some_and(|(k, at)| k == key && now - at < 2.0);
    let shown = if just { copied } else { label };
    if Button::standard(shown).icon(Icon::Copy).accessible_name(label).show(ui).clicked() {
        ui.ctx().copy_text(text.to_owned());
        st.copied = Some((key.to_string(), now));
    }
    if just {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
}

/// Text in the monospace face, selectable (addresses, ids, a token).
pub fn mono(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.add(egui::Label::new(egui::RichText::new(text).monospace().size(size::SECONDARY).color(t.c.text)).wrap());
}

/// The fill of a tinted panel (the code's box).
pub fn tint(ui: &Ui) -> Color32 {
    Theme::of(ui.ctx()).c.accent_soft
}

/// Keep the field `key` being typed in `st`, starting from `value`.
/// Returns the text typed and whether typing ended (focus left or Enter)
/// with a value different from `value`.
pub fn kept_field(ui: &mut Ui, st: &mut super::PageState, key: &str, value: &str, name: &str, width: f32) -> Option<String> {
    let text = st.typing.entry(key.to_string()).or_insert_with(|| value.to_string());
    let resp = crate::ui::inputs::TextField::new(text, name).width(width).show(ui);
    let done = resp.lost_focus();
    if !resp.has_focus() && !done && *text != value {
        // Changed elsewhere while not typing: show the new value.
        *text = value.to_string();
    }
    if done {
        let typed = text.trim().to_string();
        if typed != value {
            return Some(typed);
        }
    }
    None
}

/// A coloured bar at the top of a page (*Playing to HomePod pairs hasn't
/// been tested…*), its button a standard one so the page keeps one primary
/// action. True when the button was clicked.
pub fn bar(ui: &mut Ui, kind: crate::ui::notice::BarKind, title: &str, text: &str, action: Option<&str>) -> bool {
    use crate::ui::notice::BarKind;
    let t = Theme::of(ui.ctx());
    let (fill, mark) = match kind {
        BarKind::Info => (t.c.accent_soft, Mark::Info),
        BarKind::Warning => (t.c.amber_bg, Mark::Warning),
        BarKind::Error => (t.c.red_bg, Mark::Error),
        BarKind::Done => (t.c.green_bg, Mark::Done),
    };
    let mut clicked = false;
    egui::Frame::none().fill(fill).stroke(t.card_stroke()).rounding(radius::CARD).inner_margin(egui::Margin { left: 14.0, right: 14.0, top: 11.0, bottom: 11.0 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let id = ui.next_auto_id().with("bar-button");
        let button_w: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(0.0);
        ui.horizontal(|ui| {
            let words_w = ui.available_width() - if action.is_some() { button_w + 12.0 } else { 0.0 };
            ui.allocate_ui(Vec2::new(words_w.max(120.0), 0.0), |ui| {
                let _ = Para::new(text).mark(mark).lead(title).size(size::SECONDARY).tone(Tone::Primary).show(ui);
            });
            if let Some(a) = action {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let r = Button::standard(a).show(ui);
                    clicked = r.clicked();
                    if (r.rect.width() - button_w).abs() > 0.5 {
                        ui.ctx().data_mut(|d| d.insert_temp(id, r.rect.width()));
                        ui.ctx().request_discard("a bar's button changed width");
                    }
                });
            }
        });
    });
    clicked
}
