//! The trial itself; `main.rs` runs it on Windows. It uses nothing but
//! eframe, so it can also be built on its own for Linux under X11.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{ViewportBuilder, ViewportClass, ViewportCommand, ViewportId, WindowLevel};

#[derive(Default)]
struct Shared {
    log: Vec<String>,
    /// Frames drawn per window, per whole second since the start.
    frames: std::collections::BTreeMap<(&'static str, u64), u32>,
    panel_focused_once: bool,
    panel_last_focus: Option<bool>,
}

struct Spike {
    start: Instant,
    seconds: u64,
    shared: Arc<Mutex<Shared>>,
    panel_open: bool,
    panel_reopen_at: Option<f64>,
    toast_open: bool,
    settings_open: bool,
    dialog_open: bool,
    root_frames: u64,
    root_visible: bool,
    done: bool,
}

fn note(shared: &Arc<Mutex<Shared>>, start: Instant, what: String) {
    let line = format!("{:6.2}s  {what}", start.elapsed().as_secs_f64());
    println!("{line}");
    shared.lock().unwrap().log.push(line);
}

fn count(shared: &Arc<Mutex<Shared>>, start: Instant, who: &'static str) {
    *shared.lock().unwrap().frames.entry((who, start.elapsed().as_secs())).or_default() += 1;
}

pub fn run() -> eframe::Result {
    let mut root_visible = false;
    let mut seconds = 14;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => root_visible = args.next().as_deref() == Some("visible"),
            "--seconds" => seconds = args.next().and_then(|s| s.parse().ok()).unwrap_or(14),
            _ => {}
        }
    }
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default().with_title("Stream To Speaker (root)").with_inner_size([320.0, 200.0]).with_visible(root_visible),
        ..Default::default()
    };
    eframe::run_native(
        "window-spike",
        options,
        Box::new(move |_cc| {
            let start = Instant::now();
            let shared = Arc::new(Mutex::new(Shared::default()));
            note(&shared, start, format!("root created, visible={root_visible}"));
            Ok(Box::new(Spike {
                start,
                seconds,
                shared,
                panel_open: true,
                panel_reopen_at: None,
                toast_open: false,
                settings_open: false,
                dialog_open: false,
                root_frames: 0,
                root_visible,
                done: false,
            }))
        }),
    )
}

