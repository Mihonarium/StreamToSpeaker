//! The app's ordinary window with the panel and its pages, the flyout by the clock,
//! dialogs, the small windows by the clock, the tray icon, and the root
//! window that holds them while none shows. What a window shows is drawn
//! elsewhere: the panel by [`crate::surface::panel`], a view by a [`ViewContent`],
//! a dialog by a [`WindowContent`].
//!
//! What depends on Windows (the monitors and the taskbar, the light or dark
//! setting, moving the panel's window) sits behind [`Desktop`], with
//! [`FakeDesktop`] for tests; how the root and the panel behave are two
//! switches, [`RootMode`] and [`PanelMode`], so that a fallback is a
//! setting, not a rewrite.

pub mod content;
pub mod page;
#[cfg(test)]
mod eframe_model;
pub mod icons;
pub mod main_window;
pub mod panel_window;
pub mod passes;
pub mod placement;
pub mod popup;
pub mod scenes;
#[cfg(any(windows, test))]
mod scroll;
pub mod views;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use app::{run, run_app, run_scripted};

use egui::{Pos2, Vec2, ViewportBuilder, WindowLevel};

use crate::model::command::{Command, Open};

use crate::model::Snapshot;
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
    /// Like any ordinary window it has its taskbar button and is in
    /// Alt+Tab. This form is available through the debug switch.
    Window,
}

impl PanelMode {
    /// Whether losing focus closes this panel.
    pub fn closes_on_blur(self) -> bool {
        self == PanelMode::Flyout
    }
}

/// How the shell starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellOptions {
    pub root: RootMode,
    pub panel: PanelMode,
    /// Show the tray icon. Without it the ordinary window opens at start and closing it quits.
    pub tray: bool,
    /// Open the ordinary window on a personal launch. Sign-in starts in the tray.
    pub open_at_start: bool,
}

impl ShellOptions {
    /// The options from `get` (the environment, see [`env_switches`]):
    /// `STREAM_TO_SPEAKER_ROOT` is `hidden` or `tool-window` (the default),
    /// `STREAM_TO_SPEAKER_PANEL` is `flyout` (the default) or `window`.
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

    /// The options from the environment; the defaults in a release build
    /// (see [`env_switches`]).
    pub fn from_env(tray: bool) -> Self {
        Self::from(env_switches(), tray)
    }

    /// What the shell does with these options: without a tray icon the
    /// panel is a window and opens at start.
    pub fn effective(self) -> Self {
        if self.tray {
            self
        } else {
            ShellOptions { open_at_start: true, ..self }
        }
    }
}

/// The scenario `STREAM_TO_SPEAKER_SCENARIO` names, if it names one: the
/// shell then runs on the scripted engine, for trying every state without
/// speakers. Never in a release build (see [`env_switches`]).
pub fn scripted_scenario() -> Option<&'static str> {
    scenario_from(env_switches())
}

fn scenario_from(get: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    get("STREAM_TO_SPEAKER_SCENARIO").and_then(|n| crate::model::samples::all().into_iter().find(|s| s.name == n.trim()).map(|s| s.name))
}

/// The switches for trying the shell's states and its other forms
/// (`STREAM_TO_SPEAKER_SCENARIO`, `STREAM_TO_SPEAKER_ROOT`,
/// `STREAM_TO_SPEAKER_PANEL`), from the environment. Only a debug build
/// reads them; a release build behaves as if none were set, so nothing
/// left in someone's environment changes how the app they installed runs.
fn env_switches() -> impl Fn(&str) -> Option<String> {
    switches(cfg!(debug_assertions), |name| std::env::var(name).ok())
}

/// The switches `get` gives when `read`; none otherwise.
fn switches(read: bool, get: impl Fn(&str) -> Option<String>) -> impl Fn(&str) -> Option<String> {
    move |name| if read { get(name) } else { None }
}

