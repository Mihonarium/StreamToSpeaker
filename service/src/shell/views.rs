//! Stand-ins for the panel's views until their real contents exist: each
//! has its real bar and a plain line, a speaker's view the delay block, so
//! the frame can be drawn and tested. And the rule for reopening the panel
//! on the view it closed on.

use egui::Ui;

use super::{Ask, ViewBar, ViewContent, Views};
use crate::model::command::{Command, Open};
use crate::model::places::Row;
use crate::model::snapshot::{Snapshot, Third};
use crate::model::state::{Place, SpeakerId};
use crate::model::text::t;
use crate::ui::text::Para;
use crate::ui::theme::size;

/// The stand-in views.
pub struct StandInViews;

impl Views for StandInViews {
    fn view(&self, open: &Open, _snap: &Snapshot) -> Option<Box<dyn ViewContent>> {
        let kind = match open {
            Open::SpeakerSettings(id) => StandIn::Speaker(id.clone()),
            Open::Settings => StandIn::Settings,
            Open::SpeakerMissing => StandIn::SpeakerMissing,
            Open::CantHear(_) => StandIn::CantHear,
            Open::MusicAssistantSettings | Open::MusicAssistantHowTo => StandIn::MusicAssistant,
            _ => return None,
        };
        Some(Box::new(StandInView { kind, delay: crate::ui::delay::DelayState::opened() }))
    }
}

/// Which view a stand-in stands in for.
#[derive(Clone, Debug, PartialEq)]
pub enum StandIn {
    Speaker(SpeakerId),
    Settings,
    SpeakerMissing,
    CantHear,
    MusicAssistant,
}

/// A view with its real bar and a plain line.
pub struct StandInView {
    pub kind: StandIn,
    delay: crate::ui::delay::DelayState,
}

impl StandInView {
    pub fn new(kind: StandIn) -> Self {
        StandInView { kind, delay: crate::ui::delay::DelayState::opened() }
    }

    fn row<'s>(&self, snap: &'s Snapshot) -> Option<&'s Row> {
        match &self.kind {
            StandIn::Speaker(id) => snap.list.rows().find(|r| r.place == Place::Speaker(id.clone())),
            StandIn::MusicAssistant => snap.list.music_assistant.as_ref().map(|m| &m.row),
            _ => None,
        }
    }

    /// The row's *Play a test sound*, from its menu.
    fn test_sound<'s>(&self, snap: &'s Snapshot) -> Option<&'s crate::model::snapshot::MenuItem> {
        self.row(snap)?.menu.iter().find(|m| matches!(m.command, Command::PlayTestSound(_)) && m.enabled)
    }
}

impl ViewContent for StandInView {
    fn bar(&self, snap: &Snapshot) -> ViewBar {
        let lang = snap.lang;
        match &self.kind {
            StandIn::Speaker(id) => {
                let row = self.row(snap);
                ViewBar {
                    title: row.map(|r| r.title.clone()).unwrap_or_else(|| id.as_str().to_string()),
                    line: row.map(|r| r.tooltip.clone()),
                    button: self.test_sound(snap).map(|m| m.label.clone()),
                }
            }
            StandIn::Settings => ViewBar { title: snap.footer.settings.label.clone(), ..Default::default() },
            StandIn::SpeakerMissing => ViewBar { title: snap.footer.speaker_missing.label.clone(), ..Default::default() },
            StandIn::CantHear => ViewBar { title: t!(lang, CantHear), ..Default::default() },
            StandIn::MusicAssistant => {
                let row = self.row(snap);
                ViewBar { title: t!(lang, MaName), line: row.map(|r| r.tooltip.clone()), button: None }
            }
        }
    }

    fn bar_button(&mut self, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        if let Some(m) = self.test_sound(snap) {
            ask(Ask::Command(m.command.clone()));
        }
    }

    fn show(&mut self, ui: &mut Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        // The delay block, for the speaker the sound is on.
        if let (StandIn::Speaker(id), Some(Third::Delay(d))) = (&self.kind, &snap.top.third) {
            let current = snap.list.rows().any(|r| r.current && r.place == Place::Speaker(id.clone()));
            if current {
                for c in crate::panel::delay_block(ui, d, &mut self.delay) {
                    ask(Ask::Command(c));
                }
                ui.add_space(12.0);
            }
        }
        let _ = Para::new(STAND_IN_LINE).size(size::SECONDARY).show(ui);
    }

    fn has_links(&self, _snap: &Snapshot) -> bool {
        self.kind == StandIn::Settings
    }

    fn links(&mut self, ui: &mut Ui, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        if self.kind == StandIn::Settings && crate::ui::button::Button::link(SETTINGS_WINDOW).show(ui).clicked() {
            ask(Ask::Window(Open::Settings));
        }
    }
}

