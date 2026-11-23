//! The app's windows as windows: the panel by the clock, the Settings
//! window, dialogs, the tray icon, and the root window that holds them
//! while none shows. What a window shows is drawn elsewhere: the panel by
//! [`crate::panel`], Settings and dialogs by a [`WindowContent`].
//!
//! What depends on Windows (the monitors and the taskbar, the light or dark
//! setting, moving the panel's window) sits behind [`Desktop`], with
//! [`FakeDesktop`] for tests; how the root and the panel behave are two
//! switches, [`RootMode`] and [`PanelMode`], so that a fallback is a
//! setting, not a rewrite.

pub mod content;
pub mod icons;
pub mod panel_window;
pub mod placement;
pub mod scenes;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use app::{app_icon, run, run_app, run_scripted, scripted_scenario};

use egui::{Pos2, Vec2, ViewportBuilder, WindowLevel};

use crate::model::command::{Command, Open};
use crate::model::snapshot::Snapshot;
use placement::{Monitor, PxRect};

/// What holds the app's windows while none of them shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootMode {
    /// The main window, hidden after its first frame. Windows may stop
    /// painting a hidden window, and the other windows are drawn in its
    /// frames.
    Hidden,
    /// A one-pixel transparent window that takes no clicks, with no
    /// taskbar button and out of Alt+Tab, which Windows keeps painting.
    ToolWindow,
}

/// How the panel behaves as a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelMode {
    /// A flyout by the clock: no frame, on top, out of Alt+Tab; closes when
    /// another window takes the focus, on Esc, or on a click on the icon.
    Flyout,
    /// A small always-on-top window with a title bar by the clock, which
    /// Esc, its × or a click on the icon close; losing the focus doesn't.
    Window,
}

impl PanelMode {
    /// Whether losing the focus closes the panel. Never while it is pinned
    /// (first run), which stays until it is finished or closed.
    pub fn closes_on_blur(self, pinned: bool) -> bool {
        self == PanelMode::Flyout && !pinned
    }
}

/// How the shell starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellOptions {
    pub root: RootMode,
    pub panel: PanelMode,
    /// Show the tray icon (off with `--no-tray`). Without it the panel is
    /// a window of its own, open from the start, and closing it quits.
    pub tray: bool,
    /// Open the panel at start (started from the Start menu, not at
    /// sign-in). First run opens it either way.
    pub open_at_start: bool,
}

impl ShellOptions {
    /// The options from `get` (the environment): `STREAM_TO_SPEAKER_ROOT`
    /// is `hidden` or `tool-window` (the default), `STREAM_TO_SPEAKER_PANEL`
    /// is `flyout` (the default) or `window`.
    pub fn from(get: impl Fn(&str) -> Option<String>, tray: bool) -> Self {
        let get = |k: &str| get(k).map(|v| v.trim().to_ascii_lowercase());
        ShellOptions {
            root: match get("STREAM_TO_SPEAKER_ROOT").as_deref() {
                Some("hidden") => RootMode::Hidden,
                _ => RootMode::ToolWindow,
            },
            panel: match get("STREAM_TO_SPEAKER_PANEL").as_deref() {
                Some("window") => PanelMode::Window,
                _ => PanelMode::Flyout,
            },
            tray,
            open_at_start: false,
        }
    }

    pub fn from_env(tray: bool) -> Self {
        Self::from(|k| std::env::var(k).ok(), tray)
    }

    /// What the shell does with these options: without a tray icon the
    /// panel is a window and opens at start.
    pub fn effective(self) -> Self {
        if self.tray {
            self
        } else {
            ShellOptions { panel: PanelMode::Window, open_at_start: true, ..self }
        }
    }
}

/// The root window's title (no one sees it).
pub const ROOT_TITLE: &str = "Stream To Speaker (background)";

/// The root window's builder for `mode`.
pub fn root_builder(mode: RootMode) -> ViewportBuilder {
    let base = ViewportBuilder::default()
        .with_title(ROOT_TITLE)
        .with_taskbar(false)
        .with_decorations(false)
        .with_resizable(false)
        .with_inner_size([1.0, 1.0]);
    match mode {
        RootMode::Hidden => base.with_visible(false),
        RootMode::ToolWindow => base
            .with_position([0.0, 0.0])
            .with_transparent(true)
            .with_mouse_passthrough(true)
            .with_active(false)
            .with_window_level(WindowLevel::AlwaysOnBottom),
    }
}

