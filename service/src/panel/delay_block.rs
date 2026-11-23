//! The delay block: always open while the place is a speaker whose delay
//! can change. The line *Delay  about 0.15 s*; the four buttons; what
//! matters now, when something does; and the bottom line, the change with
//! *Reset* at the left and *Other amount…* at the right, which opens the
//! amount field in place of the buttons. Every word is the model's.

use egui::{Event, Key, Rect, Sense, Ui, Vec2, WidgetInfo, WidgetType};

use crate::model::command::{Command, DelayStep};
use crate::model::delay::{amount_field, AmountField, DelayBody, DelayView, Enter, PanelDelay};
use crate::model::snapshot::{Button as ModelButton, ButtonStyle, Tone as ModelTone};
use crate::model::text::Lang;
use crate::ui::button::{Button, IconButton};
use crate::ui::icon::Icon;
use crate::ui::look::look_as;
use crate::ui::text::{Mark, Para, ParaClick, Tone};
use crate::ui::theme::{focus_ring, radius, size, space, Theme};

/// What the block keeps between frames: the amount field, while open.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockState {
    amount: Option<Amount>,
}

#[derive(Clone, Debug, PartialEq)]
struct Amount {
    digits: String,
    /// What Enter presses.
    enter: Enter,
    focus_pending: bool,
}

impl Default for Amount {
    fn default() -> Self {
        Amount { digits: String::new(), enter: Enter::Remove, focus_pending: true }
    }
}

impl BlockState {
    /// Open the amount field, focused and empty (*Other amount…*).
    pub fn open_amount(&mut self) {
        self.amount = Some(Amount::default());
    }

    pub fn amount_is_open(&self) -> bool {
        self.amount.is_some()
    }

    /// The field with `digits` typed (pictures and tests).
    pub fn with_amount(digits: &str) -> Self {
        BlockState { amount: Some(Amount { digits: digits.into(), ..Default::default() }) }
    }

    /// The digits typed, if any.
    pub fn typed(&self) -> Option<&str> {
        self.amount.as_ref().map(|a| a.digits.as_str())
    }
}

/// What the block did.
#[derive(Default)]
pub struct BlockOutput {
    pub commands: Vec<Command>,
    /// Esc closed the amount field (the panel stays open).
    pub esc_taken: bool,
}

const GAP_X: f32 = 8.0;
const GAP_Y: f32 = 8.0;

fn mark_and_tone(tone: ModelTone) -> (Mark, Tone) {
    match tone {
        ModelTone::Plain => (Mark::None, Tone::Secondary),
        ModelTone::Info => (Mark::Info, Tone::Secondary),
        ModelTone::Warning => (Mark::Warning, Tone::Amber),
        ModelTone::Error => (Mark::Error, Tone::Red),
        ModelTone::Busy => (Mark::Busy, Tone::Secondary),
    }
}

/// A model button as the block draws it, `width` wide.
fn step_button(ui: &mut Ui, b: &ModelButton, width: f32) -> bool {
    let mut button = match b.style {
        ButtonStyle::Primary => Button::primary(&b.label),
        _ => Button::standard(&b.label),
    }
    .width(width)
    .max_width(width)
    .enabled(b.enabled);
    if let Some(s) = &b.spoken {
        button = button.accessible_name(s.clone());
    }
    button.show(ui).clicked()
}

/// The head line: *Delay  about 0.15 s*. It takes the keyboard: ← removes
/// and → adds the small step, with Shift the large one, with Ctrl 5 ms.
fn head(ui: &mut Ui, d: &PanelDelay, body: &DelayBody, out: &mut BlockOutput) {
    let t = Theme::of(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::hover());
    let id = ui.id().with("delay-head");
    let resp = ui.interact(rect, id, Sense::focusable_noninteractive());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &d.spoken));
    let l = look_as(ui, &resp, &d.spoken);
    let p = ui.painter();
    let label = p.layout_no_wrap(d.label.clone(), t.font(size::SECONDARY), t.c.text_secondary);
    let value_color = if d.reconnecting { t.c.text_secondary } else { t.c.text };
    let value_font = if d.reconnecting { t.font(size::SECONDARY) } else { t.strong(size::SECONDARY) };
    let lw = label.size().x;
    let y = rect.center().y;
    p.galley(egui::pos2(rect.left(), y - label.size().y / 2.0), label, t.c.text_secondary);
    let mut job = egui::text::LayoutJob::simple_singleline(d.value.clone(), value_font, value_color);
    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - lw - 8.0).max(20.0));
    let value = ui.fonts(|f| f.layout_job(job));
    p.galley(egui::pos2(rect.left() + lw + 8.0, y - value.size().y / 2.0), value, value_color);
    if l.focused {
        focus_ring(p, rect.expand2(Vec2::new(4.0, 0.0)), radius::CONTROL, false, &t);
    }
    if !resp.has_focus() {
        return;
    }
    ui.memory_mut(|m| m.set_focus_lock_filter(id, egui::EventFilter { horizontal_arrows: true, ..Default::default() }));
    let DelayBody::Buttons { remove_large, remove_small, add_large, add_small } = body else { return };
    let key = ui.input(|i| {
        i.events.iter().find_map(|e| match e {
            Event::Key { key: k @ (Key::ArrowLeft | Key::ArrowRight), pressed: true, modifiers, .. } => Some((*k, *modifiers)),
            _ => None,
        })
    });
    let Some((k, mods)) = key else { return };
    let remove = k == Key::ArrowLeft;
    let command = if mods.ctrl || mods.command {
        match &remove_small.command {
            Command::ChangeDelay { speaker, .. } => {
                Some(Command::ChangeDelay { speaker: speaker.clone(), step: if remove { DelayStep::Remove(5) } else { DelayStep::Add(5) } })
            }
            _ => None,
        }
    } else {
        let b = match (remove, mods.shift) {
            (true, true) => remove_large,
            (true, false) => remove_small,
            (false, true) => add_large,
            (false, false) => add_small,
        };
        b.enabled.then(|| b.command.clone())
    };
    out.commands.extend(command);
}