/// The app's mark, for the windows' title bars and taskbar buttons: the
/// same one every time. egui tells a window's icon from the one before by
/// where it is kept, so a new copy on every frame would send every window
/// its icon again on every frame.
pub fn app_icon() -> std::sync::Arc<egui::IconData> {
    static ICON: std::sync::OnceLock<std::sync::Arc<egui::IconData>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| std::sync::Arc::new(egui::IconData { rgba: include_bytes!("../../../assets/window-brand-64.rgba").to_vec(), width: 64, height: 64 }))
        .clone()
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

/// The flyout's window title: its name for Windows' accessibility tools,
/// and how the shell finds it while it is hidden. It differs from the
/// ordinary window's title (the app's name), open at the same time.
pub const PANEL_TITLE: &str = "Stream To Speaker controls";

/// The panel's window for `mode`: hidden until the shell has measured it
/// and put it in its place; on the taskbar only as an ordinary window.
pub fn panel_builder(mode: PanelMode) -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(PANEL_TITLE)
        .with_inner_size([panel_window::WIDTH, 400.0])
        .with_visible(false)
        .with_resizable(false)
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_taskbar(mode == PanelMode::Window)
        .with_minimize_button(false)
        .with_maximize_button(false)
        .with_decorations(mode == PanelMode::Window)
}

/// What a view or a window's content asks of the shell while it is drawn.
pub enum Ask {
    /// Send this to the engine, from the app's window.
    Command(Command),
    /// Open this dialog (it waits while another is open).
    Dialog(Box<dyn WindowContent>),
    /// Return to the panel in the ordinary window.
    OpenPanel,
    /// Show this page in the ordinary window, over the one showing.
    View(Box<dyn ViewContent>),
    /// Open what this asks for: a view (over the one showing), a dialog,
    /// Windows' own settings.
    Open(Open),
    /// Go back one view.
    Back,
}

/// The bar at the top of a view: the back arrow, then these.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewBar {
    pub title: String,
    /// Under the title, smaller (*Sonos · playing now*).
    pub line: Option<String>,
    /// One button at the right end (*Test sound*); a press calls
    /// [`ViewContent::bar_button`].
    pub button: Option<String>,
}

/// A view of the panel: it replaces the everyday content, at the same
/// width (a speaker's options, Settings, *Speaker missing?*, *Can't hear
/// it?*, Music Assistant). The panel draws the bar, the back arrow, the
/// scrolling and the line of links around it; going back is the panel's
/// too (the arrow, Esc, Backspace outside a field, Alt+Left). A view
/// carries what it needs (its port, its id) the way a window's content
/// does; opening one never changes what plays.
pub trait ViewContent {
    /// The bar's words.
    fn bar(&self, snap: &Snapshot) -> ViewBar;
    fn has_back(&self) -> bool { true }
    /// The page is covered by another page or its window is closed.
    fn hidden(&mut self) {}

    /// The bar's button was pressed.
    fn bar_button(&mut self, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let _ = (snap, ask);
    }
    /// The content under the bar, in the panel's width; it scrolls when it
    /// is taller than the room. What it asks goes through `ask`.
    fn show(&mut self, ui: &mut egui::Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask));
    /// Whether the view has a line of links at the bottom.
    fn has_links(&self, snap: &Snapshot) -> bool {
        let _ = snap;
        false
    }
    /// The line of links at the bottom, laid out left to right.
    fn links(&mut self, ui: &mut egui::Ui, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) {
        let _ = (ui, snap, ask);
    }
    /// A field holds text that wasn't used yet: the panel reopens on this
    /// view however long it was closed.
    fn holds_unused_text(&self) -> bool {
        false
    }
}

/// The views another part of the app hands the panel, by what asks for
/// them. Views are asked before windows.
pub trait Views {
    /// The view `open` asks for, if it is one of the panel's:
    /// `Open::SpeakerSettings(id)` (a speaker's options, one per row, so
    /// two ways to one speaker are two views), `Open::Settings`,
    /// `Open::SpeakerMissing`, `Open::CantHear(_)`,
    /// `Open::MusicAssistantSettings` and `Open::MusicAssistantHowTo`.
    fn view(&self, open: &Open, snap: &Snapshot) -> Option<Box<dyn ViewContent>>;
}

