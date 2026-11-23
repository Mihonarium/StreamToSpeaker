//! A speaker's delay, as words: the figure every row shows, what was
//! removed or added, the control with its four buttons, the typed amount,
//! the line under the buttons, the confirmation of a shortcut and the
//! command line's replies.
//!
//! The numbers come in as plain data (the speaker's own delay, what was
//! removed or added, the limits, how a change applies). The one piece of
//! arithmetic here is what a press does at a limit, so that every surface
//! and the engine agree on it.

use super::command::{Command, DelayOutcome, DelayStep, Effect, Outcome, Refusal};
use super::facts::{
    Applies, ChoiceFacts, DelayChoice, DelayFacts, DelayNote, DelayRange, DelaySteps, Progress,
    SpeakerDelay,
};
use super::snapshot::{Button, ButtonStyle, Snapshot, Tone};
use super::state::SpeakerId;
use super::text::{t, Lang, Number};

/// A delay figure, before it is put in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Figure {
    /// An estimate: "about 0.15 s".
    About(Number),
    /// An estimate below 0.1 s: "under 0.1 s" (a speaker always keeps some).
    Under(Number),
    /// From a sync check: "0.21 s, measured".
    Measured(Number),
}

/// The figure for a speaker's delay, what was removed or added included.
///
/// Estimates: under 0.1 s below that; below 1 s to the nearest 0.05 s; from
/// 1 s to the nearest 0.1 s; halves round up. Measured: to the hundredth.
pub fn figure(d: SpeakerDelay) -> Figure {
    let total = (d.own_ms as i64 + d.change_ms as i64).max(0);
    if d.measured {
        return Figure::Measured(Number { value: (total + 5) / 10, scale: 2 }.normalized());
    }
    estimate(total as u64)
}

fn estimate(total_ms: u64) -> Figure {
    if total_ms < 100 {
        Figure::Under(Number { value: 1, scale: 1 })
    } else if total_ms < 1000 {
        let rounded = (total_ms + 25) / 50 * 50;
        Figure::About(Number { value: rounded as i64 / 10, scale: 2 }.normalized())
    } else {
        let rounded = (total_ms + 50) / 100 * 100;
        Figure::About(Number { value: rounded as i64 / 100, scale: 1 }.normalized())
    }
}

/// "about 0.15 s".
pub fn figure_text(lang: Lang, f: Figure) -> String {
    match f {
        Figure::About(n) => t!(lang, FigureAbout, seconds = n),
        Figure::Under(n) => t!(lang, FigureUnder, seconds = n),
        Figure::Measured(n) => t!(lang, FigureMeasured, seconds = n),
    }
}

/// "about 0.15 s delay", as a row's second line says it.
pub fn row_delay_text(lang: Lang, f: Figure) -> String {
    match f {
        Figure::About(n) => t!(lang, RowDelayAbout, seconds = n),
        Figure::Under(n) => t!(lang, RowDelayUnder, seconds = n),
        Figure::Measured(n) => t!(lang, RowDelayMeasured, seconds = n),
    }
}

/// "about 0.15 seconds", for screen readers.
pub fn spoken_figure(lang: Lang, f: Figure) -> String {
    match f {
        Figure::About(n) => t!(lang, SpokenFigureAbout, seconds = n),
        Figure::Under(n) => t!(lang, SpokenFigureUnder, seconds = n),
        Figure::Measured(n) => t!(lang, SpokenFigureMeasured, seconds = n),
    }
}

/// "about 0.15 second delay", a row's delay for screen readers.
pub fn spoken_row_delay(lang: Lang, f: Figure) -> String {
    match f {
        Figure::About(n) => t!(lang, SpokenRowDelayAbout, seconds = n),
        Figure::Under(n) => t!(lang, SpokenRowDelayUnder, seconds = n),
        Figure::Measured(n) => t!(lang, SpokenRowDelayMeasured, seconds = n),
    }
}

/// "50 ms removed" or "100 ms added"; nothing at the speaker's own delay.
/// Never a signed number.
pub fn change_text(lang: Lang, change_ms: i32) -> Option<String> {
    match change_ms {
        0 => None,
        c if c < 0 => Some(t!(lang, ChangeRemoved, ms = c.unsigned_abs())),
        c => Some(t!(lang, ChangeAdded, ms = c)),
    }
}

fn spoken_change(lang: Lang, change_ms: i32, speaker: &str) -> String {
    match change_ms {
        0 => t!(lang, SpokenOwnDelay, speaker = speaker),
        c if c < 0 => t!(lang, SpokenRemoved, ms = c.unsigned_abs()),
        c => t!(lang, SpokenAdded, ms = c),
    }
}