/// The panel's window title: what Windows and screen readers call it. The
/// shell finds the panel's window by it while the panel is hidden
/// (dialogs have the same title but are never hidden).
pub const PANEL_TITLE: &str = crate::PRODUCT_NAME;

/// The panel's window for `mode`: hidden until the shell has measured it
/// and put it in its place.
pub fn panel_builder(mode: PanelMode) -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(PANEL_TITLE)
        .with_inner_size([crate::panel::WIDTH, 400.0])
        .with_visible(false)
        .with_resizable(false)
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_taskbar(false)
        .with_minimize_button(false)
        .with_maximize_button(false)
        .with_decorations(mode == PanelMode::Window)
}

/// What kind of window a content needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKind {
    /// The Settings window: one at a time; asking for another page shows
    /// it in the open one.
    Settings,
    /// A dialog: one at a time; another waits until the open one closes.
    Dialog,
}

/// What a window's content asks of the shell while it is drawn.
pub enum Ask {
    /// Send this to the engine (from the Settings window).
    Command(Command),
    /// Open this dialog (it waits while another is open).
    Dialog(Box<dyn WindowContent>),
    /// Open the panel by the clock.
    OpenPanel,
}

/// What a Settings page or a dialog draws, in a window the shell made,
/// placed and themed.
pub trait WindowContent {
    /// The window's title.
    fn title(&self, snap: &Snapshot) -> String;
    /// The inner size it opens at, and its smallest, in points.
    fn sizes(&self) -> (Vec2, Vec2);
    /// Draw into the window's `ctx`; what it asks of the shell goes out
    /// through `ask`. True closes the window (a dialog's *Cancel*, or a
    /// dialog the app no longer needs).
    fn show(&mut self, ctx: &egui::Context, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) -> bool;
    /// Show `open` in this window instead (another page of Settings).
    /// False when it isn't one of this window's.
    fn navigate(&mut self, open: &Open) -> bool {
        let _ = open;
        false
    }
    /// The window's × was clicked: undo what an open dialog started.
    fn closed(&mut self) {}
}

/// What the app's windows hold, and what opening something does.
pub trait Contents {
    /// Do what `open` asks: give the content of the window it opens (a
    /// page of Settings, a dialog), or do it elsewhere (Windows' own
    /// settings, a web page, starting a pairing) and give `None`.
    /// `from_window` says the request came from the app's own windows (the
    /// panel, Settings); only those may start a connection.
    fn open(&self, open: &Open, snap: &Snapshot, from_window: bool) -> Option<(WindowKind, Box<dyn WindowContent>)>;
    /// The dialog the app needs open by itself now (a TV's code, Music
    /// Assistant's code), with a key that stays the same while the request
    /// does: a dialog closed by hand isn't opened again for the same key.
    fn wanted(&self) -> Option<(String, Box<dyn WindowContent>)> {
        None
    }
}

/// The shell's ties to the rest of the app.
pub struct Wiring {
    /// The engine (or the scripted one).
    pub port: std::sync::Arc<dyn crate::model::port::Port>,
    pub contents: Box<dyn Contents>,
    /// Hands the engine the shell's door for navigation commands that come
    /// from elsewhere (a notification, the phone page). `None`: only the
    /// shell's own surfaces open things.
    pub route_opens: Option<Box<dyn FnOnce(Box<dyn Fn(Open) + Send + Sync>)>>,
    /// Quit for good: give the sound back and end the app.
    pub quit: Box<dyn Fn()>,
    /// The app is quitting another way (Ctrl+C, an update): close.
    pub quitting: Box<dyn Fn() -> bool>,
}

/// Until Settings and the dialogs are drawn (the scripted engine): their
/// windows, saying which page or dialog was asked for.
pub struct Placeholders;

impl Contents for Placeholders {
    fn open(&self, open: &Open, _snap: &Snapshot, _from_window: bool) -> Option<(WindowKind, Box<dyn WindowContent>)> {
        placeholder(open)
    }
}

/// A placeholder window for `open`, if it is one of the app's.
pub fn placeholder(open: &Open) -> Option<(WindowKind, Box<dyn WindowContent>)> {
    let kind = placeholder_kind(open)?;
    Some((kind, Box::new(Placeholder { kind, open: open.clone() })))
}

