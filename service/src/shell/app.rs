//! The shell as an eframe application: the root window, which only holds
//! the others; the panel, Settings and dialogs as windows of their own,
//! drawn in the root's frames; the tray icon; a second launch.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use eframe::egui;
use egui::{Frame, ViewportBuilder, ViewportCommand, ViewportId};
use windows_sys::Win32::Foundation::HWND;

use super::content::WantedGate;
use super::panel_window::{OpenView, PanelWindow, Seen, Step, Toggle, TrayClick};
use super::popup::{popup_builder, popup_rect, PopupContent, PopupKind, StandInPopup};
use super::views::ViewMemory;
use super::tray::{Tray, TrayEvent};
use super::placement::PxRect;
use super::win::{self, WindowsDesktop};
use super::{centred, panel_builder, root_builder, Ask, Desktop, Placeholders, RootMode, ShellOptions, ViewContent, Views, WindowContent, WindowKind, Wiring, ROOT_TITLE};
use crate::panel::view::{show_view, ViewState};
use crate::model::command::{Command, Open, Surface};
use crate::model::state::Place;
use crate::model::port::Watch;
use crate::model::scripted::{RealTime, Scenario, ScriptedEngine};
use crate::model::snapshot::Snapshot;
use crate::ui::theme::{self, Mode};

fn panel_id() -> ViewportId {
    ViewportId::from_hash_of("stream_to_speaker::shell::panel")
}

fn settings_id() -> ViewportId {
    ViewportId::from_hash_of("stream_to_speaker::shell::settings")
}

fn dialog_id() -> ViewportId {
    ViewportId::from_hash_of("stream_to_speaker::shell::dialog")
}

fn popup_id(kind: PopupKind) -> ViewportId {
    ViewportId::from_hash_of(("stream_to_speaker::shell::popup", kind))
}

/// A small window that must not take the focus, while it shows.
struct OpenPopup {
    kind: PopupKind,
    content: Box<dyn PopupContent + Send>,
    /// Where it was put, once it is there (then it shows).
    rect: Option<PxRect>,
}

/// How often the shell looks at what has no event of its own (a second
/// launch, the light or dark setting, a dialog the app wants) while
/// nothing is open.
const IDLE: Duration = Duration::from_millis(250);
/// How often the open panel redraws (the level meter).
const METER: Duration = Duration::from_millis(33);

/// The scenario `STREAM_TO_SPEAKER_SCENARIO` names, if it names one: the
/// shell then runs on the scripted engine, for trying every state without
/// speakers.
pub fn scripted_scenario() -> Option<Scenario> {
    std::env::var("STREAM_TO_SPEAKER_SCENARIO").ok().and_then(|n| Scenario::from_name(n.trim()))
}

/// Run the shell on the scripted engine in `scenario`, with the views on
/// the scripted settings and placeholder windows for the rest.
pub fn run_scripted(opts: ShellOptions, scenario: Scenario) -> anyhow::Result<()> {
    let engine = Arc::new(ScriptedEngine::new(scenario));
    let _clock = RealTime::start(engine.clone(), Duration::from_millis(50));
    let settings: Arc<dyn crate::settings::SettingsPort> = Arc::new(crate::settings::scripted::ScriptedSettings::new());
    let views = Box::new(crate::settings::views::SettingsViews::new(settings));
    // The two small windows' stand-ins show once at start, to try them.
    let popups: Box<dyn FnOnce(super::popup::PopupDoor)> = Box::new(|door| {
        door(PopupKind::Confirmation, Box::new(StandInPopup::new(PopupKind::Confirmation, "The confirmation by the clock")));
        door(PopupKind::Notification, Box::new(StandInPopup::new(PopupKind::Notification, "A notification")));
    });
    run(
        opts,
        Wiring {
            port: engine,
            views,
            contents: Box::new(Placeholders),
            route_opens: None,
            quit: Box::new(|| {}),
            quitting: Box::new(|| false),
            popups: Some(popups),
            tick: Box::new(|| {}),
        },
    )
}

