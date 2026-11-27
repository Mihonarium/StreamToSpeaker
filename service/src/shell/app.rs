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
use super::panel_window::{FrameOutput, PanelWindow, Step, Toggle, TrayClick};
use super::passes::{dialog_pass, Passes};
use super::scroll::ScrollState;
use super::popup::{popup_builder, popup_pass, popup_rect, PopupKind, Slots};
use super::main_window::{MainWindow, Windows, OpenView};
use super::views::ViewMemory;
use super::tray::{Tray, TrayEvent};
use super::placement::PxRect;
use super::win::{self, WindowsDesktop};
use super::{app_icon, centred, root_builder, Ask, Desktop, RootMode, ShellOptions, ViewContent, Views, WindowContent, Wiring, ROOT_TITLE};
use crate::model::command::{Command, Open, Surface};
use crate::model::state::Place;
use crate::model::port::{Port, Watch};
use crate::model::fake::ScriptedEngine;
use crate::model::Snapshot;
use crate::ui::theme::{self, Mode};

fn panel_id() -> ViewportId {
    ViewportId::from_hash_of("stream_to_speaker::shell::panel")
}

fn main_id() -> ViewportId {
    ViewportId::from_hash_of("stream_to_speaker::shell::main")
}

fn dialog_id() -> ViewportId {
    ViewportId::from_hash_of("stream_to_speaker::shell::dialog")
}

fn popup_id(kind: PopupKind, seq: u64) -> ViewportId {
    ViewportId::from_hash_of(("stream_to_speaker::shell::popup", kind, seq))
}

/// A small window that must not take the focus, while it shows.
struct OpenPopup {
    kind: PopupKind,
    seq: u64,
    height: Option<f32>,
    /// Where it was put, once it is there (then it shows).
    rect: Option<PxRect>,
    /// The extended style bits of its window the log last named.
    style: Option<u32>,
}

/// How often the shell looks at what has no event of its own (a second
/// launch, the light or dark setting, a dialog the app wants) while
/// nothing is open.
const IDLE: Duration = Duration::from_millis(250);
/// How often the open panel redraws (the level meter).
const METER: Duration = Duration::from_millis(33);

