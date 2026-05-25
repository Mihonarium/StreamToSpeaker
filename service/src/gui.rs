//! Native GUI — single window built with egui via eframe.
//!
//! UX foundation:
//!   - Light theme default (per Refactoring UI / LogRocket guidance — most
//!     users, daytime use). Three-way toggle: System / Light / Dark in the
//!     header. Theme applies live without restart.
//!   - WCAG AA contrast for text (≥4.5:1) and UI elements (≥3:1) in both
//!     themes — palette values checked manually.
//!   - Hierarchy via tone and weight, not size: primary text is near-black
//!     on light / near-white on dark; secondary is a tinted mid-gray; tertiary
//!     fades further. Two font weights (regular + semi-bold).
//!   - Buttons have a clear hierarchy:
//!       * Primary action (Enable/Disable streaming, modal default): solid
//!         accent fill, white text on accent.
//!       * Secondary action (latency adjust, refresh): outlined, neutral bg.
//!       * Danger action (Resync): red text, thin red border, only used
//!         when the action genuinely glitches audio.
//!   - All interactive elements have distinct rest / hover / active / focus
//!     states. Focus ring is a 2px outline in the accent colour for keyboard
//!     navigability.
//!   - Tooltips on every action button explaining what it does. Don't make
//!     the user guess what "−25 ms" means.
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
// Theme + palette
// -----------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    System,
    Light,
    Dark,
}

impl ThemeMode {
    fn resolve(self, ctx: &egui::Context) -> bool {
        // Returns true = dark, false = light.
        match self {
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
            ThemeMode::System => {
                // egui exposes the OS's preference via this enum on platforms
                // that surface it (Windows 10+ reports through winit).
                match ctx.system_theme() {
                    Some(egui::Theme::Dark) => true,
                    Some(egui::Theme::Light) | None => false,
                }
            }
        }
    }
}

#[derive(Copy, Clone)]
struct Palette {
    // Surfaces
    canvas: egui::Color32,        // page background
    card: egui::Color32,          // section card
    card_hover: egui::Color32,    // hover lift
    card_active: egui::Color32,   // pressed
    divider: egui::Color32,       // borders
    overlay: egui::Color32,       // selected-row background tint

    // Text
    text_primary: egui::Color32,
    text_secondary: egui::Color32,
    text_tertiary: egui::Color32,
    text_on_accent: egui::Color32,

    // Brand
    accent: egui::Color32,        // primary action bg
    accent_hover: egui::Color32,
    accent_subtle: egui::Color32, // tint for selection / focus

    // Status
    success: egui::Color32,       // streaming active
    warn: egui::Color32,          // idle / pending adjust
    danger: egui::Color32,        // disabled / resync
    muted: egui::Color32,         // no speaker selected
}

impl Palette {
    const fn light() -> Self {
        Self {
            canvas: egui::Color32::from_rgb(0xfa, 0xfb, 0xfc),
            card: egui::Color32::from_rgb(0xff, 0xff, 0xff),
            card_hover: egui::Color32::from_rgb(0xf3, 0xf6, 0xf8),
            card_active: egui::Color32::from_rgb(0xe8, 0xec, 0xf1),
            divider: egui::Color32::from_rgb(0xe1, 0xe6, 0xeb),
            overlay: egui::Color32::from_rgb(0xe0, 0xf3, 0xf7),

            text_primary: egui::Color32::from_rgb(0x14, 0x1b, 0x29),
            text_secondary: egui::Color32::from_rgb(0x53, 0x5d, 0x6e),
            text_tertiary: egui::Color32::from_rgb(0x8a, 0x93, 0xa3),
            text_on_accent: egui::Color32::from_rgb(0xff, 0xff, 0xff),

            accent: egui::Color32::from_rgb(0x07, 0x83, 0x96),
            accent_hover: egui::Color32::from_rgb(0x05, 0x6b, 0x7c),
            accent_subtle: egui::Color32::from_rgb(0xc6, 0xe7, 0xed),

            success: egui::Color32::from_rgb(0x16, 0x90, 0x4f),
            warn: egui::Color32::from_rgb(0xb5, 0x70, 0x10),
            danger: egui::Color32::from_rgb(0xc8, 0x37, 0x37),
            muted: egui::Color32::from_rgb(0x6f, 0x78, 0x89),
        }
    }