/// Run the shell on the running app: the engine as its port, the views and
/// dialogs on the app's settings, Quit through the app's own quit (which
/// gives the sound back).
pub fn run_app(app: Arc<crate::app::App>, engine: Arc<crate::engine::Engine>, opts: ShellOptions) -> anyhow::Result<()> {
    let settings: Arc<dyn crate::settings::SettingsPort> = Arc::new(crate::settings::real::AppSettings::new(app.clone()));
    let (quit, quitting) = (app.clone(), app.clone());
    let route = engine.clone();
    run(
        opts,
        Wiring {
            port: engine,
            views: Box::new(crate::settings::views::SettingsViews::new(settings.clone())),
            contents: Box::new(super::content::SettingsContents {
                port: settings,
                open_windows_page: win::open_windows_page,
                needs_password: {
                    let app = app.clone();
                    Box::new(move |id| app.is_password_protected(id) && (app.airplay_password(id).is_none() || app.asked_for_password(id)))
                },
            }),
            route_opens: Some(Box::new(move |opener| route.on_open(opener))),
            quit: Box::new(move || quit.request_shutdown()),
            quitting: Box::new(move || quitting.is_shutting_down()),
            popups: None,
            tick: {
                let app = app.clone();
                Box::new(move || {
                    let _ = app.retry_settings_save_if_due(std::time::Instant::now());
                })
            },
        },
    )
}

/// Run the shell until Quit.
pub fn run(opts: ShellOptions, wiring: Wiring) -> anyhow::Result<()> {
    let opts = opts.effective();
    let native = eframe::NativeOptions { viewport: root_builder(opts.root).with_icon(app_icon()), persist_window: false, ..Default::default() };
    eframe::run_native(
        ROOT_TITLE,
        native,
        Box::new(move |cc| {
            crate::fonts::install(&cc.egui_ctx);
            let Wiring { port, views, contents, route_opens, quit, quitting, popups, tick } = wiring;
            let (popup_tx, popup_rx) = channel::<(PopupKind, Box<dyn PopupContent + Send>)>();
            if let Some(hand) = popups {
                let ctx = cc.egui_ctx.clone();
                hand(Box::new(move |kind, content| {
                    let _ = popup_tx.send((kind, content));
                    ctx.request_repaint();
                }));
            }
            let ctx = cc.egui_ctx.clone();
            let watch = port.watch(Arc::new(move || ctx.request_repaint()));
            let (open_tx, open_rx) = channel::<Open>();
            if let Some(route) = route_opens {
                let ctx = cc.egui_ctx.clone();
                route(Box::new(move |o| {
                    let _ = open_tx.send(o);
                    ctx.request_repaint();
                }));
            }
            let root = {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                cc.window_handle().ok().and_then(|h| match h.as_raw() {
                    RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
                    _ => None,
                })
            };
            let (tray_tx, tray_rx) = channel();
            Ok(Box::new(Shell {
                port,
                _watch: watch,
                views,
                memory: ViewMemory::default(),
                now: 0.0,
                contents,
                quit,
                quitting,
                tick,
                opts,
                desktop: WindowsDesktop::default(),
                root,
                frames: 0,
                mode: None,
                dark_taskbar: false,
                looked_at: f64::NEG_INFINITY,
                panel: None,
                toggle: Toggle::default(),
                settings: None,
                dialog: None,
                waiting: VecDeque::new(),
                wanted: WantedGate::default(),
                tray: None,
                tray_tx,
                tray_rx,
                opens: open_rx,
                popups: Vec::new(),
                popup_rx,
                tray_tickets: Vec::new(),
                started: false,
                closing: false,
            }))
        }),
    )
    .map_err(|e| anyhow!("the windows couldn't start: {e}"))
}

/// The app's mark, for the windows' title bars and taskbar buttons.
pub fn app_icon() -> Arc<egui::IconData> {
    Arc::new(egui::IconData { rgba: include_bytes!("../../../assets/window-brand-64.rgba").to_vec(), width: 64, height: 64 })
}

