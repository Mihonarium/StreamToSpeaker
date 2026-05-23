//! Native GUI — single window built with egui via eframe.
//!
//! Visual design notes (so future edits stay coherent):
//!   - Tight dark palette: near-black canvas (#10141a), elevated cards
//!     (#1c2230), brand accent in cyan-teal (#5ccfe6). Status colours
//!     borrow Material's swatches lightly desaturated so they sit well
//!     against the dark canvas.
//!   - Sections are cards: a Frame with rounded corners, a 1 px stroke
//!     in `panel.divider`, and a coloured accent stripe on the left
//!     edge of the status banner driven by streaming state.
//!   - Typography hierarchy uses egui's TextStyle: Heading for section
//!     titles, Body for primary content, Small for hints.
//!   - Buttons: default = neutral; "Resync" gets a muted-danger
//!     treatment; "Disable" / "Enable" gets the brand-accent treatment.
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

// -----------------------------------------------------------------------------
// Palette
// -----------------------------------------------------------------------------

mod palette {
    use eframe::egui::Color32;

    pub const CANVAS: Color32 = Color32::from_rgb(0x10, 0x14, 0x1a);
    pub const CARD: Color32 = Color32::from_rgb(0x1c, 0x22, 0x30);
    pub const CARD_HOVER: Color32 = Color32::from_rgb(0x24, 0x2c, 0x3c);
    pub const DIVIDER: Color32 = Color32::from_rgb(0x2a, 0x32, 0x42);
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xea, 0xed, 0xf2);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0x9a, 0xa1, 0xae);
    pub const ACCENT: Color32 = Color32::from_rgb(0x5c, 0xcf, 0xe6);
    pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x3f, 0x8e, 0x9d);
    pub const SUCCESS: Color32 = Color32::from_rgb(0x6c, 0xc6, 0x8e);
    pub const WARN: Color32 = Color32::from_rgb(0xe6, 0xc3, 0x7c);
    pub const DANGER: Color32 = Color32::from_rgb(0xe6, 0x7e, 0x80);
    pub const MUTED: Color32 = Color32::from_rgb(0x6e, 0x77, 0x88);
}

// -----------------------------------------------------------------------------
// Theme — applied once at startup
// -----------------------------------------------------------------------------

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = palette::CANVAS;
    visuals.window_fill = palette::CARD;
    visuals.window_stroke = egui::Stroke::new(1.0, palette::DIVIDER);
    visuals.faint_bg_color = palette::CARD;
    visuals.extreme_bg_color = palette::CANVAS;
    visuals.code_bg_color = palette::CARD;
    visuals.override_text_color = Some(palette::TEXT_PRIMARY);
    // weak_text_color is a method in egui 0.29 that derives from
    // widgets.noninteractive.fg_stroke.color — we already set that
    // above, so the derived "weak" tone follows automatically.
    visuals.hyperlink_color = palette::ACCENT;
    visuals.selection.bg_fill = palette::ACCENT_DIM;
    visuals.selection.stroke = egui::Stroke::new(1.0, palette::ACCENT);
    visuals.widgets.noninteractive.bg_fill = palette::CARD;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, palette::DIVIDER);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, palette::TEXT_SECONDARY);
    visuals.widgets.inactive.bg_fill = palette::CARD;
    visuals.widgets.inactive.weak_bg_fill = palette::CARD;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, palette::DIVIDER);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, palette::TEXT_PRIMARY);
    visuals.widgets.inactive.rounding = 6.0.into();
    visuals.widgets.hovered.bg_fill = palette::CARD_HOVER;
    visuals.widgets.hovered.weak_bg_fill = palette::CARD_HOVER;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, palette::ACCENT_DIM);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, palette::TEXT_PRIMARY);
    visuals.widgets.hovered.rounding = 6.0.into();
    visuals.widgets.active.bg_fill = palette::ACCENT_DIM;
    visuals.widgets.active.weak_bg_fill = palette::ACCENT_DIM;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, palette::ACCENT);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, palette::TEXT_PRIMARY);
    visuals.widgets.active.rounding = 6.0.into();
    visuals.widgets.open.bg_fill = palette::CARD;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, palette::ACCENT_DIM);
    visuals.window_rounding = 10.0.into();
    visuals.menu_rounding = 8.0.into();
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(16.0);
    style.spacing.interact_size.y = 28.0;
    ctx.set_style(style);
}

