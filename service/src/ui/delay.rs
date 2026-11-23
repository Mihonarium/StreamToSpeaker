//! The delay control: a speaker's delay, what has been removed from it or
//! added to it, and buttons to remove or add milliseconds.
//!
//! ```text
//! Delay   about 0.15 s · 50 ms removed   ↺
//! [  Remove 100 ms  ]   [   Add 100 ms   ]
//! [  Remove 25 ms   ]   [   Add 25 ms    ]
//! Removing… 25 ms to go
//! ```
//!
//! Two forms. [`DelayControl`], on a speaker's page: the head line above
//! the buttons; clicking the delay (or Enter, a digit, − or + on it) swaps
//! the buttons for one field and two buttons that repeat its number, and
//! the line under them says what Enter will do before anything changes.
//! [`DelayLine`], in the panel: one closed line, *Delay  about 0.15 s*,
//! that opens the four buttons and one line (*50 ms removed · Reset*).
//!
//! Nothing moves under the pointer: the field and its buttons take exactly
//! the room of the four buttons, and the line under them keeps its room
//! while the pointer is on the control.

use egui::{Key, Rect, Sense, Ui, Vec2, WidgetInfo, WidgetType};

use super::button::{Button, IconButton};
use super::icon::Icon;
use super::look::{look_as, Look};
use super::text::{Mark, Para, ParaClick, Tone};
use super::theme::{focus_ring, radius, size, space, Theme};

/// A delay in seconds as people read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelayFigure {
    pub ms: u32,
    /// Measured with the sync check, rather than estimated.
    pub measured: bool,
}

impl DelayFigure {
    pub fn about(ms: u32) -> Self {
        DelayFigure { ms, measured: false }
    }

    pub fn measured(ms: u32) -> Self {
        DelayFigure { ms, measured: true }
    }

    /// *about 0.15 s*, *under 0.1 s*, *about 1.7 s*, *0.16 s, measured*.
    /// Estimates are rounded, halves up: below 1 s to 0.05 s, from 1 s to
    /// 0.1 s; below 0.1 s they say *under 0.1 s*, since a speaker always
    /// keeps some. Measured figures show hundredths.
    pub fn short(self) -> String {
        if self.measured {
            format!("{}, measured", seconds_measured(self.ms))
        } else if self.ms < 100 {
            "under 0.1 s".into()
        } else {
            format!("about {}", seconds_rounded(self.ms))
        }
    }

    /// As a row's second line says it: *about 0.15 s delay*,
    /// *1.74 s delay, measured*.
    pub fn row(self) -> String {
        if self.measured {
            format!("{} delay, measured", seconds_measured(self.ms))
        } else {
            format!("{} delay", self.short())
        }
    }

    /// As a screen reader says it: *about 0.15 seconds*.
    pub fn spoken(self) -> String {
        let short = self.short();
        let (number, rest) = match short.split_once(" s") {
            Some((n, r)) => (n, r),
            None => (short.as_str(), ""),
        };
        let unit = if number.ends_with(" 1") || number == "1" { "second" } else { "seconds" };
        format!("{number} {unit}{rest}")
    }
}

fn trim(whole: u32, frac: u32, digits: usize) -> String {
    let mut f = format!("{frac:0digits$}");
    while f.ends_with('0') {
        f.pop();
    }
    if f.is_empty() {
        format!("{whole} s")
    } else {
        format!("{whole}.{f} s")
    }
}

fn seconds_rounded(ms: u32) -> String {
    if ms < 1000 {
        let r = (ms + 25) / 50 * 50;
        trim(r / 1000, (r % 1000) / 10, 2)
    } else {
        let r = (ms + 50) / 100 * 100;
        trim(r / 1000, (r % 1000) / 100, 1)
    }
}

fn seconds_measured(ms: u32) -> String {
    let cs = (ms + 5) / 10;
    format!("{}.{:02} s", cs / 100, cs % 100)
}

/// What has been removed from or added to a delay, in the buttons' words:
/// *50 ms removed*, *100 ms added*, or nothing at the speaker's own delay.
pub fn change_words(change_ms: i32) -> Option<String> {
    match change_ms {
        0 => None,
        c if c < 0 => Some(format!("{} ms removed", c.unsigned_abs())),
        c => Some(format!("{c} ms added")),
    }
}

fn change_spoken(change_ms: i32) -> String {
    match change_ms {
        0 => "nothing removed or added".into(),
        c if c < 0 => format!("{} milliseconds removed", c.unsigned_abs()),
        c => format!("{c} milliseconds added"),
    }
}

/// Which way a change goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Remove,
    Add,
}

/// The speaker's limits on what can be removed and added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_removed_ms: u32,
    pub max_added_ms: u32,
}

impl Limits {
    /// How much of `amount` can be removed or added from `change_ms`
    /// before the limit: a step that would pass it stops at it.
    pub fn allowed(self, change_ms: i32, dir: Direction, amount: u32) -> u32 {
        let room = match dir {
            Direction::Remove => (self.max_removed_ms as i64 + change_ms as i64).max(0),
            Direction::Add => (self.max_added_ms as i64 - change_ms as i64).max(0),
        };
        (amount as i64).min(room) as u32
    }

    /// Whether nothing more can go that way.
    pub fn at_limit(self, change_ms: i32, dir: Direction) -> bool {
        self.allowed(change_ms, dir, 1) == 0
    }
}

fn apply(change_ms: i32, dir: Direction, amount: u32) -> i32 {
    match dir {
        Direction::Remove => change_ms - amount as i32,
        Direction::Add => change_ms + amount as i32,
    }
}

