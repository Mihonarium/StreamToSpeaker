//! The ordinary window. Its client area holds either the shared panel and
//! its tray hint, or a page. The flyout has no navigation state.
use egui::{pos2, vec2, Rect, Sense, Ui, Vec2, ViewportBuilder, ViewportCommand};

use super::page::{show_view, ViewState};
use super::panel_window::PanelWindow;
use super::placement::{monitor_for, Monitor, PxRect};
use super::{Ask, ViewContent};
use crate::settings::views::FirstRunPage;
use crate::model::command::FirstRunAction;
use crate::model::situation::FirstRunStep;
use crate::model::{Command, Open, Snapshot};
use crate::surface::{grid, panel};
use crate::ui::theme::Theme;

pub const WIDTH: f32 = panel::WIDTH;
pub const TITLE_HEIGHT: f32 = 32.0;
pub(super) const MIN_HEIGHT: f32 = grid::PLACE + grid::RULE + grid::ROW + grid::MIN_SCROLL + 2.0 * grid::TIGHT;
pub type OpenView = (Box<dyn ViewContent>, ViewState);

/// Only the ordinary window owns pages. A hand-off from the tray closes
/// the flyout even when the destination window already exists.
#[derive(Default)]
pub struct Windows {
    pub flyout: Option<PanelWindow>,
    pub main: Option<MainWindow>,
}

pub fn activation(first_frame: bool, opts: super::ShellOptions, second_launch: bool) -> bool {
    second_launch || first_frame && opts.effective().open_at_start
}

impl Windows {
    pub fn open(&mut self, create: impl FnOnce() -> MainWindow) -> &mut MainWindow {
        self.flyout = None;
        self.main.get_or_insert_with(create)
    }

    #[cfg(any(windows, test))]
    pub(crate) fn close(&mut self, memory: &mut super::views::ViewMemory<OpenView>, now: f64, first_run: bool) -> bool {
        let Some(mut window) = self.main.take() else { return false };
        if let Some((page, _)) = window.pages.last_mut() { page.hidden(); }
        if first_run { memory.forget(); } else { memory.closed(window.pages, now); }
        true
    }
}

pub fn tray_opens_window(tray: bool, first_run: Option<&FirstRunStep>) -> bool {
    !tray || first_run.is_some()
}

pub fn close_commands(tray: bool, first_run: bool) -> impl Iterator<Item = Command> {
    first_run.then_some(Command::FirstRunAction(FirstRunAction::Close)).into_iter()
        .chain([if tray { Command::WindowClosed } else { Command::Quit }])
}

pub fn foreground_commands() -> [ViewportCommand; 2] {
    [ViewportCommand::Minimized(false), ViewportCommand::Focus]
}

pub struct MainWindow {
    pub panel: panel::PanelUi,
    pub pages: Vec<OpenView>,
    pub(crate) welcome: Option<(FirstRunPage, ViewState)>,
    pub geometry: Geometry,
    /// Windows says it is minimized.
    pub minimized: bool,
}

impl MainWindow {
    pub fn new(chosen: Option<f32>, monitor: Option<Monitor>) -> Self {
        let mut panel = panel::PanelUi::default();
        panel.focus_place();
        Self {
            panel,
            pages: Vec::new(),
            welcome: None,
            geometry: Geometry::new(chosen, monitor),
            minimized: false,
        }
    }

    /// The panel is in view here: the window isn't minimized and holds no
    /// page.
    pub fn shows_panel(&self) -> bool {
        self.pages.is_empty() && self.welcome.is_none() && !self.minimized
    }

    pub(crate) fn sync_first_run(&mut self, next: Option<&FirstRunStep>) {
        match (&mut self.welcome, next) {
            (Some((page, _)), Some(step)) if page.same_page(step) => page.step = step.clone(),
            (_, Some(step)) => {
                self.home();
                self.welcome = Some((FirstRunPage::new(step.clone()), ViewState::opened()));
            }
            (Some(_), None) => {
                self.home();
                self.welcome = None;
            }
            (None, None) => {}
        }
    }

    pub fn home(&mut self) {
        self.pages.clear();
        self.panel.focus_place();
    }

    pub fn page(&mut self, view: Box<dyn ViewContent>, push: bool) {
        if let Some((current, _)) = self.pages.last_mut() { current.hidden(); }
        if !push {
            self.pages.clear();
        }
        self.pages.push((view, ViewState::opened()));
    }