/// What a dialog draws, in a window the shell made, placed and themed.
pub trait WindowContent {
    /// The window's title.
    fn title(&self, snap: &Snapshot) -> String;
    /// The inner size it opens at, and its smallest, in points.
    fn sizes(&self) -> (Vec2, Vec2);
    /// Draw into the window's `ctx`; what it asks of the shell goes out
    /// through `ask`. True closes the window (a dialog's *Cancel*, or a
    /// dialog the app no longer needs).
    fn show(&mut self, ctx: &egui::Context, snap: &Snapshot, ask: &mut dyn FnMut(Ask)) -> bool;
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
    fn open(&self, open: &Open, snap: &Snapshot, from_window: bool) -> Option<Box<dyn WindowContent>>;
    /// The dialog the app needs open by itself now (a TV's code, Music
    /// Assistant's code), with a key that stays the same while the request
    /// does: a dialog closed by hand isn't opened again for the same key.
    fn wanted(&self) -> Option<(String, Box<dyn WindowContent>)> {
        None
    }
    /// `speaker` was picked in the app's windows: the dialog to fill in
    /// before it can play (its password), if one is needed. The pick is
    /// then made by the dialog.
    fn before_play(&self, speaker: &crate::model::state::SpeakerId, snap: &Snapshot) -> Option<Box<dyn WindowContent>> {
        let _ = (speaker, snap);
        None
    }
}

/// The shell's ties to the rest of the app.
pub struct Wiring {
    /// The last height chosen by dragging the ordinary window, in points.
    pub window_height: Option<f32>,
    pub save_height: Box<dyn Fn(f32)>,
    /// The engine (or the scripted one).
    pub port: std::sync::Arc<dyn crate::model::port::Port>,
    /// The panel's views (asked first).
    pub views: Box<dyn Views>,
    /// The windows of their own (dialogs) and what else opening does.
    pub contents: Box<dyn Contents>,
    /// Hands the engine the shell's door for navigation commands that come
    /// from elsewhere (a notification, the phone page). `None`: only the
    /// shell's own surfaces open things.
    pub route_opens: Option<Box<dyn FnOnce(Box<dyn Fn(crate::model::Surface, Open) + Send + Sync>)>>,
    /// Quit for good: give the sound back and end the app.
    pub quit: Box<dyn Fn(crate::model::Surface)>,
    /// The app is quitting another way (Ctrl+C, an update): close.
    pub quitting: Box<dyn Fn() -> bool>,
    /// Called every frame (at least four times a second): what the app
    /// does while its windows run (a settings save that failed is tried
    /// again).
    pub tick: Box<dyn Fn()>,
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

/// For the log: what a window's extended style bits (`GWL_EXSTYLE`) say
/// about how it behaves, the ones the shell sets or relies on: out of
/// Alt+Tab and the taskbar (tool window), never activated, on top, and
/// whether the mouse passes through it. `WS_EX_TRANSPARENT` alone only
/// orders painting (the window is painted after its siblings beneath it);
/// on a layered window (`WS_EX_LAYERED`) it makes the mouse pass through to
/// the windows beneath, as Microsoft's extended window styles and layered
/// windows pages describe.
pub fn ex_style_words(ex: u32) -> String {
    const TRANSPARENT: u32 = 0x0000_0020;
    const LAYERED: u32 = 0x0008_0000;
    const WORDS: [(u32, &str); 4] = [(0x0000_0080, "tool window"), (0x0800_0000, "never active"), (0x0004_0000, "app window"), (0x0000_0008, "on top")];
    let mut words: Vec<&str> = WORDS.iter().filter(|(bit, _)| ex & bit != 0).map(|(_, w)| *w).collect();
    match (ex & LAYERED != 0, ex & TRANSPARENT != 0) {
        (true, true) => words.push("layered and transparent: clicks pass through"),
        (true, false) => words.push("layered"),
        (false, true) => words.push("transparent (painted after its siblings)"),
        (false, false) => {}
    }
    if words.is_empty() {
        "plain".to_string()
    } else {
        words.join(", ")
    }
}

/// Extended style bits the shell gives one of its windows and keeps there:
/// `set` always on, `clear` always off, whatever else writes the window's
/// style. (winit writes a window's whole extended style again whenever it
/// changes a flag of its own, showing the window among them, and its style
/// has no tool-window or never-activate bit and has `WS_EX_APPWINDOW`,
/// which forces a window onto the taskbar.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeptStyle {
    pub set: u32,
    pub clear: u32,
}