/// The line under the amount field: what Enter will do, said before
/// anything changes. The flag is true when a limit cuts the amount.
pub fn amount_line(speaker: &str, typed: Option<u32>, dir: Direction, change_ms: i32, limits: Limits) -> (String, bool) {
    let Some(amount) = typed.filter(|a| *a > 0) else {
        return ("Type how many milliseconds, then choose Remove or Add.".into(), false);
    };
    let allowed = limits.allowed(change_ms, dir, amount);
    let verb = match dir {
        Direction::Remove => "removes",
        Direction::Add => "adds",
    };
    if allowed < amount {
        let limit = match dir {
            Direction::Remove if limits.max_removed_ms == 0 => {
                return (format!("{speaker} already plays with as little delay as it can."), true)
            }
            Direction::Remove => format!("{speaker} allows at most {} ms removed", limits.max_removed_ms),
            Direction::Add => format!("{speaker} allows at most {} ms added", limits.max_added_ms),
        };
        if allowed == 0 {
            return (format!("{limit}."), true);
        }
        return (format!("{limit}, so Enter {verb} {allowed} ms."), true);
    }
    let result = match change_words(apply(change_ms, dir, amount)) {
        Some(w) => format!("{w} in all"),
        None => format!("back to {speaker}'s own delay"),
    };
    (format!("Enter {verb} {amount} ms: {result}."), false)
}

/// The one small line under the buttons, when there is something to say.
/// The engine picks which one applies; the words are here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelayNote {
    /// A change the speaker refused; the app went back to `now_ms`.
    NotAccepted { asked_ms: i32, now_ms: i32 },
    /// The speaker ran dry after a removal, so some went back.
    DroppedOut { had_ms: i32, now_ms: i32 },
    /// An AirPlay speaker on Steady, after a Remove press.
    SteadyOffer { steady_ms: u32, short_ms: u32 },
    /// At the limit of removing.
    MostRemoved,
    /// At the limit of adding.
    MostAdded,
    /// On Steady, at the limit of removing.
    SteadyMostRemoved,
    /// A speaker that can't have any delay removed.
    AsLittleAsItCan,
    /// A change applying gradually, on Sonos and other UPnP speakers.
    Removing { to_go_ms: u32 },
    Adding { to_go_ms: u32 },
    /// A change that will apply by reconnecting.
    WillReconnect,
    Reconnecting,
    /// Right after a switch between Short delay and Steady.
    ChoiceSwitched { to_short: bool, now_ms: i32, other_ms: i32 },
    /// Dropouts the app saw, and adding some delay may help.
    DropoutsAdd { times: u32, minutes: u32 },
    /// Dropouts on Short delay; Steady may help.
    DropoutsSteady { times: u32, minutes: u32 },
    /// Before anything was ever removed or added.
    Newcomer,
}

/// What a note's link does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoteLink {
    UseShortDelay,
    UseSteady,
    AddLarge,
    BackToOwn,
}

struct NoteWords {
    mark: Mark,
    tone: Tone,
    text: String,
    links: Vec<(String, NoteLink)>,
}

impl DelayNote {
    fn words(&self, speaker: &str, limits: Limits, large_step: u32) -> NoteWords {
        let plain = |text: String| NoteWords { mark: Mark::None, tone: Tone::Secondary, text, links: vec![] };
        let warn = |text: String| NoteWords { mark: Mark::Warning, tone: Tone::Amber, text, links: vec![] };
        let word = |c: i32| change_words(c).unwrap_or_else(|| "nothing removed or added".into());
        let short_link = vec![("Use Short delay".to_string(), NoteLink::UseShortDelay)];
        match self {
            DelayNote::NotAccepted { asked_ms, now_ms } => {
                warn(format!("{speaker} didn't accept {}, so it's back to {}.", word(*asked_ms), word(*now_ms)))
            }
            DelayNote::DroppedOut { had_ms, now_ms } => {
                let back = (now_ms - had_ms).unsigned_abs();
                warn(format!("{speaker} dropped out with {}, so {back} ms went back: {}.", word(*had_ms), word(*now_ms)))
            }
            DelayNote::SteadyOffer { steady_ms, short_ms } => NoteWords {
                mark: Mark::Info,
                tone: Tone::Secondary,
                text: format!(
                    "{speaker} is {} late on Steady. For video, use Short delay ({}).",
                    DelayFigure::about(*steady_ms).short(),
                    DelayFigure::about(*short_ms).short()
                ),
                links: short_link,
            },
            DelayNote::MostRemoved => warn(format!("{speaker} allows at most {} ms removed.", limits.max_removed_ms)),
            DelayNote::MostAdded => warn(format!("{speaker} allows at most {} ms added.", limits.max_added_ms)),
            DelayNote::SteadyMostRemoved => NoteWords {
                links: short_link,
                ..warn(format!(
                    "On Steady, {speaker} allows at most {} ms removed. For less delay, use Short delay.",
                    limits.max_removed_ms
                ))
            },
            DelayNote::AsLittleAsItCan => plain(format!("{speaker} already plays with as little delay as it can.")),
            DelayNote::Removing { to_go_ms } => plain(format!("Removing… {to_go_ms} ms to go")),
            DelayNote::Adding { to_go_ms } => plain(format!("Adding… {to_go_ms} ms to go")),
            DelayNote::WillReconnect => NoteWords {
                mark: Mark::Busy,
                ..plain(format!("{speaker} reconnects to apply this: a few seconds of silence."))
            },
            DelayNote::Reconnecting => NoteWords { mark: Mark::Busy, ..plain(format!("Reconnecting {speaker}…")) },
            DelayNote::ChoiceSwitched { to_short, now_ms, other_ms } => {
                let (new, old) = if *to_short { ("Short delay", "Steady") } else { ("Steady", "Short delay") };
                let mut s = format!("{new} starts with {}.", word(*now_ms));
                if let Some(o) = change_words(*other_ms) {
                    s.push_str(&format!(" {old} keeps its {o}."));
                }
                plain(s)
            }
            DelayNote::DropoutsAdd { times, minutes } => NoteWords {
                mark: Mark::Info,
                tone: Tone::Secondary,
                text: format!("{speaker} dropped out {times} times in {minutes} minutes. Adding {large_step} ms may help."),
                links: vec![(format!("Add {large_step} ms"), NoteLink::AddLarge)],
            },
            DelayNote::DropoutsSteady { times, minutes } => NoteWords {
                mark: Mark::Info,
                tone: Tone::Secondary,
                text: format!("{speaker} dropped out {times} times in {minutes} minutes. Steady may help."),
                links: vec![("Use Steady".to_string(), NoteLink::UseSteady)],
            },
            DelayNote::Newcomer => plain("Sound behind the picture? Remove some delay. Sound breaking up? Add some.".into()),
        }
    }
}

