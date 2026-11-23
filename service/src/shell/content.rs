//! The dialogs that keep a window of their own, on a settings port (the
//! running app's, or the scripted one), and what each request to open
//! something outside the panel's views does.

use std::sync::Arc;

use egui::Vec2;

use super::{Ask, Contents, WindowContent, WindowKind};
use crate::model::command::Open;
use crate::model::snapshot::Snapshot;
use crate::model::state::{NowState, Secret, SpeakerId};
use crate::settings::port::Wanted;
use crate::settings::{Act, Dialog, PasswordDialog, SettingsPort};

/// A page of Windows' own settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsPage {
    Sound,
    VolumeMixer,
    Network,
}

impl WindowsPage {
    /// The address that opens it.
    pub fn uri(self) -> &'static str {
        match self {
            WindowsPage::Sound => "ms-settings:sound",
            WindowsPage::VolumeMixer => "ms-settings:apps-volume",
            WindowsPage::Network => "ms-settings:network-status",
        }
    }
}

/// Whether `id` is the speaker that waits for its password now.
fn wants_password(snap: &Snapshot, id: &SpeakerId) -> bool {
    matches!(&snap.state, NowState::NeedsSecret { speaker, secret: Secret::Password } if speaker == id)
}

/// What a request to pair opens when it comes from outside the app's
/// windows (a notification): the speaker's view, where *Enter code…*
/// starts it, because only a pick in the app's window may connect. A
/// password is asked for in its dialog wherever the request comes from.
pub fn pair_from_outside(open: &Open, snap: &Snapshot, from_window: bool) -> Option<Open> {
    match open {
        Open::Pair(id) if !from_window && !wants_password(snap, id) => Some(Open::SpeakerSettings(id.clone())),
        _ => None,
    }
}

/// The dialogs on `port`, and what opens outside the app.
pub struct SettingsContents {
    pub port: Arc<dyn SettingsPort>,
    /// Opens a page of Windows' settings.
    pub open_windows_page: fn(WindowsPage),
    /// Whether a speaker must be given its password before it can play
    /// (it has one and none is kept, or it refused the one it was given).
    pub needs_password: Box<dyn Fn(&str) -> bool>,
}

impl SettingsContents {
    fn dialog(&self, dialog: Dialog) -> Option<(WindowKind, Box<dyn WindowContent>)> {
        Some((WindowKind::Dialog, Box::new(DialogWindow { dialog, port: self.port.clone() })))
    }
}

fn name_of(snap: &Snapshot, id: &SpeakerId) -> String {
    snap.names.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone()).unwrap_or_else(|| id.as_str().to_string())
}

impl Contents for SettingsContents {
    fn open(&self, open: &Open, snap: &Snapshot, from_window: bool) -> Option<(WindowKind, Box<dyn WindowContent>)> {
        match open {
            // A speaker that asks for its password: the dialog that
            // connects with it once the person types it. Otherwise a
            // pairing with a code, from the window: it starts at once (its
            // dialog then opens by itself). From elsewhere the shell opens
            // the speaker's view instead (see [`pair_from_outside`]).
            Open::Pair(id) if wants_password(snap, id) => self.dialog(Dialog::Password(PasswordDialog::to_connect(id.as_str(), &name_of(snap, id)))),
            Open::Pair(id) if from_window => {
                self.port.act(Act::PairAgain(id.as_str().to_string()));
                None
            }
            Open::Donate => {
                self.port.act(Act::Donate);
                None
            }
            Open::SoundSettings => {
                (self.open_windows_page)(WindowsPage::Sound);
                None
            }
            Open::VolumeMixer => {
                (self.open_windows_page)(WindowsPage::VolumeMixer);
                None
            }
            Open::NetworkSettings => {
                (self.open_windows_page)(WindowsPage::Network);
                None
            }
            Open::Repair | Open::RestartWindows => {
                log::warn!("{open:?} isn't available yet");
                None
            }
            _ => None,
        }
    }

    fn before_play(&self, speaker: &SpeakerId, snap: &Snapshot) -> Option<Box<dyn WindowContent>> {
        if !(self.needs_password)(speaker.as_str()) {
            return None;
        }
        let dialog = Dialog::Password(PasswordDialog::to_connect(speaker.as_str(), &name_of(snap, speaker)));
        Some(Box::new(DialogWindow { dialog, port: self.port.clone() }))
    }

