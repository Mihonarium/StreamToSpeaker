//! What the app lets people change, as views of the panel: a speaker's
//! options (from the *›* at the end of its row), Settings (from the gear),
//! *Speaker missing?*, *Can't hear it?*, Music Assistant, and About, Lab
//! and the guides from the bottom of Settings.
//!
//! A thing is changed where it is: no sections to search, no second level.
//! Each view reads and changes the app through a [`SettingsPort`]; what it
//! asks of the panel (going back, another view, a dialog window, a command
//! for the engine) goes out as an [`Ask`].

mod about;
mod cant_hear;
mod first_run;
pub(crate) use first_run::FirstRunPage;
mod missing;
mod music_assistant;
mod parts;
mod settings_list;
mod speaker;

#[cfg(test)]
pub(crate) mod tests;

pub mod scenes;

use std::sync::Arc;

use crate::model::command::Open;
use crate::model::Snapshot;
use crate::shell::{Ask, ViewContent, Views};

pub use about::{AboutView, LabView};
pub use cant_hear::CantHearView;
pub use missing::{CheckView, MissingView};
pub use music_assistant::MusicAssistantView;
pub use settings_list::SettingsView;
pub use speaker::SpeakerOptions;

use super::{Dialog, SettingsPort};

/// The panel's views on a settings port.
pub struct SettingsViews {
    port: Arc<dyn SettingsPort>,
}

impl SettingsViews {
    pub fn new(port: Arc<dyn SettingsPort>) -> Self {
        SettingsViews { port }
    }
}

impl Views for SettingsViews {
    fn view(&self, open: &Open, _snap: &Snapshot) -> Option<Box<dyn ViewContent>> {
        let port = self.port.clone();
        Some(match open {
            Open::SpeakerSettings(id) => Box::new(SpeakerOptions::new(port, id.as_str())),
            Open::Settings => Box::new(SettingsView::new(port)),
            Open::SpeakerMissing => Box::new(MissingView::new(port)),
            Open::AddByAddress => Box::new(MissingView::new(port).to_add()),
            Open::CantHear(target) => Box::new(CantHearView::for_speaker(port, target.clone())),
            Open::MusicAssistantSettings | Open::MusicAssistantHowTo => Box::new(MusicAssistantView::new(port)),
            Open::WhatsNew => Box::new(AboutView::new(port)),
            _ => return None,
        })
    }
}

/// Ask the shell to open `dialog` in a window of its own.
fn open_dialog(port: &Arc<dyn SettingsPort>, dialog: Dialog, ask: &mut dyn FnMut(Ask)) {
    ask(Ask::Dialog(Box::new(crate::shell::content::DialogWindow::new(dialog, port.clone()))));
}