/// Everything the control shows, as plain values.
#[derive(Clone, Debug, PartialEq)]
pub struct DelayView<'a> {
    /// The speaker's name as its row shows it.
    pub speaker: &'a str,
    /// The speaker's delay, including what was removed or added (the
    /// target, while a change applies).
    pub delay: DelayFigure,
    /// Removed (negative) or added (positive), in ms: the target while a
    /// change applies.
    pub change_ms: i32,
    pub limits: Limits,
    /// The two step sizes: 25 and 100 unless the Lab changed them.
    pub small_step_ms: u32,
    pub large_step_ms: u32,
    pub note: Option<DelayNote>,
    /// An AirPlay 2 speaker on Steady: nothing can be changed; the control
    /// says so and offers Short delay, of this delay.
    pub fixed_with_short_ms: Option<u32>,
    /// Show *Other amount…* at the end of the head line (a speaker's page).
    pub other_amount_link: bool,
    /// A reconnect applying a change (AirPlay speakers and HomePods): the
    /// panel's closed line says so instead of the figure.
    pub reconnecting: Option<Reconnect>,
}

/// What a reconnect is applying, from what the speaker had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconnect {
    Remove(u32),
    Add(u32),
    BackToOwn,
}

impl Reconnect {
    /// *reconnecting to remove 100 ms…*, *…to go back to Office's own
    /// delay…*.
    pub fn words(self, speaker: &str) -> String {
        match self {
            Reconnect::Remove(n) => format!("reconnecting to remove {n} ms…"),
            Reconnect::Add(n) => format!("reconnecting to add {n} ms…"),
            Reconnect::BackToOwn => format!("reconnecting to go back to {speaker}'s own delay…"),
        }
    }
}

/// What the person did. Amounts are already cut to the speaker's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelayAction {
    Remove(u32),
    Add(u32),
    /// Back to the speaker's own delay (↺).
    BackToOwn,
    UseShortDelay,
    UseSteady,
}

/// What the control remembers between frames: whether the amount field is
/// open and what is typed in it. Owned by the window showing the control.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DelayState {
    amount: Option<Amount>,
    /// Room kept for the line under the buttons while the pointer is on
    /// the control.
    held_note_height: f32,
    /// Where the control was last drawn, to close the field on a click
    /// elsewhere.
    last_rect: Option<Rect>,
    /// Whether the panel's delay line shows its buttons.
    open: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Amount {
    digits: String,
    dir_add: bool,
    focus_pending: bool,
}

impl DelayState {
    /// Open the amount field (the tray's *Other amount…* does this),
    /// focused and empty.
    pub fn open_amount(&mut self) {
        self.amount = Some(Amount { focus_pending: true, ..Default::default() });
    }

    /// Open the amount field, focused, with `digits` typed and Enter set to
    /// remove or add. For pictures and tests.
    pub fn with_amount(digits: &str, add: bool) -> Self {
        DelayState { amount: Some(Amount { digits: digits.into(), dir_add: add, focus_pending: true }), ..Default::default() }
    }

    pub fn amount_is_open(&self) -> bool {
        self.amount.is_some()
    }

    pub fn close_amount(&mut self) {
        self.amount = None;
    }

    /// The panel's delay line, opened (for pictures and tests; the panel
    /// opens closed).
    pub fn opened() -> Self {
        DelayState { open: true, ..Default::default() }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }
}

/// The delay control: `DelayControl::new(&view, &mut state).show(ui)`.
#[must_use = "call .show(ui)"]
pub struct DelayControl<'v, 's> {
    view: &'v DelayView<'v>,
    state: &'s mut DelayState,
    max_width: f32,
}

const GAP_X: f32 = 8.0;
const GAP_Y: f32 = 6.0;
const HEAD_H: f32 = 24.0;

impl<'v, 's> DelayControl<'v, 's> {
    pub fn new(view: &'v DelayView<'v>, state: &'s mut DelayState) -> Self {
        DelayControl { view, state, max_width: f32::INFINITY }
    }