    fn wanted(&self) -> Option<(String, Box<dyn WindowContent>)> {
        let key = match self.port.wanted_dialog()? {
            Wanted::PairingCode(a) => format!("code {}", a.id),
            Wanted::Password(a) => format!("password {}", a.id),
            Wanted::SendspinCode(a) => format!("sendspin {}", a.id),
            Wanted::MaCode(m) => format!("music assistant {}", m.code),
        };
        let dialog = Dialog::wanted(self.port.as_ref())?;
        Some((key, Box::new(DialogWindow { dialog, port: self.port.clone() })))
    }
}

/// A dialog of Settings, in a window of its own.
pub struct DialogWindow {
    dialog: Dialog,
    port: Arc<dyn SettingsPort>,
}

impl DialogWindow {
    /// `dialog` on `port`, for [`super::Ask::Dialog`].
    pub fn new(dialog: Dialog, port: Arc<dyn SettingsPort>) -> Self {
        DialogWindow { dialog, port }
    }
}

impl WindowContent for DialogWindow {
    fn title(&self, _snap: &Snapshot) -> String {
        Dialog::window_title(self.port.lang())
    }

    fn sizes(&self) -> (Vec2, Vec2) {
        let s = Vec2::from(self.dialog.size());
        (s, s)
    }

    fn show(&mut self, ctx: &egui::Context, _snap: &Snapshot, _ask: &mut dyn FnMut(Ask)) -> bool {
        !self.dialog.show(ctx, self.port.as_ref())
    }

    fn closed(&mut self) {
        self.dialog.cancel(self.port.as_ref());
    }
}

/// Decides when the dialog the app wants opens by itself: when no dialog
/// is open, and not again for the same request once it was closed by hand.
#[derive(Debug, Default)]
pub struct WantedGate {
    /// The request of the dialog that is open now, if it opened by itself.
    open: Option<String>,
    /// A request whose dialog was closed while the app still wanted it.
    declined: Option<String>,
}

impl WantedGate {
    /// The app wants the dialog for `key` (or none): whether to open it,
    /// given whether a dialog is open.
    pub fn admit(&mut self, key: Option<&str>, dialog_open: bool) -> bool {
        if self.declined.as_deref() != key {
            self.declined = None;
        }
        match key {
            Some(k) if !dialog_open && self.declined.as_deref() != Some(k) => {
                self.open = Some(k.to_string());
                true
            }
            _ => false,
        }
    }