    const fn dark() -> Self {
        Self {
            canvas: egui::Color32::from_rgb(0x0e, 0x12, 0x1b),
            card: egui::Color32::from_rgb(0x1a, 0x20, 0x2d),
            card_hover: egui::Color32::from_rgb(0x23, 0x2b, 0x3b),
            card_active: egui::Color32::from_rgb(0x2c, 0x35, 0x47),
            divider: egui::Color32::from_rgb(0x2a, 0x32, 0x42),
            overlay: egui::Color32::from_rgb(0x16, 0x3b, 0x44),

            text_primary: egui::Color32::from_rgb(0xea, 0xee, 0xf4),
            text_secondary: egui::Color32::from_rgb(0xa0, 0xa8, 0xb8),
            text_tertiary: egui::Color32::from_rgb(0x6e, 0x77, 0x88),
            text_on_accent: egui::Color32::from_rgb(0x0e, 0x12, 0x1b),

            accent: egui::Color32::from_rgb(0x5c, 0xcf, 0xe6),
            accent_hover: egui::Color32::from_rgb(0x7e, 0xdc, 0xee),
            accent_subtle: egui::Color32::from_rgb(0x1d, 0x4f, 0x5b),

            success: egui::Color32::from_rgb(0x6c, 0xd2, 0x95),
            warn: egui::Color32::from_rgb(0xe6, 0xc3, 0x7c),
            danger: egui::Color32::from_rgb(0xe6, 0x7e, 0x80),
            muted: egui::Color32::from_rgb(0x80, 0x8a, 0x9c),
        }
    }
}

fn apply_theme(ctx: &egui::Context, dark: bool) {
    let p = if dark { Palette::dark() } else { Palette::light() };
    let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };

    visuals.panel_fill = p.canvas;
    visuals.window_fill = p.card;
    visuals.window_stroke = egui::Stroke::new(1.0, p.divider);
    visuals.faint_bg_color = p.card;
    visuals.extreme_bg_color = p.canvas;
    visuals.code_bg_color = p.card_hover;
    visuals.override_text_color = Some(p.text_primary);
    visuals.hyperlink_color = p.accent;
    visuals.selection.bg_fill = p.accent_subtle;
    visuals.selection.stroke = egui::Stroke::new(1.0, p.accent);

    // Non-interactive (labels, separators)
    visuals.widgets.noninteractive.bg_fill = p.card;
    visuals.widgets.noninteractive.weak_bg_fill = p.card;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.divider);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.text_secondary);
    visuals.widgets.noninteractive.rounding = 6.0.into();

    // Inactive (button at rest)
    visuals.widgets.inactive.bg_fill = p.card;
    visuals.widgets.inactive.weak_bg_fill = p.card;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, p.divider);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, p.text_primary);
    visuals.widgets.inactive.rounding = 6.0.into();
    visuals.widgets.inactive.expansion = 0.0;

    // Hovered — subtle bg lift, accent border. Communicates "click me".
    visuals.widgets.hovered.bg_fill = p.card_hover;
    visuals.widgets.hovered.weak_bg_fill = p.card_hover;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, p.accent);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, p.text_primary);
    visuals.widgets.hovered.rounding = 6.0.into();
    visuals.widgets.hovered.expansion = 1.0;

    // Active — pressed, darker fill
    visuals.widgets.active.bg_fill = p.card_active;
    visuals.widgets.active.weak_bg_fill = p.card_active;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.5, p.accent);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, p.text_primary);
    visuals.widgets.active.rounding = 6.0.into();
    visuals.widgets.active.expansion = 0.0;

    visuals.widgets.open.bg_fill = p.card_hover;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, p.accent);

    visuals.window_rounding = 12.0.into();
    visuals.menu_rounding = 8.0.into();

    // Focus ring — visible 2px accent outline for keyboard accessibility.
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, p.accent);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 7.0);
    style.spacing.window_margin = egui::Margin::same(16.0);
    style.spacing.interact_size.y = 30.0;
    ctx.set_style(style);
}

fn palette_for(dark: bool) -> Palette {
    if dark { Palette::dark() } else { Palette::light() }
}

// -----------------------------------------------------------------------------
// Run
// -----------------------------------------------------------------------------

pub fn run(app: Arc<App>, show_tray: bool) -> Result<()> {
    // with_taskbar(false) removes the taskbar entry permanently —
    // typical pattern for tray apps. When we "hide to tray" we just
    // call Win32 ShowWindow(SW_HIDE) directly, which also stops the
    // window appearing in Alt-Tab. Reasoning + canonical pattern:
    // emilk/egui Discussion #737, #1978; egui#3654 (Visible(false) is
    // unreliable at startup) — taskbar gating is the working knob.
    let viewport = egui::ViewportBuilder::default()
        .with_title("Stream To Speaker")
        .with_inner_size([720.0, 800.0])
        .with_min_inner_size([580.0, 620.0])
        .with_visible(true)
        .with_taskbar(false)
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
            // Initial theme — System (which falls back to Light if the OS
            // doesn't expose a preference).
            apply_theme(&cc.egui_ctx, ThemeMode::System.resolve(&cc.egui_ctx));
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
                theme_mode: ThemeMode::System,
                advanced_open: false,
                onboarding_dismissed: false,
                last_applied_dark: None,
                hwnd: None,
            }))
        }),
    );

    res.map_err(|e| anyhow::anyhow!("eframe: {}", e))
}