    /// No wider than this (a speaker's page keeps it to 470 points).
    pub fn max_width(mut self, w: f32) -> Self {
        self.max_width = w;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Option<DelayAction> {
        // Every gap inside is the control's own, whatever the spacing of
        // the window around it.
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            self.show_inner(ui)
        })
        .inner
    }

    fn show_inner(self, ui: &mut Ui) -> Option<DelayAction> {
        let DelayControl { view: v, state, max_width } = self;
        let t = Theme::of(ui.ctx());
        let width = ui.available_width().min(max_width);
        let top = ui.cursor().min;
        let mut action = None;

        // A click outside closes the amount field without a change.
        if state.amount.is_some() {
            let pressed_outside = ui.input(|i| {
                i.pointer.any_pressed() && i.pointer.press_origin().is_some_and(|p| state.last_rect.is_some_and(|r| !r.contains(p)))
            });
            if pressed_outside {
                state.amount = None;
            }
        }

        // The head line.
        let (head_rect, head_alloc) = ui.allocate_exact_size(Vec2::new(width, HEAD_H), Sense::hover());
        let id = head_alloc.id;
        let label = ui.painter().layout_no_wrap("Delay".into(), t.font(size::SECONDARY), t.c.text_secondary);
        let figure = ui.painter().layout_no_wrap(v.delay.short(), t.strong(size::CONTROL), t.c.text);
        let change = change_words(v.change_ms).map(|w| ui.painter().layout_no_wrap(format!("· {w}"), t.font(size::CONTROL), t.c.text));
        let mut x = head_rect.left();
        let cy = head_rect.center().y;
        let place = |x: f32, g: &std::sync::Arc<egui::Galley>| egui::pos2(x, cy - g.size().y / 2.0);
        let label_pos = place(x, &label);
        x += label.size().x + 10.0;
        let figure_pos = place(x, &figure);
        x += figure.size().x;
        let change_pos = change.as_ref().map(|g| {
            x += 7.0;
            let p = place(x, g);
            x += g.size().x;
            p
        });
        let editable = v.fixed_with_short_ms.is_none();
        let head_hit = Rect::from_min_max(head_rect.left_top(), egui::pos2(x + 3.0, head_rect.bottom()));
        let head = ui.interact(head_hit, id.with("head"), if editable { Sense::click() } else { Sense::hover() });
        let spoken = format!(
            "Delay on {}: {}, {}.{}",
            v.speaker,
            v.delay.spoken(),
            change_spoken(v.change_ms),
            if editable { " Press Enter to type an amount." } else { "" }
        );
        head.widget_info(|| WidgetInfo::labeled(WidgetType::DragValue, editable, &spoken));
        let head_look = look_as(ui, &head, &spoken);
        if editable && (head_look.hovered || head_look.pressed) {
            ui.painter().rect_filled(head_hit.expand2(Vec2::new(4.0, 0.0)), radius::CONTROL, t.c.hover);
        }
        ui.painter().galley(label_pos, label, t.c.text_secondary);
        ui.painter().galley(figure_pos, figure, t.c.text);
        if let (Some(g), Some(p)) = (change, change_pos) {
            ui.painter().galley(p, g, t.c.text);
        }
        if head_look.focused {
            focus_ring(ui.painter(), head_hit.expand2(Vec2::new(4.0, 0.0)), radius::CONTROL, true, &t);
        }
        if v.change_ms != 0 && editable {
            let r = Rect::from_center_size(egui::pos2(x + 7.0 + 11.0, cy), Vec2::splat(22.0));
            let name = format!("Back to {}'s own delay", v.speaker);
            let reset = super::inside(ui, r, |ui: &mut Ui| IconButton::new(Icon::Reset, &name).size(22.0, 14.0).color(t.c.text_secondary).show(ui));
            if reset.clicked() {
                action = Some(DelayAction::BackToOwn);
            }
        }
        if v.other_amount_link && editable {
            let g = ui.painter().layout_no_wrap("Other amount…".into(), t.font(size::SMALL), t.c.accent);
            let r = Rect::from_min_size(egui::pos2(head_rect.right() - g.size().x, cy - 10.0), Vec2::new(g.size().x, 20.0));
            if super::inside(ui, r, |ui: &mut Ui| Button::link("Other amount…").text_size(size::SMALL).show(ui)).clicked() {
                state.amount = Some(Amount { focus_pending: true, ..Default::default() });
            }
        }

        // Keys on the head line: arrows remove or add, Enter, a digit, − or
        // + open the field.
        if editable && head.has_focus() {
            if let Some(a) = arrow_keys(ui, head.id, v) {
                action = Some(a);
            }
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                state.amount = Some(Amount { focus_pending: true, ..Default::default() });
            }
            let typed: Option<String> = ui.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Text(s) if s.chars().all(|c| c.is_ascii_digit() || "+-−".contains(c)) => Some(s.clone()),
                    _ => None,
                })
            });
            if let Some(s) = typed {
                let mut a = Amount { focus_pending: true, ..Default::default() };
                take_typing(&mut a, &s);
                state.amount = Some(a);
            }
        }
        if editable && head.clicked() {
            state.amount = Some(Amount { focus_pending: true, ..Default::default() });
        }

        ui.add_space(7.0);
        let col_w = ((width - GAP_X) / 2.0).max(0.0);
        let mut amount_line_words: Option<(String, bool)> = None;

        if let Some(short_ms) = v.fixed_with_short_ms {
            let text = format!(
                "On Steady, {} keeps this delay itself, so none of it can be removed yet. For video, use Short delay ({}).",
                v.speaker,
                DelayFigure::about(short_ms).short()
            );
            let clicked = ui
                .allocate_ui(Vec2::new(width, 0.0), |ui| Para::new(&text).mark(Mark::Info).size(size::SMALL).link("Use Short delay").show(ui))
                .inner;
            if clicked == Some(ParaClick::Link(0)) {
                action = Some(DelayAction::UseShortDelay);
            }
        } else if let Some(amount) = state.amount.as_mut() {
            // Row 1: Amount [ 150 ] ms              ✕
            let (row, _) = ui.allocate_exact_size(Vec2::new(width, space::SMALL_BUTTON_HEIGHT), Sense::hover());
            let lab = ui.painter().layout_no_wrap("Amount".into(), t.font(size::SECONDARY), t.c.text_secondary);
            let lab_w = lab.size().x;
            ui.painter().galley(egui::pos2(row.left(), row.center().y - lab.size().y / 2.0), lab, t.c.text_secondary);
            let field = Rect::from_min_size(egui::pos2(row.left() + lab_w + 8.0, row.top()), Vec2::new(68.0, row.height()));
            let field_id = id.with("amount");
            let field_shape = ui.painter().add(egui::Shape::Noop);
            let before = amount.digits.clone();
            let mut text = amount.digits.clone();
            let edit_rect = field.shrink2(Vec2::new(8.0, 2.0));
            let edit = super::inside(ui, edit_rect, |ui: &mut Ui| {
                ui.add_sized(
                    edit_rect.size(),
                    egui::TextEdit::singleline(&mut text)
                    .id(field_id)
                    .frame(false)
                    .font(t.strong(size::CONTROL))
                    .text_color(t.c.text)
                    .margin(egui::Margin::ZERO)
                    .vertical_align(egui::Align::Center),
            )
            });
            edit.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, FIELD_NAME));
            let field_look = look_as(ui, &edit, FIELD_NAME);
            ui.painter().set(field_shape, field_shapes(field, field_look, &t));
            if text != before {
                amount.digits.clear();
                take_typing(amount, &text);
            }
            if amount.focus_pending {
                edit.request_focus();
                amount.focus_pending = false;
            }
            let ms = ui.painter().layout_no_wrap("ms".into(), t.font(size::SECONDARY), t.c.text_secondary);
            ui.painter().galley(egui::pos2(field.right() + 8.0, row.center().y - ms.size().y / 2.0), ms, t.c.text_secondary);
            let close = Rect::from_center_size(egui::pos2(row.right() - 12.0, row.center().y), Vec2::splat(24.0));
            let closed = super::inside(ui, close, |ui: &mut Ui| {
                IconButton::new(Icon::Close, "Close without a change").size(24.0, 14.0).color(t.c.text_secondary).show(ui)
            })
            .clicked();
            ui.add_space(GAP_Y);

            // Row 2: [ Remove 150 ms ] [ Add 150 ms ], the default outlined.
            let typed = amount.digits.parse::<u32>().ok().filter(|n| *n > 0);
            let dir = if amount.dir_add { Direction::Add } else { Direction::Remove };
            let mut chosen = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP_X;
                for d in [Direction::Remove, Direction::Add] {
                    let verb = if d == Direction::Remove { "Remove" } else { "Add" };
                    let allowed = typed.map(|n| v.limits.allowed(v.change_ms, d, n)).unwrap_or(0);
                    let words = match typed {
                        Some(n) => format!("{verb} {n} ms"),
                        None => verb.to_string(),
                    };
                    let r = Button::standard(&words)
                        .small()
                        .width(col_w)
                        .enabled(allowed > 0)
                        .default_action(d == dir && allowed > 0)
                        .accessible_name(typed.map_or(verb.to_string(), |n| format!("{verb} {n} milliseconds of delay")))
                        .show(ui);
                    if r.clicked() {
                        chosen = Some((d, allowed));
                    }
                }
            });
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if enter {
                if let Some(n) = typed {
                    chosen = Some((dir, v.limits.allowed(v.change_ms, dir, n)));
                }
            }
            let escape = ui.input(|i| i.key_pressed(Key::Escape)) && (edit.has_focus() || edit.lost_focus());
            amount_line_words = Some(amount_line(v.speaker, typed, dir, v.change_ms, v.limits));
            if let Some((d, n)) = chosen.filter(|(_, n)| *n > 0) {
                action = Some(if d == Direction::Remove { DelayAction::Remove(n) } else { DelayAction::Add(n) });
                state.amount = None;
            } else if closed || escape {
                state.amount = None;
            }
        } else if let Some(a) = step_buttons(ui, v, width) {
            action = Some(a);
        }

        let amount_words = amount_line_words.map(|(text, warn)| NoteWords {
            mark: if warn { Mark::Warning } else { Mark::None },
            tone: if warn { Tone::Amber } else { Tone::Secondary },
            text,
            links: vec![],
        });
        let note = match amount_words {
            Some(w) => Some(w),
            None if v.fixed_with_short_ms.is_none() => v.note.as_ref().map(|n| n.words(v.speaker, v.limits, v.large_step_ms)),
            None => None,
        };
        if let Some(a) = under_line(ui, v, state, top, width, note, size::CAPTION) {
            action = Some(a);
        }
        state.last_rect = Some(Rect::from_min_max(top, egui::pos2(top.x + width, ui.cursor().min.y)));
        action
    }
}