    /// The open dialog closed; `still_wanted` is the app's request now.
    pub fn closed(&mut self, still_wanted: Option<&str>) {
        if let Some(k) = self.open.take() {
            if still_wanted == Some(k.as_str()) {
                self.declined = Some(k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::port::Port;
    use crate::model::scripted::{Scenario, ScriptedEngine};
    use crate::settings::port::{CodeAsk, CodeStage};
    use crate::settings::scripted::ScriptedSettings;
    use std::sync::Mutex;

    static OPENED: Mutex<Vec<WindowsPage>> = Mutex::new(Vec::new());

    fn record(p: WindowsPage) {
        OPENED.lock().unwrap().push(p);
    }

    fn contents() -> (SettingsContents, Arc<ScriptedSettings>) {
        let s = Arc::new(ScriptedSettings::new());
        (SettingsContents { port: s.clone(), open_windows_page: record, needs_password: Box::new(|id| id == "office") }, s)
    }

    fn snap(s: Scenario) -> Arc<Snapshot> {
        ScriptedEngine::new(s).snapshot()
    }

    #[test]
    fn each_request_for_settings_opens_a_view_of_the_panel_not_a_window() {
        use crate::shell::Views;
        let (c, s) = contents();
        let views = crate::settings::views::SettingsViews::new(s.clone());
        let playing = snap(Scenario::Playing);
        for (open, title) in [
            (Open::Settings, "Settings"),
            (Open::SpeakerSettings(SpeakerId::new("kitchen")), "Kitchen"),
            (Open::MusicAssistantSettings, "Music Assistant"),
            (Open::MusicAssistantHowTo, "Music Assistant"),
            (Open::CantHear(None), "Can’t hear it?"),
            (Open::SpeakerMissing, "Speaker missing?"),
            (Open::AddByAddress, "Speaker missing?"),
            (Open::WhatsNew, "About"),
        ] {
            let view = views.view(&open, &playing).unwrap_or_else(|| panic!("{open:?}: no view"));
            assert_eq!(view.bar(&playing).title, title, "{open:?}");
            assert!(c.open(&open, &playing, true).is_none(), "{open:?}: no window");
        }
        assert!(views.view(&Open::Donate, &playing).is_none());
    }

    #[test]
    fn pairing_asks_for_the_password_or_starts_from_the_window_and_windows_pages_open_outside() {
        let (c, s) = contents();
        // The scripted engine's speaker that asks for a password.
        let needs = snap(Scenario::NeedsPairing);
        if let NowState::NeedsSecret { speaker, secret } = &needs.state {
            let got = c.open(&Open::Pair(speaker.clone()), &needs, true);
            match secret {
                Secret::Password => assert_eq!(got.map(|(k, _)| k), Some(WindowKind::Dialog)),
                Secret::Code => {
                    assert!(got.is_none());
                    assert_eq!(s.take_acts(), vec![Act::PairAgain(speaker.as_str().to_string())]);
                }
            }
        }
        // Another speaker (a pairing the speaker forgot): pair again from
        // the window.
        assert!(c.open(&Open::Pair(SpeakerId::new("den-tv")), &snap(Scenario::Playing), true).is_none());
        assert_eq!(s.take_acts(), vec![Act::PairAgain("den-tv".into())]);
        // From elsewhere (a notification): the speaker's view, nothing
        // started; a password is asked for wherever the request comes from.
        assert_eq!(
            pair_from_outside(&Open::Pair(SpeakerId::new("den-tv")), &snap(Scenario::Playing), false),
            Some(Open::SpeakerSettings(SpeakerId::new("den-tv")))
        );
        assert_eq!(pair_from_outside(&Open::Pair(SpeakerId::new("den-tv")), &snap(Scenario::Playing), true), None);
        let engine = ScriptedEngine::new(Scenario::Playing);
        let mut f = engine.facts();
        f.state = NowState::NeedsSecret { speaker: SpeakerId::new("office"), secret: Secret::Password };
        engine.set_facts(f);
        let asking = engine.snapshot();
        assert_eq!(pair_from_outside(&Open::Pair(SpeakerId::new("office")), &asking, false), None);
        assert_eq!(c.open(&Open::Pair(SpeakerId::new("office")), &asking, false).map(|(k, _)| k), Some(WindowKind::Dialog));
        assert!(s.take_acts().is_empty());
        assert!(c.open(&Open::Donate, &snap(Scenario::Playing), true).is_none());
        assert_eq!(s.take_acts(), vec![Act::Donate]);
        OPENED.lock().unwrap().clear();
        for open in [Open::SoundSettings, Open::VolumeMixer, Open::NetworkSettings] {
            assert!(c.open(&open, &snap(Scenario::Playing), true).is_none());
        }
        assert_eq!(*OPENED.lock().unwrap(), vec![WindowsPage::Sound, WindowsPage::VolumeMixer, WindowsPage::Network]);
        assert!(WindowsPage::Sound.uri().starts_with("ms-settings:"));
    }

    #[test]
    fn a_speaker_with_a_password_and_none_kept_asks_for_it_before_it_plays() {
        let (c, s) = contents();
        let playing = snap(Scenario::Playing);
        assert!(c.before_play(&SpeakerId::new("kitchen"), &playing).is_none());
        let d = c.before_play(&SpeakerId::new("office"), &playing).expect("the password dialog");
        assert_eq!(d.title(&playing), Dialog::window_title(s.lang()));
        assert!(s.take_acts().is_empty(), "nothing is started before it is typed");
    }

    #[test]
    fn the_dialog_the_app_wants_opens_by_itself_once() {
        let (c, s) = contents();
        assert!(c.wanted().is_none());
        s.set(|st| st.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den-tv".into(), name: "Den TV".into(), stage: CodeStage::Showing, seconds_left: Some(90) })));
        let (key, _) = c.wanted().unwrap();
        // The countdown doesn't make it another request.
        s.set(|st| st.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den-tv".into(), name: "Den TV".into(), stage: CodeStage::Showing, seconds_left: Some(80) })));
        assert_eq!(c.wanted().unwrap().0, key);

        let mut g = WantedGate::default();
        assert!(g.admit(Some(&key), false));
        // Open: nothing more opens.
        assert!(!g.admit(Some(&key), true));
        // Closed by hand while still wanted: not again for the same request.
        g.closed(Some(&key));
        assert!(!g.admit(Some(&key), false));
        // Once the app stops asking, the next request opens.
        assert!(!g.admit(None, false));
        assert!(g.admit(Some(&key), false));
        // Closed because the app no longer wants it: the next one opens.
        g.closed(None);
        assert!(g.admit(Some("code other"), false));
        // Another dialog is open: it waits.
        let mut g = WantedGate::default();
        assert!(!g.admit(Some(&key), true));
        assert!(g.admit(Some(&key), false));
    }
}