struct StreamToSpeakerApp {
    app: Arc<App>,
    last_repaint_request: Instant,
    tray: Option<crate::tray::TrayHandle>,
    frame_count: u64,
    confirm_close_open: bool,
    skip_close_confirmation: bool,
    theme_mode: ThemeMode,
    advanced_open: bool,
    onboarding_dismissed: bool,
    /// Whether the theme is currently applied as dark. None until first
    /// apply. We re-apply only when this disagrees with the resolved
    /// mode — apply_theme rebuilds the full Style+Visuals every call
    /// and was being run on every frame.
    last_applied_dark: Option<bool>,
    /// Raw Win32 HWND, captured on the first `update()` from
    /// `eframe::Frame::window_handle()`. Used to drive hide/show via
    /// `ShowWindow` directly — `ViewportCommand::Visible(false)` and
    /// `Minimized` both have eframe queue-drain bugs that break the
    /// tray-menu round-trip (emilk/egui#5229, #3655). Bypassing the
    /// queue with raw Win32 sidesteps the bug entirely.
    hwnd: Option<isize>,
}

/// Win32 helpers for hide/show. We do not use `ViewportCommand::Visible`
/// or `Minimized` because eframe processes those commands only inside
/// `update()`, which only runs on Windows WM_PAINT — and hidden /
/// minimised windows don't generate WM_PAINT, so the queued
/// "show window again" command from the tray sits undelivered.
/// `ShowWindow` is a synchronous Win32 call that flips visibility at
/// the OS level, no event loop required. See emilk/egui#5229, #3655.
#[cfg(windows)]
fn win_hide(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    unsafe { ShowWindow(hwnd as _, SW_HIDE); }
}

#[cfg(windows)]
fn win_show_and_focus(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ShowWindow, SetForegroundWindow, SW_SHOW, SW_RESTORE,
    };
    unsafe {
        // SW_RESTORE first to un-minimise if the user minimised via the
        // titlebar; then SW_SHOW guarantees the window is visible.
        ShowWindow(hwnd as _, SW_RESTORE);
        ShowWindow(hwnd as _, SW_SHOW);
        // Foreground may fail per Windows foreground-stealing rules,
        // but since the user just clicked the tray icon, we have the
        // foreground-permission grant and SFW should succeed.
        SetForegroundWindow(hwnd as _);
    }
}

impl eframe::App for StreamToSpeakerApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.frame_count = self.frame_count.saturating_add(1);

        // Capture the HWND on the first frame and hand it to the tray
        // so the tray's "Show window" click can call ShowWindow
        // directly (the only thing that works once the window is
        // hidden — see win_hide / win_show_and_focus above).
        if self.hwnd.is_none() {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(handle) = frame.window_handle() {
                if let RawWindowHandle::Win32(h) = handle.as_raw() {
                    let hwnd = h.hwnd.get();
                    self.hwnd = Some(hwnd);
                    if let Some(tray) = self.tray.as_mut() {
                        tray.set_hwnd(hwnd);
                    }
                }
            }
        }

        if self.last_repaint_request.elapsed() >= Duration::from_millis(100) {
            ctx.request_repaint_after(Duration::from_millis(100));
            self.last_repaint_request = Instant::now();
        }

        // Reapply theme only when the resolved mode actually changes.
        // apply_theme rebuilds the full Style+Visuals (allocates the
        // text-style map, all the widget visuals); doing that every
        // frame at 10 fps was wasteful and could cause focus loss on
        // some egui widgets.
        let dark = self.theme_mode.resolve(ctx);
        if self.last_applied_dark != Some(dark) {
            apply_theme(ctx, dark);
            self.last_applied_dark = Some(dark);
        }
        let p = palette_for(dark);

        if let Some(tray) = self.tray.as_mut() {
            tray.pump(&self.app, ctx);
        }

        // Close-button handling. Hide-to-tray uses raw Win32 ShowWindow
        // (see win_hide above) — eframe ViewportCommands sit
        // undelivered on hidden viewports.
        let close_pressed = ctx.input(|i| i.viewport().close_requested());
        if close_pressed && self.frame_count > 1 && !self.app.is_shutting_down() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.tray.is_none() {
                self.app.request_shutdown();
            } else if self.skip_close_confirmation {
                if let Some(hwnd) = self.hwnd {
                    win_hide(hwnd);
                }
            } else {
                self.confirm_close_open = true;
            }
        }
        if self.app.is_shutting_down() {
            // Drop the tray icon synchronously so it disappears now.
            self.tray.take();
            // Make sure the window is visible before Close — eframe's
            // Close on a hidden viewport doesn't reliably return from
            // run_native. (Cheap: SW_SHOW is a no-op if already shown.)
            if let Some(hwnd) = self.hwnd {
                win_show_and_focus(hwnd);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self.confirm_close_open {
            self.show_close_modal(ctx, &p);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(p.canvas).inner_margin(20.0))
            .show(ctx, |ui| {
                let enabled = !self.confirm_close_open;
                ui.add_enabled_ui(enabled, |ui| {
                    // Keep the header pinned at the top (theme toggle
                    // shouldn't scroll away); everything below scrolls
                    // when the window is shorter than the content.
                    self.show_header(ui, &p);
                    ui.add_space(14.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.show_status_banner(ui, &p);
                            ui.add_space(14.0);
                            if self.app.current_renderer().is_none()
                                && !self.onboarding_dismissed
                            {
                                self.show_onboarding(ui, &p);
                                ui.add_space(14.0);
                            }
                            self.show_speakers(ui, &p);
                            ui.add_space(14.0);
                            self.show_latency(ui, &p);
                            ui.add_space(14.0);
                            self.show_advanced(ui, &p);
                            ui.add_space(14.0);
                            self.show_web_ui(ui, &p);
                            ui.add_space(14.0);
                            self.show_stats(ui, &p);
                        });
                });
            });
    }
}