// -----------------------------------------------------------------------------
// Run
// -----------------------------------------------------------------------------

/// Run the GUI. Blocks until the user quits (via tray menu or window
/// closing if no tray is shown). The tray icon, if requested, is built
/// inside this function so its lifetime spans the egui event loop.
pub fn run(app: Arc<App>, show_tray: bool) -> Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title("Stream To Speaker")
        .with_inner_size([680.0, 760.0])
        .with_min_inner_size([560.0, 600.0])
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
            apply_theme(&cc.egui_ctx);
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

        if self.last_repaint_request.elapsed() >= Duration::from_millis(100) {
            ctx.request_repaint_after(Duration::from_millis(100));
            self.last_repaint_request = Instant::now();
        }

        if let Some(tray) = self.tray.as_mut() {
            tray.pump(&self.app, ctx);
        }

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

        if self.confirm_close_open {
            self.show_close_modal(ctx);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(palette::CANVAS).inner_margin(20.0))
            .show(ctx, |ui| {
                let enabled = !self.confirm_close_open;
                ui.add_enabled_ui(enabled, |ui| {
                    self.show_status_banner(ui);
                    ui.add_space(14.0);
                    self.show_speakers(ui);
                    ui.add_space(14.0);
                    self.show_latency(ui);
                    ui.add_space(14.0);
                    self.show_advanced(ui);
                    ui.add_space(14.0);
                    self.show_stats(ui);
                });
            });
    }
}

// -----------------------------------------------------------------------------
// Card helper — every section uses the same wrapping frame
// -----------------------------------------------------------------------------

fn card<R>(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(palette::CARD)
        .stroke(egui::Stroke::new(1.0, palette::DIVIDER))
        .rounding(10.0)
        .inner_margin(egui::Margin::symmetric(16.0, 14.0))
        .show(ui, content)
        .inner
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(11.0)
            .strong()
            .color(palette::TEXT_SECONDARY)
            .extra_letter_spacing(1.5),
    );
    ui.add_space(2.0);
}

impl StreamToSpeakerApp {
    fn show_status_banner(&self, ui: &mut egui::Ui) {
        let enabled = self.app.is_streaming_enabled();
        let active = self.app.stream_active.load(Ordering::Acquire);
        let current = self.app.current_renderer();

        let (icon, accent, headline, detail) = match (&current, enabled, active) {
            (None, _, _) => (
                "?",
                palette::MUTED,
                "No speaker selected".to_string(),
                "Pick a speaker below to start streaming".to_string(),
            ),
            (Some(_), false, _) => (
                "⊘",
                palette::DANGER,
                "Streaming disabled".to_string(),
                "The speaker is free for other use".to_string(),
            ),
            (Some(r), true, true) => (
                "▶",
                palette::SUCCESS,
                format!("Streaming to {}", r.friendly_name),
                format!("{} · {} pkt/s", r.ip, self.packets_per_sec()),
            ),
            (Some(r), true, false) => (
                "‖",
                palette::WARN,
                format!("Idle on {}", r.friendly_name),
                format!("{} · silence", r.ip),
            ),
        };

        // Use a custom frame with an accent stripe on the left edge.
        egui::Frame::none()
            .fill(palette::CARD)
            .stroke(egui::Stroke::new(1.0, palette::DIVIDER))
            .rounding(10.0)
            .inner_margin(egui::Margin {
                left: 0.0,
                right: 16.0,
                top: 14.0,
                bottom: 14.0,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Accent stripe — colored bar on the left edge
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(4.0, 56.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 0.0, accent);
                    ui.add_space(12.0);

                    // Big status icon
                    ui.label(
                        egui::RichText::new(icon)
                            .color(accent)
                            .size(34.0)
                            .strong(),
                    );
                    ui.add_space(10.0);

                    // Headline + detail
                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(headline)
                                .size(17.0)
                                .strong()
                                .color(palette::TEXT_PRIMARY),
                        );
                        ui.label(
                            egui::RichText::new(detail)
                                .size(12.0)
                                .color(palette::TEXT_SECONDARY),
                        );
                    });