    pub fn back(&mut self) {
        self.pages.pop();
        if self.pages.is_empty() {
            self.panel.focus_place();
        }
    }

    pub fn natural_height(&self, ui: &Ui, snap: &Snapshot, tray: bool) -> f32 {
        panel::heights(ui, &snap.panel, &self.panel).0
            + if tray {
                Strip::new(ui, snap).height()
            } else {
                0.0
            }
    }

    /// Draw exactly the same client area into a native viewport or a gallery scene.
    pub fn draw(&mut self, ui: &mut Ui, snap: &Snapshot, tray: bool, height: f32) -> Body {
        self.sync_first_run(snap.scene.situation.first_run.as_ref());
        if self.pages.is_empty() {
            if let Some((view, state)) = &mut self.welcome {
                view.tray = tray;
                state.cap = Some(height);
                state.bottom_links = true;
                let mut page = show_view(ui, view, snap, state, 0);
                if page.back {
                    page.asks.push(Ask::Command(Command::FirstRunAction(FirstRunAction::ChooseAnother)));
                }
                return Body { natural: page.natural_height, commands: Vec::new(), asks: page.asks };
            }
        }
        let strip = tray.then(|| Strip::new(ui, snap));
        let strip_h = strip.as_ref().map_or(0.0, Strip::height);
        let (full, minimum) = panel::heights(ui, &snap.panel, &self.panel);
        let mut body = Body {
            natural: full + strip_h,
            commands: vec![],
            asks: vec![],
        };
        let depth = (self.pages.len() + usize::from(self.welcome.is_some())).saturating_sub(1);
        if let Some((view, state)) = self.pages.last_mut() {
            state.cap = Some(height);
            let page = show_view(ui, view.as_mut(), snap, state, depth);
            body.asks = page.asks;
            if page.back {
                body.asks.push(Ask::Back);
            }
        } else {
            let draw = |ui: &mut Ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                // Fill behind the footer’s rounded flyout corners right up to the window edge.
                let top = ui.cursor().min + vec2(0.0, grid::PLACE);
                let bottom = full.min((height - strip_h).max(minimum));
                ui.painter().rect_filled(
                    Rect::from_min_size(top, vec2(WIDTH, bottom - grid::PLACE)),
                    0.0,
                    Theme::of(ui.ctx()).c.chrome,
                );
                let out = panel::show(ui, &snap.panel, &mut self.panel, height - strip_h);
                body.commands = out.commands;
                if let Some(strip) = strip {
                    strip.show(ui);
                }
            };
            // Below the smallest scrolling list (e.g. on a short display),
            // keep the footer and its controls reachable as well.
            if minimum + strip_h > height + 0.5 {
                egui::ScrollArea::vertical()
                    .id_salt("whole-panel")
                    .max_height(height)
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, draw);
            } else {
                draw(ui);
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
                body.commands.push(Command::Open(Open::Settings));
            }
        }
        body
    }
}

pub struct Body {
    pub natural: f32,
    pub commands: Vec<Command>,
    pub asks: Vec<Ask>,
}

struct Strip {
    words: std::sync::Arc<egui::Galley>,
}
impl Strip {
    fn new(ui: &Ui, snap: &Snapshot) -> Self {
        let t = Theme::of(ui.ctx());
        let mut job = egui::text::LayoutJob::simple_singleline(
            snap.window.strip.clone(),
            t.font(12.0),
            t.c.text_secondary,
        );
        job.break_on_newline = true;
        job.sections[0].format.line_height = Some(16.0);
        job.wrap.max_width = 296.0;
        Self {
            words: ui.fonts(|f| f.layout_job(job)),
        }
    }
    fn height(&self) -> f32 {
        grid::RULE + 12.0 + self.words.size().y
    }
    fn show(self, ui: &mut Ui) {
        let t = Theme::of(ui.ctx());
        let (r, response) = ui.allocate_exact_size(vec2(WIDTH, self.height()), Sense::hover());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, true, self.words.text())
        });
        ui.painter().rect_filled(r, 0.0, t.c.chrome);
        ui.painter().rect_filled(
            ui.painter()
                .round_rect_to_pixels(Rect::from_min_size(r.min, vec2(WIDTH, grid::RULE))),
            0.0,
            t.c.divider,
        );
        let top = r.top() + grid::RULE + 6.0;
        let icon = Rect::from_min_size(pos2(r.left() + grid::ICON - 8.0, top), Vec2::splat(16.0));
        ui.painter().image(
            super::icons::app_mark(ui).id(),
            icon,
            Rect::from_min_max(egui::Pos2::ZERO, pos2(1.0, 1.0)),
            t.c.accent,
        );
        ui.painter().galley(
            ui.painter()
                .round_pos_to_pixels(pos2(r.left() + grid::TEXT, top)),
            self.words,
            t.c.text_secondary,
        );
    }
}

