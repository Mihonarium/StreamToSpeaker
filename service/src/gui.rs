//! Native GUI — single window built with egui via eframe.
//!
//! UX layout (top to bottom):
//!   1. Status banner — "are we streaming, and where to?"
//!   2. Speakers — list with radio buttons + refresh
//!   3. Latency — drain/pad buttons, pending indicator, resync
//!   4. Advanced (collapsible) — clock-fudge ppm, silence pace, step
//!   5. Stats — packets/s, subscriber count, uptime
//!
//! Closing the window minimises to tray rather than exiting; only the
//! tray menu's "Quit" actually exits.

#![cfg(windows)]

use anyhow::Result;
use eframe::egui;
use log::warn;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app::App;

/// Run the GUI. Blocks until the user quits (via tray menu or window
/// closing if no tray is shown). The tray icon, if requested, is built
/// inside this function so its lifetime spans the egui event loop.
pub fn run(app: Arc<App>, show_tray: bool) -> Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title("Stream To Speaker")
        .with_inner_size([640.0, 720.0])
        .with_min_inner_size([520.0, 560.0])
        // Belt-and-braces — egui's default *should* be visible on
        // Windows but a few combinations have shipped where it
        // isn't, leaving us with only a tray icon.
        .with_visible(true)
        .with_close_button(true);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let app_for_eframe = app.clone();
    let res = eframe::run_native(
        "Stream To Speaker",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            // Repaint at ~10 Hz so the live stats tick smoothly without
            // burning the CPU when nothing is changing.
            cc.egui_ctx.request_repaint_after(Duration::from_millis(100));

            let tray = if show_tray {
                match crate::tray::spawn(app_for_eframe.clone(), cc.egui_ctx.clone()) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        warn!("tray icon disabled (build failed): {:#}", e);
                        None
                    }
                }
            } else {
                None
            };

            Ok(Box::new(StreamToSpeakerApp {
                app: app_for_eframe,
                last_repaint_request: Instant::now(),
                tray,
                frame_count: 0,
                confirm_close_open: false,
                skip_close_confirmation: false,
            }))
        }),
    );

    res.map_err(|e| anyhow::anyhow!("eframe: {}", e))
}

struct StreamToSpeakerApp {
    app: Arc<App>,
    last_repaint_request: Instant,
    /// Tray icon + menu. Owned here so the tray lives as long as the
    /// egui app. `!Send` (muda uses Rc internally), so it has to live
    /// on the main thread.
    tray: Option<crate::tray::TrayHandle>,
    /// egui occasionally fires `close_requested()` on the very first
    /// frame on Windows; if we react to it we hide the window before
    /// it ever paints. Bump on each update; ignore close-requests
    /// before frame 2.
    frame_count: u64,
    /// True while the "are you sure" modal is shown.
    confirm_close_open: bool,
    /// If the user ticks "don't ask again", subsequent X presses go
    /// straight to minimise (when tray is up) or quit (when not).
    skip_close_confirmation: bool,
}

impl eframe::App for StreamToSpeakerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame_count = self.frame_count.saturating_add(1);

        // Periodic repaint for live stats and so the tray pump runs
        // even when nothing else triggers a frame.
        if self.last_repaint_request.elapsed() >= Duration::from_millis(100) {
            ctx.request_repaint_after(Duration::from_millis(100));
            self.last_repaint_request = Instant::now();
        }

        // Drain tray + menu events; update tray status text. Cheap when
        // nothing happened.
        if let Some(tray) = self.tray.as_mut() {
            tray.pump(&self.app, ctx);
        }

        // Handle close-button presses.
        //   - On the very first frame egui sometimes synthesises a
        //     close_requested on Windows; we'd hide the window before
        //     it paints once. Skip the first frame entirely.
        //   - With a tray: show the confirm modal unless the user has
        //     opted out, in which case minimise straight to tray.
        //   - Without a tray: X = quit (no way to get the window back).
        let close_pressed = ctx.input(|i| i.viewport().close_requested());
        if close_pressed && self.frame_count > 1 && !self.app.is_shutting_down() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.tray.is_none() {
                self.app.request_shutdown();
            } else if self.skip_close_confirmation {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.confirm_close_open = true;
            }
        }
        if self.app.is_shutting_down() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Modal — drawn on top of the main UI when set.
        if self.confirm_close_open {
            self.show_close_modal(ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Dim everything behind the modal so it reads as modal.
            let enabled = !self.confirm_close_open;
            ui.add_enabled_ui(enabled, |ui| {
                ui.add_space(4.0);
                self.show_status_banner(ui);
                ui.add_space(12.0);
                self.show_speakers(ui);
                ui.add_space(12.0);
                self.show_latency(ui);
                ui.add_space(12.0);
                self.show_advanced(ui);
                ui.add_space(12.0);
                self.show_stats(ui);
            });
        });
    }
}