                    // Enable / Disable button on the right
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if current.is_some() {
                                let label = if enabled {
                                    egui::RichText::new("Disable").color(palette::TEXT_PRIMARY)
                                } else {
                                    egui::RichText::new("Enable").strong().color(palette::CANVAS)
                                };
                                let btn = if enabled {
                                    egui::Button::new(label).fill(palette::CARD_HOVER)
                                } else {
                                    egui::Button::new(label).fill(palette::ACCENT)
                                };
                                if ui.add_sized([90.0, 32.0], btn).clicked() {
                                    if let Err(e) = self.app.set_streaming_enabled(!enabled) {
                                        warn!("toggle streaming failed: {}", e);
                                    }
                                }
                            }
                        },
                    );
                });
            });
    }

    fn show_speakers(&self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                section_label(ui, "Speakers");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let _ = ui.add(
                        egui::Button::new(
                            egui::RichText::new("↻  Refresh").size(12.0),
                        )
                        .fill(palette::CARD)
                        .stroke(egui::Stroke::new(1.0, palette::DIVIDER)),
                    );
                });
            });
            ui.add_space(4.0);

            let view = self.app.speaker_view();
            if view.speakers.is_empty() {
                ui.label(
                    egui::RichText::new("No speakers discovered yet")
                        .color(palette::TEXT_SECONDARY)
                        .italics(),
                );
                return;
            }

            egui::ScrollArea::vertical()
                .max_height(180.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for sp in view.speakers {
                        speaker_row(ui, &sp, |id| {
                            if let Err(e) = self.app.select_speaker(id) {
                                warn!("select speaker failed: {}", e);
                            }
                        });
                    }
                });
        });
    }

    fn show_latency(&self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            let pending = self.app.pending_latency_ms();
            ui.horizontal(|ui| {
                section_label(ui, "Latency");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (color, text) = if pending == 0 {
                        (palette::TEXT_SECONDARY, "stable".to_string())
                    } else {
                        (palette::WARN, format!("{:+} ms pending", -pending))
                    };
                    ui.label(egui::RichText::new(text).size(12.0).color(color));
                });
            });
            ui.add_space(8.0);

            // Drain / Pad buttons — two grouped pairs with a clear gap
            ui.horizontal(|ui| {
                let drain_btn = |label: &str| {
                    egui::Button::new(egui::RichText::new(label).color(palette::TEXT_PRIMARY))
                        .fill(palette::CANVAS)
                        .stroke(egui::Stroke::new(1.0, palette::DIVIDER))
                };
                if ui.add_sized([78.0, 32.0], drain_btn("−100 ms")).clicked() {
                    self.app.adjust_latency(100);
                }
                if ui.add_sized([72.0, 32.0], drain_btn("−25 ms")).clicked() {
                    self.app.adjust_latency(25);
                }
                ui.add_space(24.0);
                if ui.add_sized([72.0, 32.0], drain_btn("+25 ms")).clicked() {
                    self.app.adjust_latency(-25);
                }
                if ui.add_sized([78.0, 32.0], drain_btn("+100 ms")).clicked() {
                    self.app.adjust_latency(-100);
                }
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            // Resync = muted-danger styling, hover tooltip explains.
            let resync_btn = egui::Button::new(
                egui::RichText::new("⟳  Resync speaker (hard reset)").color(palette::DANGER),
            )
            .fill(palette::CARD)
            .stroke(egui::Stroke::new(1.0, palette::DANGER.gamma_multiply(0.6)));
            if ui
                .add_sized([260.0, 30.0], resync_btn)
                .on_hover_text(
                    "UPnP Stop + Play. The speaker discards its prebuffer; \
                     brief audio glitch but trims accumulated latency in one shot.",
                )
                .clicked()
            {
                if let Err(e) = self.app.resync() {
                    warn!("resync failed: {}", e);
                }
            }
        });
    }

    fn show_advanced(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            egui::CollapsingHeader::new(
                egui::RichText::new("Advanced")
                    .size(11.0)
                    .strong()
                    .color(palette::TEXT_SECONDARY)
                    .extra_letter_spacing(1.5),
            )
            .id_salt("advanced_section")
            .default_open(false)
            .show(ui, |ui| {
                ui.add_space(4.0);

                let mut ppm = self.app.rate_fudge_ppm.load(Ordering::Relaxed);
                advanced_row(
                    ui,
                    "Clock drift compensation",
                    "Positive over-produces; negative drops. Try +50 to +100 if the buffer slowly runs out, negative if it overflows.",
                    |ui| {
                        ui.add(egui::DragValue::new(&mut ppm).range(-1000..=1000).suffix(" ppm"));
                        if ui.small_button("reset").clicked() { ppm = 0; }
                    },
                );
                self.app.set_rate_fudge_ppm(ppm);

                ui.add_space(8.0);

                let mut pace = self.app.silence_pace_ms.load(Ordering::Relaxed) as i64;
                advanced_row(
                    ui,
                    "Silence pacing",
                    "10 = real-time. >10 sends slower than real-time during pauses, draining the speaker's prebuffer so post-pause latency is smaller.",
                    |ui| {
                        ui.add(egui::DragValue::new(&mut pace).range(1..=100).suffix(" ms"));
                        if ui.small_button("reset").clicked() { pace = 10; }
                    },
                );
                self.app.set_silence_pace_ms(pace.max(1) as u64);

                ui.add_space(8.0);

                let mut step = self.app.latency_adjust_step_frames.load(Ordering::Relaxed) as i64;
                advanced_row(
                    ui,
                    "Latency-adjust step",
                    "Max frames added or dropped per packet when servicing a drain / pad request. Larger = snappier, more audible click.",
                    |ui| {
                        ui.add(egui::DragValue::new(&mut step).range(1..=256).suffix(" frames"));
                        if ui.small_button("reset").clicked() { step = 4; }
                    },
                );
                self.app.set_latency_adjust_step_frames(step.max(1) as u32);
            });
        });
    }

    fn show_stats(&self, ui: &mut egui::Ui) {
        let subs = self.app.subscriber_count();
        let uptime = self.app.uptime_secs();
        let pkts_total = self.app.packets_published();
        let pkts_sec = self.packets_per_sec();

        card(ui, |ui| {
            section_label(ui, "Stats");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                stat_pill(ui, &format!("{}", pkts_sec), "pkt / s");
                stat_pill(ui, &format!("{}", subs), if subs == 1 { "listener" } else { "listeners" });
                stat_pill(ui, &format_duration(uptime), "uptime");
                stat_pill(ui, &humanize_count(pkts_total), "packets");
            });
        });
    }

    fn packets_per_sec(&self) -> u64 {
        let up = self.app.uptime_secs().max(1);
        self.app.packets_published() / up
    }

    fn show_close_modal(&mut self, ctx: &egui::Context) {
        let mut still_open = self.confirm_close_open;
        let mut new_skip = self.skip_close_confirmation;
        let mut action: Option<CloseAction> = None;

        egui::Window::new(
            egui::RichText::new("Close Stream To Speaker?")
                .size(15.0)
                .strong(),
        )
            .open(&mut still_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(440.0)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(palette::CARD)
                    .stroke(egui::Stroke::new(1.0, palette::DIVIDER))
                    .rounding(12.0)
                    .inner_margin(20.0),
            )
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Minimise to the system tray to keep streaming in the background, \
                         or quit the app entirely.",
                    )
                    .color(palette::TEXT_SECONDARY),
                );
                ui.add_space(10.0);
                ui.checkbox(
                    &mut new_skip,
                    "Always minimise to tray — don't ask again this session",
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_sized(
                            [160.0, 32.0],
                            egui::Button::new(
                                egui::RichText::new("📥  Minimise to tray")
                                    .strong()
                                    .color(palette::CANVAS),
                            )
                            .fill(palette::ACCENT),
                        )
                        .clicked()
                    {
                        action = Some(CloseAction::MinimiseToTray);
                    }
                    if ui
                        .add_sized(
                            [140.0, 32.0],
                            egui::Button::new(
                                egui::RichText::new("Quit").color(palette::DANGER),
                            )
                            .fill(palette::CARD)
                            .stroke(egui::Stroke::new(1.0, palette::DANGER.gamma_multiply(0.6))),
                        )
                        .clicked()
                    {
                        action = Some(CloseAction::Quit);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_sized([80.0, 32.0], egui::Button::new("Cancel"))
                            .clicked()
                        {
                            action = Some(CloseAction::Cancel);
                        }
                    });
                });
            });

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