struct Shell {
    port: Arc<dyn crate::model::port::Port>,
    _watch: Watch,
    views: Box<dyn Views>,
    /// The views the panel closed on, to open on them again.
    memory: ViewMemory<OpenView>,
    /// This frame's time, in egui's seconds.
    now: f64,
    contents: Box<dyn super::Contents>,
    quit: Box<dyn Fn()>,
    quitting: Box<dyn Fn() -> bool>,
    tick: Box<dyn Fn()>,
    opts: ShellOptions,
    desktop: WindowsDesktop,
    /// The root window's handle.
    root: Option<isize>,
    frames: u64,
    mode: Option<Mode>,
    dark_taskbar: bool,
    /// When the settings were last read, in egui's seconds.
    looked_at: f64,
    panel: Option<PanelWindow>,
    toggle: Toggle,
    settings: Option<Box<dyn WindowContent>>,
    dialog: Option<Box<dyn WindowContent>>,
    /// Dialogs asked for while one was open.
    waiting: VecDeque<Box<dyn WindowContent>>,
    wanted: WantedGate,
    tray: Option<Tray>,
    tray_tx: Sender<TrayEvent>,
    tray_rx: Receiver<TrayEvent>,
    /// Navigation commands from other surfaces, through the engine.
    opens: Receiver<Open>,
    popups: Vec<OpenPopup>,
    popup_rx: Receiver<(PopupKind, Box<dyn PopupContent + Send>)>,
    /// Places picked in the tray's menu, until the engine has answered.
    tray_tickets: Vec<crate::model::command::Ticket>,
    started: bool,
    closing: bool,
}

impl Shell {
    fn root_hwnd(&self) -> Option<HWND> {
        self.root.map(|h| h as HWND)
    }

    /// The root stays out of sight: hidden, or a pixel no one can reach.
    fn keep_root_away(&mut self) {
        let Some(root) = self.root_hwnd() else { return };
        match self.opts.root {
            RootMode::Hidden => {
                if self.frames >= 2 && win::is_visible(root) {
                    win::hide(root);
                }
            }
            RootMode::ToolWindow => {
                if self.frames == 1 {
                    win::keep_out_of_alt_tab(root, true);
                }
            }
        }
    }

    /// Follow Windows' light or dark setting for apps and for the taskbar.
    fn follow_settings(&mut self, ctx: &egui::Context, now: f64) {
        if now - self.looked_at < 1.0 {
            return;
        }
        self.looked_at = now;
        let mode = if self.desktop.apps_are_dark() { Mode::Dark } else { Mode::Light };
        if self.mode != Some(mode) {
            self.mode = Some(mode);
            ctx.set_theme(match mode {
                Mode::Dark => egui::Theme::Dark,
                Mode::Light => egui::Theme::Light,
            });
            theme::apply(ctx, mode);
        }
        self.dark_taskbar = self.desktop.taskbar_is_dark();
    }

    fn sync_tray(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        if !self.opts.tray {
            return;
        }
        match &mut self.tray {
            Some(tray) => tray.sync(&snap.tray, self.dark_taskbar),
            None => {
                let ctx = ctx.clone();
                match Tray::new(&snap.tray, self.dark_taskbar, self.tray_tx.clone(), move || ctx.request_repaint()) {
                    Ok(t) => self.tray = Some(t),
                    Err(e) => {
                        // Nothing to open the panel from: it becomes a
                        // window of its own.
                        log::warn!("no tray icon: {e:#}");
                        self.opts = ShellOptions { tray: false, ..self.opts }.effective();
                    }
                }
            }
        }
    }

    fn open_panel(&mut self, ctx: &egui::Context, icon: Option<super::placement::PxRect>) {
        match &self.panel {
            Some(p) if p.shown() => ctx.send_viewport_cmd_to(panel_id(), ViewportCommand::Focus),
            Some(_) => {}
            None => {
                let mut p = PanelWindow::open(icon.or_else(|| self.tray.as_ref().and_then(|t| t.rect())));
                // On the view it closed on, a moment ago or with text typed.
                p.views = self.memory.reopen(self.now, |(v, _)| v.holds_unused_text());
                self.panel = Some(p);
                // Opened with the mouse: the focus ring waits for a key.
                crate::ui::look::set_focus_visible(ctx, false);
            }
        }
    }

    /// Show `view` in the panel: over the one showing (`push`), or in place
    /// of whatever shows, opening the panel if it is closed.
    fn show_view(&mut self, ctx: &egui::Context, view: Box<dyn ViewContent>, push: bool) {
        if self.panel.is_none() {
            self.open_panel(ctx, None);
            self.memory.forget();
            if let Some(p) = &mut self.panel {
                p.views.clear();
            }
        }
        if let Some(p) = &mut self.panel {
            if !push {
                p.views.clear();
            }
            p.views.push((view, ViewState::opened()));
        }
        ctx.send_viewport_cmd_to(panel_id(), ViewportCommand::Focus);
    }