/// What a press does: the change after it, stopped at the speaker's limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pressed {
    pub before_ms: i32,
    pub after_ms: i32,
    /// The step wanted to go past a limit and stopped at it.
    pub stopped_at_limit: bool,
}

/// What `step` does to a change of `before_ms` within `range`. A step that
/// would pass a limit stops at it; one that starts beyond a limit (the
/// range shrank) never moves further past it.
pub fn press(range: DelayRange, before_ms: i32, step: DelayStep) -> Pressed {
    let lower = -(range.max_removed_ms as i64);
    let upper = range.max_added_ms as i64;
    let before = before_ms as i64;
    let (target, after) = match step {
        DelayStep::Remove(ms) => {
            let target = before - ms as i64;
            (target, if target >= lower { target } else { lower.min(before) })
        }
        DelayStep::Add(ms) => {
            let target = before + ms as i64;
            (target, if target <= upper { target } else { upper.max(before) })
        }
        DelayStep::SetTotal(total) => {
            let target = total as i64;
            (target, target.clamp(lower, upper))
        }
        DelayStep::Reset => (0, 0),
    };
    Pressed { before_ms, after_ms: after as i32, stopped_at_limit: after != target }
}

/// The numbers of the delay of the speaker being played on, for surfaces
/// that compute from them (the typed amount, the API).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelayView {
    pub speaker: SpeakerId,
    pub name: String,
    pub delay: SpeakerDelay,
    pub range: DelayRange,
    pub applies: Applies,
    pub progress: Progress,
    pub steps: DelaySteps,
    pub choice: Option<ChoiceFacts>,
}

impl DelayView {
    /// The API's `applies`: `live`, `reconnect` or `none`.
    pub fn applies_word(&self) -> &'static str {
        match self.applies {
            Applies::Live => "live",
            Applies::Reconnect => "reconnect",
            Applies::Fixed => "none",
        }
    }

    /// The API's `applying_ms_left`.
    pub fn applying_ms_left(&self) -> u32 {
        match self.progress {
            Progress::Live { to_go_ms } => to_go_ms.unsigned_abs(),
            _ => 0,
        }
    }

    /// The API's `reconnecting`.
    pub fn reconnecting(&self) -> bool {
        matches!(self.progress, Progress::ReconnectPending | Progress::Reconnecting)
    }
}

