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

use super::panel_window::{PanelWindow, Seen, Step, Toggle, TrayClick};
use super::tray::{Tray, TrayEvent};
use super::win::{self, WindowsDesktop};
use super::{centred, panel_builder, placeholder, root_builder, Desktop, MakeWindow, RootMode, ShellOptions, WindowContent, WindowKind, ROOT_TITLE};
use crate::model::command::{Command, Open, Surface};
use crate::model::port::{Port, Watch};
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

/// How often the shell looks at what has no event of its own (a second
/// launch, the light or dark setting) while nothing is open.
const IDLE: Duration = Duration::from_millis(250);
/// How often the open panel redraws (the level meter).
const METER: Duration = Duration::from_millis(33);

/// Run the shell on a scripted engine, until the engine itself is wired in.
/// `STREAM_TO_SPEAKER_SCENARIO` names the scenario it starts in (`playing`
/// when unset or unknown).
pub fn run_scripted(opts: ShellOptions) -> anyhow::Result<()> {
    let scenario = std::env::var("STREAM_TO_SPEAKER_SCENARIO").ok().and_then(|n| Scenario::from_name(n.trim())).unwrap_or(Scenario::Playing);
    let engine = Arc::new(ScriptedEngine::new(scenario));
    let _clock = RealTime::start(engine.clone(), Duration::from_millis(50));
    run(opts, engine, placeholder)
}

/// Run the shell for `port` until Quit. `make_window` gives the contents of
/// the Settings window and the dialogs.
pub fn run(opts: ShellOptions, port: Arc<dyn Port>, make_window: MakeWindow) -> anyhow::Result<()> {
    let native = eframe::NativeOptions { viewport: root_builder(opts.root), persist_window: false, ..Default::default() };
    eframe::run_native(
        ROOT_TITLE,
        native,
        Box::new(move |cc| {
            crate::fonts::install(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let watch = port.watch(Arc::new(move || ctx.request_repaint()));
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
                opts,
                make_window,
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
                tray: None,
                tray_tx,
                tray_rx,
                started: false,
            }))
        }),
    )
    .map_err(|e| anyhow!("the windows couldn't start: {e}"))
}

struct Shell {
    port: Arc<dyn Port>,
    _watch: Watch,
    opts: ShellOptions,
    make_window: MakeWindow,
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
    tray: Option<Tray>,
    tray_tx: Sender<TrayEvent>,
    tray_rx: Receiver<TrayEvent>,
    started: bool,
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
                        log::warn!("no tray icon: {e:#}");
                        self.opts.tray = false;
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
                self.panel = Some(PanelWindow::open(icon.or_else(|| self.tray.as_ref().and_then(|t| t.rect()))));
                // Opened with the mouse: the focus ring waits for a key.
                crate::ui::look::set_focus_visible(ctx, false);
            }
        }
    }

    fn close_panel(&mut self) {
        if let Some(p) = self.panel.take() {
            if p.shown() {
                let _ = self.port.send(Surface::Panel, Command::PanelShown(false));
            }
        }
    }

    fn open_window(&mut self, ctx: &egui::Context, open: &Open) {
        if let Some(s) = &mut self.settings {
            if s.navigate(open) {
                ctx.send_viewport_cmd_to(settings_id(), ViewportCommand::Focus);
                return;
            }
        }
        let Some((kind, content)) = (self.make_window)(open) else { return };
        match kind {
            WindowKind::Settings => {
                self.settings = Some(content);
                ctx.send_viewport_cmd_to(settings_id(), ViewportCommand::Focus);
            }
            WindowKind::Dialog if self.dialog.is_some() => self.waiting.push_back(content),
            WindowKind::Dialog => self.dialog = Some(content),
        }
    }

    /// Do what a command asks of the windows, then hand it to the engine.
    fn route(&mut self, ctx: &egui::Context, from: Surface, command: Command) {
        match &command {
            Command::Open(Open::Panel) => self.open_panel(ctx, None),
            Command::Open(open) => self.open_window(ctx, open),
            Command::Quit => ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close),
            _ => {}
        }
        let _ = self.port.send(from, command);
    }

    fn content_builder(&self, content: &dyn WindowContent, snap: &Snapshot) -> ViewportBuilder {
        let (size, min) = content.sizes();
        let b = ViewportBuilder::default().with_title(content.title(snap)).with_inner_size(size).with_min_inner_size(min);
        match centred(&self.desktop.monitors(), size) {
            Some(p) => b.with_position(p),
            None => b,
        }
    }

    /// Draw the panel's window; what it sends.
    fn panel_frame(&mut self, ctx: &egui::Context, snap: &Snapshot, now: f64) -> Vec<Command> {
        let Some(panel) = self.panel.as_mut() else { return Vec::new() };
        let mode = self.opts.panel;
        let pinned = snap.first_run.is_some();
        let (port, desktop) = (&self.port, &mut self.desktop);
        let (commands, step) = ctx.show_viewport_immediate(panel_id(), panel_builder(mode), |ctx, _class| {
            let t = theme::Theme::of(ctx);
            let (focused, inner_height, close) = ctx.input(|i| {
                let v = i.viewport();
                (v.focused, v.inner_rect.map(|r| r.height()), v.close_requested())
            });
            if close {
                // Its × (the small-window form): the shell closes it.
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            }
            let out = egui::CentralPanel::default()
                .frame(Frame::none().fill(t.c.surface))
                .show(ctx, |ui| crate::panel::show(ui, snap, port.level(), &mut panel.state))
                .inner;
            let seen = Seen { natural_height: out.natural_height, close: out.close || close, inner_height, focused };
            let step = panel.after_frame(seen, pinned, mode, desktop);
            if step == Step::Show {
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            ctx.request_repaint_after(METER);
            (out.commands, step)
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

    /// Draw the Settings window or the dialog in `slot`; what it sends.
    fn content_frame(&mut self, ctx: &egui::Context, snap: &Snapshot, kind: WindowKind) -> Vec<Command> {
        let (id, slot) = match kind {
            WindowKind::Settings => (settings_id(), self.settings.take()),
            WindowKind::Dialog => (dialog_id(), self.dialog.take()),
        };
        let Some(mut content) = slot else { return Vec::new() };
        let builder = self.content_builder(content.as_ref(), snap);
        let mut commands = Vec::new();
        let keep = ctx.show_viewport_immediate(id, builder, |ctx, _class| {
            let closing = ctx.input(|i| i.viewport().close_requested());
            if closing {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            }
            let done = content.show(ctx, snap, &mut |c| commands.push(c));
            !(closing || done)
        });
        match kind {
            WindowKind::Settings if keep => self.settings = Some(content),
            WindowKind::Dialog if keep => self.dialog = Some(content),
            WindowKind::Dialog => self.dialog = self.waiting.pop_front(),
            WindowKind::Settings => {}
        }
        commands
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
        let now = ctx.input(|i| i.time);
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
        // A second launch, or the first start in first run: the panel.
        if crate::single_instance::take_activation() || (!self.started && snap.first_run.is_some()) {
            self.open_panel(ctx, None);
        }
        self.started = true;

        sent.extend(self.panel_frame(ctx, &snap, now).into_iter().map(|c| (Surface::Panel, c)));
        sent.extend(self.content_frame(ctx, &snap, WindowKind::Settings).into_iter().map(|c| (Surface::Settings, c)));
        sent.extend(self.content_frame(ctx, &snap, WindowKind::Dialog).into_iter().map(|c| (Surface::Settings, c)));
        for (from, c) in sent {
            self.route(ctx, from, c);
        }
        ctx.request_repaint_after(IDLE);
    }
}