/// The two columns of step buttons: *Remove* on the left, *Add* on the
/// right, the large step above the small one. A column greys only at its
/// limit; a press that would pass the limit stops at it.
fn step_buttons(ui: &mut Ui, v: &DelayView, width: f32) -> Option<DelayAction> {
    let col_w = ((width - GAP_X) / 2.0).max(0.0);
    let mut action = None;
    for (row, (step, step_name)) in [(v.large_step_ms, "large"), (v.small_step_ms, "small")].into_iter().enumerate() {
        if row > 0 {
            ui.add_space(GAP_Y);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP_X;
            for d in [Direction::Remove, Direction::Add] {
                let verb = if d == Direction::Remove { "Remove" } else { "Add" };
                let at_limit = v.limits.at_limit(v.change_ms, d);
                let words = format!("{verb} {step} ms");
                let r = ui
                    .push_id((step_name, verb), |ui| {
                        Button::standard(&words)
                            .small()
                            .width(col_w)
                            .enabled(!at_limit)
                            .accessible_name(format!("{verb} {step} milliseconds of delay"))
                            .show(ui)
                    })
                    .inner;
                if r.clicked() {
                    let n = v.limits.allowed(v.change_ms, d, step);
                    if n > 0 {
                        action = Some(if d == Direction::Remove { DelayAction::Remove(n) } else { DelayAction::Add(n) });
                    }
                }
            }
        });
    }
    action
}