/// `WS_EX_TOOLWINDOW`: out of the taskbar and of Alt+Tab.
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
/// `WS_EX_NOACTIVATE`: a click doesn't make it the foreground window.
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
/// `WS_EX_APPWINDOW`: forced onto the taskbar while shown.
const WS_EX_APPWINDOW: u32 = 0x0004_0000;

impl KeptStyle {
    /// Out of Alt+Tab and the taskbar: the panel as a flyout.
    pub const OUT_OF_ALT_TAB: KeptStyle = KeptStyle { set: WS_EX_TOOLWINDOW, clear: WS_EX_APPWINDOW };
    /// Out of Alt+Tab and the taskbar and never activated: the root window
    /// and the small windows by the clock.
    pub const NEVER_ACTIVE: KeptStyle = KeptStyle { set: WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, clear: WS_EX_APPWINDOW };

    /// The extended style `ex` as the window keeps it.
    pub fn applied_to(self, ex: u32) -> u32 {
        (ex | self.set) & !self.clear
    }
}

/// Which window of the shell, for the style it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellWindow {
    Root(RootMode),
    Panel(PanelMode),
    Popup,
}

impl ShellWindow {
    /// The style the shell keeps on this window, if any.
    pub fn kept_style(self) -> Option<KeptStyle> {
        match self {
            // Hidden for good once the first frame is drawn.
            ShellWindow::Root(RootMode::Hidden) => None,
            ShellWindow::Root(RootMode::ToolWindow) | ShellWindow::Popup => Some(KeptStyle::NEVER_ACTIVE),
            ShellWindow::Panel(PanelMode::Flyout) => Some(KeptStyle::OUT_OF_ALT_TAB),
            // An ordinary window, as winit makes it: on the taskbar and in
            // Alt+Tab.
            ShellWindow::Panel(PanelMode::Window) => None,
        }
    }
}

/// What the shell asks of Windows and does through it.
pub trait Desktop {
    /// Every monitor, with its work area and scale.
    fn monitors(&self) -> Vec<Monitor>;
    /// The work area on which to centre a newly opened ordinary window.
    fn opening_monitor(&mut self, screens: &[Monitor]) -> std::task::Poll<Option<Monitor>>;
    /// The taskbar is dark (its icons must be light).
    fn taskbar_is_dark(&self) -> bool;
    /// Apps are set to dark.
    fn apps_are_dark(&self) -> bool;
    /// Put the panel's window at `rect` with `mode`'s look; true once it is
    /// there (moving a window to a monitor of another scale may take
    /// Windows a frame or two).
    fn place_panel(&mut self, rect: PxRect, mode: PanelMode) -> bool;
    /// Put the hidden small window called `title` at `rect`, never to be
    /// activated; true once it is there.
    fn place_popup(&mut self, title: &str, rect: PxRect) -> bool;
    /// Whether the panel's window is on screen, once it has been placed.
    fn panel_window_shown(&self) -> Option<bool>;
    /// The extended style bits of the panel's window, once it has been
    /// placed (see [`ex_style_words`]).
    fn panel_ex_style(&self) -> Option<u32>;
    /// The window that has the foreground, if one has (see
    /// [`describe_foreground`]).
    fn foreground(&self) -> Option<Foreground>;
}

