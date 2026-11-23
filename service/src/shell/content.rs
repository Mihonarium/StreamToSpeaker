//! The Settings window and its dialogs as the shell's window content, on a
//! settings port (the running app's, or the scripted one), and what each
//! request to open something does.

use std::sync::Arc;

use egui::Vec2;

use super::{Ask, Contents, WindowContent, WindowKind};
use crate::model::command::Open;
use crate::model::snapshot::Snapshot;
use crate::model::state::{NowState, Secret, SpeakerId};
use crate::settings::port::Wanted;
use crate::settings::{self, Act, AddDialog, Dialog, PasswordDialog, Request, SettingsPort};

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

/// The page of Settings `open` asks for, if it asks for one.
pub fn page_of(open: &Open) -> Option<settings::Page> {
    use settings::Page;
    Some(match open {
        Open::Settings => Page::Speakers,
        Open::SpeakerSettings(id) => Page::Speaker(id.as_str().to_string()),
        Open::MusicAssistantSettings | Open::MusicAssistantHowTo => Page::MusicAssistant,
        Open::CantHear(_) => Page::CantHear,
        Open::SpeakerMissing => Page::CantFind,
        Open::WhatsNew => Page::About,
        _ => return None,
    })
}

/// The Settings window and its dialogs on `port`.
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
        let page = |page: settings::Page| {
            let mut window = settings::Window::new();
            window.open(page);
            Some((WindowKind::Settings, Box::new(SettingsWindow { window, port: self.port.clone() }) as Box<dyn WindowContent>))
        };
        if let Some(p) = page_of(open) {
            return page(p);
        }
        match open {
            Open::AddByAddress => self.dialog(Dialog::AddAddress(AddDialog::default())),
            // A speaker that asks for its password: the dialog that
            // connects with it once the person types it. Otherwise a
            // pairing with a code: from the window it starts at once (its
            // dialog then opens by itself); from elsewhere the speaker's
            // page, where *Pair again* starts it, because only a pick in
            // the app's window may connect.
            Open::Pair(id) => match &snap.state {
                NowState::NeedsSecret { speaker, secret: Secret::Password } if speaker == id => {
                    self.dialog(Dialog::Password(PasswordDialog::to_connect(id.as_str(), &name_of(snap, id))))
                }
                _ if from_window => {
                    self.port.act(Act::PairAgain(id.as_str().to_string()));
                    None
                }
                _ => page(settings::Page::Speaker(id.as_str().to_string())),
            },
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

/// The Settings window.
pub struct SettingsWindow {
    window: settings::Window,
    port: Arc<dyn SettingsPort>,
}

impl SettingsWindow {
    /// The page it shows.
    pub fn page(&self) -> settings::Page {
        self.window.page()
    }
}

impl WindowContent for SettingsWindow {
    fn title(&self, _snap: &Snapshot) -> String {
        settings::title(self.port.lang())
    }

    fn sizes(&self) -> (Vec2, Vec2) {
        (Vec2::from(settings::DEFAULT_SIZE), Vec2::from(settings::MIN_SIZE))
    }

    fn show(&mut self, ctx: &egui::Context, _snap: &Snapshot, ask: &mut dyn FnMut(Ask)) -> bool {
        for r in self.window.show(ctx, self.port.as_ref()) {
            match r {
                Request::Dialog(dialog) => ask(Ask::Dialog(Box::new(DialogWindow { dialog, port: self.port.clone() }))),
                Request::OpenPanel => ask(Ask::OpenPanel),
            }
        }
        false
    }

    fn navigate(&mut self, open: &Open) -> bool {
        match page_of(open) {
            Some(page) => {
                self.window.open(page);
                true
            }
            None => false,
        }
    }
}

/// A dialog of Settings, in a window of its own.
pub struct DialogWindow {
    dialog: Dialog,
    port: Arc<dyn SettingsPort>,
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
    fn each_request_opens_its_page_of_the_one_settings_window() {
        let (c, _) = contents();
        let playing = snap(Scenario::Playing);
        for (open, page) in [
            (Open::Settings, settings::Page::Speakers),
            (Open::SpeakerSettings(SpeakerId::new("kitchen")), settings::Page::Speaker("kitchen".into())),
            (Open::MusicAssistantSettings, settings::Page::MusicAssistant),
            (Open::MusicAssistantHowTo, settings::Page::MusicAssistant),
            (Open::CantHear(None), settings::Page::CantHear),
            (Open::SpeakerMissing, settings::Page::CantFind),
            (Open::WhatsNew, settings::Page::About),
        ] {
            let (kind, _) = c.open(&open, &playing, true).unwrap();
            assert_eq!(kind, WindowKind::Settings, "{open:?}");
            assert_eq!(page_of(&open), Some(page));
        }
        // The open window shows another page instead.
        let mut w = SettingsWindow { window: settings::Window::new(), port: c.port.clone() };
        assert!(w.navigate(&Open::SpeakerMissing));
        assert_eq!(w.page(), settings::Page::CantFind);
        assert!(!w.navigate(&Open::AddByAddress));
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
        // From elsewhere (a notification): the speaker's page, nothing
        // started.
        let (kind, _) = c.open(&Open::Pair(SpeakerId::new("den-tv")), &snap(Scenario::Playing), false).unwrap();
        assert_eq!(kind, WindowKind::Settings);
        assert!(s.take_acts().is_empty());
        assert!(c.open(&Open::Donate, &snap(Scenario::Playing), true).is_none());
        assert_eq!(s.take_acts(), vec![Act::Donate]);
        assert_eq!(c.open(&Open::AddByAddress, &snap(Scenario::Playing), true).map(|(k, _)| k), Some(WindowKind::Dialog));
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