/// The delay control, in words.
#[derive(Clone, Debug, PartialEq)]
pub struct DelayControl {
    /// "Delay".
    pub label: String,
    /// "about 0.15 s".
    pub figure: String,
    /// "50 ms removed"; nothing at the speaker's own delay.
    pub change: Option<String>,
    /// ↺, named "Back to Kitchen’s own delay"; only while something is
    /// removed or added.
    pub reset: Option<Button>,
    pub body: DelayBody,
    /// The one small line under the buttons, when there is something to say.
    pub line: Option<DelayLine>,
    /// What a screen reader says for the head line.
    pub spoken: String,
    /// The new total, announced after each change ("75 milliseconds
    /// removed").
    pub spoken_total: String,
    /// Announced when a change that was still applying is done.
    pub spoken_done: String,
    /// Over the phone's two columns: "Sound behind the picture?" and "Sound
    /// breaking up?".
    pub column_headings: [String; 2],
    /// *Other amount…*, where a surface shows it as a link (a speaker's
    /// page).
    pub other_amount: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DelayBody {
    /// Remove on the left, Add on the right, the large step above the small.
    Buttons { remove_large: Button, remove_small: Button, add_large: Button, add_small: Button },
    /// The delay can't be changed here yet; one line and its remedy instead.
    Fixed { text: String, action: Button },
}

#[derive(Clone, Debug, PartialEq)]
pub struct DelayLine {
    pub text: String,
    pub tone: Tone,
    pub action: Option<Button>,
}

fn delay_button(lang: Lang, speaker: &SpeakerId, step: DelayStep, enabled: bool) -> Button {
    let (label, spoken) = match step {
        DelayStep::Remove(ms) => (t!(lang, RemoveMs, ms = ms), t!(lang, SpokenRemoveMs, ms = ms)),
        DelayStep::Add(ms) => (t!(lang, AddMs, ms = ms), t!(lang, SpokenAddMs, ms = ms)),
        DelayStep::SetTotal(_) | DelayStep::Reset => unreachable!("not a delay button"),
    };
    Button {
        label,
        spoken: Some(spoken),
        command: Command::ChangeDelay { speaker: speaker.clone(), step },
        style: ButtonStyle::Secondary,
        enabled,
    }
}

fn use_choice(lang: Lang, speaker: &SpeakerId, choice: DelayChoice) -> Button {
    Button::link(
        match choice {
            DelayChoice::Short => t!(lang, UseShortDelay),
            DelayChoice::Steady => t!(lang, UseSteady),
        },
        Command::UseDelayChoice { speaker: speaker.clone(), choice },
    )
}

/// The delay control for one speaker. The panel, the phone and a speaker's
/// page all draw this.
pub fn delay_control(
    lang: Lang,
    name: &str,
    delay: SpeakerDelay,
    facts: &DelayFacts,
    steps: DelaySteps,
    hint_retired: bool,
) -> DelayControl {
    let id = &facts.speaker;
    let fig = figure(delay);
    let fixed = facts.applies == Applies::Fixed;
    let change = if fixed { 0 } else { delay.change_ms };
    let change_words = change_text(lang, change);
    let at_removed_limit = change <= -(facts.range.max_removed_ms as i32);
    let at_added_limit = change >= facts.range.max_added_ms as i32;
    let short_figure = |c: &ChoiceFacts| figure_text(lang, estimate(c.short_ms as u64));

    let body = if fixed {
        let short = facts.choice.as_ref().map(short_figure).unwrap_or_default();
        DelayBody::Fixed {
            text: t!(lang, FixedOnSteady, speaker = name, short = &short),
            action: use_choice(lang, id, DelayChoice::Short),
        }
    } else {
        DelayBody::Buttons {
            remove_large: delay_button(lang, id, DelayStep::Remove(steps.large_ms), !at_removed_limit),
            remove_small: delay_button(lang, id, DelayStep::Remove(steps.small_ms), !at_removed_limit),
            add_large: delay_button(lang, id, DelayStep::Add(steps.large_ms), !at_added_limit),
            add_small: delay_button(lang, id, DelayStep::Add(steps.small_ms), !at_added_limit),
        }
    };

    let line = if fixed { None } else { delay_line(lang, name, change, facts, steps, hint_retired) };

    let spoken_fig = spoken_figure(lang, fig);
    let spoken = match change {
        0 => t!(lang, SpokenDelayOwn, speaker = name, figure = &spoken_fig),
        c => t!(
            lang,
            SpokenDelay,
            speaker = name,
            figure = &spoken_fig,
            change = &spoken_change(lang, c, name)
        ),
    };
    let spoken_total = spoken_change(lang, change, name);
    DelayControl {
        label: t!(lang, DelayLabel),
        figure: figure_text(lang, fig),
        change: change_words,
        reset: (change != 0).then(|| Button {
            label: t!(lang, BackToOwn, speaker = name),
            spoken: None,
            command: Command::ChangeDelay { speaker: id.clone(), step: DelayStep::Reset },
            style: ButtonStyle::Icon,
            enabled: true,
        }),
        body,
        line,
        spoken,
        spoken_done: t!(lang, SpokenDone, result = &spoken_total),
        spoken_total,
        column_headings: [t!(lang, SoundBehind), t!(lang, SoundBreakingUp)],
        other_amount: t!(lang, OtherAmount),
    }
}

/// The line under the buttons: the first that applies of the numbered
/// cases below.
fn delay_line(
    lang: Lang,
    name: &str,
    change: i32,
    facts: &DelayFacts,
    steps: DelaySteps,
    hint_retired: bool,
) -> Option<DelayLine> {
    let id = &facts.speaker;
    let plain = |text: String, tone: Tone| Some(DelayLine { text, tone, action: None });
    let on_steady = facts.choice.map(|c| c.current) == Some(DelayChoice::Steady);

    // 1. A change that didn't hold.
    match facts.note {
        Some(DelayNote::NotAccepted { tried_ms, back_to_ms }) => {
            let tried = change_text(lang, tried_ms).unwrap_or_default();
            let text = match change_text(lang, back_to_ms) {
                Some(total) => t!(lang, NotAcceptedBack, speaker = name, tried = &tried, total = &total),
                None => t!(lang, NotAcceptedOwn, speaker = name, tried = &tried),
            };
            return plain(text, Tone::Warning);
        }
        Some(DelayNote::DroppedOut { with_ms, back_to_ms }) => {
            let was = change_text(lang, with_ms).unwrap_or_default();
            let text = match change_text(lang, back_to_ms) {
                Some(total) => t!(
                    lang,
                    DroppedOutBack,
                    speaker = name,
                    was = &was,
                    ms = (back_to_ms - with_ms).unsigned_abs(),
                    total = &total
                ),
                None => t!(lang, DroppedOutOwn, speaker = name, was = &was),
            };
            return plain(text, Tone::Warning);
        }
        _ => {}
    }

    // 2. Remove pressed on Steady: offer Short delay.
    if let (Some(DelayNote::SteadyRemovePressed), Some(c)) = (facts.note, facts.choice) {
        if on_steady {
            let late = figure_text(lang, estimate(c.steady_ms as u64));
            let short = figure_text(lang, estimate(c.short_ms as u64));
            return Some(DelayLine {
                text: t!(lang, SteadyRemoveHint, speaker = name, late = &late, short = &short),
                tone: Tone::Info,
                action: Some(use_choice(lang, id, DelayChoice::Short)),
            });
        }
    }

    // 3. At a limit.
    let max_removed = facts.range.max_removed_ms;
    let max_added = facts.range.max_added_ms;
    if change <= -(max_removed as i32) {
        if max_removed == 0 {
            return plain(t!(lang, NoLessDelay, speaker = name), Tone::Plain);
        }
        if on_steady {
            return Some(DelayLine {
                text: t!(lang, LimitSteadyRemoved, speaker = name, max = max_removed),
                tone: Tone::Warning,
                action: Some(use_choice(lang, id, DelayChoice::Short)),
            });
        }
        return plain(t!(lang, LimitRemoved, speaker = name, max = max_removed), Tone::Warning);
    }
    if change >= max_added as i32 {
        return plain(t!(lang, LimitAdded, speaker = name, max = max_added), Tone::Warning);
    }

    // 4. While a change is applied.
    match facts.progress {
        Progress::Live { to_go_ms } if to_go_ms < 0 => {
            return plain(t!(lang, Removing, ms = to_go_ms.unsigned_abs()), Tone::Plain)
        }
        Progress::Live { to_go_ms } if to_go_ms > 0 => {
            return plain(t!(lang, Adding, ms = to_go_ms), Tone::Plain)
        }
        Progress::ReconnectPending => {
            return plain(t!(lang, ReconnectPending, speaker = name), Tone::Busy)
        }
        Progress::Reconnecting => {
            return plain(t!(lang, ReconnectRunning, speaker = name), Tone::Busy)
        }
        _ => {}
    }

    // 5. Right after a switch of delay choice.
    if let (Some(DelayNote::ChoiceSwitched { other_kept_ms }), Some(c)) = (facts.note, facts.choice) {
        let now = match (c.current, change_text(lang, change)) {
            (DelayChoice::Short, None) => t!(lang, SwitchedShortNothing),
            (DelayChoice::Short, Some(total)) => t!(lang, SwitchedShortWith, total = &total),
            (DelayChoice::Steady, None) => t!(lang, SwitchedSteadyNothing),
            (DelayChoice::Steady, Some(total)) => t!(lang, SwitchedSteadyWith, total = &total),
        };
        let other = change_text(lang, other_kept_ms).map(|total| match c.current {
            DelayChoice::Short => t!(lang, KeepsSteady, total = &total),
            DelayChoice::Steady => t!(lang, KeepsShort, total = &total),
        });
        let text = match other {
            Some(other) => format!("{now} {other}"),
            None => now,
        };
        return plain(text, Tone::Plain);
    }

    // 6. Dropouts the app saw.
    if let Some(DelayNote::Dropouts { count }) = facts.note {
        let on_short = facts.choice.map(|c| c.current) == Some(DelayChoice::Short);
        return Some(if on_short {
            DelayLine {
                text: t!(lang, DropoutsSteady, speaker = name, count = count),
                tone: Tone::Info,
                action: Some(use_choice(lang, id, DelayChoice::Steady)),
            }
        } else {
            let mut add = delay_button(lang, id, DelayStep::Add(steps.large_ms), true);
            add.style = ButtonStyle::Link;
            DelayLine {
                text: t!(lang, DropoutsAdd, speaker = name, count = count, ms = steps.large_ms),
                tone: Tone::Info,
                action: Some(add),
            }
        });
    }

    // 7. At the speaker's own delay, before anything was ever changed.
    if change == 0 && !hint_retired {
        return plain(t!(lang, SymptomHint), Tone::Plain);
    }
    None
}

/// The key that sends a typed amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enter {
    Remove,
    Add,
}

