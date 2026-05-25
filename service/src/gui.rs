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
// Design tokens
// -----------------------------------------------------------------------------
//
// Centralized spacing + corner-radius scales. Replaces the scatter of
// hard-coded `5.0`, `7.0`, `13.0`, `14.0`, `18.0`, `22.0` literals that
// accumulated as the GUI grew — those produced no consistent rhythm,
// failed Fluent's 4 epx grid, and were the most-flagged issue in the
// recent UX audit. Every layout call in this file SHOULD read from
// these constants.
//
// Spacing scale follows Fluent 2's tokens (4-epx grid):
//   XS=8  S=12  M=16  L=24  XL=32
// (4 itself is reserved for micro-adjustments and isn't surfaced as a
// named constant — if you reach for 4, ask whether you actually want
// 8 first.)
//
// Corner radii are the two Fluent values:
//   RADIUS_CONTROL=4  for in-page controls (buttons, list rows, pills)
//   RADIUS_SURFACE=8  for overlays (cards, modal, app window, menus)
#[allow(dead_code)] // sp::L is not used today; kept for upcoming spacing sweep.
mod sp {
    pub const XS: f32 = 8.0;
    pub const S: f32 = 12.0;
    pub const M: f32 = 16.0;
    pub const L: f32 = 24.0;
}
const RADIUS_CONTROL: f32 = 4.0;
const RADIUS_SURFACE: f32 = 8.0;
/// Standard interactive control height. One value across primary /
/// secondary / link / danger / segmented buttons so the rhythm doesn't
/// drift between adjacent rows.
const CONTROL_HEIGHT: f32 = 32.0;

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

/// Read the Windows accent colour from
/// `HKCU\\SOFTWARE\\Microsoft\\Windows\\DWM\\AccentColor` (DWORD,
/// stored 0xAABBGGRR — the alpha+endian convention DWM uses).
/// Returns the accent as `(R, G, B)` so the caller can derive the
/// hover / subtle variants. None if the key is missing.
#[cfg(windows)]
fn read_system_accent() -> Option<(u8, u8, u8)> {
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW, REG_DWORD,
    };
    let path: Vec<u16> = "SOFTWARE\\Microsoft\\Windows\\DWM\0"
        .encode_utf16()
        .collect();
    let mut hkey: HKEY = std::ptr::null_mut();
    let r = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut hkey)
    };
    if r != 0 {
        return None;
    }
    let val_name: Vec<u16> = "AccentColor\0".encode_utf16().collect();
    let mut data: u32 = 0;
    let mut size: u32 = 4;
    let mut ty: u32 = 0;
    let r = unsafe {
        RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            &mut data as *mut u32 as *mut u8,
            &mut size,
        )
    };
    unsafe { RegCloseKey(hkey) };
    if r != 0 || ty != REG_DWORD {
        return None;
    }
    // 0xAABBGGRR — extract R, G, B.
    let r = (data & 0xFF) as u8;
    let g = ((data >> 8) & 0xFF) as u8;
    let b = ((data >> 16) & 0xFF) as u8;
    Some((r, g, b))
}

#[cfg(not(windows))]
fn read_system_accent() -> Option<(u8, u8, u8)> {
    None
}

/// True if Windows' High Contrast mode is on (Accessibility → Contrast
/// themes, or the WIN+ALT+PRINTSCREEN toggle). When this is on, an
/// app that overrides Visuals defeats the user's accessibility setup
/// — better to drop our custom palette and let egui's defaults
/// through, which at least track the dark/light flip.
#[cfg(windows)]
fn is_high_contrast_on() -> bool {
    use windows_sys::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETHIGHCONTRAST,
    };
    let mut hc: HIGHCONTRASTW = unsafe { std::mem::zeroed() };
    hc.cbSize = std::mem::size_of::<HIGHCONTRASTW>() as u32;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            std::mem::size_of::<HIGHCONTRASTW>() as u32,
            &mut hc as *mut HIGHCONTRASTW as *mut std::ffi::c_void,
            0,
        )
    };
    if ok == 0 {
        return false;
    }
    (hc.dwFlags & HCF_HIGHCONTRASTON) != 0
}

#[cfg(not(windows))]
fn is_high_contrast_on() -> bool {
    false
}

/// Enable Win11's Mica window backdrop. The window's titlebar +
/// background tint follow the desktop, giving the app a native
/// Win11 look instead of a flat opaque rectangle.
///
/// Returns true if the OS accepted the call (Win11 build 22000+);
/// false on Win10 / older Win11 (DwmSetWindowAttribute rejects the
/// DWMWA_SYSTEMBACKDROP_TYPE attribute).
///
/// NOTE: Mica is only visible if the window background is at least
/// partially transparent. Right now we paint the canvas opaque, so
/// Mica is enabled but covered. A follow-up commit needs to add
/// `ViewportBuilder::with_transparent(true)` + an `App::clear_color`
/// override returning `[0,0,0,0]` to actually expose the tint.
#[cfg(windows)]
fn try_enable_mica(hwnd: isize) -> bool {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMSBT_MAINWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
    };
    let value: i32 = DWMSBT_MAINWINDOW as i32;
    let hr = unsafe {
        DwmSetWindowAttribute(
            hwnd as _,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            &value as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        )
    };
    hr == 0 // S_OK
}