fn placeholder_kind(open: &Open) -> Option<WindowKind> {
    match open {
        Open::Settings | Open::SpeakerSettings(_) | Open::MusicAssistantSettings | Open::MusicAssistantHowTo | Open::CantHear(_) | Open::SpeakerMissing | Open::WhatsNew => Some(WindowKind::Settings),
        Open::AddByAddress | Open::Pair(_) => Some(WindowKind::Dialog),
        _ => None,
    }
}

struct Placeholder {
    kind: WindowKind,
    open: Open,
}

impl WindowContent for Placeholder {
    fn title(&self, _snap: &Snapshot) -> String {
        match self.kind {
            WindowKind::Settings => format!("{} Settings", crate::PRODUCT_NAME),
            WindowKind::Dialog => format!("{} – {:?}", crate::PRODUCT_NAME, self.open),
        }
    }

    fn sizes(&self) -> (Vec2, Vec2) {
        match self.kind {
            WindowKind::Settings => (Vec2::new(880.0, 640.0), Vec2::new(640.0, 480.0)),
            WindowKind::Dialog => (Vec2::new(480.0, 260.0), Vec2::new(360.0, 200.0)),
        }
    }

    fn show(&mut self, ctx: &egui::Context, _snap: &Snapshot, _ask: &mut dyn FnMut(Ask)) -> bool {
        let mut close = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(format!("{:?}", self.open));
            if self.kind == WindowKind::Dialog && ui.button("Close").clicked() {
                close = true;
            }
        });
        close
    }

    fn navigate(&mut self, open: &Open) -> bool {
        if self.kind == WindowKind::Settings && placeholder_kind(open) == Some(WindowKind::Settings) {
            self.open = open.clone();
            true
        } else {
            false
        }
    }
}

/// Where a window of `size` points opens: in the middle of the primary
/// monitor's work area, in egui's points for a new window (which egui
/// turns into pixels at the primary monitor's scale).
pub fn centred(monitors: &[Monitor], size: Vec2) -> Option<Pos2> {
    let m = monitors.iter().find(|m| m.primary).or_else(|| monitors.first())?;
    let s = m.scale.max(0.5);
    let (cx, cy) = ((m.work.left + m.work.right) as f32 / 2.0, (m.work.top + m.work.bottom) as f32 / 2.0);
    Some(Pos2::new((cx / s - size.x / 2.0).max(m.work.left as f32 / s), (cy / s - size.y / 2.0).max(m.work.top as f32 / s)))
}

/// What the shell asks of Windows and does through it.
pub trait Desktop {
    /// Every monitor, with its work area and scale.
    fn monitors(&self) -> Vec<Monitor>;
    /// The taskbar is dark (its icons must be light).
    fn taskbar_is_dark(&self) -> bool;
    /// Apps are set to dark.
    fn apps_are_dark(&self) -> bool;
    /// Put the panel's window at `rect` with `mode`'s look; true once it is
    /// there (moving a window to a monitor of another scale may take
    /// Windows a frame or two).
    fn place_panel(&mut self, rect: PxRect, mode: PanelMode) -> bool;
}

/// A desktop for tests: fixed monitors and themes; it records where the
/// panel was put and can be slow to put it there.
#[derive(Clone, Debug, Default)]
pub struct FakeDesktop {
    pub monitors: Vec<Monitor>,
    pub dark_taskbar: bool,
    pub dark_apps: bool,
    /// How many calls to `place_panel` fail before one succeeds.
    pub slow: u32,
    pub placed: Vec<PxRect>,
}

impl Desktop for FakeDesktop {
    fn monitors(&self) -> Vec<Monitor> {
        self.monitors.clone()
    }

    fn taskbar_is_dark(&self) -> bool {
        self.dark_taskbar
    }

    fn apps_are_dark(&self) -> bool {
        self.dark_apps
    }