/// The one line under the buttons, with its links; it keeps its room while
/// the pointer is on the control (which starts at `top`), so nothing below
/// jumps between two presses.
fn under_line(ui: &mut Ui, v: &DelayView, state: &mut DelayState, top: egui::Pos2, width: f32, words: Option<NoteWords>, em: f32) -> Option<DelayAction> {
    let mut action = None;
    let note_top = ui.cursor().min.y;
    if let Some(w) = words {
        ui.add_space(if em > size::CAPTION { 8.0 } else { GAP_Y });
        let mut para = Para::new(&w.text).mark(w.mark).tone(w.tone).size(em);
        for (l, _) in &w.links {
            para = para.link(l);
        }
        if let Some(ParaClick::Link(i)) = ui.allocate_ui(Vec2::new(width, 0.0), |ui| para.show(ui)).inner {
            action = w.links.get(i).map(|(_, what)| match what {
                NoteLink::UseShortDelay => DelayAction::UseShortDelay,
                NoteLink::UseSteady => DelayAction::UseSteady,
                NoteLink::AddLarge => DelayAction::Add(v.limits.allowed(v.change_ms, Direction::Add, v.large_step_ms)),
                NoteLink::BackToOwn => DelayAction::BackToOwn,
            });
        }
    }
    let note_h = ui.cursor().min.y - note_top;
    let whole = Rect::from_min_max(top, egui::pos2(top.x + width, ui.cursor().min.y.max(note_top + state.held_note_height)));
    let pointer_on = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| whole.contains(p));
    let keep = if pointer_on { state.held_note_height.max(note_h) } else { note_h };
    if keep > note_h {
        ui.add_space(keep - note_h);
    }
    state.held_note_height = keep;
    action
}

/// The arrow keys on a focused delay line or head line: ←/→ remove or add
/// the small step, with Shift the large one, with Ctrl 5 ms.
fn arrow_keys(ui: &mut Ui, id: egui::Id, v: &DelayView) -> Option<DelayAction> {
    ui.memory_mut(|m| m.set_focus_lock_filter(id, egui::EventFilter { horizontal_arrows: true, ..Default::default() }));
    // The modifiers held with the key press itself.
    let press = ui.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Key { key: k @ (Key::ArrowLeft | Key::ArrowRight), pressed: true, modifiers, .. } => Some((*k, *modifiers)),
            _ => None,
        })
    });
    let (key, mods) = press?;
    let left = key == Key::ArrowLeft;
    let step = if mods.ctrl || mods.command {
        5
    } else if mods.shift {
        v.large_step_ms
    } else {
        v.small_step_ms
    };
    let dir = if left { Direction::Remove } else { Direction::Add };
    let n = v.limits.allowed(v.change_ms, dir, step);
    (n > 0).then(|| if left { DelayAction::Remove(n) } else { DelayAction::Add(n) })
}

/// The delay in the panel: one line, *Delay  about 0.15 s* with a chevron,
/// that opens the four buttons and one line under them (*50 ms removed ·
/// Reset*). The arrow keys work on the closed line too. Any other amount
/// is typed on the speaker's page ([`DelayControl`]).
#[must_use = "call .show(ui)"]
pub struct DelayLine<'v, 's> {
    view: &'v DelayView<'v>,
    state: &'s mut DelayState,
}

/// The closed line's height.
const LINE_H: f32 = 32.0;

impl<'v, 's> DelayLine<'v, 's> {
    pub fn new(view: &'v DelayView<'v>, state: &'s mut DelayState) -> Self {
        DelayLine { view, state }
    }

    pub fn show(self, ui: &mut Ui) -> Option<DelayAction> {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            self.show_inner(ui)
        })
        .inner
    }

    fn show_inner(self, ui: &mut Ui) -> Option<DelayAction> {
        let DelayLine { view: v, state } = self;
        let t = Theme::of(ui.ctx());
        let width = ui.available_width();
        let top = ui.cursor().min;
        let mut action = None;

        let (line, alloc) = ui.allocate_exact_size(Vec2::new(width, LINE_H), Sense::hover());
        // The line reaches 8 points past the text on each side, like a
        // menu item, so its pointed-at fill has room around the words.
        let hit = line.expand2(Vec2::new(8.0, 0.0));
        let resp = ui.interact(hit, alloc.id.with("line"), Sense::click());
        let spoken = format!("Delay on {}: {}, {}.", v.speaker, v.delay.spoken(), change_spoken(v.change_ms));
        resp.widget_info(|| WidgetInfo::selected(WidgetType::CollapsingHeader, true, state.open, &spoken));
        let l = look_as(ui, &resp, &spoken);
        let p = ui.painter();
        if l.pressed || l.hovered {
            p.rect_filled(hit, radius::CONTROL, if l.pressed { t.c.pressed } else { t.c.hover });
        }
        let label = p.layout_no_wrap("Delay".into(), t.font(size::SECONDARY), t.c.text_secondary);
        let cy = line.center().y;
        let mut x = line.left();
        p.galley(egui::pos2(x, cy - label.size().y / 2.0), label.clone(), t.c.text_secondary);
        x += label.size().x + 6.0;
        let room = (line.right() - 22.0 - x).max(20.0);
        let (words, font, color) = match &v.reconnecting {
            Some(r) => (r.words(v.speaker), t.font(size::SECONDARY), t.c.text_secondary),
            None => (v.delay.short(), t.strong(size::SECONDARY), t.c.text),
        };
        let mut job = egui::text::LayoutJob::simple_singleline(words, font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        let value = ui.fonts(|f| f.layout_job(job));
        p.galley(egui::pos2(x, cy - value.size().y / 2.0), value, color);
        let chevron = if state.open { Icon::ChevronUp } else { Icon::ChevronDown };
        chevron.paint_weight(p, Rect::from_center_size(egui::pos2(line.right() - 7.0, cy), Vec2::splat(14.0)), t.c.text_secondary, 1.6);
        if l.focused {
            focus_ring(p, hit, radius::CONTROL, true, &t);
        }
        if resp.clicked() {
            state.open = !state.open;
        }
        if resp.has_focus() {
            action = arrow_keys(ui, resp.id, v);
        }

        if state.open {
            ui.add_space(2.0);
            let section_top = ui.cursor().min;
            let words = if let Some(short_ms) = v.fixed_with_short_ms {
                Some(NoteWords {
                    mark: Mark::Info,
                    tone: Tone::Secondary,
                    text: format!(
                        "On Steady, {} keeps this delay itself, so none of it can be removed yet. For video, use Short delay ({}).",
                        v.speaker,
                        DelayFigure::about(short_ms).short()
                    ),
                    links: vec![("Use Short delay".into(), NoteLink::UseShortDelay)],
                })
            } else {
                if let Some(a) = step_buttons(ui, v, width) {
                    action = Some(a);
                }
                let reset = ("Reset".to_string(), NoteLink::BackToOwn);
                match (&v.note, change_words(v.change_ms)) {
                    (Some(n), change) => {
                        let mut w = n.words(v.speaker, v.limits, v.large_step_ms);
                        if change.is_some() {
                            w.links.push(reset);
                        }
                        Some(w)
                    }
                    (None, Some(change)) => Some(NoteWords { mark: Mark::None, tone: Tone::Secondary, text: change, links: vec![reset] }),
                    (None, None) => None,
                }
            };
            if let Some(a) = under_line(ui, v, state, section_top, width, words, size::SMALL) {
                action = Some(a);
            }
            ui.add_space(8.0);
        }
        state.last_rect = Some(Rect::from_min_max(top, egui::pos2(top.x + width, ui.cursor().min.y)));
        action
    }
}