/// The amount field in place of the buttons, while open: *Amount [ ] ms ×*,
/// then *Remove N ms* and *Add N ms*, then what Enter will do.
fn amount_rows(ui: &mut Ui, lang: Lang, view: Option<&DelayView>, first: Option<&AmountField>, a: &mut Amount, out: &mut BlockOutput) -> bool {
    let t = Theme::of(ui.ctx());
    let typed = a.digits.parse::<u32>().ok().filter(|n| *n > 0);
    let field = match (view, typed) {
        (Some(v), _) => amount_field(lang, v, typed, a.enter),
        (None, _) => match first {
            Some(f) => f.clone(),
            None => return false,
        },
    };
    let width = ui.available_width();
    // Row 1.
    let (row, _) = ui.allocate_exact_size(Vec2::new(width, space::SMALL_BUTTON_HEIGHT), Sense::hover());
    let lab = ui.painter().layout_no_wrap(field.label.clone(), t.font(size::SECONDARY), t.c.text_secondary);
    let lab_w = lab.size().x;
    ui.painter().galley(egui::pos2(row.left(), row.center().y - lab.size().y / 2.0), lab, t.c.text_secondary);
    let frame = Rect::from_min_size(egui::pos2(row.left() + lab_w + 8.0, row.top()), Vec2::new(72.0, row.height()));
    let field_shape = ui.painter().add(egui::Shape::Noop);
    let mut text = a.digits.clone();
    let edit_rect = frame.shrink2(Vec2::new(8.0, 2.0));
    let edit = crate::ui::inside(ui, edit_rect, |ui: &mut Ui| {
        ui.add_sized(
            edit_rect.size(),
            egui::TextEdit::singleline(&mut text)
                .id(ui.id().with("delay-amount"))
                .frame(false)
                .font(t.strong(size::CONTROL))
                .text_color(t.c.text)
                .margin(egui::Margin::ZERO)
                .vertical_align(egui::Align::Center),
        )
    });
    edit.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, &field.spoken));
    let look = look_as(ui, &edit, &field.spoken);
    ui.painter().set(field_shape, crate::ui::delay::field_shapes(frame, look, &t));
    // Digits only, at most five.
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
    a.digits = digits;
    if a.focus_pending {
        edit.request_focus();
        a.focus_pending = false;
    }
    let unit = ui.painter().layout_no_wrap(field.unit.clone(), t.font(size::SECONDARY), t.c.text_secondary);
    ui.painter().galley(egui::pos2(frame.right() + 8.0, row.center().y - unit.size().y / 2.0), unit, t.c.text_secondary);
    let close_rect = Rect::from_center_size(egui::pos2(row.right() - 12.0, row.center().y), Vec2::splat(24.0));
    let closed = crate::ui::inside(ui, close_rect, |ui: &mut Ui| IconButton::new(Icon::Close, &field.close).size(24.0, 14.0).color(t.c.text_secondary).show(ui)).clicked();
    ui.add_space(GAP_Y);

    // Row 2: the two buttons, the one Enter presses outlined.
    let col = ((width - GAP_X) / 2.0).max(0.0);
    let mut pressed: Option<Command> = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = GAP_X;
        for (b, dir) in [(&field.remove, Enter::Remove), (&field.add, Enter::Add)] {
            let r = Button::standard(&b.label).small().width(col).max_width(col).enabled(b.enabled).default_action(a.enter == dir && b.enabled).show(ui);
            if r.clicked() {
                pressed = Some(b.command.clone());
            }
        }
    });
    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    if enter {
        let b = if a.enter == Enter::Remove { &field.remove } else { &field.add };
        if b.enabled {
            pressed = Some(b.command.clone());
        }
    }
    // ↑ and ↓ in the field choose what Enter does.
    if edit.has_focus() {
        let (up, down) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown)));
        if up {
            a.enter = Enter::Add;
        } else if down {
            a.enter = Enter::Remove;
        }
    }
    ui.add_space(6.0);
    let (mark, tone) = mark_and_tone(field.tone);
    let _ = Para::new(&field.line).mark(mark).tone(tone).size(size::CAPTION).show(ui);
    let esc = ui.input(|i| i.key_pressed(Key::Escape)) && (edit.has_focus() || edit.lost_focus());
    if esc {
        out.esc_taken = true;
    }
    match pressed {
        Some(c) => {
            out.commands.push(c);
            false
        }
        None => !(closed || esc),
    }
}