    fn place_panel(&mut self, rect: PxRect, _mode: PanelMode) -> bool {
        self.placed.push(rect);
        if self.slow > 0 {
            self.slow -= 1;
            false
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_read_the_environment_and_default_to_the_flyout_and_the_tool_window() {
        let none = ShellOptions::from(|_| None, true);
        assert_eq!((none.root, none.panel, none.tray), (RootMode::ToolWindow, PanelMode::Flyout, true));
        let set = ShellOptions::from(
            |k| match k {
                "STREAM_TO_SPEAKER_ROOT" => Some("Hidden".into()),
                "STREAM_TO_SPEAKER_PANEL" => Some(" window ".into()),
                _ => None,
            },
            false,
        );
        assert_eq!((set.root, set.panel, set.tray), (RootMode::Hidden, PanelMode::Window, false));
    }

    #[test]
    fn only_the_flyout_closes_when_it_loses_the_focus_and_never_while_pinned() {
        assert!(PanelMode::Flyout.closes_on_blur(false));
        assert!(!PanelMode::Flyout.closes_on_blur(true));
        assert!(!PanelMode::Window.closes_on_blur(false));
    }

    #[test]
    fn the_panels_window_starts_hidden_on_top_and_off_the_taskbar() {
        let b = panel_builder(PanelMode::Flyout);
        assert_eq!(b.visible, Some(false));
        assert_eq!(b.decorations, Some(false));
        assert_eq!(b.window_level, Some(WindowLevel::AlwaysOnTop));
        assert_eq!(b.taskbar, Some(false));
        assert_eq!(b.title.as_deref(), Some(PANEL_TITLE));
        assert_eq!(panel_builder(PanelMode::Window).decorations, Some(true));
    }

    #[test]
    fn the_root_is_hidden_or_a_transparent_pixel_that_takes_no_clicks() {
        let hidden = root_builder(RootMode::Hidden);
        assert_eq!(hidden.visible, Some(false));
        let tool = root_builder(RootMode::ToolWindow);
        assert_eq!((tool.transparent, tool.taskbar, tool.inner_size), (Some(true), Some(false), Some(Vec2::new(1.0, 1.0))));
        assert_eq!((tool.mouse_passthrough, tool.active), (Some(true), Some(false)));
        assert_ne!(tool.visible, Some(false));
        assert_ne!(ROOT_TITLE, PANEL_TITLE);
    }

    #[test]
    fn pages_open_settings_dialogs_open_dialogs_and_windows_own_settings_neither() {
        use crate::model::state::SpeakerId;
        assert_eq!(placeholder(&Open::Settings).map(|(k, _)| k), Some(WindowKind::Settings));
        assert_eq!(placeholder(&Open::SpeakerSettings(SpeakerId::new("kitchen"))).map(|(k, _)| k), Some(WindowKind::Settings));
        assert_eq!(placeholder(&Open::Pair(SpeakerId::new("den-tv"))).map(|(k, _)| k), Some(WindowKind::Dialog));
        assert!(placeholder(&Open::SoundSettings).is_none());
        assert!(placeholder(&Open::Donate).is_none());
        assert!(placeholder(&Open::Panel).is_none());
        // Another page shows in the open Settings window; a dialog's
        // request doesn't.
        let (_, mut settings) = placeholder(&Open::Settings).unwrap();
        let snap0 = crate::model::port::Port::snapshot(&crate::model::scripted::ScriptedEngine::new(crate::model::scripted::Scenario::Playing));
        assert!(Placeholders.open(&Open::SoundSettings, &snap0, true).is_none());
        assert!(settings.navigate(&Open::SpeakerMissing));
        assert!(!settings.navigate(&Open::AddByAddress));
        // No window but the panel has the panel's title.
        let snap = crate::model::scripted::ScriptedEngine::new(crate::model::scripted::Scenario::Playing);
        let snap = crate::model::port::Port::snapshot(&snap);
        for open in [Open::Settings, Open::AddByAddress, Open::Pair(SpeakerId::new("den-tv"))] {
            assert_ne!(placeholder(&open).unwrap().1.title(&snap), PANEL_TITLE);
        }
    }

    #[test]
    fn settings_and_dialogs_open_in_the_middle_of_the_primary_monitor() {
        let primary = Monitor { bounds: PxRect::new(0, 0, 2880, 1620), work: PxRect::new(0, 0, 2880, 1548), scale: 1.5, primary: true };
        let other = Monitor { bounds: PxRect::new(-1920, 0, 0, 1080), work: PxRect::new(-1920, 0, 0, 1032), scale: 1.0, primary: false };
        let p = centred(&[other, primary], Vec2::new(480.0, 260.0)).unwrap();
        assert_eq!(p, Pos2::new(960.0 - 240.0, 516.0 - 130.0));
        // Bigger than the work area: its corner stays on screen.
        let p = centred(&[primary], Vec2::new(4000.0, 4000.0)).unwrap();
        assert_eq!(p, Pos2::new(0.0, 0.0));
    }
}