    fn close_panel(&mut self) {
        if let Some(p) = self.panel.take() {
            if p.shown() {
                let _ = self.port.send(Surface::Panel, Command::PanelShown(false));
            }
            self.memory.closed(p.views, self.now);
        }
        // Without a tray icon the panel is the app's window.
        if !self.opts.tray {
            self.quit_now();
        }
    }

    fn quit_now(&mut self) {
        if !self.closing {
            self.closing = true;
            (self.quit)();
        }
    }

    fn open_dialog(&mut self, content: Box<dyn WindowContent>) {
        if self.dialog.is_some() {
            self.waiting.push_back(content);
        } else {
            self.dialog = Some(content);
        }
    }

    /// Open what `open` asks for: the panel, one of its views (over the one
    /// showing with `push`), a dialog, or something outside the app.
    /// `from_window`: asked in the panel or Settings.
    fn open(&mut self, ctx: &egui::Context, open: &Open, snap: &Snapshot, from_window: bool, push: bool) {
        if matches!(open, Open::Panel) {
            self.open_panel(ctx, None);
            return;
        }
        let redirected = super::content::pair_from_outside(open, snap, from_window);
        let open = redirected.as_ref().unwrap_or(open);
        if let Some(view) = self.views.view(open, snap) {
            self.show_view(ctx, view, push);
            return;
        }
        self.open_window(ctx, open, snap, from_window);
    }

    /// Open what `open` asks for as a window (or outside the app), even
    /// where it is also a view.
    fn open_window(&mut self, ctx: &egui::Context, open: &Open, snap: &Snapshot, from_window: bool) {
        if let Some(s) = &mut self.settings {
            if s.navigate(open) {
                ctx.send_viewport_cmd_to(settings_id(), ViewportCommand::Focus);
                return;
            }
        }
        match self.contents.open(open, snap, from_window) {
            Some((WindowKind::Settings, content)) => {
                self.settings = Some(content);
                ctx.send_viewport_cmd_to(settings_id(), ViewportCommand::Focus);
            }
            Some((WindowKind::Dialog, content)) => self.open_dialog(content),
            None => {}
        }
    }

    /// Do what a command asks of the windows; send the rest to the engine.
    fn route(&mut self, ctx: &egui::Context, from: Surface, command: Command, snap: &Snapshot) {
        let window = matches!(from, Surface::Panel | Surface::Settings);
        // A speaker picked in the panel that needs its password first: the
        // dialog, which connects once it is typed.
        if let (true, Command::Play(Place::Speaker(id)) | Command::PlayTakingOver(id)) = (window, &command) {
            if let Some(dialog) = self.contents.before_play(id, snap) {
                self.open_dialog(dialog);
                return;
            }
        }
        match command {
            Command::Open(open) => self.open(ctx, &open, snap, window, false),
            Command::Quit => self.quit_now(),
            command => {
                let pick = matches!(command, Command::Play(_) | Command::PlayTakingOver(_));
                let ticket = self.port.send(from, command);
                if pick && from == Surface::Tray {
                    self.tray_tickets.push(ticket);
                }
            }
        }
    }

    fn content_builder(&self, content: &dyn WindowContent, snap: &Snapshot, kind: WindowKind) -> ViewportBuilder {
        let (size, min) = content.sizes();
        let b = ViewportBuilder::default()
            .with_title(content.title(snap))
            .with_inner_size(size)
            .with_min_inner_size(min)
            .with_icon(app_icon())
            .with_resizable(kind == WindowKind::Settings)
            .with_minimize_button(kind == WindowKind::Settings)
            .with_maximize_button(kind == WindowKind::Settings);
        match centred(&self.desktop.monitors(), size) {
            Some(p) => b.with_position(p),
            None => b,
        }
    }