/// Run the shell on the scripted engine in `scenario`, with the views on
/// the scripted settings and placeholder windows for the rest.
pub fn run_scripted(opts: ShellOptions, scenario: &str) -> anyhow::Result<()> {
    let engine = Arc::new(ScriptedEngine::new(scenario));
    let clock = engine.clone();
    let scripted_settings: Arc<dyn crate::settings::SettingsPort> = Arc::new(crate::settings::scripted::ScriptedSettings::new());
    let views = Box::new(crate::settings::views::SettingsViews::new(scripted_settings.clone()));
    run(
        opts,
        Wiring {
            window_height: None,
            save_height: Box::new(|_| {}),
            port: engine.clone(),
            views,
            contents: Box::new(super::content::SettingsContents {
                port: scripted_settings,
                open_windows_page: win::open_windows_page,
                needs_password: Box::new(|_| false),
            }),
            route_opens: None,
            quit: Box::new(move |from| { engine.send(from, Command::Quit); }),
            quitting: Box::new(|| false),

            tick: Box::new(move || clock.tick()),
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
    let window_height = app.user_config.lock().unwrap().window_height;
    run(
        opts,
        Wiring {
            window_height,
            save_height: {
                let app = app.clone();
                Box::new(move |height| {
                    let mut cfg = app.user_config.lock().unwrap();
                    cfg.window_height = Some(height);
                    cfg.save();
                })
            },
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
            route_opens: Some(Box::new(move |opener| route.on_open_from(opener))),
            quit: Box::new(move |from| quit.request_shutdown_from(from)),
            quitting: Box::new(move || quitting.is_shutting_down()),

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
    log::info!(
        "shell: the panel as {:?}, the root as {:?}, tray icon {}, the ordinary window opens at start: {}",
        opts.panel,
        opts.root,
        if opts.tray { "on" } else { "off" },
        if opts.open_at_start { "yes" } else { "no" }
    );
    let native = eframe::NativeOptions { viewport: root_builder(opts.root).with_icon(app_icon()), persist_window: false, ..Default::default() };
    eframe::run_native(
        ROOT_TITLE,
        native,
        Box::new(move |cc| {
            crate::fonts::install(&cc.egui_ctx);
            let Wiring { window_height, save_height, port, views, contents, route_opens, quit, quitting, tick } = wiring;
            let ctx = cc.egui_ctx.clone();
            let watch = port.watch(Arc::new(move || ctx.request_repaint()));
            let (open_tx, open_rx) = channel::<(Surface, Open)>();
            let local_opens = route_opens.is_none();
            if let Some(route) = route_opens {
                let ctx = cc.egui_ctx.clone();
                route(Box::new(move |from, o| {
                    let _ = open_tx.send((from, o));
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
                monitors: Vec::new(),
                windows: Windows::default(),
                panel_scroll: ScrollState::default(),
                window_scroll: ScrollState::default(),
                dialog_scroll: ScrollState::default(),
                panel_reported: false,
                window_height,
                save_height,
                pending_height: None,
                toggle: Toggle::default(),
                dialog: None,
                waiting: VecDeque::new(),
                wanted: WantedGate::default(),
                tray: None,
                tray_tx,
                tray_rx,
                opens: open_rx,
                popup: None,
                slots: Slots::default(),
                local_opens,
                tray_tickets: Vec::new(),
                started: false,
                first_run: None,
                root_seen: None,
                closing: false,
            }))
        }),
    )
    .map_err(|e| anyhow!("the windows couldn't start: {e}"))
}

struct Shell {
    port: Arc<dyn crate::model::port::Port>,
    _watch: Watch,
    views: Box<dyn Views>,
    /// Pages remembered when the ordinary window closes.
    memory: ViewMemory<OpenView>,
    /// This frame's time, in egui's seconds.
    now: f64,
    contents: Box<dyn super::Contents>,
    quit: Box<dyn Fn(Surface)>,
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
    monitors: Vec<super::placement::Monitor>,
    windows: Windows,
    panel_scroll: ScrollState,
    window_scroll: ScrollState,
    dialog_scroll: ScrollState,
    panel_reported: bool,
    window_height: Option<f32>,
    save_height: Box<dyn Fn(f32)>,
    pending_height: Option<(f32, f64)>,
    toggle: Toggle,
    dialog: Option<Box<dyn WindowContent>>,
    /// Dialogs asked for while one was open.
    waiting: VecDeque<Box<dyn WindowContent>>,
    wanted: WantedGate,
    tray: Option<Tray>,
    tray_tx: Sender<TrayEvent>,
    tray_rx: Receiver<TrayEvent>,
    /// Navigation commands from other surfaces, through the engine.
    opens: Receiver<(Surface, Open)>,
    popup: Option<OpenPopup>,
    slots: Slots,
    local_opens: bool,
    /// Places picked in the tray's menu, until the engine has answered.
    tray_tickets: Vec<crate::model::command::Ticket>,
    started: bool,
    /// The previous scene, to raise the window when first run is requested again.
    first_run: Option<bool>,
    /// The root window's visibility and extended style bits the log last
    /// named.
    root_seen: Option<(bool, u32)>,
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
                    if let Some(keep) = super::ShellWindow::Root(RootMode::ToolWindow).kept_style() {
                        win::keep_ex_style(root, keep);
                    }
                }
            }
        }
        // Whether it stays as it was made (hidden, or out of Alt+Tab and
        // never active), after the shell set it and after Windows showed it.
        let seen = (win::is_visible(root), win::ex_style(root));
        if self.root_seen != Some(seen) {
            log::info!(
                "shell: the root window is {} (its style: {}){}",
                if seen.0 { "shown" } else { "hidden" },
                super::ex_style_words(seen.1),
                if self.root_seen.is_some() { ", changed" } else { "" }
            );
            self.root_seen = Some(seen);
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
        if self.windows.main.is_some() { self.monitors = self.desktop.monitors(); }
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

    /// Open the panel, or give it the focus if it is open; `why` is for
    /// the log.
    fn open_panel(&mut self, ctx: &egui::Context, icon: Option<super::placement::PxRect>, why: &str) {
        if super::main_window::tray_opens_window(self.opts.tray, self.port.snapshot().scene.situation.first_run.as_ref()) {
            self.open_window(ctx, true);
            return;
        }
        match &self.windows.flyout {
            Some(p) if p.shown() => {
                log::info!("panel: focusing it ({why})");
                ctx.send_viewport_cmd_to(panel_id(), ViewportCommand::Focus);
            }
            Some(_) => {}
            None => {
                let icon = icon.or_else(|| self.tray.as_ref().and_then(|t| t.rect()));
                match icon {
                    Some(r) => log::info!("panel: opening ({why}; the tray icon is {r})"),
                    None => log::info!("panel: opening ({why}; no tray icon)"),
                }
                self.windows.flyout = Some(PanelWindow::open(icon));
                self.panel_scroll = ScrollState::default();
                // Opened with the mouse: the focus ring waits for a key.
                crate::ui::look::set_focus_visible(ctx, false);
            }
        }
    }

    fn open_window(&mut self, ctx: &egui::Context, home: bool) {
        let already_open = self.windows.main.is_some();
        if !already_open {
            self.window_scroll = ScrollState::default();
            self.port.send(Surface::Panel, Command::WindowOpened);
            crate::ui::look::set_focus_visible(ctx, false);
            self.monitors = self.desktop.monitors();
        }
        let window = self.windows.open(|| {
            let mut w = MainWindow::new(self.window_height, None);
            w.pages = self.memory.reopen(self.now, |(v, _)| v.holds_unused_text());
            w
        });
        window.sync_first_run(self.port.snapshot().scene.situation.first_run.as_ref());
        if home { window.home(); }
        if already_open {
            for c in super::main_window::foreground_commands() { ctx.send_viewport_cmd_to(main_id(), c); }
        }
        ctx.request_repaint();
    }

    fn show_view(&mut self, ctx: &egui::Context, view: Box<dyn ViewContent>, push: bool) {
        self.open_window(ctx, false);
        self.windows.main.as_mut().unwrap().page(view, push);
    }

    fn close_panel(&mut self, why: &str) {
        self.windows.flyout = None;
        log::info!("flyout: closed ({why})");
    }

    /// Tell the engine whether the panel is in view (then a pop-up by the
    /// clock would only repeat it): the flyout, or the window showing it.
    fn report_panel(&mut self) {
        let open = self.windows.flyout.is_some() || self.windows.main.as_ref().is_some_and(MainWindow::shows_panel);
        if self.panel_reported != open {
            self.port.send(Surface::Panel, Command::PanelShown(open));
            self.panel_reported = open;
        }
    }

    fn save_window_height(&mut self, flush: bool) {
        if let Some((height, at)) = self.pending_height {
            if flush || self.now - at >= 0.5 {
                (self.save_height)(height);
                self.pending_height = None;
            }
        }
    }

    fn close_window(&mut self) {
        let first_run = self.port.snapshot().scene.situation.first_run.is_some();
        if self.windows.close(&mut self.memory, self.now, first_run) {
            self.report_panel();
            self.save_window_height(true);
            for command in super::main_window::close_commands(self.opts.tray, first_run) {
                if command == Command::Quit { self.quit_now(Surface::Panel); }
                else { self.port.send(Surface::Panel, command); }
            }
        }
    }

    fn quit_now(&mut self, from: Surface) {
        if !self.closing {
            self.closing = true;
            (self.quit)(from);
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
            self.open_window(ctx, true);
            return;
        }
        let redirected = super::content::pair_from_outside(open, snap, from_window);
        let open = redirected.as_ref().unwrap_or(open);
        if let Some(view) = self.views.view(open, snap) {
            self.show_view(ctx, view, push);
            return;
        }
        // A dialog, or something outside the app.
        if let Some(dialog) = self.contents.open(open, snap, from_window) {
            self.open_dialog(dialog);
        }
    }

    /// Do what a command asks of the windows; send the rest to the engine.
    fn route(&mut self, ctx: &egui::Context, from: Surface, command: Command, snap: &Snapshot) {
        let window = matches!(from, Surface::Panel | Surface::Settings);
        // A speaker picked in the panel that needs its password first: the
        // dialog, which connects once it is typed.
        if let (true, Command::Switch { place: Place::Speaker(id), .. } | Command::PlayTakingOver(id)) = (window, &command) {
            if let Some(dialog) = self.contents.before_play(id, snap) {
                self.open_dialog(dialog);
                return;
            }
        }
        match command {
            Command::Open(open) => {
                self.port.send(from, Command::Open(open.clone()));
                if self.local_opens { self.open(ctx, &open, snap, window, false); }
            }
            Command::FirstRunAction(crate::model::command::FirstRunAction::ShowAgain) => {
                self.port.send(from, command);
                if snap.scene.situation.first_run.is_some() { self.open_window(ctx, true); }
            }
            Command::Quit => self.quit_now(from),
            command => {
                let pick = matches!(command, Command::Switch { .. } | Command::PlayTakingOver(_));
                let ticket = self.port.send(from, command);
                if pick && from == Surface::Tray {
                    self.tray_tickets.push(ticket);
                }
            }
        }
    }

    fn dialog_builder(&self, content: &dyn WindowContent, snap: &Snapshot) -> ViewportBuilder {
        let (size, min) = content.sizes();
        let b = ViewportBuilder::default()
            .with_title(content.title(snap))
            .with_inner_size(size)
            .with_min_inner_size(min)
            .with_icon(app_icon())
            .with_resizable(false)
            .with_minimize_button(false)
            .with_maximize_button(false);
        match centred(&self.desktop.monitors(), size) {
            Some(p) => b.with_position(p),
            None => b,
        }
    }

    /// Draw the flyout and send its panel actions once per frame.
    fn panel_frame(&mut self, ctx: &egui::Context, snap: &Snapshot, now: f64) -> Vec<Ask> {
        let Some(panel) = self.windows.flyout.as_mut() else { return Vec::new() };
        let mode = self.opts.panel;
        let desktop = &mut self.desktop;
        let builder = panel.builder(mode);
        let mut frame = FrameOutput::default();
        ctx.show_viewport_immediate(panel_id(), builder, |ctx, _class| {
            self.panel_scroll.apply(ctx);
            let t = theme::Theme::of(ctx);
            panel.pass(ctx, &mut frame, mode, desktop, |ctx, panel| {
                egui::CentralPanel::default().frame(Frame::none().fill(t.c.surface).stroke(t.card_stroke()))
                    .show(ctx, |ui| panel.everyday(ui, snap)).inner
            });
            ctx.request_repaint_after(METER);
        });
        match frame.step {
            Step::Show => {}
            Step::CloseOnBlur => {
                if self.toggle.blur_closes(now) {
                    self.close_panel("it lost the focus");
                }
            }
            Step::Close if frame.passes.asked_to_close => self.close_panel("its window was asked to close"),
            Step::Close => self.close_panel("Esc"),
            Step::Wait => {}
        }
        self.send_panel(ctx, snap, &mut frame);
        frame.passes.asks
    }

    fn send_panel(&mut self, ctx: &egui::Context, snap: &Snapshot, frame: &mut FrameOutput) {
        let port = self.port.clone();
        frame.send_panel(port.as_ref(), |command| {
            if self.local_opens {
                if let Command::Open(open) = command { self.open(ctx, open, snap, true, false); }
            }
        });
    }

    fn window_frame(&mut self, ctx: &egui::Context, snap: &Snapshot) -> Vec<Ask> {
        let Some(window) = self.windows.main.as_mut() else { return Vec::new() };
        let mut frame = FrameOutput::default();
        let passes = &mut frame.passes;
        let monitors = &self.monitors;
        if !window.geometry.shown && window.geometry.target_monitor.is_none() {
            match self.desktop.opening_monitor(monitors) {
                std::task::Poll::Ready(m) => window.geometry.target_monitor = m,
                std::task::Poll::Pending => { ctx.request_repaint_after(METER); return Vec::new(); }
            }
        }
        ctx.show_viewport_immediate(main_id(), window.geometry.builder(), |ctx, _| {
            self.window_scroll.apply(ctx);
            passes.notice_close(ctx);
            let t = theme::Theme::of(ctx);
            let (inner, outer, minimized) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().outer_rect, i.viewport().minimized));
            window.minimized = minimized.unwrap_or(false);
            let body = egui::CentralPanel::default().frame(Frame::none().fill(t.c.surface)).show(ctx, |ui| {
                window.draw(ui, snap, self.opts.tray, ui.available_height())
            }).inner;
            if passes.end(ctx, false, body.commands, body.asks) && !passes.asked_to_close {
                for c in window.geometry.update(body.natural, monitors, inner, outer, ctx.pixels_per_point()) {
                    ctx.send_viewport_cmd(c);
                }
            }
            ctx.request_repaint_after(METER);
        });
        if let Some(h) = window.geometry.chosen.filter(|h| Some(*h) != self.window_height) {
            self.window_height = Some(h);
            self.pending_height = Some((h, self.now));
        }
        if passes.asked_to_close { self.close_window(); return Vec::new(); }
        self.send_panel(ctx, snap, &mut frame);
        frame.passes.asks
    }

    /// Draw the small windows that must not take the focus; place a new
    /// one, then show it without activating it.
    fn popup_frames(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(content) = self.slots.current(snap) else { self.popup = None; return };
        let (kind, seq) = (content.kind, content.seq);
        if self.popup.as_ref().is_none_or(|p| (p.kind, p.seq) != (kind, seq)) {
            self.popup = Some(OpenPopup { kind, seq, height: None, rect: None, style: None });
        }
        let p = self.popup.as_mut().unwrap();
        let size = egui::vec2(super::popup::WIDTH, p.height.unwrap_or(160.0));
        let id = popup_id(kind, seq);
        if p.height.is_some() {
            let icon = self.tray.as_ref().and_then(|t| t.rect());
            let under = self.windows.flyout.as_ref().and_then(|p| p.rect());
            if let Some(rect) = popup_rect(&self.desktop.monitors(), icon, size, under) {
                if p.rect != Some(rect) && self.desktop.place_popup(kind.title(), rect) {
                    p.rect = Some(rect);
                    ctx.send_viewport_cmd_to(id, ViewportCommand::Visible(true));
                }
            }
        }
        if let Some(style) = self.desktop.popup_ex_style(kind.title()).filter(|s| Some(*s) != p.style) {
            log::debug!("{}: {}", kind.title(), super::ex_style_words(style));
            p.style = Some(style);
        }
        let mut passes = Passes::default();
        ctx.show_viewport_immediate(id, popup_builder(kind, size, p.rect.is_some()).with_icon(app_icon()), |ctx, _| {
            p.height = Some(popup_pass(ctx, &mut passes, content, snap.lang));
        });
        if let Some(command) = passes.commands.into_iter().next() {
            if let Some((from, command)) = self.slots.take_action(command) {
                self.route(ctx, from, command, snap);
            }
            self.popup = None;
        }
    }

    /// Do what a view or a window's content asked.
    fn answer(&mut self, ctx: &egui::Context, ask: Ask, snap: &Snapshot, sent: &mut Vec<(Surface, Command)>) {
        match ask {
            Ask::Command(c) => sent.push((Surface::Panel, c)),
            Ask::Dialog(d) => self.open_dialog(d),
            Ask::OpenPanel => self.open_window(ctx, true),
            Ask::View(v) => self.show_view(ctx, v, true),
            Ask::Open(o) => self.open(ctx, &o, snap, true, true),
            Ask::Back => {
                if let Some(w) = &mut self.windows.main { w.back(); }
            }
        }
    }

    /// Draw the dialog; what it asks.
    fn dialog_frame(&mut self, ctx: &egui::Context, snap: &Snapshot) -> Vec<Ask> {
        let Some(mut content) = self.dialog.take() else { return Vec::new() };
        let builder = self.dialog_builder(content.as_ref(), snap);
        let mut passes = Passes::default();
        ctx.show_viewport_immediate(dialog_id(), builder, |ctx, _class| {
            self.dialog_scroll.apply(ctx);
            dialog_pass(ctx, &mut passes, content.as_mut(), snap);
        });
        if passes.asked_to_close {
            // Its ×: undo what the dialog started.
            content.closed();
        }
        if !passes.ends_the_window() {
            self.dialog = Some(content);
        } else {
            let still = self.contents.wanted().map(|(k, _)| k);
            self.wanted.closed(still.as_deref());
            self.dialog_scroll = ScrollState::default();
            self.dialog = self.waiting.pop_front();
        }
        passes.asks
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
            self.save_window_height(true);
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
        let first_run = snap.scene.situation.first_run.is_some();
        if self.first_run == Some(false) && first_run {
            self.open_window(ctx, true);
        }
        self.first_run = Some(first_run);

        let mut sent: Vec<(Surface, Command)> = Vec::new();
        while let Ok(ev) = self.tray_rx.try_recv() {
            let open = self.windows.flyout.is_some();
            match ev {
                TrayEvent::Press => {
                    log::info!("tray: left button down on the icon (the panel is {})", if open { "open" } else { "closed" });
                    self.toggle.press(open, now)
                }
                TrayEvent::DoublePress => {
                    log::info!("tray: left button down again on the icon, a double click (the panel is {})", if open { "open" } else { "closed" });
                    self.toggle.double_press(open, now)
                }
                TrayEvent::Release(rect) => {
                    let click = self.toggle.release(open, now);
                    log::info!("tray: left button up on the icon ({rect}): {click:?}");
                    match click {
                        TrayClick::Open => self.open_panel(ctx, Some(rect), "a click on the tray icon"),
                        TrayClick::Close => self.close_panel("a click on the tray icon"),
                        TrayClick::Nothing => {}
                        TrayClick::Keep => {
                            if let Some(p) = &mut self.windows.flyout {
                                p.forget_focus();
                            }
                            self.open_panel(ctx, None, "a double click on the tray icon");
                        }
                    }
                }
                TrayEvent::Menu(id) => {
                    if let Some(c) = self.tray.as_ref().and_then(|t| t.command(&id)) {
                        sent.push((Surface::Tray, c));
                    }
                }
            }
        }
        // From other surfaces, through the engine.
        while let Ok((from, open)) = self.opens.try_recv() {
            self.open(ctx, &open, &snap, matches!(from, Surface::Panel | Surface::Settings), false);
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
            self.open_panel(ctx, None, "a place picked in the tray's menu can only be picked in it");
        }
        // A personal launch, including first run, opens or raises the ordinary window.
        if super::main_window::activation(!self.started, self.opts, crate::single_instance::take_activation()) {
            self.open_window(ctx, false);
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

        self.popup_frames(ctx, &snap);
        let mut asks = self.panel_frame(ctx, &snap, now);
        asks.extend(self.window_frame(ctx, &snap));
        asks.extend(self.dialog_frame(ctx, &snap));
        for a in asks {
            self.answer(ctx, a, &snap, &mut sent);
        }
        for (from, c) in sent {
            self.route(ctx, from, c, &snap);
        }
        self.report_panel();
        self.save_window_height(false);
        ctx.request_repaint_after(IDLE);
    }
}