/// Draw the block for `d` (and the snapshot's `view` of the same speaker,
/// for the amount field's words as digits are typed).
pub fn block(ui: &mut Ui, lang: Lang, d: &PanelDelay, view: Option<&DelayView>, st: &mut BlockState) -> BlockOutput {
    let mut out = BlockOutput::default();
    let t = Theme::of(ui.ctx());
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let width = ui.available_width();
        head(ui, d, &d.body, &mut out);
        ui.add_space(6.0);
        if let Some(a) = st.amount.as_mut() {
            let keep = amount_rows(ui, lang, view, d.amount.as_ref(), a, &mut out);
            if !keep {
                st.amount = None;
            }
        } else {
            match &d.body {
                DelayBody::Buttons { remove_large, remove_small, add_large, add_small } => {
                    let col = ((width - GAP_X) / 2.0).max(0.0);
                    for (i, pair) in [[remove_large, add_large], [remove_small, add_small]].into_iter().enumerate() {
                        if i > 0 {
                            ui.add_space(GAP_Y);
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = GAP_X;
                            for b in pair {
                                if step_button(ui, b, col) {
                                    out.commands.push(b.command.clone());
                                }
                            }
                        });
                    }
                }
                DelayBody::Fixed { text, action } => {
                    let clicked = Para::new(text).mark(Mark::Info).size(size::SECONDARY).link(&action.label).show(ui);
                    if clicked == Some(ParaClick::Link(0)) {
                        out.commands.push(action.command.clone());
                    }
                }
            }
        }
        // What matters now.
        if let Some(n) = &d.note {
            ui.add_space(8.0);
            let (mark, tone) = mark_and_tone(n.tone);
            let mut p = Para::new(&n.text).mark(mark).tone(tone).size(size::CAPTION);
            if let Some(a) = &n.action {
                p = p.link(&a.label);
            }
            if p.show(ui) == Some(ParaClick::Link(0)) {
                out.commands.extend(n.action.as_ref().map(|a| a.command.clone()));
            }
        }
        // The bottom line: the change and Reset at the left, Other amount…
        // at the right (not while the field is open).
        let other = d.other_amount.as_deref().filter(|_| st.amount.is_none());
        if d.change.is_some() || d.reset.is_some() || other.is_some() {
            ui.add_space(8.0);
            let (row, _) = ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::hover());
            let mut x = row.left();
            if let Some(change) = &d.change {
                let g = ui.painter().layout_no_wrap(change.clone(), t.font(size::CAPTION), t.c.text_secondary);
                let w = g.size().x;
                ui.painter().galley(egui::pos2(x, row.center().y - g.size().y / 2.0), g, t.c.text_secondary);
                x += w;
            }
            if let Some(reset) = &d.reset {
                if d.change.is_some() {
                    let dot = ui.painter().layout_no_wrap(" · ".into(), t.font(size::CAPTION), t.c.text_secondary);
                    let w = dot.size().x;
                    ui.painter().galley(egui::pos2(x, row.center().y - dot.size().y / 2.0), dot, t.c.text_secondary);
                    x += w;
                }
                let w = ui.fonts(|f| f.layout_no_wrap(reset.label.clone(), t.font(size::CAPTION), t.c.accent).size().x);
                let r = Rect::from_min_size(egui::pos2(x, row.top()), Vec2::new(w, row.height()));
                let name = reset.spoken.clone().unwrap_or(reset.label.clone());
                if crate::ui::inside(ui, r, |ui: &mut Ui| Button::link(&reset.label).text_size(size::CAPTION).accessible_name(name).show(ui)).clicked() {
                    out.commands.push(reset.command.clone());
                }
            }
            if let Some(o) = other {
                let w = ui.fonts(|f| f.layout_no_wrap(o.to_owned(), t.font(size::CAPTION), t.c.accent).size().x);
                let r = Rect::from_min_size(egui::pos2(row.right() - w, row.top()), Vec2::new(w, row.height()));
                if crate::ui::inside(ui, r, |ui: &mut Ui| Button::link(o).text_size(size::CAPTION).show(ui)).clicked() {
                    st.open_amount();
                }
            }
        }
    });
    out
}