    /// Draw the panel's window; what it sends and what its view asks.
    fn panel_frame(&mut self, ctx: &egui::Context, snap: &Snapshot, now: f64) -> (Vec<Command>, Vec<Ask>) {
        let Some(panel) = self.panel.as_mut() else { return (Vec::new(), Vec::new()) };
        let mode = self.opts.panel;
        let pinned = snap.first_run.is_some();
        let (port, desktop) = (&self.port, &mut self.desktop);
        let builder = panel_builder(mode).with_icon(app_icon());
        let (commands, step) = ctx.show_viewport_immediate(panel_id(), builder, |ctx, _class| {
            let t = theme::Theme::of(ctx);
            let (focused, inner_height, close) = ctx.input(|i| {
                let v = i.viewport();
                (v.focused, v.inner_rect.map(|r| r.height()), v.close_requested())
            });
            if close {
                // Its × (the small-window form): the shell closes it.
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            }
            let cap = panel.state.height;
            let depth = panel.views.len().saturating_sub(1);
            let (out, asks) = egui::CentralPanel::default()
                .frame(Frame::none().fill(t.c.surface))
                .show(ctx, |ui| match panel.views.last_mut() {
                    Some((view, vs)) => {
                        vs.cap = cap;
                        let o = show_view(ui, view.as_mut(), snap, vs, depth);
                        let mut asks = o.asks;
                        if o.back {
                            asks.push(Ask::Back);
                        }
                        (crate::panel::PanelOutput { natural_height: o.natural_height, ..Default::default() }, asks)
                    }
                    None => (crate::panel::show(ui, snap, port.level(), &mut panel.state), Vec::new()),
                })
                .inner;
            let seen = Seen { natural_height: out.natural_height, close: out.close || close, inner_height, focused };
            let step = panel.after_frame(seen, pinned, mode, desktop);
            if step == Step::Show {
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            ctx.request_repaint_after(METER);
            ((out.commands, asks), step)
        });
        match step {
            Step::Show => {
                let _ = self.port.send(Surface::Panel, Command::PanelShown(true));
            }
            Step::CloseOnBlur => {
                self.toggle.closed_by_blur(now);
                self.close_panel();
            }
            Step::Close => self.close_panel(),
            Step::Wait => {}
        }
        commands
    }

    /// Draw the small windows that must not take the focus; place a new
    /// one, then show it without activating it.
    fn popup_frames(&mut self, ctx: &egui::Context, snap: &Snapshot, now: f64) {
        let monitors = self.desktop.monitors();
        let icon = self.tray.as_ref().and_then(|t| t.rect());
        let mut under: Option<PxRect> = self.panel.as_ref().and_then(|p| p.rect());
        let mut keep = Vec::new();
        for mut p in std::mem::take(&mut self.popups) {
            let size = p.content.size();
            if p.rect.is_none() {
                if let Some(rect) = popup_rect(&monitors, icon, size, under) {
                    if self.desktop.place_popup(p.kind.title(), rect) {
                        p.rect = Some(rect);
                        ctx.send_viewport_cmd_to(popup_id(p.kind), ViewportCommand::Visible(true));
                    }
                }
            }
            let t = theme::Theme::of(ctx);
            let content = &mut p.content;
            let gone = ctx.show_viewport_immediate(popup_id(p.kind), popup_builder(p.kind, size).with_icon(app_icon()), |ctx, _class| {
                egui::CentralPanel::default()
                    .frame(Frame::none().fill(t.c.surface).stroke(t.card_stroke()))
                    .show(ctx, |ui| content.show(ui, snap, now, &mut |_| {}))
                    .inner
            });
            if !gone {
                under = p.rect.or(under);
                ctx.request_repaint_after(Duration::from_millis(250));
                keep.push(p);
            }
        }
        self.popups = keep;
    }

    /// Do what a view or a window's content asked.
    fn answer(&mut self, ctx: &egui::Context, ask: Ask, snap: &Snapshot, sent: &mut Vec<(Surface, Command)>) {
        match ask {
            Ask::Command(c) => sent.push((Surface::Panel, c)),
            Ask::Dialog(d) => self.open_dialog(d),
            Ask::OpenPanel => self.open_panel(ctx, None),
            Ask::View(v) => self.show_view(ctx, v, true),
            Ask::Open(o) => self.open(ctx, &o, snap, true, true),
            Ask::Window(o) => self.open_window(ctx, &o, snap, true),
            Ask::Back => {
                if let Some(p) = &mut self.panel {
                    p.views.pop();
                    if p.views.is_empty() {
                        // Back on the everyday content, the keyboard where
                        // it starts.
                        p.state.focus_current_row = true;
                    }
                }
            }
        }
    }

    /// Draw the Settings window or the dialog; what it asks.
    fn content_frame(&mut self, ctx: &egui::Context, snap: &Snapshot, kind: WindowKind) -> Vec<Ask> {
        let (id, slot) = match kind {
            WindowKind::Settings => (settings_id(), self.settings.take()),
            WindowKind::Dialog => (dialog_id(), self.dialog.take()),
        };
        let Some(mut content) = slot else { return Vec::new() };
        let builder = self.content_builder(content.as_ref(), snap, kind);
        let mut asks = Vec::new();
        let keep = ctx.show_viewport_immediate(id, builder, |ctx, _class| {
            let closing = ctx.input(|i| i.viewport().close_requested());
            if closing {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                content.closed();
            }
            let done = content.show(ctx, snap, &mut |a| asks.push(a));
            !(closing || done)
        });
        match kind {
            WindowKind::Settings if keep => self.settings = Some(content),
            WindowKind::Dialog if keep => self.dialog = Some(content),
            WindowKind::Dialog => {
                let still = self.contents.wanted().map(|(k, _)| k);
                self.wanted.closed(still.as_deref());
                self.dialog = self.waiting.pop_front();
            }
            WindowKind::Settings => {}
        }
        asks
    }
}

impl eframe::App for Shell {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // The root is a transparent pixel; every other window paints its
        // own background.
        [0.0; 4]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        self.now = ctx.input(|i| i.time);
        if self.closing || (self.quitting)() {
            self.closing = true;
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
            return;
        }
        let now = ctx.input(|i| i.time);
        (self.tick)();
        self.keep_root_away();
        self.follow_settings(ctx, now);
        let snap = self.port.snapshot();
        self.sync_tray(ctx, &snap);

