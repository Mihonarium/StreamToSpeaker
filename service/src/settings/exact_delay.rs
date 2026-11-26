//! The speaker page edits the delay shown for the selected way.
use crate::model::{command::DelayStep, facts::SpeakerDelay, text::Lang};
use crate::settings::words::w;
use crate::ui::{button::{Button, IconButton}, icon::Icon, inputs::TextField, theme::{size, Theme}};
use egui::{text::{CCursor, CCursorRange}, Key, Modifiers, Ui};

pub(super) struct ExactDelay {
    digits: String,
    initial_ms: i64,
    select_on_open: bool,
}

impl ExactDelay {
    pub fn new(delay: SpeakerDelay) -> Self {
        let initial_ms = delay.total_ms();
        Self { digits: initial_ms.to_string(), initial_ms, select_on_open: true }
    }

    pub fn has_edit(&self) -> bool {
        self.digits.parse::<i64>().ok() != Some(self.initial_ms)
    }

    /// Returns whether to close and the change to send, if any.
    pub fn show(&mut self, ui: &mut Ui, lang: Lang, own_ms: u32) -> (bool, Option<DelayStep>) {
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            return (true, None);
        }
        ui.horizontal(|ui| {
            let label = w!(lang, DelayLabel);
            let theme = Theme::of(ui.ctx());
            let font = theme.font(size::BODY);
            ui.label(egui::RichText::new(&label).font(font.clone()).color(theme.c.text));
            let field = TextField::new(&mut self.digits, &label).width(72.0).show(ui);
            if self.select_on_open {
                field.request_focus();
                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), field.id) {
                    let end = CCursor::new(self.digits.len());
                    state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), end)));
                    state.store(ui.ctx(), field.id);
                }
                self.select_on_open = false;
            }
            ui.label(egui::RichText::new(w!(lang, MsUnit)).font(font).color(theme.c.text));
            let value = self.digits.trim().parse::<i64>().ok();
            let set = Button::standard(&w!(lang, SetDelayValue)).text_size(size::BODY)
                .enabled(value.is_some()).show(ui).clicked();
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let close = IconButton::new(Icon::Close, &w!(lang, Close)).show(ui).clicked();
            let step = value.filter(|_| set || enter).map(|ms| {
                let change = ms.saturating_sub(i64::from(own_ms)).clamp(i32::MIN.into(), i32::MAX.into());
                DelayStep::SetTotal(change as i32)
            });
            (close || step.is_some(), step)
        }).inner
    }
}