/// The amount field that replaces the buttons while typing.
#[derive(Clone, Debug, PartialEq)]
pub struct AmountField {
    /// "Amount".
    pub label: String,
    /// "ms".
    pub unit: String,
    /// The field's name for screen readers.
    pub spoken: String,
    /// ✕, which closes the field without a change.
    pub close: String,
    pub remove: Button,
    pub add: Button,
    /// The panel's line: what Enter will do, before anything changes.
    pub line: String,
    pub tone: Tone,
    /// The phone's lines: both results, since there is no Enter there.
    pub phone_lines: Vec<String>,
}

/// The amount field for `typed` milliseconds (`None` or 0 before a digit is
/// typed), with Enter pressing `enter`.
pub fn amount_field(lang: Lang, view: &DelayView, typed: Option<u32>, enter: Enter) -> AmountField {
    let name = view.name.as_str();
    let change = view.delay.change_ms;
    let mk = |label: String, step: DelayStep, enabled: bool, outlined: bool| Button {
        label,
        spoken: None,
        command: Command::ChangeDelay { speaker: view.speaker.clone(), step },
        style: if outlined { ButtonStyle::Primary } else { ButtonStyle::Secondary },
        enabled,
    };
    let base = |remove, add| AmountField {
        label: t!(lang, AmountLabel),
        unit: t!(lang, MsUnit),
        spoken: t!(lang, SpokenAmountField),
        close: t!(lang, Cancel),
        remove,
        add,
        line: String::new(),
        tone: Tone::Plain,
        phone_lines: Vec::new(),
    };
    let Some(ms) = typed.filter(|&ms| ms > 0) else {
        let mut field = base(
            mk(t!(lang, RemoveBare), DelayStep::Remove(0), false, false),
            mk(t!(lang, AddBare), DelayStep::Add(0), false, false),
        );
        field.line = t!(lang, AmountPrompt);
        field.phone_lines = vec![field.line.clone()];
        return field;
    };
    let removed = press(view.range, change, DelayStep::Remove(ms));
    let added = press(view.range, change, DelayStep::Add(ms));
    let mut field = base(
        mk(
            t!(lang, RemoveMs, ms = ms),
            DelayStep::Remove(ms),
            removed.after_ms != change,
            enter == Enter::Remove,
        ),
        mk(t!(lang, AddMs, ms = ms), DelayStep::Add(ms), added.after_ms != change, enter == Enter::Add),
    );
    let max_removed = view.range.max_removed_ms;
    let max_added = view.range.max_added_ms;
    let (line, tone) = match enter {
        Enter::Remove => enter_line(lang, name, ms, removed, true, max_removed),
        Enter::Add => enter_line(lang, name, ms, added, false, max_added),
    };
    field.line = line;
    field.tone = tone;
    field.phone_lines = vec![
        phone_line(lang, name, ms, removed, true, max_removed),
        phone_line(lang, name, ms, added, false, max_added),
    ];
    field
}