        let mut sent: Vec<(Surface, Command)> = Vec::new();
        while let Ok(ev) = self.tray_rx.try_recv() {
            let open = self.panel.is_some();
            match ev {
                TrayEvent::Press => self.toggle.press(open, now),
                TrayEvent::Release(rect) => match self.toggle.release(open, now) {
                    TrayClick::Open => self.open_panel(ctx, Some(rect)),
                    TrayClick::Close => self.close_panel(),
                    TrayClick::Nothing => {}
                },
                TrayEvent::Menu(id) => {
                    if let Some(c) = self.tray.as_ref().and_then(|t| t.command(&id)) {
                        sent.push((Surface::Tray, c));
                    }
                }
            }
        }
        // From other surfaces, through the engine.
        while let Ok(open) = self.opens.try_recv() {
            self.open(ctx, &open, &snap, false, false);
        }
        // A newer small window of a kind replaces the one showing.
        while let Ok((kind, content)) = self.popup_rx.try_recv() {
            self.popups.retain(|p| p.kind != kind);
            self.popups.push(OpenPopup { kind, content, rect: None });
        }
        // A place picked in the tray's menu that only the window may play
        // (a receiver that announces another speaker's identity): the
        // panel, where it can be picked.
        let mut refused = false;
        self.tray_tickets.retain(|t| match t.try_outcome() {
            Some(o) => {
                refused |= matches!(o.result, Err(crate::model::command::Refusal::OnlyFromWindow { .. }));
                false
            }
            None => true,
        });
        if refused {
            self.open_panel(ctx, None);
        }
        // A second launch; at start, first run or a start from the Start
        // menu: the panel.
        let at_start = !self.started && (snap.first_run.is_some() || self.opts.open_at_start);
        if crate::single_instance::take_activation() || at_start {
            self.open_panel(ctx, None);
        }
        self.started = true;
        // The dialog the app needs now (a TV's code), by itself.
        if let Some((key, content)) = self.contents.wanted() {
            if self.wanted.admit(Some(&key), self.dialog.is_some()) {
                self.dialog = Some(content);
            }
        } else {
            self.wanted.admit(None, self.dialog.is_some());
        }

        self.popup_frames(ctx, &snap, now);
        let (commands, mut asks) = self.panel_frame(ctx, &snap, now);
        sent.extend(commands.into_iter().map(|c| (Surface::Panel, c)));
        asks.extend(self.content_frame(ctx, &snap, WindowKind::Settings));
        asks.extend(self.content_frame(ctx, &snap, WindowKind::Dialog));
        for a in asks {
            self.answer(ctx, a, &snap, &mut sent);
        }
        for (from, c) in sent {
            self.route(ctx, from, c, &snap);
        }
        ctx.request_repaint_after(IDLE);
    }
}