/// All heights are client heights in points. A resize requested by the app
/// must be acknowledged before a later native resize counts as a drag.
pub struct Geometry {
    pub chosen: Option<f32>,
    pub shown: bool,
    pub(super) target_monitor: Option<Monitor>,
    requested: Option<f32>,
    acknowledged: bool,
    limits: Option<(f32, f32, f32)>,
    frames: u32,
}

impl Geometry {
    pub fn new(chosen: Option<f32>, monitor: Option<Monitor>) -> Self {
        Self {
            chosen: chosen.filter(|h| h.is_finite() && *h > 0.0),
            shown: false,
            target_monitor: monitor,
            requested: None,
            acknowledged: false,
            limits: None,
            frames: 0,
        }
    }

    pub fn builder(&self) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title(crate::PRODUCT_NAME)
            .with_icon(super::app_icon())
            .with_decorations(true)
            .with_taskbar(true)
            .with_resizable(true)
            .with_maximize_button(false)
            .with_inner_size([WIDTH, 500.0])
            .with_min_inner_size([WIDTH, 1.0])
            .with_max_inner_size([WIDTH, 16384.0])
            .with_visible(self.shown)
    }

    pub fn update(
        &mut self,
        natural: f32,
        monitors: &[Monitor],
        inner: Option<Rect>,
        outer: Option<Rect>,
        scale: f32,
    ) -> Vec<ViewportCommand> {
        let mut cmds = vec![];
        self.frames += 1;
        let near = |a: f32, b: f32| (a - b).abs() <= 1.0 / scale + 0.01;
        let current = outer.map(|r| {
            let c = r.center() * scale;
            PxRect::new(c.x as i32, c.y as i32, c.x as i32, c.y as i32)
        });
        let monitor = if self.shown {
            None
        } else {
            self.target_monitor
        }
        .or_else(|| monitor_for(monitors, current).copied());
        let frame = outer.zip(inner).map_or(vec2(2.0, TITLE_HEIGHT), |(o, i)| {
            (o.size() - i.size()).max(Vec2::ZERO)
        });
        let max = monitor
            .map_or(1000.0, |m| m.work.height() as f32 / m.scale - frame.y)
            .floor()
            .max(1.0);
        let min = MIN_HEIGHT.min(max);
        let limits = (min, max, scale);
        let actual = inner.map(|r| r.height());
        if self.shown && self.acknowledged && self.limits == Some(limits) {
            if let (Some(h), Some(previous)) = (actual, self.requested) {
                if !near(h, previous) && h >= min - 1.0 && h <= max + 1.0 {
                    self.chosen = Some(h.round());
                }
            }
        }
        if self.limits != Some(limits) {
            cmds.extend([
                ViewportCommand::MinInnerSize(vec2(WIDTH, min)),
                ViewportCommand::MaxInnerSize(vec2(WIDTH, max)),
            ]);
            self.limits = Some(limits);
        }
        let want = self.chosen.unwrap_or(natural).clamp(min, max);
        self.acknowledged = actual.is_some_and(|h| near(h, want));
        if self.requested != Some(want) || !self.acknowledged && !self.shown {
            cmds.push(ViewportCommand::InnerSize(vec2(WIDTH, want)));
        }
        self.requested = Some(want);
        if !self.shown {
            let mut positioned = true;
            if let Some(m) = monitor {
                // OuterPosition uses this viewport's current scale, including
                // on the frame before Windows switches it to the target DPI.
                let centre = pos2(
                    (m.work.left + m.work.right) as f32 / 2.0,
                    (m.work.top + m.work.bottom) as f32 / 2.0,
                );
                let origin = (centre - (vec2(WIDTH, want) + frame) * (0.5 * m.scale)) / scale;
                positioned = outer.is_some_and(|r| r.min.distance(origin) < 2.0 / scale);
                if !positioned {
                    cmds.push(ViewportCommand::OuterPosition(origin));
                }
            }
            if self.acknowledged && positioned || self.frames >= 12 {
                self.shown = true;
                cmds.extend([ViewportCommand::Visible(true), ViewportCommand::Focus]);
            }
        }
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{fake::ScriptedEngine, Port, SpeakerId, Surface};
    use crate::shell::{Desktop, FakeDesktop, ShellOptions, Views};

    #[test]
    fn welcome_is_the_window_root_with_help_above_it() {
        use crate::model::command::FirstRunAction as A;
        use crate::settings::{scripted::ScriptedSettings, views::SettingsViews};
        let port = ScriptedEngine::new("pc-plain");
        let send = |act| { port.send(Surface::Panel, Command::FirstRunAction(act)); };
        let sync = |window: &mut MainWindow| window.sync_first_run(port.snapshot().scene.situation.first_run.as_ref());
        let views = SettingsViews::new(std::sync::Arc::new(ScriptedSettings::new()));
        let mut main = MainWindow::new(None, None);
        let id = SpeakerId::new("kitchen");
        assert!(main.shows_panel());
        assert!(!tray_opens_window(true, None));
        for finish in [A::Heard, A::UseWithoutChecking] {
            send(A::ShowAgain);
            sync(&mut main);
            assert!(tray_opens_window(true, port.snapshot().scene.situation.first_run.as_ref()));
            assert!(!main.shows_panel());
            send(A::Chime(id.clone()));
            sync(&mut main);
            let step_two = port.snapshot().scene.situation.first_run.clone();
            assert!(matches!(&step_two, Some(FirstRunStep::Two { speaker, .. }) if speaker == &id));
            assert!(port.snapshot().scene.situation.place_speaker().is_none());

            let destination = Open::CantHear(Some(id.clone()));
            main.page(views.view(&destination, &port.snapshot()).unwrap(), true);
            port.send(Surface::Panel, Command::PanelShown(false));
            sync(&mut main);
            assert_eq!(main.pages.len(), 1);
            main.minimized = true;
            sync(&mut main);
            main.minimized = false;
            main.back();
            assert!(main.pages.is_empty());
            assert_eq!(Some(&main.welcome.as_ref().unwrap().0.step), step_two.as_ref());

            // Use the frame arrow itself to return to the picker.
            let ctx = crate::ui::preview::context(crate::fonts::definitions(&[], &mut |_| None).0, crate::ui::theme::Mode::Light);
            let mut commands = Vec::new();
            for frame in 0..5 {
                let mut input = crate::ui::preview::input(vec2(WIDTH, 500.0), 1.0, frame as f64 * 0.1);
                if frame >= 2 {
                    let pos = egui::pos2(22.0, 31.0);
                    input.events.push(egui::Event::PointerMoved(pos));
                    if frame > 2 {
                        input.events.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: frame == 3, modifiers: egui::Modifiers::NONE });
                    }
                }
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
                        for ask in main.draw(ui, &port.snapshot(), true, 500.0).asks {
                            if let Ask::Command(c) = ask { commands.push(c); }
                        }
                    });
                });
            }
            assert_eq!(commands, [Command::FirstRunAction(A::ChooseAnother)]);
            port.send(Surface::Panel, commands.pop().unwrap());
            sync(&mut main);
            assert_eq!(main.welcome.as_ref().unwrap().0.step, FirstRunStep::One);
            send(A::Chime(id.clone()));
            send(finish);
            sync(&mut main);
            assert_eq!(main.welcome.as_ref().unwrap().0.step, FirstRunStep::Three { speaker: id.clone() });
            assert_eq!(port.snapshot().scene.situation.place_speaker(), Some(&id));
            send(A::StartAtSignIn(false));
            assert!(!port.snapshot().scene.start_at_sign_in);
            send(A::Done);
            sync(&mut main);
            assert!(main.shows_panel());
            // Replay the next case from This PC.
            port.publish(crate::model::samples::named("pc-plain"));
        }
        for sample in ["pc-welcome-pick", "pc-welcome-chime"] {
            let port = ScriptedEngine::new(sample);
            for c in close_commands(true, true) { port.send(Surface::Panel, c); }
            assert!(port.snapshot().scene.situation.first_run.is_none());
            assert!(port.snapshot().popup.is_some());
        }
    }

    fn desktop() -> FakeDesktop {
        let primary = Monitor {
            bounds: PxRect::new(0, 0, 1200, 900),
            work: PxRect::new(0, 0, 1200, 850),
            scale: 1.0,
            primary: true,
        };
        let other = Monitor {
            bounds: PxRect::new(-1500, 0, 0, 1200),
            work: PxRect::new(-1500, 0, 0, 1080),
            scale: 1.5,
            primary: false,
        };
        FakeDesktop {
            monitors: vec![primary, other],
            pointer: Some(PxRect::new(-50, 20, -50, 20)),
            ..Default::default()
        }
    }

    fn pointer_monitor(d: &mut FakeDesktop) -> Option<Monitor> {
        let monitors = d.monitors();
        match d.opening_monitor(&monitors) {
            std::task::Poll::Ready(m) => m,
            std::task::Poll::Pending => panic!("the fake desktop resolves immediately"),
        }
    }

    #[test]
    fn launch_activation_and_closing_with_and_without_a_tray() {
        for tray in [true, false] {
            let opts = ShellOptions::from(|_| None, tray);
            assert_eq!(activation(true, opts, false), !tray);
            assert!(activation(
                true,
                ShellOptions {
                    open_at_start: true,
                    ..opts
                },
                false
            ));
            assert!(activation(false, opts, true));
            assert!(!activation(false, opts, false));
            let mut desktop = desktop();
            let mut windows = Windows::default();
            let main = windows.open(|| MainWindow::new(None, pointer_monitor(&mut desktop)));
            let b = main.geometry.builder();
            assert_eq!(b.title.as_deref(), Some(crate::PRODUCT_NAME));
            assert_eq!(
                (b.decorations, b.taskbar, b.resizable),
                (Some(true), Some(true), Some(true))
            );
            assert_ne!(b.window_level, Some(egui::WindowLevel::AlwaysOnTop));
            windows.open(|| panic!("activation must reuse the window"));
            assert_eq!(
                foreground_commands(),
                [ViewportCommand::Minimized(false), ViewportCommand::Focus]
            );
            let port = ScriptedEngine::new("pc-plain");
            for command in close_commands(tray, false) { port.send(Surface::Panel, command); }
            if tray {
                assert_eq!(
                    port.snapshot().popup.as_ref().unwrap().what.link.as_ref(),
                    Some(&port.snapshot().window.quit)
                );
                assert_eq!(port.received(), [(Surface::Panel, Command::WindowClosed)]);
            } else {
                assert_eq!(port.received(), [(Surface::Panel, Command::Quit)]);
                assert!(port.snapshot().popup.is_none());
            }
        }
    }

    #[test]
    fn start_on_the_pointer_display_then_follow_the_panel_until_a_drag() {
        let mut d = desktop();
        let mut g = Geometry::new(None, pointer_monitor(&mut d));
        let mut native = super::super::eframe_model::Window::made(g.builder(), false);
        let cmds = g.update(900.0, &d.monitors, None, None, 1.0);
        // The other screen has 720 logical points of work area.
        assert!(cmds.contains(&ViewportCommand::InnerSize(vec2(WIDTH, 688.0))));
        let pos = cmds
            .iter()
            .find_map(|c| match c {
                ViewportCommand::OuterPosition(p) => Some(*p),
                _ => None,
            })
            .unwrap();
        assert!(pos.x < 0.0 && pos.y.abs() < 0.001);
        let rect = |h| Rect::from_min_size(pos / 1.5, vec2(WIDTH + 2.0, h + TITLE_HEIGHT));
        let inner = |h| Rect::from_min_size(pos / 1.5 + vec2(1.0, TITLE_HEIGHT), vec2(WIDTH, h));
        let b = g.builder();
        let cmds = g.update(
            900.0,
            &d.monitors,
            Some(inner(688.0)),
            Some(rect(688.0)),
            1.5,
        );
        native.frame(&b, cmds, true);
        assert!(native.visible);
        g.update(
            550.0,
            &d.monitors,
            Some(inner(688.0)),
            Some(rect(688.0)),
            1.5,
        );
        g.update(
            550.0,
            &d.monitors,
            Some(inner(550.0)),
            Some(rect(550.0)),
            1.5,
        );
        assert_eq!(g.chosen, None);
        g.update(
            550.0,
            &d.monitors,
            Some(inner(480.0)),
            Some(rect(480.0)),
            1.5,
        );
        assert_eq!(g.chosen, Some(480.0));
        let cmds = g.update(
            800.0,
            &d.monitors,
            Some(inner(480.0)),
            Some(rect(480.0)),
            1.5,
        );
        assert!(!cmds
            .iter()
            .any(|c| matches!(c, ViewportCommand::InnerSize(_))));
    }

    #[test]
    fn a_panel_gear_or_link_and_the_tray_open_the_ordinary_window() {
        let port = ScriptedEngine::new("kitchen-playing");
        let snap = port.snapshot();
        let views = crate::settings::views::SettingsViews::new(std::sync::Arc::new(
            crate::settings::scripted::ScriptedSettings::new(),
        ));
        let ctx = crate::ui::preview::context(
            crate::fonts::definitions(&[], &mut |_| None).0,
            crate::ui::theme::Mode::Light,
        );
        let mut windows = Windows::default();
        let new = || MainWindow::new(None, None);
        for (from_tray, target) in [
            (false, Open::SpeakerSettings(SpeakerId::new("kitchen"))),
            (false, Open::Settings),
            (true, Open::SpeakerSettings(SpeakerId::new("kitchen"))),
        ] {
            if from_tray {
                windows.flyout = Some(PanelWindow::open(None));
            } else {
                windows.open(new);
            }
            let mut at = None;
            let input = || crate::ui::preview::input(vec2(WIDTH, 900.0), 1.0, 0.0);
            let draw = |ctx: &egui::Context, windows: &mut Windows, at: &mut Option<egui::Pos2>| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::none())
                    .show(ctx, |ui| {
                        let state = if from_tray {
                            &mut windows.flyout.as_mut().unwrap().state
                        } else {
                            &mut windows.main.as_mut().unwrap().panel
                        };
                        let out = panel::show(ui, &snap.panel, state, 900.0);
                        *at = out
                            .controls
                            .iter()
                            .find(|c| c.command == Some(Command::Open(target.clone())))
                            .map(|c| c.rect.center());
                        for c in out.commands {
                            port.send(Surface::Panel, c);
                        }
                    });
            };
            let _ = ctx.run(input(), |ctx| draw(ctx, &mut windows, &mut at));
            let pos = at.unwrap();
            let mut click = input();
            click.events.push(egui::Event::PointerMoved(pos));
            for pressed in [true, false] {
                click.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let before = port.received().len();
            let _ = ctx.run(click, |ctx| draw(ctx, &mut windows, &mut at));
            assert_eq!(
                &port.received()[before..],
                &[(Surface::Panel, Command::Open(target.clone()))]
            );
            windows
                .open(new)
                .page(views.view(&target, &snap).unwrap(), false);
            assert!(windows.flyout.is_none());
            let main = windows.main.as_mut().unwrap();
            assert_eq!(main.pages.len(), 1);
            let mut back = input();
            back.events.push(egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
            for raw in [input(), back] {
                let _ = ctx.run(raw, |ctx| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::none())
                        .show(ctx, |ui| {
                            let out = main.draw(ui, &snap, true, 620.0);
                            if out.asks.iter().any(|a| matches!(a, Ask::Back)) {
                                main.back();
                            }
                        });
                });
            }
            assert!(main.pages.is_empty());
        }
        assert_eq!(snap.tray.menu[1].command, Some(Command::Open(Open::Panel)));
        let window = windows.open(new);
        let _ = ctx.run(
            crate::ui::preview::input(vec2(WIDTH, 900.0), 1.0, 0.0),
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::none())
                    .show(ctx, |ui| {
                        let natural = panel::heights(ui, &snap.panel, &window.panel).0;
                        let hint = Strip::new(ui, &snap).height();
                        assert_eq!(window.draw(ui, &snap, false, 900.0).natural, natural);
                        assert_eq!(window.draw(ui, &snap, true, 900.0).natural, natural + hint);
                    });
            },
        );
    }
}