/// Take what was typed into the field: digits (four at most), `+` sets
/// Enter to add, `-` or `−` to remove.
fn take_typing(a: &mut Amount, typed: &str) {
    for c in typed.chars() {
        match c {
            '0'..='9' if a.digits.len() < 4 => a.digits.push(c),
            '+' => a.dir_add = true,
            '-' | '−' => a.dir_add = false,
            _ => {}
        }
    }
}

/// The amount field's accessible name.
const FIELD_NAME: &str = "Milliseconds to remove or add";

/// A text field's box: the control fill and border, and a bottom edge that
/// turns into a 2-point accent line while it has the focus.
pub(crate) fn field_shapes(rect: Rect, l: Look, t: &Theme) -> egui::Shape {
    let (h, color) = if l.focused { (2.0, t.c.accent) } else { (1.0, t.c.control_strong.gamma_multiply(0.6)) };
    let fill = if !l.enabled {
        t.c.control.gamma_multiply(0.4)
    } else if l.focused {
        t.c.card
    } else if l.hovered {
        t.c.control_hover
    } else {
        t.c.control
    };
    let bottom = Rect::from_min_max(egui::pos2(rect.left() + 1.0, rect.bottom() - h), egui::pos2(rect.right() - 1.0, rect.bottom()));
    egui::Shape::Vec(vec![
        egui::Shape::rect_filled(rect, radius::CONTROL, fill),
        egui::Shape::rect_stroke(rect.shrink(0.5), radius::CONTROL, egui::Stroke::new(1.0_f32, t.c.control_border)),
        egui::Shape::rect_filled(bottom, 0.0, if l.enabled { color } else { t.c.control_border }),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_figures_as_rows_show_them() {
        // (ms, measured, short, row)
        let table: &[(u32, bool, &str, &str)] = &[
            (200, false, "about 0.2 s", "about 0.2 s delay"),
            (150, false, "about 0.15 s", "about 0.15 s delay"),
            (125, false, "about 0.15 s", "about 0.15 s delay"), // halves round up
            (75, false, "under 0.1 s", "under 0.1 s delay"),
            (99, false, "under 0.1 s", "under 0.1 s delay"),
            (100, false, "about 0.1 s", "about 0.1 s delay"),
            (250, false, "about 0.25 s", "about 0.25 s delay"),
            (300, false, "about 0.3 s", "about 0.3 s delay"),
            (500, false, "about 0.5 s", "about 0.5 s delay"),
            (975, false, "about 1 s", "about 1 s delay"),
            (1700, false, "about 1.7 s", "about 1.7 s delay"),
            (1750, false, "about 1.8 s", "about 1.8 s delay"),
            (1900, false, "about 1.9 s", "about 1.9 s delay"),
            (2000, false, "about 2 s", "about 2 s delay"),
            (210, true, "0.21 s, measured", "0.21 s delay, measured"),
            (1740, true, "1.74 s, measured", "1.74 s delay, measured"),
            (160, true, "0.16 s, measured", "0.16 s delay, measured"),
        ];
        for &(ms, measured, short, row) in table {
            let f = DelayFigure { ms, measured };
            assert_eq!(f.short(), short, "{ms} ms");
            assert_eq!(f.row(), row, "{ms} ms");
        }
    }

    #[test]
    fn spoken_figures() {
        assert_eq!(DelayFigure::about(150).spoken(), "about 0.15 seconds");
        assert_eq!(DelayFigure::about(50).spoken(), "under 0.1 seconds");
        assert_eq!(DelayFigure::measured(160).spoken(), "0.16 seconds, measured");
        assert_eq!(DelayFigure::about(1000).spoken(), "about 1 second");
    }

    #[test]
    fn changes_in_the_buttons_words() {
        assert_eq!(change_words(-50).as_deref(), Some("50 ms removed"));
        assert_eq!(change_words(100).as_deref(), Some("100 ms added"));
        assert_eq!(change_words(0), None);
    }

    const SONOS: Limits = Limits { max_removed_ms: 200, max_added_ms: 1000 };

    #[test]
    fn a_step_past_the_limit_stops_at_it() {
        // Remove 100 ms at 150 ms removed on a Sonos gives 200 ms removed.
        assert_eq!(SONOS.allowed(-150, Direction::Remove, 100), 50);
        assert_eq!(SONOS.allowed(-200, Direction::Remove, 25), 0);
        assert!(SONOS.at_limit(-200, Direction::Remove));
        assert!(!SONOS.at_limit(-200, Direction::Add));
        assert_eq!(SONOS.allowed(900, Direction::Add, 150), 100);
        // A Sendspin speaker: nothing can be removed at its own delay.
        let sendspin = Limits { max_removed_ms: 0, max_added_ms: 800 };
        assert!(sendspin.at_limit(0, Direction::Remove));
        assert_eq!(sendspin.allowed(100, Direction::Remove, 25), 25, "what was added can be removed again");
    }

    #[test]
    fn the_amount_line_says_what_enter_will_do() {
        use Direction::*;
        // (typed, direction, change before, words, amber)
        let table: &[(Option<u32>, Direction, i32, &str, bool)] = &[
            (None, Remove, -50, "Type how many milliseconds, then choose Remove or Add.", false),
            (Some(150), Remove, -50, "Enter removes 150 ms: 200 ms removed in all.", false),
            (Some(40), Add, -50, "Enter adds 40 ms: 10 ms removed in all.", false),
            (Some(100), Add, -50, "Enter adds 100 ms: 50 ms added in all.", false),
            (Some(50), Add, -50, "Enter adds 50 ms: back to Kitchen's own delay.", false),
            (Some(100), Remove, -150, "Kitchen allows at most 200 ms removed, so Enter removes 50 ms.", true),
            (Some(250), Add, 850, "Kitchen allows at most 1000 ms added, so Enter adds 150 ms.", true),
            (Some(25), Remove, -200, "Kitchen allows at most 200 ms removed.", true),
            (Some(0), Remove, 0, "Type how many milliseconds, then choose Remove or Add.", false),
        ];
        for &(typed, dir, change, words, amber) in table {
            assert_eq!(amount_line("Kitchen", typed, dir, change, SONOS), (words.to_string(), amber), "{typed:?} {dir:?} from {change}");
        }
        let sendspin = Limits { max_removed_ms: 0, max_added_ms: 800 };
        assert_eq!(
            amount_line("Desk Speaker", Some(50), Direction::Remove, 0, sendspin),
            ("Desk Speaker already plays with as little delay as it can.".to_string(), true)
        );
    }

    #[test]
    fn typing_takes_digits_and_a_sign_for_enter() {
        let mut a = Amount::default();
        take_typing(&mut a, "+40");
        assert_eq!((a.digits.as_str(), a.dir_add), ("40", true));
        take_typing(&mut a, "-");
        assert!(!a.dir_add);
        take_typing(&mut a, "12345x");
        assert_eq!(a.digits, "4012", "four digits at most");
        let mut b = Amount::default();
        take_typing(&mut b, "−150");
        assert_eq!((b.digits.as_str(), b.dir_add), ("150", false));
    }

    #[test]
    fn notes_in_the_spec_words() {
        let w = |n: DelayNote, speaker: &str, limits: Limits| n.words(speaker, limits, 100).text;
        assert_eq!(
            w(DelayNote::NotAccepted { asked_ms: -300, now_ms: -200 }, "Office", SONOS),
            "Office didn't accept 300 ms removed, so it's back to 200 ms removed."
        );
        assert_eq!(
            w(DelayNote::DroppedOut { had_ms: -200, now_ms: -150 }, "Kitchen", SONOS),
            "Kitchen dropped out with 200 ms removed, so 50 ms went back: 150 ms removed."
        );
        assert_eq!(
            w(DelayNote::SteadyOffer { steady_ms: 2000, short_ms: 300 }, "Office", SONOS),
            "Office is about 2 s late on Steady. For video, use Short delay (about 0.3 s)."
        );
        assert_eq!(w(DelayNote::MostRemoved, "Kitchen", SONOS), "Kitchen allows at most 200 ms removed.");
        assert_eq!(w(DelayNote::MostAdded, "Kitchen", SONOS), "Kitchen allows at most 1000 ms added.");
        let steady = Limits { max_removed_ms: 1000, max_added_ms: 1000 };
        assert_eq!(
            w(DelayNote::SteadyMostRemoved, "Office", steady),
            "On Steady, Office allows at most 1000 ms removed. For less delay, use Short delay."
        );
        assert_eq!(w(DelayNote::AsLittleAsItCan, "Desk Speaker", SONOS), "Desk Speaker already plays with as little delay as it can.");
        assert_eq!(w(DelayNote::Removing { to_go_ms: 75 }, "Kitchen", SONOS), "Removing… 75 ms to go");
        assert_eq!(w(DelayNote::Adding { to_go_ms: 125 }, "Kitchen", SONOS), "Adding… 125 ms to go");
        assert_eq!(w(DelayNote::WillReconnect, "Office", SONOS), "Office reconnects to apply this: a few seconds of silence.");
        assert_eq!(w(DelayNote::Reconnecting, "Office", SONOS), "Reconnecting Office…");
        assert_eq!(
            w(DelayNote::ChoiceSwitched { to_short: true, now_ms: 0, other_ms: -300 }, "Office", SONOS),
            "Short delay starts with nothing removed or added. Steady keeps its 300 ms removed."
        );
        assert_eq!(
            w(DelayNote::ChoiceSwitched { to_short: true, now_ms: -100, other_ms: -300 }, "Office", SONOS),
            "Short delay starts with 100 ms removed. Steady keeps its 300 ms removed."
        );
        assert_eq!(
            w(DelayNote::DropoutsAdd { times: 3, minutes: 10 }, "Kitchen", SONOS),
            "Kitchen dropped out 3 times in 10 minutes. Adding 100 ms may help."
        );
        assert_eq!(
            w(DelayNote::DropoutsSteady { times: 3, minutes: 10 }, "Office", SONOS),
            "Office dropped out 3 times in 10 minutes. Steady may help."
        );
        assert_eq!(
            w(DelayNote::Newcomer, "Kitchen", SONOS),
            "Sound behind the picture? Remove some delay. Sound breaking up? Add some."
        );
    }
}