/// The window that has the foreground, as Windows tells it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Foreground {
    /// Its window class.
    pub class: String,
    /// Its process.
    pub pid: u32,
    /// The file name of its program, if Windows says.
    pub program: Option<String>,
    /// Its title, when it is one of this app's windows.
    pub own_title: Option<String>,
}

/// Windows' own shell windows, by class and program, and what they are.
const SHELL_WINDOWS: [(&str, &str, &str); 14] = [
    ("Shell_TrayWnd", "explorer.exe", "the taskbar"),
    ("Shell_SecondaryTrayWnd", "explorer.exe", "the taskbar on another display"),
    ("NotifyIconOverflowWindow", "explorer.exe", "the hidden icons"),
    ("TopLevelWindowForOverflowXamlIsland", "explorer.exe", "the hidden icons"),
    ("Progman", "explorer.exe", "the desktop"),
    ("WorkerW", "explorer.exe", "the desktop"),
    ("#32768", "explorer.exe", "a menu of the taskbar or the desktop"),
    ("XamlExplorerHostIslandWindow", "explorer.exe", "switching windows"),
    ("MultitaskingViewFrame", "explorer.exe", "switching windows"),
    ("Windows.UI.Core.CoreWindow", "StartMenuExperienceHost.exe", "Start"),
    ("Windows.UI.Core.CoreWindow", "SearchHost.exe", "search"),
    ("Windows.UI.Core.CoreWindow", "SearchApp.exe", "search"),
    ("Windows.UI.Core.CoreWindow", "ShellExperienceHost.exe", "a panel of the taskbar"),
    ("Windows.UI.Core.CoreWindow", "ShellHost.exe", "a panel of the taskbar"),
];

/// For the log: who has the foreground. The log file is something people
/// attach to reports, so at the level a release writes only Windows' own
/// shell windows (the taskbar, the hidden icons, Start, recognised by their
/// class and their program) and this app's windows are named; any other
/// window is "another program". `detailed` (the debug level) names every
/// window by its class and program.
pub fn describe_foreground(fg: Option<&Foreground>, detailed: bool) -> String {
    let Some(fg) = fg else { return "none".to_string() };
    if let Some(title) = &fg.own_title {
        return format!("this app's window \"{title}\" ({})", fg.class);
    }
    let program = fg.program.as_deref();
    let shell = SHELL_WINDOWS.iter().find(|(class, shell_program, _)| *class == fg.class && program.is_some_and(|p| p.eq_ignore_ascii_case(shell_program)));
    match (shell, detailed) {
        (Some((class, shell_program, what)), _) => format!("{what} ({class}, {shell_program})"),
        (None, true) => format!("{} ({})", fg.class, program.map_or_else(|| format!("process {}", fg.pid), str::to_string)),
        (None, false) => "another program".to_string(),
    }
}

/// A desktop for tests: fixed monitors and themes; it records where the
/// panel was put and can be slow to put it there.
#[derive(Clone, Debug, Default)]
pub struct FakeDesktop {
    pub monitors: Vec<Monitor>,
    pub pointer: Option<PxRect>,
    pub dark_taskbar: bool,
    pub dark_apps: bool,
    /// How many calls to `place_panel` fail before one succeeds.
    pub slow: u32,
    pub placed: Vec<PxRect>,
    /// What `panel_window_shown` says.
    pub panel_shown: Option<bool>,
    /// What `foreground` says.
    pub foreground: Option<Foreground>,
}

impl Desktop for FakeDesktop {
    fn monitors(&self) -> Vec<Monitor> {
        self.monitors.clone()
    }

