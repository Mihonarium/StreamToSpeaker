//! The state a control is drawn in: resting, pointed at, pressed, focused
//! from the keyboard, or disabled.
//!
//! Every part asks [`look`] instead of reading the response itself, so the
//! preview can show a control in a state the pointer can't put several
//! controls in at once (one pointed at, the one beside it pressed).

use std::collections::HashMap;

use egui::{Id, Response, Ui};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Look {
    pub hovered: bool,
    pub pressed: bool,
    pub focused: bool,
    pub enabled: bool,
}

/// A state the preview puts a control in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Force {
    Hover,
    Press,
    Focus,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Forced(pub HashMap<Id, Vec<Force>>);

pub(crate) fn forced_id() -> Id {
    Id::new("stream_to_speaker::ui::look::forced")
}

/// How the control behind `response` looks now.
pub fn look(ui: &Ui, response: &Response) -> Look {
    let enabled = response.enabled();
    let mut l = Look {
        hovered: enabled && response.hovered(),
        pressed: enabled && response.is_pointer_button_down_on(),
        focused: response.has_focus(),
        enabled,
    };
    let forced = ui.ctx().data(|d| d.get_temp::<Forced>(forced_id()).and_then(|f| f.0.get(&response.id).cloned()));
    for f in forced.unwrap_or_default() {
        match f {
            Force::Hover => l.hovered = enabled,
            Force::Press => {
                l.hovered = enabled;
                l.pressed = enabled;
            }
            Force::Focus => l.focused = true,
        }
    }
    l
}