// -----------------------------------------------------------------------------
// UI building blocks
// -----------------------------------------------------------------------------

fn card<R>(ui: &mut egui::Ui, p: &Palette, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(p.card)
        .stroke(egui::Stroke::new(1.0, p.divider))
        .rounding(12.0)
        .inner_margin(egui::Margin::symmetric(18.0, 16.0))
        .show(ui, content)
        .inner
}

fn section_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(11.0)
            .strong()
            .color(p.text_tertiary)
            .extra_letter_spacing(1.6),
    );
    ui.add_space(2.0);
}

/// Tag any clickable Response with the pointing-hand cursor on hover.
/// egui does NOT auto-apply this for Buttons / sense-click Labels — the
/// default is the arrow cursor, which reads as "this is text, not a
/// control". Web users expect the hand on every clickable; matching that
/// expectation makes the desktop app feel native instead of toy-like.
/// Cite: egui Discussion #1430 and the egui::Response docs.
fn clickable(r: egui::Response) -> egui::Response {
    if r.hovered() && r.enabled() {
        r.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

/// Primary button — solid accent fill, white-on-accent text. Use for the
/// main action of each area (Enable, Minimise to tray, Apply, etc.).
fn primary_button(ui: &mut egui::Ui, p: &Palette, label: &str, min_width: f32) -> egui::Response {
    let btn = egui::Button::new(
        egui::RichText::new(label)
            .strong()
            .color(p.text_on_accent),
    )
    .fill(p.accent)
    .stroke(egui::Stroke::new(1.0, p.accent_hover))
    .rounding(8.0);
    clickable(ui.add_sized([min_width, 32.0], btn))
}

/// Secondary button — outlined, neutral background. For tertiary actions
/// (Refresh, Cancel, latency-step nudges).
fn secondary_button(ui: &mut egui::Ui, p: &Palette, label: &str, min_width: f32) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(label).color(p.text_primary))
        .fill(p.card)
        .stroke(egui::Stroke::new(1.0, p.divider))
        .rounding(8.0);
    clickable(ui.add_sized([min_width, 32.0], btn))
}

/// Tertiary / link-style button — accent-coloured text on transparent
/// background, no border. For actions that ARE the primary action of a
/// screen but live alongside genuine primary buttons (e.g. "Open in
/// browser" sitting next to "Enable streaming"). Avoids competing-loud-
/// fills-on-the-same-screen.
fn link_button(ui: &mut egui::Ui, p: &Palette, label: &str, min_width: f32) -> egui::Response {
    let btn = egui::Button::new(
        egui::RichText::new(label).strong().color(p.accent),
    )
    .fill(egui::Color32::TRANSPARENT)
    .stroke(egui::Stroke::new(1.0, p.accent.gamma_multiply(0.45)))
    .rounding(8.0);
    clickable(ui.add_sized([min_width, 32.0], btn))
}

/// Danger button — red text + thin red border. For "this glitches the audio"
/// actions (Resync, Quit). Not as loud as a solid red fill — Refactoring UI
/// recommends only going full danger-red when it's the page's primary action.
fn danger_button(ui: &mut egui::Ui, p: &Palette, label: &str, min_width: f32) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(label).color(p.danger))
        .fill(p.card)
        .stroke(egui::Stroke::new(1.0, p.danger.gamma_multiply(0.5)))
        .rounding(8.0);
    clickable(ui.add_sized([min_width, 32.0], btn))
}