fn enter_line(lang: Lang, name: &str, ms: u32, p: Pressed, remove: bool, max: u32) -> (String, Tone) {
    let moved = (p.after_ms - p.before_ms).unsigned_abs();
    if moved == 0 {
        let text = if remove {
            t!(lang, EnterNothingRemoved, speaker = name, max = max)
        } else {
            t!(lang, EnterNothingAdded, speaker = name, max = max)
        };
        return (text, Tone::Warning);
    }
    if p.stopped_at_limit {
        let text = if remove {
            t!(lang, EnterLimitRemoved, speaker = name, max = max, ms = moved)
        } else {
            t!(lang, EnterLimitAdded, speaker = name, max = max, ms = moved)
        };
        return (text, Tone::Warning);
    }
    let text = match (change_text(lang, p.after_ms), remove) {
        (None, true) => t!(lang, EnterRemovesToOwn, ms = ms, speaker = name),
        (None, false) => t!(lang, EnterAddsToOwn, ms = ms, speaker = name),
        (Some(total), true) => t!(lang, EnterRemoves, ms = ms, total = &total),
        (Some(total), false) => t!(lang, EnterAdds, ms = ms, total = &total),
    };
    (text, Tone::Plain)
}

fn phone_line(lang: Lang, name: &str, ms: u32, p: Pressed, remove: bool, max: u32) -> String {
    if p.after_ms == p.before_ms {
        return if remove && max == 0 {
            t!(lang, NoLessDelay, speaker = name)
        } else if remove {
            t!(lang, LimitRemoved, speaker = name, max = max)
        } else {
            t!(lang, LimitAdded, speaker = name, max = max)
        };
    }
    match (change_text(lang, p.after_ms), remove, p.stopped_at_limit) {
        (None, true, _) => t!(lang, PhoneRemoveToOwn, ms = ms, speaker = name),
        (None, false, _) => t!(lang, PhoneAddToOwn, ms = ms, speaker = name),
        (Some(total), true, true) => t!(lang, PhoneRemoveMost, ms = ms, total = &total, speaker = name),
        (Some(total), false, true) => t!(lang, PhoneAddMost, ms = ms, total = &total, speaker = name),
        (Some(total), true, false) => t!(lang, PhoneRemoveResult, ms = ms, total = &total),
        (Some(total), false, false) => t!(lang, PhoneAddResult, ms = ms, total = &total),
    }
}

