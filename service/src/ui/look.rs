//! The state a control is drawn in: resting, pointed at, pressed, focused
//! from the keyboard, or disabled.
//!
//! Every part asks [`look`] (or [`look_as`]) instead of reading the response
//! itself, so the preview can show a control in a state the pointer can't
//! put several controls in at once (one pointed at, the one beside it
//! pressed).

use std::collections::HashMap;

use egui::{Id, Rect, Response, Ui};

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

/// The states the preview forces: by widget id, or by the start of a
/// control's accessible name within an area.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Forced {
    pub by_id: HashMap<Id, Vec<Force>>,
    pub by_name: Vec<(Rect, String, Force)>,
}

pub(crate) fn forced_id() -> Id {
    Id::new("stream_to_speaker::ui::look::forced")
}

/// How the control behind `response` looks now.
pub fn look(ui: &Ui, response: &Response) -> Look {
    look_as(ui, response, "")
}

/// As [`look`], for a control whose accessible name is `name`.
pub fn look_as(ui: &Ui, response: &Response, name: &str) -> Look {
    let enabled = response.enabled();
    let mut l = Look {
        hovered: enabled && response.hovered(),
        pressed: enabled && response.is_pointer_button_down_on(),
        focused: response.has_focus(),
        enabled,
    };
    let forces: Vec<Force> = ui.ctx().data(|d| {
        let Some(f) = d.get_temp::<Forced>(forced_id()) else { return Vec::new() };
        let mut out = f.by_id.get(&response.id).cloned().unwrap_or_default();
        if !name.is_empty() {
            for (area, prefix, force) in &f.by_name {
                if name.starts_with(prefix.as_str()) && area.contains_rect(response.rect) {
                    out.push(*force);
                }
            }
        }
        out
    });
    for f in forces {
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