/// A stand-in's line (not shipped: the real views replace it).
const STAND_IN_LINE: &str = "This view's controls come next.";
/// The stand-in Settings view's way to the Settings window, which stays
/// until the view has its contents.
const SETTINGS_WINDOW: &str = "Settings window";

/// What the panel remembers of the views it closed on, and when it opens
/// on them again: within a minute of closing, or whenever one of them
/// holds text that wasn't used yet.
pub struct ViewMemory<T> {
    closed: Option<(Vec<T>, f64)>,
}

impl<T> Default for ViewMemory<T> {
    fn default() -> Self {
        ViewMemory { closed: None }
    }
}

/// How long after closing the panel reopens on the same view, in seconds.
pub const REOPEN_WITHIN: f64 = 60.0;

impl<T> ViewMemory<T> {
    /// The panel closed at `now` showing `views` (bottom first).
    pub fn closed(&mut self, views: Vec<T>, now: f64) {
        self.closed = (!views.is_empty()).then_some((views, now));
    }

    /// The panel opens at `now` on its everyday content: the views to show
    /// again, if any. `unused_text` says whether a view holds text that
    /// wasn't used yet.
    pub fn reopen(&mut self, now: f64, unused_text: impl Fn(&T) -> bool) -> Vec<T> {
        match self.closed.take() {
            Some((views, at)) if now - at < REOPEN_WITHIN || views.iter().any(&unused_text) => views,
            _ => Vec::new(),
        }
    }

    /// The panel opens on a view someone asked for: what was remembered
    /// goes.
    pub fn forget(&mut self) {
        self.closed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::port::Port;
    use crate::model::scripted::{Scenario, ScriptedEngine};

    #[test]
    fn the_panel_reopens_on_its_view_within_a_minute_or_while_a_field_holds_text() {
        let mut m: ViewMemory<(&str, bool)> = ViewMemory::default();
        m.closed(vec![("settings", false), ("about", false)], 100.0);
        assert_eq!(m.reopen(150.0, |v| v.1), vec![("settings", false), ("about", false)]);
        // Reopened once: the next opening is the everyday view unless it
        // closed on a view again.
        assert!(m.reopen(151.0, |v| v.1).is_empty());
        m.closed(vec![("missing", false)], 100.0);
        assert!(m.reopen(160.0 + REOPEN_WITHIN, |v| v.1).is_empty(), "after a minute: the everyday view");
        m.closed(vec![("missing", true)], 100.0);
        assert_eq!(m.reopen(100.0 + 3600.0, |v| v.1), vec![("missing", true)], "an address typed and not added yet");
        m.closed(vec![("settings", false)], 100.0);
        m.forget();
        assert!(m.reopen(101.0, |v| v.1).is_empty());
        // Closed on the everyday view: nothing to reopen.
        m.closed(Vec::new(), 100.0);
        assert!(m.reopen(101.0, |v| v.1).is_empty());
    }

    #[test]
    fn each_view_has_a_stand_in_with_its_real_bar() {
        let snap = ScriptedEngine::new(Scenario::Playing).snapshot();
        let kitchen = snap.list.rows().find(|r| r.title == "Kitchen").unwrap();
        let Place::Speaker(id) = kitchen.place.clone() else { panic!() };
        let views = StandInViews;
        let v = views.view(&Open::SpeakerSettings(id.clone()), &snap).unwrap();
        let bar = v.bar(&snap);
        assert_eq!(bar.title, "Kitchen");
        assert_eq!(bar.line.as_deref(), Some(kitchen.tooltip.as_str()));
        assert!(bar.button.is_some(), "a test sound");
        assert_eq!(views.view(&Open::Settings, &snap).unwrap().bar(&snap).title, "Settings");
        assert_eq!(views.view(&Open::SpeakerMissing, &snap).unwrap().bar(&snap).title, "Speaker missing?");
        assert_eq!(views.view(&Open::CantHear(None), &snap).unwrap().bar(&snap).title, "Can’t hear it?");
        assert_eq!(views.view(&Open::MusicAssistantSettings, &snap).unwrap().bar(&snap).title, "Music Assistant");
        assert!(views.view(&Open::AddByAddress, &snap).is_none());
        assert!(views.view(&Open::Panel, &snap).is_none());
        // Its test sound is the row's.
        let mut v = views.view(&Open::SpeakerSettings(id.clone()), &snap).unwrap();
        let mut asked = Vec::new();
        v.bar_button(&snap, &mut |a| asked.push(a));
        assert!(matches!(asked.as_slice(), [Ask::Command(Command::PlayTestSound(s))] if *s == id));
    }
}