/// The confirmation a delay shortcut shows by the clock for 2 seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirmation {
    /// Small, on top: the speaker's name (or the app's).
    pub over: String,
    /// Large: the total so far.
    pub big: String,
    /// Small, below: the press, or how it applies.
    pub small: String,
    /// Amber: a press that stopped at, or was stopped by, a limit.
    pub warning: bool,
}

/// The confirmation for the outcome of a shortcut's press.
pub fn confirmation(lang: Lang, outcome: &Outcome) -> Confirmation {
    let snap = &outcome.snapshot;
    match &outcome.result {
        Ok(Effect::Delay(d)) => {
            let view = snap.delay.as_ref().filter(|v| v.speaker == d.speaker);
            let name = snap.speaker_name(&d.speaker);
            let moved = (d.after_ms - d.before_ms).unsigned_abs();
            let removing = step_removes(d);
            if moved == 0 {
                let max_removed = view.map(|v| v.range.max_removed_ms).unwrap_or(0);
                let max_added = view.map(|v| v.range.max_added_ms).unwrap_or(0);
                let (big, small) = if removing && max_removed == 0 {
                    (t!(lang, ConfirmCantRemove), t!(lang, ConfirmNoLess, speaker = name))
                } else if removing {
                    (
                        t!(lang, ConfirmCantRemove),
                        t!(lang, ConfirmLimitRemoved, speaker = name, max = max_removed),
                    )
                } else {
                    (t!(lang, ConfirmCantAdd), t!(lang, ConfirmLimitAdded, speaker = name, max = max_added))
                };
                return Confirmation { over: name.to_string(), big, small, warning: true };
            }
            let big = change_text(lang, d.after_ms)
                .unwrap_or_else(|| t!(lang, ConfirmOwn, speaker = name));
            let reconnects = view.map(|v| v.applies) == Some(Applies::Reconnect);
            let small = if d.stopped_at_limit {
                if removing {
                    t!(lang, ConfirmRemovedMost, ms = moved, speaker = name)
                } else {
                    t!(lang, ConfirmAddedMost, ms = moved, speaker = name)
                }
            } else if reconnects {
                t!(lang, ConfirmReconnecting)
            } else if d.after_ms < d.before_ms {
                t!(lang, ConfirmRemoved, ms = moved)
            } else {
                t!(lang, ConfirmAdded, ms = moved)
            };
            Confirmation { over: name.to_string(), big, small, warning: d.stopped_at_limit }
        }
        Err(Refusal::FixedOnSteady { speaker }) => {
            let name = snap.speaker_name(speaker);
            Confirmation {
                over: name.to_string(),
                big: t!(lang, ConfirmNoSteady),
                small: t!(lang, ConfirmUseShortOnPage, speaker = name),
                warning: false,
            }
        }
        Err(Refusal::MusicAssistantDelay) => Confirmation {
            over: t!(lang, AppName),
            big: snap.short_status.clone(),
            small: t!(lang, ConfirmMaDelay),
            warning: false,
        },
        Ok(Effect::Done) | Err(Refusal::NoDelayHere) | Err(Refusal::UnknownPlace { .. }) => {
            Confirmation {
                over: t!(lang, AppName),
                big: snap.short_status.clone(),
                small: t!(lang, ConfirmNoSpeakerDelay),
                warning: false,
            }
        }
    }
}

fn step_removes(d: &DelayOutcome) -> bool {
    match d.step {
        DelayStep::Remove(_) => true,
        DelayStep::Add(_) => false,
        DelayStep::SetTotal(total) => total < d.before_ms,
        DelayStep::Reset => d.before_ms > 0,
    }
}

/// A reply printed by the command line, with its exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliReply {
    pub text: String,
    /// 0 done, 3 refused (1, 2 and 4 belong to the console program: not
    /// running, bad arguments, no reply).
    pub exit: u8,
}