    fn opening_monitor(&mut self, screens: &[Monitor]) -> std::task::Poll<Option<Monitor>> {
        std::task::Poll::Ready(placement::monitor_for(screens, self.pointer).copied())
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

    fn place_popup(&mut self, _title: &str, rect: PxRect) -> bool {
        self.placed.push(rect);
        true
    }

    fn panel_window_shown(&self) -> Option<bool> {
        self.panel_shown
    }

    fn panel_ex_style(&self) -> Option<u32> {
        None
    }

    fn foreground(&self) -> Option<Foreground> {
        self.foreground.clone()
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
    fn a_release_build_runs_as_if_no_switch_were_set() {
        let env = |name: &str| match name {
            "STREAM_TO_SPEAKER_SCENARIO" => Some("kitchen-playing".to_string()),
            "STREAM_TO_SPEAKER_ROOT" => Some("hidden".to_string()),
            "STREAM_TO_SPEAKER_PANEL" => Some("window".to_string()),
            _ => None,
        };
        let release = switches(false, env);
        assert_eq!(ShellOptions::from(&release, true), ShellOptions::from(|_| None, true));
        assert_eq!(scenario_from(&release), None);
        let debug = switches(true, env);
        let opts = ShellOptions::from(&debug, true);
        assert_eq!((opts.root, opts.panel), (RootMode::Hidden, PanelMode::Window));
        assert_eq!(scenario_from(&debug), Some("kitchen-playing"));
        // This build's own environment, where `PATH` is always set: read in
        // a debug build only (`cargo test --release` runs the other side).
        assert_eq!(env_switches()("PATH").is_some(), cfg!(debug_assertions));
    }

    #[test]
    fn the_log_names_windows_shell_and_this_apps_windows_and_no_other_program_unless_asked() {
        let fg = |class: &str, program: Option<&str>| Foreground { class: class.into(), pid: 4242, program: program.map(Into::into), own_title: None };
        let taskbar = fg("Shell_TrayWnd", Some("explorer.exe"));
        let hidden = fg("TopLevelWindowForOverflowXamlIsland", Some("EXPLORER.EXE"));
        let start = fg("Windows.UI.Core.CoreWindow", Some("StartMenuExperienceHost.exe"));
        let chat = fg("Chrome_WidgetWin_1", Some("chat.exe"));
        // A program that gives its window the taskbar's class is not the taskbar.
        let lookalike = fg("Shell_TrayWnd", Some("lookalike.exe"));
        let unknown = fg("SomeClass", None);
        let own = Foreground { own_title: Some("Stream To Speaker (background)".into()), ..fg("Window Class", Some("stream-to-speaker.exe")) };
        for detailed in [false, true] {
            assert_eq!(describe_foreground(Some(&taskbar), detailed), "the taskbar (Shell_TrayWnd, explorer.exe)");
            assert_eq!(describe_foreground(Some(&hidden), detailed), "the hidden icons (TopLevelWindowForOverflowXamlIsland, explorer.exe)");
            assert_eq!(describe_foreground(Some(&start), detailed), "Start (Windows.UI.Core.CoreWindow, StartMenuExperienceHost.exe)");
            assert_eq!(describe_foreground(Some(&own), detailed), "this app's window \"Stream To Speaker (background)\" (Window Class)");
            assert_eq!(describe_foreground(None, detailed), "none");
        }
        // At the level a release writes: no other program by name.
        for other in [&chat, &lookalike, &unknown] {
            assert_eq!(describe_foreground(Some(other), false), "another program");
        }
        // At the debug level: every window by class and program.
        assert_eq!(describe_foreground(Some(&chat), true), "Chrome_WidgetWin_1 (chat.exe)");
        assert_eq!(describe_foreground(Some(&lookalike), true), "Shell_TrayWnd (lookalike.exe)");
        assert_eq!(describe_foreground(Some(&unknown), true), "SomeClass (process 4242)");
    }

    #[test]
    fn the_shells_windows_keep_their_styles_when_winit_writes_the_style_again() {
        // What winit 0.30 writes when it shows a window: WS_EX_WINDOWEDGE |
        // WS_EX_ACCEPTFILES, WS_EX_APPWINDOW for every window without an
        // owner (one made "off the taskbar" too: winit deletes its taskbar
        // button instead), WS_EX_TOPMOST for one on top, WS_EX_TRANSPARENT |
        // WS_EX_LAYERED for one clicks pass through.
        let (plain, app, top, through) = (0x0000_0110, 0x0004_0000, 0x0000_0008, 0x0008_0020);
        let kept = |w: ShellWindow, ex: u32| ex_style_words(w.kept_style().map_or(ex, |k| k.applied_to(ex)));
        assert_eq!(kept(ShellWindow::Panel(PanelMode::Flyout), plain | app | top), "tool window, on top");
        assert_eq!(kept(ShellWindow::Panel(PanelMode::Window), plain | app | top), "app window, on top");
        assert_eq!(
            kept(ShellWindow::Root(RootMode::ToolWindow), plain | app | through),
            "tool window, never active, layered and transparent: clicks pass through"
        );
        assert_eq!(kept(ShellWindow::Popup, plain | app | top), "tool window, never active, on top");
        assert_eq!(ShellWindow::Root(RootMode::Hidden).kept_style(), None);
        // Bits the shell doesn't keep are left as written.
        assert_eq!(KeptStyle::NEVER_ACTIVE.applied_to(plain | top) & (plain | top), plain | top);
    }

    #[test]
    fn the_log_names_the_window_styles_the_shell_sets_and_relies_on() {
        // WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST, as a pop-up has them.
        assert_eq!(ex_style_words(0x0800_0088), "tool window, never active, on top");
        // WS_EX_WINDOWEDGE | WS_EX_ACCEPTFILES, what winit gives a plain window.
        assert_eq!(ex_style_words(0x0000_0110), "plain");
        // WS_EX_TRANSPARENT only orders painting; with WS_EX_LAYERED it lets
        // the mouse through.
        assert_eq!(ex_style_words(0x0008_0020 | 0x0004_0000), "app window, layered and transparent: clicks pass through");
        assert_eq!(ex_style_words(0x0000_0020), "transparent (painted after its siblings)");
        assert_eq!(ex_style_words(0x0008_0000), "layered");
    }

    #[test]
    fn only_the_flyout_closes_when_it_loses_the_focus() {
        assert!(PanelMode::Flyout.closes_on_blur());
        assert!(!PanelMode::Window.closes_on_blur());
    }

    #[test]
    fn the_panel_as_an_ordinary_window_has_its_taskbar_button_and_as_a_flyout_none() {
        // A window that can go behind others needs a button to bring it
        // back (and a place in Alt+Tab); a flyout by the clock has neither.
        let window = panel_builder(PanelMode::Window);
        assert_eq!((window.taskbar, window.decorations), (Some(true), Some(true)));
        assert_eq!(ShellWindow::Panel(PanelMode::Window).kept_style(), None);
        let flyout = panel_builder(PanelMode::Flyout);
        assert_eq!((flyout.taskbar, flyout.decorations), (Some(false), Some(false)));
        assert_eq!(ShellWindow::Panel(PanelMode::Flyout).kept_style(), Some(KeptStyle::OUT_OF_ALT_TAB));
        // With no tray, the app always opens its main window.
        assert!(ShellOptions::from(|_| None, false).effective().open_at_start);
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
    fn dialogs_open_in_the_middle_of_the_primary_monitor() {
        let primary = Monitor { bounds: PxRect::new(0, 0, 2880, 1620), work: PxRect::new(0, 0, 2880, 1548), scale: 1.5, primary: true };
        let other = Monitor { bounds: PxRect::new(-1920, 0, 0, 1080), work: PxRect::new(-1920, 0, 0, 1032), scale: 1.0, primary: false };
        let p = centred(&[other, primary], Vec2::new(480.0, 260.0)).unwrap();
        assert_eq!(p, Pos2::new(960.0 - 240.0, 516.0 - 130.0));
        // Bigger than the work area: its corner stays on screen.
        let p = centred(&[primary], Vec2::new(4000.0, 4000.0)).unwrap();
        assert_eq!(p, Pos2::new(0.0, 0.0));
    }
}