impl Spike {
    fn t(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

impl eframe::App for Spike {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (start, shared) = (self.start, self.shared.clone());
        self.root_frames += 1;
        count(&shared, start, "root");
        if self.root_frames == 1 {
            note(&shared, start, "root: first update".into());
        }
        // eframe 0.29 shows the root after its first frame whatever the
        // builder said; hide it again, as the app does.
        if self.root_frames == 2 && !self.root_visible {
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            note(&shared, start, "root: hide it after its first frame".into());
        }
        let t = self.t();
        let monitor = ctx.input(|i| i.viewport().monitor_size).unwrap_or(egui::vec2(1366.0, 768.0));

        // The timeline.
        if !self.toast_open && (1.0..1.5).contains(&t) {
            self.toast_open = true;
            note(&shared, start, "open the no-focus confirmation".into());
        }
        if self.toast_open && t > 4.0 {
            self.toast_open = false;
            note(&shared, start, "close the confirmation".into());
        }
        if !self.settings_open && (2.5..3.0).contains(&t) {
            self.settings_open = true;
            note(&shared, start, "open Settings (should take the focus from the panel)".into());
        }
        if !self.dialog_open && (6.0..6.5).contains(&t) {
            self.dialog_open = true;
            note(&shared, start, "open a dialog over Settings".into());
        }
        if let Some(at) = self.panel_reopen_at {
            if t >= at {
                self.panel_reopen_at = None;
                self.panel_open = true;
                note(&shared, start, "reopen the panel (as a tray click would)".into());
            }
        }
        if t > self.seconds as f64 {
            if !self.done {
                self.done = true;
                self.print_table();
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        // The panel: a deferred viewport, borderless, always on top, in the
        // bottom-right corner, repainting continuously.
        if self.panel_open {
            let size = egui::vec2(360.0, 420.0);
            let pos = egui::pos2(monitor.x - size.x - 12.0, monitor.y - size.y - 60.0);
            let s = shared.clone();
            ctx.show_viewport_deferred(
                ViewportId::from_hash_of("panel"),
                ViewportBuilder::default()
                    .with_title("Stream To Speaker panel")
                    .with_decorations(false)
                    .with_window_level(WindowLevel::AlwaysOnTop)
                    .with_taskbar(false)
                    .with_resizable(false)
                    .with_position(pos)
                    .with_inner_size(size),
                move |ctx, class| {
                    count(&s, start, "panel");
                    let focused = ctx.input(|i| i.viewport().focused);
                    {
                        let mut g = s.lock().unwrap();
                        if g.panel_last_focus != focused {
                            g.panel_last_focus = focused;
                            if focused == Some(true) {
                                g.panel_focused_once = true;
                            }
                            drop(g);
                            let kind = if matches!(class, ViewportClass::Deferred) { "its own window" } else { "drawn inside its parent" };
                            note(&s, start, format!("panel: focused={focused:?} ({kind})"));
                        }
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.heading("Panel");
                        ui.label(format!("{:.1} s", start.elapsed().as_secs_f64()));
                        ui.add(egui::Spinner::new());
                    });
                    ctx.request_repaint();
                },
            );
            // Close on focus loss, once it has had the focus.
            let (had, now) = {
                let g = shared.lock().unwrap();
                (g.panel_focused_once, g.panel_last_focus)
            };
            if had && now == Some(false) {
                self.panel_open = false;
                {
                    let mut g = shared.lock().unwrap();
                    g.panel_focused_once = false;
                    g.panel_last_focus = None;
                }
                note(&shared, start, "panel lost the focus: closing it".into());
                self.panel_reopen_at = Some(t + 2.0);
            }
        }

        // The confirmation by the clock: small, on top, asks not to be
        // activated.
        if self.toast_open {
            let s = shared.clone();
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("toast"),
                ViewportBuilder::default()
                    .with_title("confirmation")
                    .with_decorations(false)
                    .with_window_level(WindowLevel::AlwaysOnTop)
                    .with_taskbar(false)
                    .with_active(false)
                    .with_position(egui::pos2(monitor.x - 312.0, monitor.y - 140.0))
                    .with_inner_size([300.0, 64.0]),
                |ctx, _| {
                    count(&s, start, "toast");
                    let focused = ctx.input(|i| i.viewport().focused);
                    if focused == Some(true) {
                        note(&s, start, "confirmation: took the focus (it should not)".into());
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.label("Kitchen · 75 ms removed");
                    });
                },
            );
        }

        // Settings: an ordinary window.
        if self.settings_open {
            let s = shared.clone();
            let dialog_open = self.dialog_open;
            let mut close = false;
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("settings"),
                ViewportBuilder::default().with_title("Stream To Speaker — Settings").with_inner_size([640.0, 480.0]).with_position([80.0, 60.0]),
                |ctx, _| {
                    count(&s, start, "settings");
                    if ctx.input(|i| i.viewport().close_requested()) {
                        close = true;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.add_enabled_ui(!dialog_open, |ui| {
                            ui.heading("Settings");
                            let _ = ui.button("Something");
                        });
                    });
                },
            );
            if close {
                self.settings_open = false;
                note(&shared, start, "Settings closed".into());
            }
        }

        // A dialog over Settings.
        if self.dialog_open {
            let s = shared.clone();
            let mut done = false;
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("dialog"),
                ViewportBuilder::default().with_title("Pair with Den TV").with_inner_size([420.0, 220.0]).with_position([200.0, 160.0]).with_resizable(false),
                |ctx, _| {
                    count(&s, start, "dialog");
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.label("Look at Den TV: it's showing a 4-digit code now.");
                        if ui.button("Cancel").clicked() {
                            done = true;
                        }
                    });
                },
            );
            if done || t > 9.0 {
                self.dialog_open = false;
                note(&shared, start, "dialog closed".into());
            }
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

impl Spike {
    fn print_table(&self) {
        let g = self.shared.lock().unwrap();
        println!("\nframes drawn per second:");
        for who in ["root", "panel", "toast", "settings", "dialog"] {
            let row: Vec<String> = (0..=self.seconds).map(|s| g.frames.get(&(who, s)).copied().unwrap_or(0).to_string()).collect();
            println!("{who:>9}: {}", row.join(" "));
        }
    }
}