pub const EXIT_DONE: u8 = 0;
pub const EXIT_NOT_RUNNING: u8 = 1;
pub const EXIT_BAD_ARGUMENTS: u8 = 2;
pub const EXIT_REFUSED: u8 = 3;
pub const EXIT_NO_REPLY: u8 = 4;

/// The command line's reply to a command's outcome.
pub fn cli_reply(lang: Lang, outcome: &Outcome) -> CliReply {
    let snap = &outcome.snapshot;
    let refused = |text: String| CliReply { text, exit: EXIT_REFUSED };
    match &outcome.result {
        Ok(Effect::Done) => CliReply { text: lang.sentence(&snap.status), exit: EXIT_DONE },
        Ok(Effect::Delay(d)) => {
            let name = snap.speaker_name(&d.speaker);
            let view = snap.delay.as_ref().filter(|v| v.speaker == d.speaker);
            let moved = (d.after_ms - d.before_ms).unsigned_abs();
            if moved == 0 && !matches!(d.step, DelayStep::Reset | DelayStep::SetTotal(_)) {
                let max_removed = view.map(|v| v.range.max_removed_ms).unwrap_or(0);
                let max_added = view.map(|v| v.range.max_added_ms).unwrap_or(0);
                let text = match d.step {
                    DelayStep::Remove(_) if max_removed == 0 => t!(lang, CliNoLess, speaker = name),
                    DelayStep::Remove(_) => t!(lang, CliCantRemove, speaker = name, max = max_removed),
                    _ => t!(lang, CliCantAdd, speaker = name, max = max_added),
                };
                return refused(text);
            }
            let total = change_text(lang, d.after_ms);
            let head = match (d.step, total) {
                (DelayStep::Reset, _) | (DelayStep::SetTotal(_), None) => {
                    t!(lang, CliBackToOwn, speaker = name)
                }
                (DelayStep::SetTotal(_), Some(total)) if d.stopped_at_limit => {
                    t!(lang, CliSetToMost, speaker = name, total = &total)
                }
                (DelayStep::SetTotal(_), Some(total)) => t!(lang, CliSetTo, speaker = name, total = &total),
                (DelayStep::Remove(_), None) => t!(lang, CliRemovedToOwn, speaker = name, ms = moved),
                (DelayStep::Add(_), None) => t!(lang, CliAddedToOwn, speaker = name, ms = moved),
                (DelayStep::Remove(_), Some(total)) if d.stopped_at_limit => {
                    t!(lang, CliRemovedMost, speaker = name, ms = moved, total = &total)
                }
                (DelayStep::Add(_), Some(total)) if d.stopped_at_limit => {
                    t!(lang, CliAddedMost, speaker = name, ms = moved, total = &total)
                }
                (DelayStep::Remove(_), Some(total)) => {
                    t!(lang, CliRemoved, speaker = name, ms = moved, total = &total)
                }
                (DelayStep::Add(_), Some(total)) => {
                    t!(lang, CliAdded, speaker = name, ms = moved, total = &total)
                }
            };
            // After the dash, where the speaker is now: still applying. A
            // reply that ends at a limit, or back at the speaker's own
            // delay, already says where it is.
            let says_where = d.stopped_at_limit || d.step == DelayStep::Reset || d.after_ms == 0;
            let suffix = view.filter(|_| !says_where).and_then(|v| match v.progress {
                Progress::Live { to_go_ms } if to_go_ms < 0 => Some(t!(lang, CliRemovingSuffix)),
                Progress::Live { to_go_ms } if to_go_ms > 0 => Some(t!(lang, CliAddingSuffix)),
                Progress::ReconnectPending | Progress::Reconnecting => {
                    Some(t!(lang, CliReconnectingSuffix))
                }
                _ => None,
            });
            let text = match suffix {
                Some(s) => format!("{head} {s}"),
                None => head,
            };
            CliReply { text, exit: EXIT_DONE }
        }
        Err(refusal) => refused(refusal_text(lang, refusal, snap)),
    }
}

/// The words for a refused command (the command line, and the API's
/// `message`).
pub fn refusal_text(lang: Lang, refusal: &Refusal, snap: &Snapshot) -> String {
    match refusal {
        Refusal::NoDelayHere => t!(lang, RefuseNoDelay, status = &snap.short_status),
        Refusal::MusicAssistantDelay => t!(lang, RefuseMaDelay, status = &snap.short_status),
        Refusal::FixedOnSteady { speaker } => {
            t!(lang, RefuseSteady, speaker = snap.speaker_name(speaker))
        }
        Refusal::UnknownPlace { name } => t!(lang, RefuseUnknownPlace, name = name.as_str()),
    }
}