// -----------------------------------------------------------------------------
// Small composable pieces
// -----------------------------------------------------------------------------

fn speaker_row(ui: &mut egui::Ui, sp: &crate::http_server::SpeakerInfo, on_click: impl FnOnce(&str)) {
    let active = sp.active;
    let row_fill = if active { palette::ACCENT_DIM.gamma_multiply(0.35) } else { egui::Color32::TRANSPARENT };
    let row_stroke = if active { palette::ACCENT_DIM } else { palette::DIVIDER };

    let response = egui::Frame::none()
        .fill(row_fill)
        .stroke(egui::Stroke::new(1.0, row_stroke))
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Radio-style indicator
                let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                let center = rect.center();
                ui.painter().circle_stroke(
                    center,
                    6.5,
                    egui::Stroke::new(
                        1.5,
                        if active { palette::ACCENT } else { palette::TEXT_SECONDARY },
                    ),
                );
                if active {
                    ui.painter().circle_filled(center, 3.5, palette::ACCENT);
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(&sp.friendly_name)
                        .color(palette::TEXT_PRIMARY)
                        .size(14.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(&sp.ip)
                            .color(palette::TEXT_SECONDARY)
                            .size(12.0)
                            .monospace(),
                    );
                });
            });
        })
        .response
        .interact(egui::Sense::click());

    if response.hovered() && !active {
        ui.painter().rect_stroke(
            response.rect,
            6.0,
            egui::Stroke::new(1.0, palette::ACCENT_DIM),
        );
    }
    if response.clicked() && !active {
        on_click(&sp.id);
    }
    ui.add_space(4.0);
}

fn advanced_row(
    ui: &mut egui::Ui,
    label: &str,
    hint: &str,
    add_control: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label).color(palette::TEXT_PRIMARY));
            ui.label(
                egui::RichText::new(hint)
                    .size(11.0)
                    .color(palette::TEXT_SECONDARY),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            add_control(ui);
        });
    });
}

fn stat_pill(ui: &mut egui::Ui, value: &str, label: &str) {
    egui::Frame::none()
        .fill(palette::CANVAS)
        .stroke(egui::Stroke::new(1.0, palette::DIVIDER))
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(value)
                        .strong()
                        .size(14.0)
                        .color(palette::TEXT_PRIMARY),
                );
                ui.label(
                    egui::RichText::new(label)
                        .size(10.0)
                        .color(palette::TEXT_SECONDARY)
                        .extra_letter_spacing(0.5),
                );
            });
        });
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

fn humanize_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}