#[cfg(not(windows))]
fn try_enable_mica(_hwnd: isize) -> bool {
    false
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
            // Bumped from #8a93a3 (which gave ~3:1 on white — fails
            // WCAG 1.4.3 AA for body text). #6b7488 measures ~4.7:1
            // against the light card / canvas, passing the 4.5:1 bar.
            text_tertiary: egui::Color32::from_rgb(0x6b, 0x74, 0x88),
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
            // Bumped from #6e7788 (which gave ~3.7:1 on the dark card
            // — fails WCAG 1.4.3 AA). #90a0b8 measures ~5.2:1 against
            // the dark card.
            text_tertiary: egui::Color32::from_rgb(0x90, 0xa0, 0xb8),
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

/// Append Segoe UI Symbol as a fallback for the Proportional and
/// Monospace font families. egui resolves glyphs by walking each
/// family's font list in order; the default fonts (Ubuntu Light /
/// Hack) cover ASCII + Latin but not the geometric-shape, arrow,
/// chevron, and box-drawing characters used in the UI. Without
/// this fallback those glyphs render as `□`.
///
/// Loads from `%WINDIR%\Fonts\seguisym.ttf` at runtime — present
/// on every Win7+ install, so no binary-size hit. If the file is
/// missing (Windows N edition with language packs stripped) we
/// log and continue; the glyphs keep showing as squares, but
/// nothing crashes.
/// Set up the font stack the app uses. egui's bundled defaults are
/// Ubuntu Light (proportional) and Hack (mono) — neither is what
/// Windows users expect to see in a Windows-native app, and the
/// "strong" weight rendered as synthetic-bold instead of the proper
/// Semibold cut.
///
/// Installs (in priority order, per family):
/// 1. Segoe UI Regular — the canonical Windows 10/11 UI font
/// 2. Segoe UI Semibold — proper weight when `.strong()` is used
/// 3. Segoe UI Symbol — geometric shapes / arrows / chevrons
///    (falls through for glyphs the first two don't cover)
///
/// All three load from `%WINDIR%\Fonts\*.ttf` at runtime — present
/// on every Win7+ install, so no binary-size hit. Each load is
/// independent: missing files are warned and skipped. CJK
/// fallback isn't done here (Yu Gothic / Microsoft YaHei /
/// Malgun Gothic ship as TrueType collections, .ttc, which need
/// a separate parser to extract the right face) — see Audit M1.
fn install_symbol_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // Prepend (not append) so Segoe UI wins over the bundled Ubuntu
    // Light for Latin text. Symbol stays at the END as a fallback
    // for glyphs Segoe UI doesn't cover (arrows, geometric shapes).
    let fonts_dir = std::path::PathBuf::from(
        std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_string()),
    )
    .join("Fonts");

    if let Some(bytes) = read_font(&fonts_dir.join("segoeui.ttf")) {
        fonts.font_data.insert(
            "segoe_ui".to_owned(),
            egui::FontData::from_owned(bytes),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe_ui".to_owned());
    }
    if let Some(bytes) = read_font(&fonts_dir.join("seguisb.ttf")) {
        fonts.font_data.insert(
            "segoe_ui_semibold".to_owned(),
            egui::FontData::from_owned(bytes),
        );
        // egui doesn't have separate Regular / Semibold families; the
        // best we can do without a custom FontFamily::Name is to keep
        // Semibold available as a fallback (so a glyph absent from
        // Regular pulls from Semibold first, before Symbol). For an
        // explicit Semibold path we'd register a named family — TODO.
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(1, "segoe_ui_semibold".to_owned());
    }
    if let Some(bytes) = read_font(&fonts_dir.join("seguisym.ttf")) {
        fonts.font_data.insert(
            "segoe_ui_symbol".to_owned(),
            egui::FontData::from_owned(bytes),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("segoe_ui_symbol".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("segoe_ui_symbol".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn read_font(path: &std::path::Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(b) => Some(b),
        Err(e) => {
            warn!("font {} not loaded ({}); using fallback", path.display(), e);
            None
        }
    }
}

fn apply_theme(ctx: &egui::Context, dark: bool, system_accent: Option<(u8, u8, u8)>) {
    // High-contrast OS mode: don't override visuals at all. The user
    // has explicitly opted into a system colour scheme designed for
    // their accessibility needs; clobbering it with our hand-picked
    // palette defeats the point. Drop back to egui's stock light/dark
    // visuals and the system text colours. Proper HC integration
    // (reading GetSysColor for window/button/highlight) is a deeper
    // follow-up; this fallback at least stops us from making things
    // WORSE.
    if is_high_contrast_on() {
        let visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        ctx.set_visuals(visuals);
        return;
    }
    let p = palette_for(dark, system_accent);
    let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };

    visuals.panel_fill = p.canvas;
    visuals.window_fill = p.card;
    visuals.window_stroke = egui::Stroke::new(1.0, p.divider);
    visuals.faint_bg_color = p.card;
    visuals.extreme_bg_color = p.canvas;
    visuals.code_bg_color = p.card_hover;
    // Don't set override_text_color — it forces every label to paint
    // at primary regardless of `ui.is_enabled()`, defeating egui's
    // built-in disabled-state fade. Individual labels that want the
    // primary color set it via RichText::color(p.text_primary)
    // explicitly. (Audit M21.)
    visuals.widgets.noninteractive.fg_stroke =
        egui::Stroke::new(1.0, p.text_primary);
    visuals.hyperlink_color = p.accent;
    visuals.selection.bg_fill = p.accent_subtle;
    visuals.selection.stroke = egui::Stroke::new(1.0, p.accent);

    // Non-interactive (labels, separators)
    visuals.widgets.noninteractive.bg_fill = p.card;
    visuals.widgets.noninteractive.weak_bg_fill = p.card;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.divider);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.text_secondary);
    visuals.widgets.noninteractive.rounding = RADIUS_CONTROL.into();

    // Inactive (button at rest)
    visuals.widgets.inactive.bg_fill = p.card;
    visuals.widgets.inactive.weak_bg_fill = p.card;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, p.divider);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, p.text_primary);
    visuals.widgets.inactive.rounding = RADIUS_CONTROL.into();
    visuals.widgets.inactive.expansion = 0.0;

    // Hovered — subtle bg lift, accent border. Communicates "click me".
    visuals.widgets.hovered.bg_fill = p.card_hover;
    visuals.widgets.hovered.weak_bg_fill = p.card_hover;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, p.accent);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, p.text_primary);
    visuals.widgets.hovered.rounding = RADIUS_CONTROL.into();
    visuals.widgets.hovered.expansion = 1.0;

    // Active — pressed, darker fill
    visuals.widgets.active.bg_fill = p.card_active;
    visuals.widgets.active.weak_bg_fill = p.card_active;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.5, p.accent);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, p.text_primary);
    visuals.widgets.active.rounding = RADIUS_CONTROL.into();
    visuals.widgets.active.expansion = 0.0;

    visuals.widgets.open.bg_fill = p.card_hover;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, p.accent);

    visuals.window_rounding = RADIUS_SURFACE.into();
    visuals.menu_rounding = RADIUS_SURFACE.into();

    // Focus ring. egui paints a 2-px stroke at `selection.stroke`
    // around any focused widget, OUTSET by `widgets.active.expansion`
    // (so 0 expansion ≈ stroke painted on the widget border itself —
    // invisible if the widget already has its own border). Bump
    // expansion to 2 epx and use a heavier accent stroke. WCAG
    // 2.4.7 / 2.4.13 require ≥ 2 px with ≥ 3:1 contrast against
    // adjacent colours.
    visuals.selection.stroke = egui::Stroke::new(2.0, p.accent);
    visuals.widgets.active.expansion = 2.0;
    // Also explicitly de-conflict hover vs focus visuals: previous
    // version of this code accidentally double-assigned
    // `widgets.hovered.bg_stroke` here, overwriting the 1.5-px
    // active stroke set earlier in the function. Keep hover as a
    // 1-px subtle accent and rely on `selection.stroke` for the
    // focused widget on top.
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
    // 8×8 sibling spacing matches Fluent's L3 default (8 epx between
    // peer controls). The previous 10×8 was off the 4-epx grid.
    style.spacing.item_spacing = egui::vec2(sp::XS, sp::XS);
    // 12-horizontal / 6-vertical padding gives ~32-epx-tall buttons
    // with 14-px Body text and matches the WinUI button proportions.
    style.spacing.button_padding = egui::vec2(sp::S, 6.0);
    style.spacing.window_margin = egui::Margin::same(sp::M);
    style.spacing.interact_size.y = CONTROL_HEIGHT;
    ctx.set_style(style);
}