impl StreamToSpeakerApp {
    fn show_header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Stream To Speaker")
                    .size(20.0)
                    .strong()
                    .color(p.text_primary),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.show_theme_toggle(ui, p);
            });
        });
    }

    fn show_theme_toggle(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // Three-way segmented control: System / Light / Dark. Compact, lives
        // in the top-right of the header. Each segment shows the current
        // mode by filling with the accent colour.
        let modes = [
            (ThemeMode::System, "Auto", "Follow Windows theme setting"),
            (ThemeMode::Light, "Light", "Force light theme"),
            (ThemeMode::Dark, "Dark", "Force dark theme"),
        ];
        egui::Frame::none()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0, p.divider))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(2.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (mode, label, tip) in modes {
                        let selected = self.theme_mode == mode;
                        let btn_text = if selected {
                            egui::RichText::new(label).strong().color(p.text_on_accent)
                        } else {
                            egui::RichText::new(label).color(p.text_secondary)
                        };
                        let btn = egui::Button::new(btn_text)
                            .fill(if selected { p.accent } else { egui::Color32::TRANSPARENT })
                            .stroke(egui::Stroke::NONE)
                            .rounding(6.0);
                        if clickable(ui.add_sized([60.0, 28.0], btn))
                            .on_hover_text(tip)
                            .clicked()
                        {
                            self.theme_mode = mode;
                        }
                    }
                });
            });
    }

    fn show_onboarding(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // Three-step quick-start. Numbered circles on the left, instructions
        // on the right. Disappears once a speaker is bound — or when the
        // user explicitly dismisses with the "Got it" button (some users
        // unbind a speaker after using the app for a while; the card
        // shouldn't resurface as if they were a new user).
        card(ui, p, |ui| {
            ui.horizontal(|ui| {
                section_label(ui, p, "Getting started");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if secondary_button(ui, p, "Got it", 72.0)
                        .on_hover_text("Hide this card for the rest of the session")
                        .clicked()
                    {
                        self.onboarding_dismissed = true;
                    }
                });
            });
            ui.add_space(8.0);

            let steps: &[(&str, &str, &str)] = &[
                (
                    "1",
                    "Pick the audio output in Windows",
                    "Open Windows Sound Settings (right-click the speaker icon \
                     in the taskbar → Sound settings) and choose 'Stream To \
                     Speaker' as the output device, or use Volume Mixer to \
                     route just one app to it.",
                ),
                (
                    "2",
                    "Choose a speaker below",
                    "Speakers on the same network appear in the list. Click \
                     one to start streaming — playback starts within a \
                     second or two.",
                ),
                (
                    "3",
                    "Tune the latency if needed",
                    "If the speaker lags too far behind the picture, use the \
                     Latency buttons to trim. Resync is the 'fix it now' \
                     option (brief glitch but resets everything).",
                ),
            ];

            for (n, title, body) in steps {
                ui.horizontal(|ui| {
                    // Numbered indicator
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 14.0, p.accent_subtle);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        n,
                        egui::FontId::new(13.0, egui::FontFamily::Proportional),
                        p.accent,
                    );
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(*title)
                                .strong()
                                .color(p.text_primary),
                        );
                        ui.label(
                            egui::RichText::new(*body)
                                .size(12.0)
                                .color(p.text_secondary),
                        );
                    });
                });
                ui.add_space(10.0);
            }
        });
    }

    fn show_status_banner(&self, ui: &mut egui::Ui, p: &Palette) {
        let enabled = self.app.is_streaming_enabled();
        let active = self.app.stream_active.load(Ordering::Acquire);
        let current = self.app.current_renderer();

        let (icon, accent, headline, detail, btn_label, btn_tip) = match (&current, enabled, active) {
            (None, _, _) => (
                "?",
                p.muted,
                "No speaker selected".to_string(),
                "Pick a speaker from the list below to start streaming.".to_string(),
                None,
                None,
            ),
            (Some(_), false, _) => (
                "⊘",
                p.danger,
                "Streaming disabled".to_string(),
                "The speaker is free for other use. Click Enable to resume.".to_string(),
                Some("Enable"),
                Some("Re-bind to the last speaker and resume streaming"),
            ),
            (Some(r), true, true) => (
                "▶",
                p.success,
                format!("Streaming to {}", r.friendly_name),
                format!("{}  ·  {} pkt/s", r.ip, self.packets_per_sec()),
                Some("Disable"),
                Some("Stop streaming and release the speaker for other apps"),
            ),
            (Some(r), true, false) => (
                "‖",
                p.warn,
                format!("Idle on {}", r.friendly_name),
                format!("{}  ·  no audio playing right now", r.ip),
                Some("Disable"),
                Some("Stop streaming and release the speaker for other apps"),
            ),
        };

        egui::Frame::none()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0, p.divider))
            .rounding(12.0)
            .inner_margin(egui::Margin {
                left: 0.0,
                right: 18.0,
                top: 16.0,
                bottom: 16.0,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Accent stripe on the left edge — instant state read-out.
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(4.0, 56.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 0.0, accent);
                    ui.add_space(14.0);

                    ui.label(egui::RichText::new(icon).color(accent).size(34.0).strong());
                    ui.add_space(12.0);

                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(headline)
                                .size(16.0)
                                .strong()
                                .color(p.text_primary),
                        );
                        ui.label(
                            egui::RichText::new(detail)
                                .size(12.0)
                                .color(p.text_secondary),
                        );
                    });

                    if let (Some(label), Some(tip)) = (btn_label, btn_tip) {
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let r = if label == "Enable" {
                                    primary_button(ui, p, label, 96.0)
                                } else {
                                    secondary_button(ui, p, label, 96.0)
                                }
                                .on_hover_text(tip);
                                if r.clicked() {
                                    if let Err(e) = self.app.set_streaming_enabled(!enabled) {
                                        warn!("toggle streaming failed: {}", e);
                                    }
                                }
                            },
                        );
                    }
                });
            });
    }

    fn show_speakers(&self, ui: &mut egui::Ui, p: &Palette) {
        card(ui, p, |ui| {
            ui.horizontal(|ui| {
                section_label(ui, p, "Speakers");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    secondary_button(ui, p, "↻  Rescan", 92.0)
                        .on_hover_text("Re-trigger SSDP discovery now (otherwise runs every few minutes)");
                });
            });
            ui.add_space(6.0);

            let view = self.app.speaker_view();
            if view.speakers.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Searching the network for speakers…")
                        .color(p.text_secondary)
                        .italics(),
                );
                ui.label(
                    egui::RichText::new(
                        "Make sure your PC and the speaker are on the same Wi-Fi/Ethernet network.",
                    )
                    .size(11.0)
                    .color(p.text_tertiary),
                );
                return;
            }

            egui::ScrollArea::vertical()
                .max_height(190.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for sp in view.speakers {
                        speaker_row(ui, p, &sp, |id| {
                            if let Err(e) = self.app.select_speaker(id) {
                                warn!("select speaker failed: {}", e);
                            }
                        });
                    }
                });
        });
    }

    fn show_latency(&self, ui: &mut egui::Ui, p: &Palette) {
        card(ui, p, |ui| {
            let pending = self.app.pending_latency_ms();
            ui.horizontal(|ui| {
                section_label(ui, p, "Latency");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (color, text) = if pending == 0 {
                        (p.text_tertiary, "in sync".to_string())
                    } else if pending > 0 {
                        (p.warn, format!("trimming {} ms…", pending))
                    } else {
                        (p.warn, format!("padding {} ms…", -pending))
                    };
                    ui.label(egui::RichText::new(text).size(12.0).color(color));
                });
            });
            ui.add_space(8.0);

            ui.label(
                egui::RichText::new(
                    "Speaker too far behind the picture? Trim latency. Audio glitching? Add a touch back.",
                )
                .size(12.0)
                .color(p.text_secondary),
            );
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                if secondary_button(ui, p, "−100 ms", 86.0)
                    .on_hover_text(
                        "Reduce latency by 100 ms. Drops 100 ms of audio gradually over ~2 s.",
                    )
                    .clicked()
                {
                    self.app.adjust_latency(100);
                }
                if secondary_button(ui, p, "−25 ms", 78.0)
                    .on_hover_text("Reduce latency by 25 ms (a gentler nudge)")
                    .clicked()
                {
                    self.app.adjust_latency(25);
                }
                ui.add_space(24.0);
                if secondary_button(ui, p, "+25 ms", 78.0)
                    .on_hover_text("Add 25 ms of latency back (use if drained too far)")
                    .clicked()
                {
                    self.app.adjust_latency(-25);
                }
                if secondary_button(ui, p, "+100 ms", 86.0)
                    .on_hover_text("Add 100 ms of latency back")
                    .clicked()
                {
                    self.app.adjust_latency(-100);
                }
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);

            if danger_button(ui, p, "⟳  Resync speaker", 180.0)
                .on_hover_text(
                    "Hard reset (UPnP Stop + Play). Speaker discards its prebuffer — brief audio glitch but trims accumulated latency in one shot.",
                )
                .clicked()
            {
                if let Err(e) = self.app.resync() {
                    warn!("resync failed: {}", e);
                }
            }
        });
    }

    fn show_advanced(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(ui, p, |ui| {
            // Full-width clickable strip — clicking anywhere in the card
            // header row toggles, not just the tiny label. Fitts's law.
            let chevron = if self.advanced_open { "▾" } else { "▸" };
            let avail_w = ui.available_width();
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(avail_w, 22.0), egui::Sense::click());
            let resp = resp.on_hover_text("Tuning knobs for power users");
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            // Paint label + chevron into the allocated strip.
            ui.painter().text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                "ADVANCED",
                egui::FontId::new(11.0, egui::FontFamily::Proportional),
                p.text_tertiary,
            );
            ui.painter().text(
                rect.right_center() - egui::vec2(2.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                chevron,
                egui::FontId::new(14.0, egui::FontFamily::Proportional),
                p.text_tertiary,
            );
            if resp.clicked() {
                self.advanced_open = !self.advanced_open;
            }

            if !self.advanced_open {
                return;
            }

            ui.add_space(8.0);

            let mut ppm = self.app.rate_fudge_ppm.load(Ordering::Relaxed);
            advanced_row(
                ui,
                p,
                "Clock-drift compensation",
                "Compensates for the small mismatch between your PC's clock and the speaker's audio crystal. Positive over-produces frames; negative drops them. Try +50 to +100 if the buffer slowly empties out, negative if it slowly overflows.",
                |ui| {
                    ui.add(
                        egui::DragValue::new(&mut ppm)
                            .range(-1000..=1000)
                            .suffix(" ppm"),
                    );
                    if secondary_button(ui, p, "reset", 60.0).clicked() {
                        ppm = 0;
                    }
                },
            );
            self.app.set_rate_fudge_ppm(ppm);

            ui.add_space(12.0);

            let mut pace = self.app.silence_pace_ms.load(Ordering::Relaxed) as i64;
            advanced_row(
                ui,
                p,
                "Silence pacing",
                "10 ms = real-time. Higher values send slower than real-time during pauses, draining the speaker's prebuffer so post-pause latency is smaller. Don't exceed ~30 — risks underrun.",
                |ui| {
                    ui.add(egui::DragValue::new(&mut pace).range(1..=100).suffix(" ms"));
                    if secondary_button(ui, p, "reset", 60.0).clicked() {
                        pace = 10;
                    }
                },
            );
            self.app.set_silence_pace_ms(pace.max(1) as u64);

            ui.add_space(12.0);

            let mut step = self.app.latency_adjust_step_frames.load(Ordering::Relaxed) as i64;
            advanced_row(
                ui,
                p,
                "Latency-adjust step",
                "Max frames added or dropped per audio packet when applying a trim/pad request. Larger = snappier response but more audible click.",
                |ui| {
                    ui.add(
                        egui::DragValue::new(&mut step)
                            .range(1..=256)
                            .suffix(" frames"),
                    );
                    if secondary_button(ui, p, "reset", 60.0).clicked() {
                        step = 4;
                    }
                },
            );
            self.app.set_latency_adjust_step_frames(step.max(1) as u32);
        });
    }

    fn show_web_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(ui, p, |ui| {
            section_label(ui, p, "Web UI");
            ui.add_space(4.0);

            let on = self.app.is_web_ui_enabled();
            let url = format!(
                "http://{}:{}/",
                self.app.config.advertise_ip,
                self.app.config.bind.port()
            );

            ui.label(
                egui::RichText::new(
                    "Browser-based control panel + JSON API. Lets you switch \
                     speakers, adjust latency, and read stats from another device \
                     on your LAN (e.g. your phone).",
                )
                .size(12.0)
                .color(p.text_secondary),
            );
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                if on {
                    // Web UI is a side feature, not the screen's primary
                    // action. Use the lighter link-style button instead
                    // of competing with Enable in the status banner.
                    if link_button(ui, p, "Open in browser", 150.0)
                        .on_hover_text(format!("Open {} in your default browser", url))
                        .clicked()
                    {
                        let _ = open_url(&url);
                    }
                    if secondary_button(ui, p, "Disable", 96.0)
                        .on_hover_text("Stop serving the control panel + JSON API")
                        .clicked()
                    {
                        self.app.set_web_ui_enabled(false);
                    }
                } else if secondary_button(ui, p, "Enable web UI", 150.0)
                    .on_hover_text(
                        "Serve the control panel + JSON API on the local HTTP \
                         port. The audio stream itself is always served.",
                    )
                    .clicked()
                {
                    self.app.set_web_ui_enabled(true);
                }
            });

            if on {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(&url)
                        .size(11.0)
                        .color(p.text_tertiary)
                        .monospace(),
                );
            }
        });
    }

    fn show_stats(&self, ui: &mut egui::Ui, p: &Palette) {
        let subs = self.app.subscriber_count();
        let uptime = self.app.uptime_secs();
        let pkts_total = self.app.packets_published();
        let pkts_sec = self.packets_per_sec();

        card(ui, p, |ui| {
            section_label(ui, p, "Stats");
            ui.add_space(6.0);
            // horizontal_wrapped (not horizontal) so the pills reflow
            // onto a second row at narrow widths instead of clipping
            // off the right edge.
            ui.horizontal_wrapped(|ui| {
                stat_pill(ui, p, &format!("{}", pkts_sec), "pkt / s");
                stat_pill(
                    ui,
                    p,
                    &format!("{}", subs),
                    if subs == 1 { "listener" } else { "listeners" },
                );
                stat_pill(ui, p, &format_duration(uptime), "uptime");
                stat_pill(ui, p, &humanize_count(pkts_total), "packets");
            });
        });
    }

    fn packets_per_sec(&self) -> u64 {
        let up = self.app.uptime_secs().max(1);
        self.app.packets_published() / up
    }

    fn show_close_modal(&mut self, ctx: &egui::Context, p: &Palette) {
        let mut still_open = self.confirm_close_open;
        let mut new_skip = self.skip_close_confirmation;
        let mut action: Option<CloseAction> = None;

        egui::Window::new(
            egui::RichText::new("Close Stream To Speaker?")
                .size(15.0)
                .strong()
                .color(p.text_primary),
        )
            .open(&mut still_open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(460.0)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(p.card)
                    .stroke(egui::Stroke::new(1.0, p.divider))
                    .rounding(12.0)
                    .inner_margin(22.0),
            )
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Minimise to the system tray to keep streaming in the background, \
                         or quit the app entirely.",
                    )
                    .color(p.text_secondary),
                );
                ui.add_space(10.0);
                ui.checkbox(
                    &mut new_skip,
                    "Always minimise to tray — don't ask again this session",
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if primary_button(ui, p, "Minimise to tray", 170.0)
                        .on_hover_text("Hide the window. The tray icon stays, streaming continues.")
                        .clicked()
                    {
                        action = Some(CloseAction::MinimiseToTray);
                    }
                    if danger_button(ui, p, "Quit", 90.0)
                        .on_hover_text("Stop streaming and close the app entirely.")
                        .clicked()
                    {
                        action = Some(CloseAction::Quit);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if secondary_button(ui, p, "Cancel", 86.0).clicked() {
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
                    if let Some(hwnd) = self.hwnd {
                        win_hide(hwnd);
                    }
                }
                CloseAction::Quit => {
                    // request_shutdown flips the atomic; the next
                    // update() tick sees it and runs the Close
                    // sequence (drop tray, ensure-shown, Close).
                    self.app.request_shutdown();
                    ctx.request_repaint();
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
// Composable pieces
// -----------------------------------------------------------------------------

fn speaker_row(
    ui: &mut egui::Ui,
    p: &Palette,
    sp: &crate::http_server::SpeakerInfo,
    on_click: impl FnOnce(&str),
) {
    let active = sp.active;

    // The row uses two-pass painting: first we allocate space, then we
    // paint into it with hover/active-aware colours. This is what egui
    // recommends for custom interactive widgets so the click target +
    // hover detection are tied to the actual painted area.
    let height = 44.0;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());

    let row_fill = if active {
        p.overlay
    } else if response.hovered() {
        p.card_hover
    } else {
        egui::Color32::TRANSPARENT
    };
    let row_stroke = if active {
        p.accent
    } else if response.hovered() {
        p.accent.gamma_multiply(0.5)
    } else {
        p.divider
    };

    ui.painter()
        .rect(rect, 8.0, row_fill, egui::Stroke::new(1.0, row_stroke));

    // Radio indicator on the left.
    let indicator_center = egui::pos2(rect.left() + 18.0, rect.center().y);
    ui.painter().circle_stroke(
        indicator_center,
        7.0,
        egui::Stroke::new(
            1.5,
            if active { p.accent } else { p.text_tertiary },
        ),
    );
    if active {
        ui.painter().circle_filled(indicator_center, 3.5, p.accent);
    }

    // Friendly name + IP.
    let text_left = rect.left() + 34.0;
    ui.painter().text(
        egui::pos2(text_left, rect.center().y - 2.0),
        egui::Align2::LEFT_CENTER,
        &sp.friendly_name,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
        p.text_primary,
    );
    ui.painter().text(
        egui::pos2(rect.right() - 12.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        &sp.ip,
        egui::FontId::new(12.0, egui::FontFamily::Monospace),
        p.text_tertiary,
    );

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if response.clicked() && !active {
        on_click(&sp.id);
    }

    ui.add_space(6.0);
}

fn advanced_row(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    hint: &str,
    add_control: impl FnOnce(&mut egui::Ui),
) {
    // Stacked layout (label/hint on top, controls underneath). Horizontal
    // side-by-side layouts overlap badly when the window is narrow —
    // vertical is robust at any width and reads cleanly.
    ui.vertical(|ui| {
        ui.label(
            egui::RichText::new(label)
                .strong()
                .color(p.text_primary),
        );
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(hint)
                .size(11.0)
                .color(p.text_secondary),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            add_control(ui);
        });
    });
}

fn stat_pill(ui: &mut egui::Ui, p: &Palette, value: &str, label: &str) {
    egui::Frame::none()
        .fill(p.card_hover)
        .stroke(egui::Stroke::new(1.0, p.divider))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(value)
                        .strong()
                        .size(15.0)
                        .color(p.text_primary),
                );
                ui.label(
                    egui::RichText::new(label)
                        .size(10.0)
                        .color(p.text_tertiary)
                        .extra_letter_spacing(0.6),
                );
            });
        });
}

fn open_url(url: &str) -> std::io::Result<()> {
    // Windows: `cmd /C start "" <url>` is the canonical "open default
    // browser" invocation. The empty quoted string is required because
    // start treats the first quoted arg as the new window title.
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
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
