//! What the app lets people change: the panel's views (a speaker's
//! options, Settings, *Speaker missing?*, *Can't hear it?*, Music
//! Assistant, About, Lab and the guides) and the dialogs that keep a
//! window of their own because they wait on another device or program (a
//! TV's code, a password, Music Assistant's code, a Sendspin speaker's
//! code, the problem report).
//!
//! A change takes effect at once and is kept: there is no Apply button,
//! and a change that needs a reconnect says so and does it. Views and
//! dialogs read and change the app only through a [`SettingsPort`].

pub mod port;
pub mod real;
pub mod scripted;
pub(crate) mod words;
pub(crate) mod common;
pub mod views;

mod dialogs;
mod scenes;
pub(crate) mod widgets;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

pub use dialogs::{CodeDialog, Dialog, PasswordDialog, ReportDialog, SendspinDialog};
pub use port::{Act, SettingsPort};

/// Every scene of the views and the dialogs.
pub fn scenes() -> Vec<crate::ui::preview::Scene> {
    let mut all = views::scenes::scenes();
    all.extend(scenes::scenes());
    all
}

/// What a view or a dialog remembers between frames: open folds, text
/// being typed.
#[derive(Debug, Default)]
pub(crate) struct PageState {
    /// Folds open (*Details*, …), by a name of their own.
    pub open: HashSet<String>,
    /// Text being typed, by field, until it is kept.
    pub typing: HashMap<String, String>,
    /// A shortcut whose keys are being recorded.
    pub recording: Option<usize>,
    /// What was just copied, and when (the button says *Copied*).
    pub copied: Option<(String, f64)>,
    /// The pairing token is shown in clear.
    pub token_shown: bool,
}

impl PageState {
    pub fn is_open(&self, fold: &str) -> bool {
        self.open.contains(fold)
    }

    pub fn toggle(&mut self, fold: &str) {
        if !self.open.remove(fold) {
            self.open.insert(fold.to_string());
        }
    }
}