fn palette_for(dark: bool, system_accent: Option<(u8, u8, u8)>) -> Palette {
    let mut p = if dark { Palette::dark() } else { Palette::light() };
    if let Some((r, g, b)) = system_accent {
        let accent = egui::Color32::from_rgb(r, g, b);
        p.accent = accent;
        // Hover: nudge towards higher contrast against the card.
        // Dark theme = brighten the accent; light theme = darken it.
        p.accent_hover = if dark {
            egui::Color32::from_rgb(
                r.saturating_add(30),
                g.saturating_add(30),
                b.saturating_add(30),
            )
        } else {
            egui::Color32::from_rgb(
                r.saturating_sub(30),
                g.saturating_sub(30),
                b.saturating_sub(30),
            )
        };
        // Subtle: heavy desaturation toward the card colour for use
        // as selection / highlight backgrounds.
        p.accent_subtle = accent.linear_multiply(0.20);
    }
    p
}

// -----------------------------------------------------------------------------
// Run
// -----------------------------------------------------------------------------

pub fn run(app: Arc<App>, show_tray: bool) -> Result<()> {
    // Note: we DON'T use ViewportBuilder::with_taskbar(false). winit
    // implements that by calling ITaskbarList::DeleteTab on the HWND,
    // which puts the window in a half-managed taskbar state where
    // subsequent SW_HIDE/SW_SHOW cycles don't reliably bring the
    // window back to the foreground. Keep the taskbar entry — when
    // the window is hidden via ShowWindow(SW_HIDE) the taskbar entry
    // disappears naturally, and on show it reappears. (Slack / Discord
    // / OBS tray apps follow this same pattern.)
    let viewport = egui::ViewportBuilder::default()
        .with_title("Stream To Speaker")
        .with_inner_size([720.0, 800.0])
        .with_min_inner_size([580.0, 620.0])
        .with_visible(true)
        .with_close_button(true);

    let options = eframe::NativeOptions {
        viewport,
        // Persist window size + position across launches (M28).
        // eframe writes to its own storage (default: roaming config),
        // independent of our user_config.json.
        persist_window: true,
        ..Default::default()
    };

    let app_for_eframe = app.clone();
    let res = eframe::run_native(
        "Stream To Speaker",
        options,
        Box::new(move |cc| {
            // Register Segoe UI Symbol as a fallback font for both
            // Proportional and Monospace families. egui's bundled
            // Ubuntu Light / Hack don't have the geometric-shape,
            // arrow and chevron glyphs we use (▶ ⊘ ‖ ↻ ▾ ▸ ⟳ →),
            // so without this they render as `□`. Loading the
            // system Segoe UI Symbol costs zero binary bytes
            // (it's on every Win7+ install) and only kicks in for
            // glyphs the default fonts can't draw.
            install_symbol_font(&cc.egui_ctx);

            // Initial theme — System (which falls back to Light if the OS
            // doesn't expose a preference).
            apply_theme(
                &cc.egui_ctx,
                ThemeMode::System.resolve(&cc.egui_ctx),
                read_system_accent(),
            );
            cc.egui_ctx.request_repaint_after(Duration::from_millis(100));

            // Capture the HWND now, in CreationContext, before any
            // update() runs. CreationContext::window_handle() is
            // populated by eframe at construction (epi_integration.rs
            // initializes Frame's raw_window_handle from the same
            // root viewport winit Window we'll later show/hide); doing
            // it here avoids the "first-frame might return Err" edge
            // case of the previous in-update() capture.
            let hwnd: Option<isize> = {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                cc.window_handle().ok().and_then(|h| match h.as_raw() {
                    RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
                    _ => None,
                })
            };

            let mut tray = if show_tray {
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
            if let Some(h) = hwnd {
                if let Some(t) = tray.as_mut() {
                    t.set_hwnd(h);
                }
                // Opt into Win11 Mica. Silently no-ops on Win10.
                // See try_enable_mica for the transparency follow-up.
                let _ = try_enable_mica(h);
            }

            let skip_close_confirmation = app_for_eframe.is_always_minimise_to_tray();
            Ok(Box::new(StreamToSpeakerApp {
                app: app_for_eframe,
                last_repaint_request: Instant::now(),
                tray,
                frame_count: 0,
                confirm_close_open: false,
                skip_close_confirmation,
                theme_mode: ThemeMode::System,
                advanced_open: false,
                onboarding_dismissed: false,
                last_applied_dark: None,
                last_applied_accent: None,
                hwnd,
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
    /// Last-applied system accent (read from registry when ThemeMode
    /// is System). Compared each frame so a change to Windows'
    /// Settings → Personalization → Accent color triggers a single
    /// re-apply, no more.
    last_applied_accent: Option<(u8, u8, u8)>,
    /// Raw Win32 HWND, captured on the first `update()` from
    /// `eframe::Frame::window_handle()`. Used to drive hide/show via
    /// `ShowWindow` directly — `ViewportCommand::Visible(false)` and
    /// `Minimized` both have eframe queue-drain bugs that break the
    /// tray-menu round-trip (emilk/egui#5229, #3655). Bypassing the
    /// queue with raw Win32 sidesteps the bug entirely.
    hwnd: Option<isize>,
}

/// Win32 helpers for hide/show. eframe's `ViewportCommand::Visible` /
/// `Minimized` are queue-processed inside `update()`, which only runs
/// on WM_PAINT — and hidden windows don't get WM_PAINT, so the queue
/// can deadlock. Raw `ShowWindow` is a synchronous OS call that
/// bypasses the queue (emilk/egui#5229, #3655).
#[cfg(windows)]
fn win_hide(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    unsafe { ShowWindow(hwnd as _, SW_HIDE); }
}

/// Restore from any state (hidden, minimised) and bring to foreground.
/// SetWindowPos + SWP_SHOWWINDOW is the most reliable way to make a
/// hidden window visible AND lift it in Z-order in one call —
/// SW_HIDE/SW_SHOW alone can desync winit's WindowFlags::VISIBLE bit
/// and leave the window Z-ordered behind the taskbar. SW_RESTORE
/// additionally handles the "user minimised via the titlebar" path.
#[cfg(windows)]
fn win_show_and_focus(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindowAsync, SetForegroundWindow, IsIconic,
        HWND_TOP, SW_RESTORE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };
    unsafe {
        let h = hwnd as _;
        SetWindowPos(h, HWND_TOP, 0, 0, 0, 0,
                     SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
        if IsIconic(h) != 0 {
            ShowWindowAsync(h, SW_RESTORE);
        }
        SetForegroundWindow(h);
    }
}

impl eframe::App for StreamToSpeakerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame_count = self.frame_count.saturating_add(1);

        if self.last_repaint_request.elapsed() >= Duration::from_millis(100) {
            ctx.request_repaint_after(Duration::from_millis(100));
            self.last_repaint_request = Instant::now();
        }

        // Reapply theme only when the resolved mode actually changes
        // or the OS accent shifts (System mode follows the Windows
        // accent colour live). apply_theme rebuilds the full
        // Style+Visuals (allocates text-style map, all the widget
        // visuals); doing that every frame at 10 fps was wasteful
        // and could cause focus loss on some egui widgets.
        let dark = self.theme_mode.resolve(ctx);
        let accent = if matches!(self.theme_mode, ThemeMode::System) {
            read_system_accent()
        } else {
            None
        };
        if self.last_applied_dark != Some(dark) || self.last_applied_accent != accent {
            apply_theme(ctx, dark, accent);
            self.last_applied_dark = Some(dark);
            self.last_applied_accent = accent;
        }
        let p = palette_for(dark, accent);

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
            .frame(egui::Frame::default().fill(p.canvas).inner_margin(sp::M))
            .show(ctx, |ui| {
                let enabled = !self.confirm_close_open;
                ui.add_enabled_ui(enabled, |ui| {
                    // Keep the header pinned at the top (theme toggle
                    // shouldn't scroll away); everything below scrolls
                    // when the window is shorter than the content.
                    self.show_header(ui, &p);
                    ui.add_space(sp::S);
                    // Reserve a right-edge inset for the auto-expanding
                    // scrollbar. egui's modern scrollbar starts as a
                    // 2 px panning indicator and morphs to 6 px on
                    // hover, drawn OVER the content (Fluent S2). Without
                    // a right inset the morphed bar overlays the right
                    // border of every card — exactly the "scrollbar
                    // touching the card borders" the audit / user
                    // reported.
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_max_width(ui.available_width() - sp::M);
                            self.show_status_banner(ui, &p);
                            self.show_error_banner(ui, &p);
                            ui.add_space(14.0);
                            // Show onboarding regardless of whether a
                            // speaker is currently bound, until the
                            // user explicitly dismisses with "Got it".
                            // Dismissal is persisted via user_config
                            // (see app.rs::dismiss_onboarding), so
                            // returning users don't see it on every
                            // launch.
                            if !self.onboarding_dismissed
                                && !self.app.is_onboarding_dismissed()
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
        .rounding(RADIUS_SURFACE)
        .inner_margin(egui::Margin::symmetric(18.0, 16.0))
        .show(ui, content)
        .inner
}

/// Card-level section heading. Fluent T2 Body Strong (14 px Semibold)
/// in `text_primary`, sentence case. The previous treatment was an
/// 11-px uppercase letter-spaced "eyebrow" — but used standalone
/// (i.e. with no real heading underneath) the eyebrow IS the heading,
/// which violates Fluent T3 (nothing below 12 px Regular) and makes
/// section titles disappear from quick-scan reads.
fn section_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(14.0)
            .strong()
            .color(p.text_primary),
    );
    ui.add_space(sp::XS);
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
    .rounding(RADIUS_CONTROL);
    clickable(ui.add_sized([min_width, CONTROL_HEIGHT], btn))
}

/// Secondary button — outlined, neutral background. For tertiary actions
/// (Refresh, Cancel, latency-step nudges).
fn secondary_button(ui: &mut egui::Ui, p: &Palette, label: &str, min_width: f32) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(label).color(p.text_primary))
        .fill(p.card)
        .stroke(egui::Stroke::new(1.0, p.divider))
        .rounding(RADIUS_CONTROL);
    clickable(ui.add_sized([min_width, CONTROL_HEIGHT], btn))
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
    .rounding(RADIUS_CONTROL);
    clickable(ui.add_sized([min_width, CONTROL_HEIGHT], btn))
}

/// Danger button — red text + thin red border. For "this glitches the audio"
/// actions (Resync, Quit). Not as loud as a solid red fill — Refactoring UI
/// recommends only going full danger-red when it's the page's primary action.
fn danger_button(ui: &mut egui::Ui, p: &Palette, label: &str, min_width: f32) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(label).color(p.danger))
        .fill(p.card)
        .stroke(egui::Stroke::new(1.0, p.danger.gamma_multiply(0.5)))
        .rounding(RADIUS_CONTROL);
    clickable(ui.add_sized([min_width, CONTROL_HEIGHT], btn))
}

impl StreamToSpeakerApp {
    fn show_header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("Stream To Speaker")
                        .size(20.0)
                        .strong()
                        .color(p.text_primary),
                );
                ui.label(
                    egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .size(11.0)
                        .color(p.text_tertiary),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.show_help_menu(ui, p);
                ui.add_space(sp::XS);
                self.show_theme_toggle(ui, p);
            });
        });
    }

    fn show_help_menu(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // "?" button → popup with About / Open log folder / Report an
        // issue. Addresses the audit's "no version label, no help link,
        // no logs link" finding (Heuristics F-22).
        let popup_id = ui.id().with("help_popup");
        let btn = egui::Button::new(
            egui::RichText::new("?")
                .size(14.0)
                .strong()
                .color(p.text_secondary),
        )
        .fill(p.card)
        .stroke(egui::Stroke::new(1.0, p.divider))
        .rounding(RADIUS_CONTROL);
        let resp = clickable(ui.add_sized([CONTROL_HEIGHT, CONTROL_HEIGHT], btn))
            .on_hover_text("About + log folder + report an issue");
        if resp.clicked() {
            ui.memory_mut(|m| m.toggle_popup(popup_id));
        }
        egui::popup::popup_below_widget(
            ui,
            popup_id,
            &resp,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                ui.set_min_width(240.0);
                ui.label(
                    egui::RichText::new(format!(
                        "Stream To Speaker v{}",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .strong()
                    .color(p.text_primary),
                );
                ui.add_space(sp::XS);
                ui.separator();
                if ui.button("Open log folder").clicked() {
                    if let Some(dir) = crate::log_dir() {
                        // explorer.exe accepts a path arg and opens
                        // Windows Explorer at that location.
                        let _ = std::process::Command::new("explorer")
                            .arg(&dir)
                            .spawn();
                    }
                    ui.memory_mut(|m| m.close_popup());
                }
                if ui.button("Report an issue on GitHub").clicked() {
                    let _ = open_url(
                        "https://github.com/Mihonarium/StreamToSpeaker/issues/new",
                    );
                    ui.memory_mut(|m| m.close_popup());
                }
            },
        );
    }

    fn show_theme_toggle(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // Three-way segmented control: System / Light / Dark. Compact, lives
        // in the top-right of the header. Each segment shows the current
        // mode by filling with the accent colour.
        // System → Light → Dark in source order, left-to-right paint
        // (the inner ui.horizontal is LTR regardless of the parent's
        // right_to_left layout). Tab follows source order, so Tab
        // and paint agree.
        let modes = [
            (ThemeMode::System, "System", "Match your Windows light/dark mode"),
            (ThemeMode::Light, "Light", "Use the light theme regardless of Windows"),
            (ThemeMode::Dark, "Dark", "Use the dark theme regardless of Windows"),
        ];
        egui::Frame::none()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0, p.divider))
            .rounding(RADIUS_CONTROL)
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
                            .rounding(RADIUS_CONTROL);
                        if clickable(ui.add_sized([60.0, CONTROL_HEIGHT], btn))
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
                    if secondary_button(ui, p, "Hide this guide", 120.0)
                        .on_hover_text("Hide this guide. It won't reappear on future launches.")
                        .clicked()
                    {
                        self.onboarding_dismissed = true;
                        self.app.dismiss_onboarding();
                    }
                });
            });
            ui.add_space(8.0);

            let steps: &[(&str, &str, &str)] = &[
                (
                    "1",
                    "Set Stream To Speaker as your Windows audio output",
                    "Right-click the speaker icon in your taskbar and choose \
                     Open sound settings. Under Output, pick Stream To Speaker. \
                     (To route only one app, open Volume mixer instead and set \
                     that app's output to Stream To Speaker.)",
                ),
                (
                    "2",
                    "Choose a speaker",
                    "Speakers on your network appear in the list below. Click \
                     one to start streaming.",
                ),
                (
                    "3",
                    "Adjust the latency if audio drifts",
                    "If audio lags the picture, use the Trim buttons. If it \
                     glitches, use the Pad buttons. Resync gives an instant \
                     fix at the cost of a brief audio click.",
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
            (Some(r), false, _) => (
                "⊘",
                p.danger,
                format!("Streaming to {} disabled", r.friendly_name),
                format!("{} is free for other apps. Press Enable to resume streaming.", r.friendly_name),
                Some("Enable streaming"),
                Some("Reconnect to the last speaker and resume streaming"),
            ),
            (Some(r), true, true) => (
                "▶",
                p.success,
                format!("Streaming to {}", r.friendly_name),
                format!("{}  ·  {} packets/sec", r.ip, self.packets_per_sec()),
                Some("Disable streaming"),
                Some("Stop streaming and release the speaker for other apps"),
            ),
            (Some(r), true, false) => (
                "‖",
                p.warn,
                format!("Standing by on {}", r.friendly_name),
                format!("{}  ·  waiting for audio", r.ip),
                Some("Disable streaming"),
                Some("Stop streaming and release the speaker for other apps"),
            ),
        };

        egui::Frame::none()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0, p.divider))
            .rounding(RADIUS_SURFACE)
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
                                let r = if label == "Enable streaming" {
                                    primary_button(ui, p, label, 140.0)
                                } else {
                                    secondary_button(ui, p, label, 140.0)
                                }
                                .on_hover_text(tip);
                                if r.clicked() {
                                    if let Err(e) = self.app.set_streaming_enabled(!enabled) {
                                        self.app.record_error(
                                            format!("Couldn't change streaming state: {}", e),
                                        );
                                    }
                                }
                            },
                        );
                    }
                });
            });
    }

    /// InfoBar-style banner that surfaces the most recent
    /// `App::record_error` for ~8 s. Previously every failure path
    /// (select speaker, toggle streaming, resync) silently `warn!`'d
    /// to the log file; the user clicked something, nothing
    /// happened, and they assumed the app was broken (Heuristics
    /// F-03). Auto-fades — no UI clutter when there's nothing wrong.
    fn show_error_banner(&self, ui: &mut egui::Ui, p: &Palette) {
        let Some(msg) = self.app.current_error() else { return; };
        ui.add_space(sp::XS);
        egui::Frame::none()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0, p.danger.gamma_multiply(0.6)))
            .rounding(RADIUS_SURFACE)
            .inner_margin(egui::Margin::symmetric(sp::M, sp::S))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("⚠")
                            .color(p.danger)
                            .size(18.0)
                            .strong(),
                    );
                    ui.add_space(sp::XS);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(&msg)
                                .size(13.0)
                                .color(p.text_primary),
                        );
                    });
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if link_button(ui, p, "Dismiss", 80.0).clicked() {
                                self.app.dismiss_error();
                            }
                        },
                    );
                });
            });
    }

    fn show_speakers(&self, ui: &mut egui::Ui, p: &Palette) {
        card(ui, p, |ui| {
            ui.horizontal(|ui| {
                section_label(ui, p, "Speakers");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if secondary_button(ui, p, "↻  Rescan", 92.0)
                        .on_hover_text(
                            "Search the network for speakers now. Otherwise this runs every few minutes.",
                        )
                        .clicked()
                    {
                        self.app.trigger_rescan();
                    }
                    // "Forget saved speaker" — only relevant when the
                    // user has a persisted last_speaker_id. Clears it
                    // and resets onboarding so the next launch feels
                    // like a fresh install. Lives here (alongside
                    // Rescan) because both are speaker-list-level
                    // management actions.
                    if self.app.saved_speaker_id().is_some() {
                        if link_button(ui, p, "Forget saved", 110.0)
                            .on_hover_text(
                                "Disconnect from the speaker and clear the saved choice. The next launch will ask you to pick again.",
                            )
                            .clicked()
                        {
                            self.app.forget_saved_speaker();
                        }
                    }
                });
            });

            // Rescan feedback: spinner while a manual scan is in
            // flight, then "Found N speaker(s) just now" for ~5 s
            // after completion so the user knows the button did
            // something (Heuristics F-04 — previously the rescan
            // gave no feedback at all).
            let scanning = self.app.rescan_in_flight.load(Ordering::Acquire);
            let last_finished = self.app.last_rescan_finished_unix.load(Ordering::Acquire);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let recently_finished = last_finished > 0 && (now - last_finished) < 5;
            if scanning {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(
                        egui::RichText::new("Scanning…")
                            .size(12.0)
                            .color(p.text_secondary),
                    );
                });
            } else if recently_finished {
                let n = self.app.last_rescan_count.load(Ordering::Acquire);
                let text = match n {
                    0 => "Scan complete — no speakers found.".to_string(),
                    1 => "Scan complete — 1 speaker found.".to_string(),
                    n => format!("Scan complete — {} speakers found.", n),
                };
                ui.label(
                    egui::RichText::new(text)
                        .size(12.0)
                        .color(p.text_secondary),
                );
            }
            ui.add_space(sp::XS);

            let view = self.app.speaker_view();
            if view.speakers.is_empty() {
                ui.add_space(4.0);
                // After ~10 s with no results, the user has waited
                // long enough that "still searching" stops being
                // useful — switch to actionable troubleshooting copy
                // (Heuristics F-05). Note: italics removed (Fluent
                // T4 — no italic in the type ramp).
                let uptime = self.app.uptime_secs();
                if uptime < 10 {
                    ui.label(
                        egui::RichText::new("Searching the network for speakers…")
                            .color(p.text_secondary),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Make sure your PC and the speaker are on the same Wi-Fi or Ethernet network.",
                        )
                        .size(12.0)
                        .color(p.text_tertiary),
                    );
                } else {
                    ui.label(
                        egui::RichText::new("Still no speakers found.")
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.add_space(sp::XS / 2.0);
                    ui.label(
                        egui::RichText::new("Common causes:")
                            .size(12.0)
                            .color(p.text_secondary),
                    );
                    for cause in [
                        "Your PC and the speaker are on different Wi-Fi networks (e.g. guest Wi-Fi, VPN).",
                        "Windows Firewall is blocking SSDP / UPnP traffic for this app.",
                        "The speaker is powered off, or hasn't finished booting yet.",
                    ] {
                        ui.label(
                            egui::RichText::new(format!("  •  {cause}"))
                                .size(12.0)
                                .color(p.text_secondary),
                        );
                    }
                    ui.add_space(sp::XS);
                    ui.label(
                        egui::RichText::new("Click Rescan above to try again.")
                            .size(12.0)
                            .color(p.text_tertiary),
                    );
                }
                return;
            }

            egui::ScrollArea::vertical()
                .max_height(190.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for sp in view.speakers {
                        speaker_row(ui, p, &sp, |id| {
                            if let Err(e) = self.app.select_speaker(id) {
                                self.app.record_error(
                                    format!("Couldn't connect to speaker: {}", e),
                                );
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
                        (p.text_tertiary, "In sync".to_string())
                    } else if pending > 0 {
                        (p.warn, format!("Trimming {} ms…", pending))
                    } else {
                        (p.warn, format!("Padding {} ms…", -pending))
                    };
                    ui.label(egui::RichText::new(text).size(12.0).color(color));
                });
            });
            ui.add_space(sp::XS);

            // Two-column layout. Each column owns ONE direction of
            // change, with a single-verb symptom-led explainer above
            // its two buttons. The previous design was a single row
            // of four near-identical −/+ buttons under a combined
            // "trim / pad" sentence, which forced the user to
            // remember which sign maps to which audio problem (the
            // explicit user complaint). Splitting visually makes the
            // direction self-documenting.
            ui.columns(2, |cols| {
                // Left column: trim latency (negative pending) for
                // when audio lags the picture.
                cols[0].label(
                    egui::RichText::new("Audio lags the picture?")
                        .size(12.0)
                        .strong()
                        .color(p.text_primary),
                );
                cols[0].label(
                    egui::RichText::new("Trim the buffer so the speaker catches up.")
                        .size(12.0)
                        .color(p.text_secondary),
                );
                cols[0].add_space(sp::XS);
                cols[0].horizontal(|ui| {
                    if secondary_button(ui, p, "−25 ms", 78.0)
                        .on_hover_text("Reduce latency by 25 ms (smaller step)")
                        .clicked()
                    {
                        self.app.adjust_latency(25);
                    }
                    if secondary_button(ui, p, "−100 ms", 86.0)
                        .on_hover_text(
                            "Reduce latency by 100 ms. Removes audio gradually over ~2 s so the trim isn't audible.",
                        )
                        .clicked()
                    {
                        self.app.adjust_latency(100);
                    }
                });

                // Right column: pad latency (positive buffer) for
                // when the speaker glitches / drops out.
                cols[1].label(
                    egui::RichText::new("Audio glitching or dropping?")
                        .size(12.0)
                        .strong()
                        .color(p.text_primary),
                );
                cols[1].label(
                    egui::RichText::new("Add headroom so the buffer doesn't run dry.")
                        .size(12.0)
                        .color(p.text_secondary),
                );
                cols[1].add_space(sp::XS);
                cols[1].horizontal(|ui| {
                    if secondary_button(ui, p, "+25 ms", 78.0)
                        .on_hover_text("Add 25 ms of headroom (smaller step)")
                        .clicked()
                    {
                        self.app.adjust_latency(-25);
                    }
                    if secondary_button(ui, p, "+100 ms", 86.0)
                        .on_hover_text("Add 100 ms of headroom (larger step)")
                        .clicked()
                    {
                        self.app.adjust_latency(-100);
                    }
                });
            });

            ui.add_space(sp::S);
            ui.separator();
            ui.add_space(sp::XS);

            // Resync stays disabled when no speaker is bound — the
            // call would just warn-and-no-op (audit F-14). Tooltip
            // explains in plain language; the previous version
            // mentioned UPnP / prebuffer, both engineer terms.
            let has_speaker = self.app.current_renderer().is_some();
            ui.add_enabled_ui(has_speaker, |ui| {
                let resp = danger_button(ui, p, "⟳  Resync speaker", 180.0);
                let resp = if has_speaker {
                    resp.on_hover_text(
                        "Stops and restarts the speaker. Causes a brief audio click but clears any accumulated latency.",
                    )
                } else {
                    resp.on_hover_text("Connect to a speaker first.")
                };
                if resp.clicked() {
                    if let Err(e) = self.app.resync() {
                        self.app.record_error(format!("Resync failed: {}", e));
                    }
                }
            });
        });
    }

    fn show_advanced(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(ui, p, |ui| {
            // Disclosure header: keyboard-focusable strip with the
            // chevron placed immediately after the label (proximity)
            // rather than at the right edge of the card. The previous
            // design had label at left edge / chevron at right edge,
            // ~600 px apart, so they read as unrelated elements —
            // exactly the issue the audit flagged. Wiring via
            // `ui.interact` with a stable id gives Tab focus, and
            // we accept Enter/Space when focused (ARIA disclosure
            // pattern). Height bumped 22 → 28 to meet the WCAG 2.5.8
            // minimum click-target size.
            let chevron = if self.advanced_open { "▾" } else { "▸" };
            let avail_w = ui.available_width();
            let id = ui.id().with("advanced_toggle");
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(avail_w, 28.0),
                egui::Sense::hover(),
            );
            let resp = ui
                .interact(rect, id, egui::Sense::click())
                .on_hover_text("Tuning knobs for power users");
            // Expose to AccessKit / screen readers.
            resp.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    resp.enabled(),
                    if self.advanced_open {
                        "Advanced (expanded)"
                    } else {
                        "Advanced (collapsed)"
                    },
                )
            });
            if resp.hovered() && resp.enabled() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let kbd_toggle = resp.has_focus()
                && ui.input(|i| {
                    i.key_pressed(egui::Key::Enter)
                        || i.key_pressed(egui::Key::Space)
                });
            if resp.clicked() || kbd_toggle {
                self.advanced_open = !self.advanced_open;
            }
            // Explicit focus ring — egui doesn't paint one on a bare
            // ui.interact rect, and the audit flagged this as a P0
            // keyboard-accessibility failure.
            if resp.has_focus() {
                ui.painter().rect_stroke(
                    rect.expand(2.0),
                    RADIUS_CONTROL,
                    egui::Stroke::new(2.0, p.accent),
                );
            }
            // Paint label + chevron. Matches the new section_label
            // (14 px Body Strong, sentence case, primary text). The
            // chevron lands immediately to the right of the label
            // text (Gestalt proximity).
            let label_font =
                egui::FontId::new(14.0, egui::FontFamily::Proportional);
            let chevron_font =
                egui::FontId::new(14.0, egui::FontFamily::Proportional);
            let label_rect = ui.painter().text(
                egui::pos2(rect.left(), rect.center().y),
                egui::Align2::LEFT_CENTER,
                "Advanced",
                label_font,
                p.text_primary,
            );
            ui.painter().text(
                egui::pos2(label_rect.right() + sp::XS, rect.center().y),
                egui::Align2::LEFT_CENTER,
                chevron,
                chevron_font,
                p.text_secondary,
            );

            if !self.advanced_open {
                return;
            }

            ui.add_space(8.0);

            let mut ppm = self.app.rate_fudge_ppm.load(Ordering::Relaxed);
            advanced_row(
                ui,
                p,
                "Clock-drift compensation",
                "Try +50 to +100 ppm if audio gradually falls behind; negative if it gradually gets ahead.",
                "Your PC and the speaker each have a tiny crystal oscillator that tracks time, and they drift apart by a few parts per million. Over many minutes that drift is enough to push audio out of sync. Positive values make the service produce frames slightly faster than real-time (catches up if the speaker is gaining); negative drops frames (catches up if the speaker is losing). Most setups need 0; if you notice audio creeping out of sync after 10–20 minutes of continuous play, nudge in steps of ±25 ppm until it stays put.",
                |ui| {
                    ui.add(
                        egui::DragValue::new(&mut ppm)
                            .range(-1000..=1000)
                            .suffix(" ppm"),
                    );
                    if secondary_button(ui, p, "Reset to 0 ppm", 130.0).clicked() {
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
                "Higher than 10 ms shrinks latency after a pause. Stay below ~30 to avoid dropouts.",
                "When nothing is playing on Windows, the service still sends frames of silence to the speaker (otherwise the speaker drops the connection). The speaker buffers a bit ahead — when real audio comes back, that buffer adds latency before you hear it. Setting this higher than 10 ms makes the silence frames go slower than real-time, draining the buffer during the quiet passages, so post-pause latency is smaller. Too high (>30) and the buffer runs dry and you hear dropouts.",
                |ui| {
                    ui.add(egui::DragValue::new(&mut pace).range(1..=100).suffix(" ms"));
                    if secondary_button(ui, p, "Reset to 10 ms", 130.0).clicked() {
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
                "Larger = the −/+ buttons act faster but produce a louder click.",
                "When you press one of the −/+ buttons in the Latency card, the service adds or drops a few audio frames each packet until it's caught up the requested amount. This setting controls how many frames per packet. Larger values reach the target faster but make a louder click; smaller values are smoother but slower. The default of 4 frames is ~0.09 ms per packet, which most listeners can't hear.",
                |ui| {
                    ui.add(
                        egui::DragValue::new(&mut step)
                            .range(1..=256)
                            .suffix(" frames"),
                    );
                    if secondary_button(ui, p, "Reset to 4 frames", 150.0).clicked() {
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
                    // of competing with Enable streaming in the status
                    // banner.
                    if link_button(ui, p, "Open in browser", 150.0)
                        .on_hover_text(format!("Open {} in your default browser", url))
                        .clicked()
                    {
                        let _ = open_url(&url);
                    }
                    if secondary_button(ui, p, "Disable web UI", 140.0)
                        .on_hover_text("Stop serving the web UI")
                        .clicked()
                    {
                        self.app.set_web_ui_enabled(false);
                    }
                } else if secondary_button(ui, p, "Enable web UI", 140.0)
                    .on_hover_text(
                        "Turn on the web control panel. \
                         The audio stream stays available either way.",
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
                stat_pill(
                    ui,
                    p,
                    &format!("{}", pkts_sec),
                    "packets/sec",
                    "10 ms audio packets sent per second from this PC.",
                );
                let listener_label = if subs == 1 { "listener" } else { "listeners" };
                let listener_tip = if subs == 1 {
                    "1 device is currently pulling the audio stream."
                } else {
                    "Devices currently pulling the audio stream from this PC."
                };
                stat_pill(ui, p, &format!("{}", subs), listener_label, listener_tip);
                stat_pill(
                    ui,
                    p,
                    &format_duration(uptime),
                    "uptime",
                    "How long the Stream To Speaker service has been running since launch.",
                );
                stat_pill(
                    ui,
                    p,
                    &humanize_count(pkts_total),
                    "packets",
                    "Total audio packets sent since launch.",
                );
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

        // Esc cancels the modal — ARIA dialog pattern, Windows
        // convention. egui's Window doesn't auto-handle Escape.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = Some(CloseAction::Cancel);
        }

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
            .default_width(448.0)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(p.card)
                    .stroke(egui::Stroke::new(1.0, p.divider))
                    .rounding(RADIUS_SURFACE)
                    .inner_margin(sp::L),
            )
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Keep streaming in the tray, or quit the app entirely.",
                    )
                    .color(p.text_secondary),
                );
                ui.add_space(sp::S);
                ui.checkbox(
                    &mut new_skip,
                    "Always minimise to tray. Don't ask again.",
                );
                ui.add_space(sp::M);
                ui.horizontal(|ui| {
                    // Order: primary (Minimise) on the left, then a
                    // sp::L (24 epx) physical gap before the danger
                    // (Quit) — Fitts says identical-size adjacent
                    // buttons of opposite consequence cause mis-
                    // clicks. Cancel sits flush right.
                    if primary_button(ui, p, "Minimise to tray", 170.0)
                        .on_hover_text("Hide the window. The tray icon stays and streaming continues.")
                        .clicked()
                    {
                        action = Some(CloseAction::MinimiseToTray);
                    }
                    ui.add_space(sp::L);
                    if danger_button(ui, p, "Quit", 96.0)
                        .on_hover_text("Stop streaming and close the app entirely.")
                        .clicked()
                    {
                        action = Some(CloseAction::Quit);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Initial focus on Cancel (the safe choice) per
                        // ARIA dialog pattern — Enter on a confirmation
                        // modal should NEVER default to a destructive
                        // action.
                        let cancel_id = ui.id().with("close_modal_cancel");
                        let resp = secondary_button(ui, p, "Cancel", 96.0);
                        if !resp.has_focus() && !ctx.memory(|m| m.focused().is_some()) {
                            ui.memory_mut(|m| m.request_focus(cancel_id));
                        }
                        let resp = ui.interact(resp.rect, cancel_id, egui::Sense::click());
                        if resp.clicked() {
                            action = Some(CloseAction::Cancel);
                        }
                    });
                });
            });

        if !still_open && action.is_none() {
            action = Some(CloseAction::Cancel);
        }

        if new_skip != self.skip_close_confirmation {
            self.skip_close_confirmation = new_skip;
            // Persist across launches — was session-local before
            // (audit Content #63 / Accessibility D-03). Onboarding
            // dismissal is persisted; this should match.
            self.app.set_always_minimise_to_tray(new_skip);
        }
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

    // Two-pass painting: allocate, then paint with hover/active/focus-
    // aware colours. The Response goes through `ui.interact` with a
    // stable id derived from the speaker — that's what makes the row
    // a Tab stop (Accessibility B-02; previously a bare allocate +
    // Sense::click() rect was pointer-only, blocking the app's
    // primary task for keyboard-only users).
    let height = 44.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let id = ui.id().with(&sp.id);
    let response = ui.interact(rect, id, egui::Sense::click());
    // Tell AccessKit / UI Automation what this rect is. Without
    // WidgetInfo, painter-based widgets are invisible to screen
    // readers (Accessibility G-01).
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::RadioButton,
            response.enabled(),
            active,
            &sp.friendly_name,
        )
    });

    // Keyboard activation: Enter or Space when this row is focused.
    let kbd_activate = response.has_focus()
        && ui.input(|i| {
            i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space)
        });

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
        .rect(rect, RADIUS_CONTROL, row_fill, egui::Stroke::new(1.0, row_stroke));

    // Explicit focus ring for keyboard users — egui doesn't paint
    // one on a bare ui.interact rect.
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            RADIUS_CONTROL,
            egui::Stroke::new(2.0, p.accent),
        );
    }

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

    if response.hovered() && response.enabled() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if (response.clicked() || kbd_activate) && !active {
        on_click(&sp.id);
    }

    ui.add_space(6.0);
}