/// `stream-to-speaker delay`: the delay of the speaker being played on and
/// how far it can go.
pub fn cli_delay_info(lang: Lang, snap: &Snapshot) -> CliReply {
    let Some(view) = snap.delay.as_ref() else {
        return CliReply { text: refusal_text(lang, &snap.delay_refusal(), snap), exit: EXIT_REFUSED };
    };
    let fig = figure_text(lang, figure(view.delay));
    let fixed = view.applies == Applies::Fixed;
    let head = match change_text(lang, view.delay.change_ms).filter(|_| !fixed) {
        Some(change) => t!(lang, CliDelayHeadChange, speaker = &view.name, figure = &fig, change = &change),
        None => t!(lang, CliDelayHead, speaker = &view.name, figure = &fig),
    };
    let range = if fixed {
        t!(lang, CliRangeNone)
    } else if view.range.max_removed_ms == 0 {
        t!(lang, CliRangeAddOnly, added = view.range.max_added_ms)
    } else {
        t!(lang, CliRangeBoth, removed = view.range.max_removed_ms, added = view.range.max_added_ms)
    };
    CliReply { text: format!("{head} {range}"), exit: EXIT_DONE }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN: Lang = Lang::English;

    fn est(own: u32, change: i32) -> String {
        figure_text(EN, figure(SpeakerDelay { own_ms: own, measured: false, change_ms: change }))
    }

    #[test]
    fn figures_round_as_the_design_says() {
        // (own ms, change ms, figure)
        let table: &[(u32, i32, &str)] = &[
            (200, 0, "about 0.2 s"),
            (200, -50, "about 0.15 s"),
            (200, -75, "about 0.15 s"), // 0.125 s: halves round up
            (200, -125, "under 0.1 s"),
            (200, -200, "under 0.1 s"),
            (200, 100, "about 0.3 s"),
            (300, -50, "about 0.25 s"),
            (2000, 0, "about 2 s"),
            (2000, -100, "about 1.9 s"),
            (2000, -300, "about 1.7 s"),
            (2000, -200, "about 1.8 s"),
            (500, 0, "about 0.5 s"),
            (1000, -25, "about 1 s"), // 0.975 s rounds to 1.00
            (1000, 50, "about 1.1 s"), // 1.05 s: halves round up
        ];
        for &(own, change, want) in table {
            assert_eq!(est(own, change), want, "own {own} change {change}");
        }
        let measured = |own, change| {
            figure_text(EN, figure(SpeakerDelay { own_ms: own, measured: true, change_ms: change }))
        };
        assert_eq!(measured(210, 0), "0.21 s, measured");
        assert_eq!(measured(210, -50), "0.16 s, measured");
        assert_eq!(measured(1740, 0), "1.74 s, measured");
        assert_eq!(
            row_delay_text(EN, figure(SpeakerDelay { own_ms: 210, measured: true, change_ms: 0 })),
            "0.21 s delay, measured"
        );
    }

    #[test]
    fn change_words_never_carry_a_sign() {
        assert_eq!(change_text(EN, -50).as_deref(), Some("50 ms removed"));
        assert_eq!(change_text(EN, 100).as_deref(), Some("100 ms added"));
        assert_eq!(change_text(EN, 0), None);
    }

    #[test]
    fn a_press_stops_at_the_limit() {
        let sonos = DelayRange { max_removed_ms: 200, max_added_ms: 1000 };
        // (before, step, after, stopped)
        let table: &[(i32, DelayStep, i32, bool)] = &[
            (-50, DelayStep::Remove(25), -75, false),
            (-150, DelayStep::Remove(100), -200, true),
            (-200, DelayStep::Remove(25), -200, true),
            (-50, DelayStep::Add(100), 50, false),
            (-50, DelayStep::Add(25), -25, false),
            (950, DelayStep::Add(100), 1000, true),
            (0, DelayStep::SetTotal(-150), -150, false),
            (0, DelayStep::SetTotal(-500), -200, true),
            (-125, DelayStep::Reset, 0, false),
            // A range that shrank never pushes a press further past it.
            (-300, DelayStep::Remove(25), -300, true),
            (-300, DelayStep::Add(25), -275, false),
        ];
        for &(before, step, after, stopped) in table {
            let p = press(sonos, before, step);
            assert_eq!((p.after_ms, p.stopped_at_limit), (after, stopped), "{before} {step:?}");
        }
        let sendspin = DelayRange { max_removed_ms: 0, max_added_ms: 800 };
        assert_eq!(press(sendspin, 0, DelayStep::Remove(25)).after_ms, 0);
    }
}