impl StreamToSpeakerApp {
    fn show_status_banner(&self, ui: &mut egui::Ui) {
        let enabled = self.app.is_streaming_enabled();
        let active = self.app.stream_active.load(Ordering::Acquire);
        let current = self.app.current_renderer();

        let (icon, icon_color, headline, detail) = match (&current, enabled, active) {
            (None, _, _) => (
                "❔",
                egui::Color32::GRAY,
                "No speaker selected".to_string(),
                "Pick a speaker below to start streaming".to_string(),
            ),
            (Some(_), false, _) => (
                "⊘",
                egui::Color32::from_rgb(200, 90, 70),
                "Streaming disabled".to_string(),
                "The speaker is free for other use".to_string(),
            ),
            (Some(r), true, true) => (
                "▶",
                egui::Color32::from_rgb(80, 180, 100),
                format!("Streaming to {}", r.friendly_name),
                format!("{} · {} pkt/s", r.ip, self.packets_per_sec()),
            ),
            (Some(r), true, false) => (
                "⏸",
                egui::Color32::from_rgb(160, 160, 100),
                format!("Idle on {}", r.friendly_name),
                format!("{} · silence", r.ip),
            ),
        };

        egui::Frame::group(ui.style())
            .fill(ui.visuals().faint_bg_color)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new(icon).color(icon_color).size(28.0));
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(headline).heading());
                        ui.label(egui::RichText::new(detail).small().color(ui.visuals().weak_text_color()));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if enabled { "Disable" } else { "Enable" };
                        if ui.button(label).clicked() {
                            if let Err(e) = self.app.set_streaming_enabled(!enabled) {
                                warn!("toggle streaming failed: {}", e);
                            }
                        }
                    });
                });
            });
    }

    fn show_speakers(&self, ui: &mut egui::Ui) {
        let view = self.app.speaker_view();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Speakers").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("↻ Refresh").clicked() {
                    // Refresh runs in the SSDP discovery thread on its
                    // own cadence; we can't force it from here without
                    // wiring a kick channel. Manual scroll for now —
                    // the next periodic sweep will land within ~5 min.
                    // A future improvement: add an app.refresh_speakers().
                }
            });
        });
        ui.separator();

        if view.speakers.is_empty() {
            ui.label(
                egui::RichText::new("No speakers discovered yet (or --no-discovery)")
                    .italics()
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        }

        egui::ScrollArea::vertical()
            .max_height(160.0)
            .show(ui, |ui| {
                for sp in view.speakers {
                    let active = sp.active;
                    let label = format!("{}    {}", sp.friendly_name, sp.ip);
                    if ui
                        .add(egui::RadioButton::new(active, label))
                        .clicked()
                        && !active
                    {
                        if let Err(e) = self.app.select_speaker(&sp.id) {
                            warn!("select speaker failed: {}", e);
                        }
                    }
                }
            });
    }

    fn show_latency(&self, ui: &mut egui::Ui) {
        let pending = self.app.pending_latency_ms();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Latency").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = format!("pending: {} ms", pending);
                let color = if pending == 0 {
                    ui.visuals().weak_text_color()
                } else {
                    egui::Color32::from_rgb(220, 170, 70)
                };
                ui.label(egui::RichText::new(text).color(color));
            });
        });
        ui.separator();

        ui.horizontal(|ui| {
            if ui.button("−100 ms").clicked() {
                self.app.adjust_latency(100);
            }
            if ui.button("−25 ms").clicked() {
                self.app.adjust_latency(25);
            }
            ui.add_space(20.0);
            if ui.button("+25 ms").clicked() {
                self.app.adjust_latency(-25);
            }
            if ui.button("+100 ms").clicked() {
                self.app.adjust_latency(-100);
            }
        });

        ui.add_space(6.0);
        if ui.button("⟳  Resync (hard reset)").on_hover_text(
            "UPnP Stop + Play — the speaker discards its prebuffer. Brief audio glitch.",
        ).clicked() {
            if let Err(e) = self.app.resync() {
                warn!("resync failed: {}", e);
            }
        }
    }

    fn show_advanced(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(egui::RichText::new("⚙  Advanced").strong())
            .default_open(false)
            .show(ui, |ui| {
                let mut ppm = self.app.rate_fudge_ppm.load(Ordering::Relaxed);
                ui.horizontal(|ui| {
                    ui.label("Clock drift compensation");
                    ui.add(egui::DragValue::new(&mut ppm).range(-1000..=1000).suffix(" ppm"));
                    if ui.button("0").clicked() {
                        ppm = 0;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Positive over-produces; negative drops. Try +50 to +100 \
                         if the buffer slowly runs out, negative if it overflows.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
                self.app.set_rate_fudge_ppm(ppm);

                ui.add_space(8.0);

                let mut pace = self.app.silence_pace_ms.load(Ordering::Relaxed) as i64;
                ui.horizontal(|ui| {
                    ui.label("Silence pacing");
                    ui.add(egui::DragValue::new(&mut pace).range(1..=100).suffix(" ms"));
                    if ui.button("10").clicked() {
                        pace = 10;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "10 = real-time. >10 sends slower than real-time during \
                         pauses, draining the speaker's prebuffer so post-pause \
                         latency is smaller.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
                self.app.set_silence_pace_ms(pace.max(1) as u64);

                ui.add_space(8.0);

                let mut step =
                    self.app.latency_adjust_step_frames.load(Ordering::Relaxed) as i64;
                ui.horizontal(|ui| {
                    ui.label("Latency-adjust step");
                    ui.add(egui::DragValue::new(&mut step).range(1..=256).suffix(" frames"));
                    if ui.button("4").clicked() {
                        step = 4;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Max frames added or dropped per packet when servicing \
                         a drain / pad request. Larger = snappier, more audible click.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
                self.app.set_latency_adjust_step_frames(step.max(1) as u32);
            });
    }

    fn show_stats(&self, ui: &mut egui::Ui) {
        let pkts = self.app.packets_published();
        let subs = self.app.subscriber_count();
        let uptime = self.app.uptime_secs();

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Stats").strong());
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{} pkt/s", self.packets_per_sec())).strong());
            ui.label("·");
            ui.label(format!("{} subscriber{}", subs, if subs == 1 { "" } else { "s" }));
            ui.label("·");
            ui.label(format!("up {}", format_duration(uptime)));
            ui.label("·");
            ui.label(format!("{} packets total", pkts));
        });
    }

    fn packets_per_sec(&self) -> u64 {
        // Approximate using total / uptime. Good enough for a live indicator.
        let up = self.app.uptime_secs().max(1);
        self.app.packets_published() / up
    }

    fn show_close_modal(&mut self, ctx: &egui::Context) {
        // Centred floating window, can't be moved or resized — feels
        // like a modal dialog without actually blocking the event loop
        // (which would also block the audio loop's GUI signals).
        let mut still_open = self.confirm_close_open;
        let mut new_skip = self.skip_close_confirmation;
        let mut action: Option<CloseAction> = None;

        egui::Window::new("Close Stream To Speaker?")
            .open(&mut still_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.label(
                    "Minimise to the system tray to keep streaming in the \
                     background, or quit the app entirely.",
                );
                ui.add_space(8.0);
                ui.checkbox(&mut new_skip, "Always minimise to tray — don't ask again");
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("📥  Minimise to tray").strong())
                        .clicked()
                    {
                        action = Some(CloseAction::MinimiseToTray);
                    }
                    if ui.button("Quit Stream To Speaker").clicked() {
                        action = Some(CloseAction::Quit);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            action = Some(CloseAction::Cancel);
                        }
                    });
                });
            });

        // egui::Window's `open` boolean is set to false when the user
        // clicks the window's own close button — treat that as Cancel.
        if !still_open && action.is_none() {
            action = Some(CloseAction::Cancel);
        }

        self.skip_close_confirmation = new_skip;
        if let Some(a) = action {
            self.confirm_close_open = false;
            match a {
                CloseAction::MinimiseToTray => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                CloseAction::Quit => {
                    self.app.request_shutdown();
                }
                CloseAction::Cancel => {}
            }
        }
    }
}

#[derive(Copy, Clone)]
enum CloseAction {
    MinimiseToTray,
    Quit,
    Cancel,
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h {}m", h, m)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}