fn advanced_row(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    hint: &str,
    plain_explain: &str,
    add_control: impl FnOnce(&mut egui::Ui),
) {
    // Stacked layout (label/hint on top, controls underneath). Horizontal
    // side-by-side layouts overlap badly when the window is narrow —
    // vertical is robust at any width and reads cleanly.
    //
    // Each row carries two depths of explanation:
    //   - `hint` (always visible, concise, may use terms-of-art)
    //   - `plain_explain` (revealed on hover of the ⓘ glyph next to
    //      the label, longer, no jargon — for users who want the
    //      what-and-why before they touch the control).
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(label)
                    .strong()
                    .color(p.text_primary),
            );
            let help = ui.label(
                egui::RichText::new("ⓘ")
                    .size(13.0)
                    .color(p.accent),
            )
            .on_hover_text(plain_explain);
            if help.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Help);
            }
        });
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(hint)
                .size(12.0)
                .color(p.text_secondary),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            add_control(ui);
        });
    });
}

fn stat_pill(
    ui: &mut egui::Ui,
    p: &Palette,
    value: &str,
    label: &str,
    tooltip: &str,
) {
    // Bumped label 10 → 12 (Fluent T3 — no text below 12 px Regular),
    // dropped the extra_letter_spacing, and added a tooltip per
    // Content #20 so "pkt / s" / "listeners" / "uptime" / "packets"
    // don't read as decoration.
    egui::Frame::none()
        .fill(p.card_hover)
        .rounding(RADIUS_CONTROL)
        .inner_margin(egui::Margin::symmetric(sp::S, sp::XS))
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
                        .size(12.0)
                        .color(p.text_secondary),
                );
            });
        })
        .response
        .on_hover_text(tooltip);
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
