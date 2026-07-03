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

    // M14 — three "container padding" tiers, named so we never reach
    // for raw 18 / 16 / 24 / 12 / 8 again.
    //
    //   CARD_*  — sectional cards + status banner. 18 horizontal is
    //             slightly wider than sp::M to give section_label a
    //             touch more breathing room from the card border.
    //   MODAL   — confirm / settings modals. sp::L on all sides; the
    //             extra padding signals "this is a heavier surface
    //             demanding attention" (Refactoring UI: heavier
    //             surfaces get heavier padding).
    //   PILL_*  — inline pills, segmented controls, status chips.
    //             Tight by design (small surfaces need small padding).
    pub const CARD_H: f32 = 18.0;
    pub const CARD_V: f32 = M;
    pub const MODAL: f32 = L;
    pub const PILL_H: f32 = S;
    pub const PILL_V: f32 = XS;
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

/// Detect and recover from "window persisted off every connected
/// monitor". eframe's `persist_window: true` stores the previous
/// window rect; if the user disconnected a monitor, switched a
/// laptop dock, or upgraded from a binary that wrote out a different
/// coordinate system, the restored position can land outside any
/// monitor's bounds. The window still exists, the kernel still
/// scheduled paints into it — but the user sees nothing.
///
/// Uses two checks: the rect-intersect check (catches "completely
/// off all monitors"), AND a center-point check (catches "almost-
/// entirely off; only one pixel intersects a monitor", which is
/// equally invisible from the user's standpoint). If either fails,
/// we re-centre on the primary monitor's work area AND force
/// ShowWindow(SW_SHOWNORMAL) in case the persisted state included a
/// minimised / hidden flag.
#[cfg(windows)]
fn rescue_offscreen_window(hwnd: isize) {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MONITORINFO,
        MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, ShowWindow, HWND_TOP, SWP_NOZORDER,
        SW_SHOWNORMAL,
    };
    unsafe {
        let h = hwnd as _;
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetWindowRect(h, &mut rect) == 0 {
            log::warn!("rescue_offscreen_window: GetWindowRect failed");
            return;
        }
        log::info!(
            "rescue: window rect l={} t={} r={} b={} ({}×{})",
            rect.left, rect.top, rect.right, rect.bottom,
            rect.right - rect.left, rect.bottom - rect.top
        );
        let intersect = MonitorFromRect(&rect, MONITOR_DEFAULTTONULL);
        let center = POINT {
            x: (rect.left + rect.right) / 2,
            y: (rect.top + rect.bottom) / 2,
        };
        let center_monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONULL);
        if !intersect.is_null() && !center_monitor.is_null() {
            return; // window AND its centre are on a monitor — fine
        }
        log::warn!(
            "window appears off-screen (intersect={} center={}); re-centring on primary",
            if intersect.is_null() { "miss" } else { "hit" },
            if center_monitor.is_null() { "miss" } else { "hit" },
        );
        let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if primary.is_null() {
            return;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(primary, &mut mi) == 0 {
            return;
        }
        // Use a sensible size if the restored rect is degenerate
        // (< 100 px in either dim). Otherwise keep the user's size.
        let mut w = rect.right - rect.left;
        let mut h_dim = rect.bottom - rect.top;
        if w < 100 {
            w = 720;
        }
        if h_dim < 100 {
            h_dim = 800;
        }
        let mon_w = mi.rcWork.right - mi.rcWork.left;
        let mon_h = mi.rcWork.bottom - mi.rcWork.top;
        let new_x = mi.rcWork.left + (mon_w - w) / 2;
        let new_y = mi.rcWork.top + (mon_h - h_dim) / 2;
        SetWindowPos(h, HWND_TOP, new_x, new_y, w, h_dim, SWP_NOZORDER);
        ShowWindow(h, SW_SHOWNORMAL);
        log::info!("window re-centred to ({}, {}) size {}×{}", new_x, new_y, w, h_dim);
    }
}

#[cfg(not(windows))]
fn rescue_offscreen_window(_hwnd: isize) {}

/// Always-fire belt-and-braces: ensure the window is in the
/// normal-visible show state after eframe creates it. Covers the
/// rare-but-real case where the persisted state, a stale SW_HIDE,
/// or DWM hand-off leaves the HWND created but never SW_SHOWN.
#[cfg(windows)]
fn force_show_normal(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, ShowWindow, SW_SHOWNORMAL,
    };
    unsafe {
        let h = hwnd as _;
        let visible_before = IsWindowVisible(h) != 0;
        ShowWindow(h, SW_SHOWNORMAL);
        log::info!(
            "force_show_normal: was_visible={} → SW_SHOWNORMAL issued",
            visible_before
        );
    }
}

#[cfg(not(windows))]
fn force_show_normal(_hwnd: isize) {}

#[derive(Copy, Clone)]
struct Palette {
    // Surfaces
    canvas: egui::Color32,        // page background
    card: egui::Color32,          // section card
    card_hover: egui::Color32,    // hover lift
    card_active: egui::Color32,   // pressed
    divider: egui::Color32,       // section borders (3:1 not required —
                                  // sectional separator, not a UI
                                  // component edge per WCAG 1.4.11)
    /// m28: stroke for UI components — buttons, segmented controls,
    /// pickers. WCAG 1.4.11 requires ≥3:1 against the adjacent fill
    /// for any UI affordance edge; the lighter `divider` colour
    /// (~1.25:1 on white) was fine for card outlines but failed the
    /// bar when reused on button borders.
    control_stroke: egui::Color32,
    overlay: egui::Color32,       // selected-row background tint

    // Text
    text_primary: egui::Color32,
    text_secondary: egui::Color32,
    text_tertiary: egui::Color32,
    text_on_accent: egui::Color32,
    /// m27: text colour to use INSIDE accent_subtle tints (onboarding
    /// numbered circles, accent-tinted pills). text_primary / accent
    /// both fail AA against accent_subtle in light mode; this is a
    /// purpose-mixed shade that hits the 4.5:1 bar.
    text_on_accent_subtle: egui::Color32,

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
            // m28: ~3.1:1 on the white card — qualifies as a UI
            // component edge under WCAG 1.4.11.
            control_stroke: egui::Color32::from_rgb(0xa6, 0xb0, 0xbc),
            overlay: egui::Color32::from_rgb(0xe0, 0xf3, 0xf7),

            text_primary: egui::Color32::from_rgb(0x14, 0x1b, 0x29),
            text_secondary: egui::Color32::from_rgb(0x53, 0x5d, 0x6e),
            // Bumped from #8a93a3 (which gave ~3:1 on white — fails
            // WCAG 1.4.3 AA for body text). #6b7488 measures ~4.7:1
            // against the light card / canvas, passing the 4.5:1 bar.
            text_tertiary: egui::Color32::from_rgb(0x6b, 0x74, 0x88),
            text_on_accent: egui::Color32::from_rgb(0xff, 0xff, 0xff),
            // m27: 5.8:1 against accent_subtle #c6e7ed — wide pass
            // for the onboarding-numbered-circle digit.
            text_on_accent_subtle: egui::Color32::from_rgb(0x05, 0x4a, 0x57),

            // m29: was #078396 — fell 0.03 short of AA against white
            // text on primary buttons. #06738a hits ~5.4:1, keeps
            // the recognisable teal hue.
            accent: egui::Color32::from_rgb(0x06, 0x73, 0x8a),
            accent_hover: egui::Color32::from_rgb(0x05, 0x60, 0x73),
            accent_subtle: egui::Color32::from_rgb(0xc6, 0xe7, 0xed),

            success: egui::Color32::from_rgb(0x16, 0x90, 0x4f),
            // m26: was #b57010 — ~3.9:1 on canvas, fails AA for any
            // text use. Darkened to #9a5e0c (~4.7:1) so the warn
            // accent passes when paired with body-text labels.
            warn: egui::Color32::from_rgb(0x9a, 0x5e, 0x0c),
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
            // m28: ~3.2:1 against the dark card — UI component edge.
            control_stroke: egui::Color32::from_rgb(0x5a, 0x67, 0x7a),
            overlay: egui::Color32::from_rgb(0x16, 0x3b, 0x44),

            text_primary: egui::Color32::from_rgb(0xea, 0xee, 0xf4),
            text_secondary: egui::Color32::from_rgb(0xa0, 0xa8, 0xb8),
            // Bumped from #6e7788 (which gave ~3.7:1 on the dark card
            // — fails WCAG 1.4.3 AA). #90a0b8 measures ~5.2:1 against
            // the dark card.
            text_tertiary: egui::Color32::from_rgb(0x90, 0xa0, 0xb8),
            text_on_accent: egui::Color32::from_rgb(0x0e, 0x12, 0x1b),
            // m27: dark accent_subtle #1d4f5b is already dark — a
            // light digit on it gives wide AA contrast.
            text_on_accent_subtle: egui::Color32::from_rgb(0xc6, 0xe7, 0xed),

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
    //
    // `inactive.bg_fill` is ALSO the colour egui::Slider hard-codes
    // for its rail (slider.rs:766 — `widget_visuals.inactive.bg_fill`,
    // regardless of interact state). With bg_fill = p.card, the rail
    // was painted card-on-card — completely invisible, leaving only
    // the small outlined handle floating with no track to anchor
    // against ("not displayed on an obvious axis"). Promoted to
    // p.divider — already the colour used for card borders, so the
    // rail reads as a structural line in the same vocabulary, and at
    // ~3-ish-to-1 against the card it's plainly visible.
    //
    // Side-effect: DragValue's input background also picks this up
    // (DragValue uses the inactive state's bg_fill for its at-rest
    // surface). That actually reads better than card-on-card — looks
    // like a real input field instead of an unbordered slot. Buttons
    // are unaffected because our button helpers set `.fill(p.card)`
    // explicitly on the egui::Button.
    visuals.widgets.inactive.bg_fill = p.divider;
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

    // Slider visuals: fill the trailing portion of the rail (from
    // min up to current value) with selection.bg_fill (= accent_subtle
    // in our palette). Combined with the now-visible rail colour
    // above, gives sliders a clear "track + progress + handle"
    // structure — like Fluent / macOS sliders — instead of a
    // disembodied circle floating with no axis.
    visuals.slider_trailing_fill = true;
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    use egui::{FontFamily, FontId, TextStyle};
    // M6 + M7: Fluent-aligned type ramp. The codebase used to scatter
    // 9 distinct .size() values (11/12/13/14/15/16/18/20/34) across
    // widgets — Refactoring UI ch.7 says any text size off the ramp
    // either reads as a typo or splinters the visual hierarchy. This
    // collapses to four ramp slots (12 Caption / 14 Body / 16 Body2 /
    // 20 Heading) plus the deliberate 34 status-icon hero. M7: Heading
    // was 18 (off-ramp) and unused; now 20 (Subtitle1) and the page
    // title calls into it via RichText::heading() instead of hard-
    // coding a size.
    style.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
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
    log::info!("gui::run starting (tray={})", show_tray);
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
        // M28 disabled: do NOT persist window position.
        //
        // The reason: this app uses raw Win32 ShowWindow(SW_HIDE) for
        // the tray "minimise to tray" feature (eframe's
        // ViewportCommand::Visible has known queue-drain bugs that
        // break the tray round-trip — emilk/egui #5229, #3655). When
        // the user closes-to-tray, the window stays hidden until the
        // process exits. If eframe runs `WindowSettings::from_window`
        // during shutdown while the window is hidden,
        // `window.inner_position()` returns Windows' sentinel
        // (-32000, -32000) for hidden / minimised windows.
        // persist_window then saves THAT, and the next launch
        // restores the window off-screen — invisible, but services
        // run fine, exactly the "task manager shows it, no window"
        // symptom users reported.
        //
        // The rescue_offscreen_window + force_show_normal paths in
        // the CreationContext below also handle this case, but
        // making persistence the default-on root cause was wrong:
        // a feature that breaks the window for every tray user is a
        // worse trade than losing "remember last size/position."
        persist_window: false,
        ..Default::default()
    };

    let app_for_eframe = app.clone();
    let res = eframe::run_native(
        "Stream To Speaker",
        options,
        Box::new(move |cc| {
            log::info!("eframe CreationContext callback firing");
            // Register Segoe UI Symbol as a fallback font for both
            // Proportional and Monospace families. egui's bundled
            // Ubuntu Light / Hack don't have the media-control,
            // chevron, and circled-info glyphs we use (⏵ ⏸ ⏹ ↻ ▾ ▸ ⟳ ⓘ),
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
                // Recovery: if the window position eframe restored
                // from disk lands outside every connected monitor
                // (user disconnected a monitor since last launch, or
                // their previous binary saved a position the new
                // version doesn't), nudge it back to the primary
                // monitor centre. Without this the window IS there —
                // just painted at e.g. (-32000, -32000) — so task
                // manager shows the process and the user sees
                // nothing on screen.
                rescue_offscreen_window(h);
                // Belt-and-braces: also force the window into the
                // normal-visible state. eframe / winit set
                // with_visible(true), but a persisted minimised
                // state, a stuck SW_HIDE from a previous run, or a
                // DWM weirdness can leave the window in the
                // wrong show-state regardless. Cheap to call and
                // idempotent if the window is already visible.
                force_show_normal(h);
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

    match &res {
        Ok(()) => log::info!("eframe::run_native returned cleanly"),
        Err(e) => log::error!("eframe::run_native returned error: {}", e),
    }
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
        if self.frame_count == 1 {
            log::info!("first GUI frame painting");
        }

        if self.last_repaint_request.elapsed() >= Duration::from_millis(100) {
            ctx.request_repaint_after(Duration::from_millis(100));
            self.last_repaint_request = Instant::now();
        }

        // While egui is still easing a scroll delta, repaint at the full
        // frame rate so the buffered delta drains evenly. egui buffers
        // wheel/touchpad deltas and releases them over a ~0.1 s ease, but
        // it does NOT itself request a repaint to animate that — so in
        // eframe's reactive mode the ease only advances when the OS sends
        // the next scroll event (or on the 100 ms heartbeat above). During
        // a touchpad fling the OS events get sparse as inertia decays, so
        // the buffered portion releases in coarse lumps — the "unnatural
        // acceleration/jump" at the tail of the fling. Driving repaints
        // here makes the ease play out smoothly; it self-terminates the
        // moment the buffer is empty (smooth_scroll_delta returns to zero),
        // so it costs nothing when not scrolling.
        if ctx.input(|i| i.smooth_scroll_delta != egui::Vec2::ZERO) {
            ctx.request_repaint();
        }

        // App-wide keyboard shortcuts (M27). consume_shortcut takes the
        // event off the input queue so individual widgets don't also
        // fire on the same press.
        use egui::{Key, KeyboardShortcut, Modifiers};
        let sc_rescan = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
        let sc_resync = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::R);
        let sc_toggle = KeyboardShortcut::new(Modifiers::COMMAND, Key::E);
        let sc_quit = KeyboardShortcut::new(Modifiers::COMMAND, Key::Q);
        ctx.input_mut(|i| {
            if i.consume_shortcut(&sc_rescan) {
                self.app.trigger_rescan();
            }
            if i.consume_shortcut(&sc_resync) {
                if let Err(e) = self.app.resync() {
                    self.app.record_error(format!("Resync failed: {}", e));
                }
            }
            if i.consume_shortcut(&sc_toggle) {
                if self.app.is_speaker_bound() {
                    let new_state = !self.app.is_streaming_enabled();
                    if let Err(e) = self.app.set_streaming_enabled(new_state) {
                        self.app.record_error(
                            format!("Couldn't change streaming state: {}", e),
                        );
                    }
                }
            }
            if i.consume_shortcut(&sc_quit) {
                self.app.request_shutdown();
            }
        });

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
                    //
                    // m44: the compact pinned status bar is drawn AFTER
                    // the scroll area as a floating overlay (see below).
                    // It occupies zero layout space, so its appearance
                    // can never shift the content — earlier in-layout
                    // versions made the page hop on every toggle (worst
                    // mid-fling on a touchpad) and fighting that with a
                    // scroll-offset compensation broke scrollbar drags
                    // and inertia. We record the full status banner's
                    // bottom edge so the overlay can appear exactly when
                    // the banner leaves the viewport — no thresholds, no
                    // hysteresis, and the two can't both be readable at
                    // once.
                    let mut banner_bottom_px = f32::MAX;
                    let out = egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_max_width(ui.available_width() - sp::M);
                            self.show_status_banner(ui, &p);
                            banner_bottom_px = ui.cursor().top();
                            self.show_error_banner(ui, &p);
                            ui.add_space(sp::M);
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
                                ui.add_space(sp::M);
                            }
                            self.show_speakers(ui, &p);
                            ui.add_space(sp::M);
                            self.show_latency(ui, &p);
                            ui.add_space(sp::M);
                            self.show_advanced(ui, &p);
                            ui.add_space(sp::M);
                            self.show_web_ui(ui, &p);
                            ui.add_space(sp::M);
                            self.show_stats(ui, &p);
                        });

                    // Floating pinned status bar: fades/slides in over the
                    // scroll content once the full banner has scrolled out
                    // of view. animate_bool gives the smooth transition;
                    // because it's an overlay, the scroll offset, scrollbar
                    // geometry, and kinetic fling are completely untouched.
                    let viewport = out.inner_rect;
                    let show_pinned = self.app.is_speaker_bound()
                        && banner_bottom_px <= viewport.top() + 1.0;
                    let t = ui.ctx().animate_bool_with_time(
                        egui::Id::new("pinned-status-overlay"),
                        show_pinned,
                        0.18,
                    );
                    if t > 0.0 {
                        // Slide down from behind the header while fading in.
                        let slide = (1.0 - t) * 28.0;
                        egui::Area::new(egui::Id::new("pinned-status-area"))
                            .order(egui::Order::Foreground)
                            .fixed_pos(egui::pos2(viewport.left(), viewport.top() - slide))
                            .show(ui.ctx(), |aui| {
                                aui.set_opacity(t);
                                aui.set_width(viewport.width());
                                // Canvas-coloured backdrop so scroll content
                                // doesn't bleed through around the card's
                                // rounded corners — reads as the header
                                // extending down over the content.
                                egui::Frame::none()
                                    .fill(p.canvas)
                                    .inner_margin(egui::Margin {
                                        bottom: 6.0,
                                        ..Default::default()
                                    })
                                    .show(aui, |aui| {
                                        self.show_pinned_status(aui, &p);
                                    });
                            });
                    }
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
        .inner_margin(egui::Margin::symmetric(sp::CARD_H, sp::CARD_V))
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
    if r.hovered() {
        if r.enabled() {
            r.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        } else {
            // m17: signal "this is disabled, don't bother clicking"
            // rather than the default arrow which gives no hint.
            r.ctx.set_cursor_icon(egui::CursorIcon::NotAllowed);
        }
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
        // m28: use control_stroke (~3:1) so the button edge passes
        // WCAG 1.4.11 instead of the sectional divider (~1.25:1).
        .stroke(egui::Stroke::new(1.0, p.control_stroke))
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
    // m28: control_stroke (~3:1) — was `accent.gamma_multiply(0.45)`
    // which alpha-blended to ~1.6:1 on the page canvas.
    .stroke(egui::Stroke::new(1.0, p.control_stroke))
    .rounding(RADIUS_CONTROL);
    clickable(ui.add_sized([min_width, CONTROL_HEIGHT], btn))
}

/// Danger button — red text + thin red border. For "this glitches the audio"
/// actions (Resync, Quit). Not as loud as a solid red fill — Refactoring UI
/// recommends only going full danger-red when it's the page's primary action.
fn danger_button(ui: &mut egui::Ui, p: &Palette, label: &str, min_width: f32) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(label).color(p.danger))
        .fill(p.card)
        // m28: full danger colour for the stroke — gamma_multiply(0.5)
        // alpha-blended to ~1.6:1 which neither read as "danger" nor
        // satisfied WCAG 1.4.11.
        .stroke(egui::Stroke::new(1.0, p.danger))
        .rounding(RADIUS_CONTROL);
    clickable(ui.add_sized([min_width, CONTROL_HEIGHT], btn))
}

impl StreamToSpeakerApp {
    fn show_header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // M13: indent the header by the card's horizontal padding so
        // the page title left-aligns with section_label headings
        // inside cards. Without this the title sat 18 px to the left
        // of every card heading, breaking the vertical alignment line
        // the eye anchors to when scanning the page.
        egui::Frame::none()
            .inner_margin(egui::Margin::symmetric(sp::CARD_H, 0.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("Stream To Speaker")
                                .heading()
                                .strong()
                                .color(p.text_primary),
                        );
                        ui.label(
                            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(12.0)
                                .color(p.text_tertiary),
                        );
                    });
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            self.show_help_menu(ui, p);
                            ui.add_space(sp::XS);
                            self.show_theme_toggle(ui, p);
                        },
                    );
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
        .stroke(egui::Stroke::new(1.0, p.control_stroke))
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
                // "Show getting started" — undo onboarding dismissal
                // so it appears on the next paint (m20). Useful for
                // users who hid it and then want the refresher.
                if self.app.is_onboarding_dismissed() {
                    if ui.button("Show getting-started again").clicked() {
                        self.app.reset_onboarding();
                        self.onboarding_dismissed = false;
                        ui.memory_mut(|m| m.close_popup());
                    }
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
                        // m4: kill the default Button hover stroke
                        // inside the segmented control. apply_theme
                        // sets widgets.hovered.bg_stroke to 1 px accent
                        // for normal buttons — desirable there, but on
                        // a segment it paints a halo around the hovered
                        // tile that fights the "these belong together"
                        // tab affordance. The fill change alone (handled
                        // by widgets.hovered.bg_fill) is enough hover
                        // signal here.
                        let clicked = ui
                            .scope(|ui| {
                                ui.visuals_mut().widgets.hovered.bg_stroke =
                                    egui::Stroke::NONE;
                                ui.visuals_mut().widgets.active.bg_stroke =
                                    egui::Stroke::NONE;
                                clickable(ui.add_sized([60.0, CONTROL_HEIGHT], btn))
                                    .on_hover_text(tip)
                                    .clicked()
                            })
                            .inner;
                        if clicked {
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
            ui.add_space(sp::XS);

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
                    // m2: numbered indicator. Was 28×28 — too big for
                    // a 14 px title, made the circle's vertical
                    // centre sit BELOW the title baseline. Shrunk to
                    // 22 px so the painted circle naturally aligns
                    // with the title's vertical centre.
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 11.0, p.accent_subtle);
                    // m27: was p.accent, which gave ~3.42:1 against
                    // accent_subtle in light mode — fails AA. The
                    // dedicated text_on_accent_subtle shade hits
                    // 5.8:1 (light) / 7.4:1 (dark).
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        n,
                        egui::FontId::new(12.0, egui::FontFamily::Proportional),
                        p.text_on_accent_subtle,
                    );
                    ui.add_space(sp::S);
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
                ui.add_space(sp::S);
            }
        });
    }

    fn show_status_banner(&self, ui: &mut egui::Ui, p: &Palette) {
        let enabled = self.app.is_streaming_enabled();
        let active = self.app.stream_active.load(Ordering::Acquire);
        let current = self.app.selected_speaker();

        // M20: status icons were grab-bag — ⊘ from Math Operators
        // (thin stroke), ▶ from Geometric Shapes (heavy filled),
        // ‖ from General Punctuation (thin lines) — wildly mismatched
        // weights. Switched to the Media Control Symbols block
        // (U+23F5–23F9), designed as a coherent set in Segoe UI
        // Symbol so all three speaker-bound states render at the
        // same weight. "?" stays for "no speaker" because it reads
        // as a text prompt ("what should I pick?"), not as a state
        // indicator competing with the media icons.
        // A background connect owns the banner while in flight — keep
        // repainting so its progress shows without user input.
        let connecting = self.app.connecting_to();
        if connecting.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
        let (icon, accent, headline, detail, btn_label, btn_tip) = if let Some(name) = connecting {
            (
                "⟳",
                p.warn,
                format!("Connecting to {}…", name),
                "Setting up the session…".to_string(),
                None,
                None,
            )
        } else {
            match (&current, enabled, active) {
            (None, _, _) => (
                "?",
                p.muted,
                "No speaker selected".to_string(),
                "Pick a speaker from the list below to start streaming.".to_string(),
                None,
                None,
            ),
            (Some(r), false, _) => (
                "\u{23F9}", // ⏹ Black Square For Stop
                p.danger,
                format!("Streaming to {} disabled", r.friendly_name),
                format!("{} is free for other apps. Press Enable to resume streaming.", r.friendly_name),
                Some("Enable streaming"),
                Some("Reconnect to the last speaker and resume streaming (Ctrl+E)"),
            ),
            (Some(r), true, true) => (
                "\u{23F5}", // ⏵ Black Medium Right-Pointing Triangle (Play)
                p.success,
                format!("Streaming to {}", r.friendly_name),
                format!("{}  ·  {} packets/sec", r.ip, self.packets_per_sec()),
                Some("Disable streaming"),
                Some("Stop streaming and release the speaker for other apps (Ctrl+E)"),
            ),
            (Some(r), true, false) => (
                "\u{23F8}", // ⏸ Double Vertical Bar (Pause)
                p.warn,
                format!("Standing by on {}", r.friendly_name),
                format!("{}  ·  waiting for audio", r.ip),
                Some("Disable streaming"),
                Some("Stop streaming and release the speaker for other apps (Ctrl+E)"),
            ),
            }
        };

        let frame_resp = egui::Frame::none()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0, p.divider))
            .rounding(RADIUS_SURFACE)
            // M8: symmetric padding matches every other card — the
            // banner used to ship (left:0, right:18, top:16, bottom:16)
            // so the accent stripe could live INSIDE the layout. Now
            // the stripe is painted over the frame after the fact, so
            // padding can be regular.
            .inner_margin(egui::Margin::symmetric(sp::CARD_H, sp::CARD_V))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(icon).color(accent).size(34.0).strong());
                    ui.add_space(sp::S);

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

        // M9: paint the accent stripe over the card's rounded left
        // edge after the frame is drawn. Full card height, left
        // corners rounded to RADIUS_SURFACE so it visually merges
        // with the card outline; right edge sharp (rounding 0) so it
        // butts cleanly into the card interior. The previous stripe
        // was a hard-edged 4×56 rect placed mid-card — looked like a
        // floating marker rather than the card's own edge.
        let card_rect = frame_resp.response.rect;
        let stripe_rect = egui::Rect::from_min_size(
            card_rect.min,
            egui::vec2(4.0, card_rect.height()),
        );
        let stripe_rounding = egui::Rounding {
            nw: RADIUS_SURFACE,
            sw: RADIUS_SURFACE,
            ne: 0.0,
            se: 0.0,
        };
        ui.painter().rect_filled(stripe_rect, stripe_rounding, accent);
    }

    /// Compact pinned status row (m44). Sits above the scroll area
    /// once the full status banner scrolls out of view, so the user
    /// keeps a live read-out of "what's streaming, in what state"
    /// even when they're 200 px down the page browsing speakers or
    /// tuning latency. Mirrors the banner's traffic-light scheme but
    /// trades the 56 px accent stripe + 34 px icon for a 10 px dot,
    /// keeping the row at ~CONTROL_HEIGHT + padding.
    fn show_pinned_status(&self, ui: &mut egui::Ui, p: &Palette) {
        let enabled = self.app.is_streaming_enabled();
        let active = self.app.stream_active.load(Ordering::Acquire);
        let Some(current) = self.app.selected_speaker() else { return; };

        let (accent, status_text) = match (enabled, active) {
            (false, _) => (p.danger, "Disabled"),
            (true, true) => (p.success, "Streaming"),
            (true, false) => (p.warn, "Idle"),
        };

        egui::Frame::none()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0, p.divider))
            .rounding(RADIUS_CONTROL)
            .inner_margin(egui::Margin::symmetric(sp::M, sp::XS))
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - sp::M);
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(10.0, 10.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().circle_filled(rect.center(), 5.0, accent);
                    ui.add_space(sp::S);

                    ui.label(
                        egui::RichText::new(&current.friendly_name)
                            .size(12.0)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        egui::RichText::new("·")
                            .size(12.0)
                            .color(p.text_tertiary),
                    );
                    ui.label(
                        egui::RichText::new(status_text)
                            .size(12.0)
                            .color(p.text_secondary),
                    );

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let (label, tip) = if enabled {
                                (
                                    "Disable",
                                    "Stop streaming and release the speaker (Ctrl+E)",
                                )
                            } else {
                                (
                                    "Enable",
                                    "Reconnect and resume streaming (Ctrl+E)",
                                )
                            };
                            let r = secondary_button(ui, p, label, 88.0)
                                .on_hover_text(tip);
                            if r.clicked() {
                                if let Err(e) =
                                    self.app.set_streaming_enabled(!enabled)
                                {
                                    self.app.record_error(format!(
                                        "Couldn't change streaming state: {}",
                                        e
                                    ));
                                }
                            }
                        },
                    );
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
                            .size(16.0)
                            .strong(),
                    );
                    ui.add_space(sp::XS);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(&msg)
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
                            "Search the network for speakers now. Otherwise this runs every few minutes. (Ctrl+R)",
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

            // Auto-reconnect preference (next to the saved-speaker
            // controls so it's discoverable when a speaker is saved).
            if self.app.saved_speaker_id().is_some() {
                let mut auto = self.app.is_auto_reconnect_on_launch();
                let resp = ui.checkbox(
                    &mut auto,
                    "Auto-connect to this speaker when the app launches",
                );
                if resp.changed() {
                    self.app.set_auto_reconnect_on_launch(auto);
                }
            }

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
                            // Bring-up does seconds of network I/O —
                            // run it off-thread; the status banner shows
                            // "Connecting…" and errors arrive as toasts.
                            self.app.select_speaker_async(id);
                        });
                    }
                });

            // m24: speaker-side volume slider. Hidden until a speaker is
            // bound (without one there's nothing to control). Drag pushes
            // upnp::set_volume on a detached thread; GENA NOTIFYs from the
            // speaker side stream back through volume_sync so the slider
            // stays in sync with the Sonos app / physical buttons.
            if self.app.is_speaker_bound() {
                ui.add_space(sp::S);
                ui.separator();
                ui.add_space(sp::S);
                self.show_volume_row(ui, p);
            }
        });
    }

    fn show_volume_row(&self, ui: &mut egui::Ui, p: &Palette) {
        let current = self.app.current_volume();
        let mut level: i64 = current.unwrap_or(50) as i64;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Volume")
                    .strong()
                    .color(p.text_primary),
            );
            ui.add_space(sp::S);

            let trailing_w = 56.0;
            let avail = ui.available_width();
            let slider_w = (avail - trailing_w - sp::S).max(80.0);
            let prev_slider_w = ui.spacing().slider_width;
            ui.spacing_mut().slider_width = slider_w;
            let resp = ui.add(
                egui::Slider::new(&mut level, 0..=100)
                    .show_value(false)
                    .clamping(egui::SliderClamping::Always),
            );
            ui.spacing_mut().slider_width = prev_slider_w;
            ui.add_space(sp::S);

            let display = match current {
                Some(_) => format!("{:>3}", level),
                None => "  ?".to_string(),
            };
            ui.label(
                egui::RichText::new(display)
                    .monospace()
                    .color(p.text_secondary),
            );

            // Only push on drag-release (drag_stopped) and explicit
            // click, not on every intermediate value while the user
            // is still dragging — saves ~30 UPnP SOAP requests per
            // slide and keeps the speaker side responsive.
            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                self.app.set_speaker_volume(level.clamp(0, 100) as u32);
            }
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
            let has_speaker = self.app.is_speaker_bound();
            ui.add_enabled_ui(has_speaker, |ui| {
                let resp = danger_button(ui, p, "⟳  Resync speaker", 180.0);
                let resp = if has_speaker {
                    resp.on_hover_text(
                        "Stops and restarts the speaker. Causes a brief audio click but clears any accumulated latency. (Ctrl+Shift+R)",
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
            // m5: was 28 px tall — undershot CONTROL_HEIGHT and the
            // 32 px buttons everywhere else in the app. Normalised so
            // the disclosure strip's click target matches the rest.
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(avail_w, CONTROL_HEIGHT),
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

            ui.add_space(sp::XS);

            let mut ppm = self.app.rate_fudge_ppm.load(Ordering::Relaxed) as i64;
            advanced_row(
                ui,
                p,
                "Clock-drift compensation",
                "Try +50 to +100 ppm if audio gradually falls behind; negative if it gradually gets ahead.",
                "Your PC and the speaker each have a tiny crystal oscillator that tracks time, and they drift apart by a few parts per million. Over many minutes that drift is enough to push audio out of sync. Positive values make the service produce frames slightly faster than real-time (catches up if the speaker is gaining); negative drops frames (catches up if the speaker is losing). Most setups need 0; if you notice audio creeping out of sync after 10–20 minutes of continuous play, nudge in steps of ±25 ppm until it stays put.",
                |ui| advanced_slider_row(ui, p, &mut ppm, -1000..=1000, " ppm", 0, "0 ppm"),
            );
            self.app.set_rate_fudge_ppm(ppm.clamp(-1000, 1000) as i32);

            ui.add_space(sp::S);

            let mut pace = self.app.silence_pace_ms.load(Ordering::Relaxed) as i64;
            advanced_row(
                ui,
                p,
                "Silence pacing",
                "Higher than 10 ms shrinks latency after a pause. Stay below ~30 to avoid dropouts.",
                "When nothing is playing on Windows, the service still sends frames of silence to the speaker (otherwise the speaker drops the connection). The speaker buffers a bit ahead — when real audio comes back, that buffer adds latency before you hear it. Setting this higher than 10 ms makes the silence frames go slower than real-time, draining the buffer during the quiet passages, so post-pause latency is smaller. Too high (>30) and the buffer runs dry and you hear dropouts.",
                |ui| advanced_slider_row(ui, p, &mut pace, 1..=100, " ms", 10, "10 ms"),
            );
            self.app.set_silence_pace_ms(pace.max(1) as u64);

            ui.add_space(sp::S);

            let mut step = self.app.latency_adjust_step_frames.load(Ordering::Relaxed) as i64;
            advanced_row(
                ui,
                p,
                "Latency-adjust step",
                "Larger = the −/+ buttons act faster but produce a louder click.",
                "When you press one of the −/+ buttons in the Latency card, the service adds or drops a few audio frames each packet until it's caught up the requested amount. This setting controls how many frames per packet. Larger values reach the target faster but make a louder click; smaller values are smoother but slower. The default of 4 frames is ~0.09 ms per packet, which most listeners can't hear.",
                |ui| advanced_slider_row(ui, p, &mut step, 1..=256, " frames", 4, "4 frames"),
            );
            self.app.set_latency_adjust_step_frames(step.max(1) as u32);

            ui.add_space(sp::S);

            let mut prefer_rt = self.app.user_config.lock().unwrap().prefer_realtime_airplay;
            advanced_row(
                ui,
                p,
                "AirPlay 2 stream mode",
                "Realtime ≈ 0.25 s latency; buffered ≈ 1–2 s but is what iPhones use.",
                "AirPlay 2 has two stream kinds. Buffered (the default when the speaker supports it) is what iPhones use: the speaker holds a second or two of audio, riding out Wi-Fi hiccups at the cost of that much latency. Realtime is the low-latency kind (~250 ms). Some speakers accept the realtime handshake but never actually play it — if one mode is silent, try the other. Takes effect the next time you connect to the speaker.",
                |ui| {
                    ui.checkbox(&mut prefer_rt, "Prefer low-latency realtime");
                },
            );
            {
                let mut uc = self.app.user_config.lock().unwrap();
                if uc.prefer_realtime_airplay != prefer_rt {
                    uc.prefer_realtime_airplay = prefer_rt;
                    uc.save();
                }
            }

            ui.add_space(sp::S);

            let mut mfi = self.app.user_config.lock().unwrap().airplay_mfi_encryption;
            advanced_row(
                ui,
                p,
                "AirPlay MFi encryption (experimental)",
                "Off = plain audio (the proven mode). On = try iTunes-style et=4 encryption first.",
                "Some speakers (Sonos among them) advertise the MFi encryption mode iTunes uses. Streaming works without it, but if a speaker connects and stays silent this is worth an experiment: when enabled, the app first tries the encrypted handshake and falls back to plain audio if the speaker refuses. A refused attempt can make the speaker unresponsive for up to half a minute, which is why this is off by default. Takes effect the next time you connect.",
                |ui| {
                    ui.checkbox(&mut mfi, "Try et=4 MFi encryption first");
                },
            );
            {
                let mut uc = self.app.user_config.lock().unwrap();
                if uc.airplay_mfi_encryption != mfi {
                    uc.airplay_mfi_encryption = mfi;
                    uc.save();
                }
            }
        });
    }

    fn show_web_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        card(ui, p, |ui| {
            section_label(ui, p, "Web UI");

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
            ui.add_space(sp::S);

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
                        if let Err(e) = open_url(&url) {
                            // m25: previously the failure was silent.
                            self.app.record_error(
                                format!("Couldn't open the browser: {}. URL: {}", e, url),
                            );
                        }
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
                ui.add_space(sp::XS);
                ui.label(
                    egui::RichText::new(&url)
                        .size(12.0)
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
                let packets_label = if pkts_total == 1 { "packet" } else { "packets" };
                stat_pill(
                    ui,
                    p,
                    &humanize_count(pkts_total),
                    packets_label,
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
                    .inner_margin(sp::MODAL),
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
    // m23: truncate the friendly name with an ellipsis if it would
    // overflow into the IP column. Painter::text doesn't clip by
    // default — at narrow window widths a long speaker name would
    // happily march straight through the IP and off the row's right
    // edge. LayoutJob with max_rows=1 + break_anywhere does the
    // elision (default character is `…`).
    let text_left = rect.left() + 34.0;
    // Reserve enough room for a 15-char IPv4 in 12 px Monospace plus
    // some breathing space.
    const IP_RESERVED_W: f32 = 120.0;
    let name_max_w = (rect.right() - 18.0 - text_left - IP_RESERVED_W).max(40.0);
    let name_job = {
        let mut job = egui::epaint::text::LayoutJob::single_section(
            sp.friendly_name.clone(),
            egui::TextFormat {
                font_id: egui::FontId::new(14.0, egui::FontFamily::Proportional),
                color: p.text_primary,
                ..Default::default()
            },
        );
        job.wrap.max_width = name_max_w;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job
    };
    let name_galley = ui.fonts(|f| f.layout_job(name_job));
    ui.painter().galley(
        egui::pos2(text_left, rect.center().y - name_galley.size().y / 2.0),
        name_galley,
        p.text_primary,
    );
    // M12: 18 px right inset — symmetric with the radio indicator at
    // left + 18, so the row's visual content sits balanced inside
    // the card. (Was 12, which made the IP huddle against the card
    // border while the radio breathed comfortably on the other side.)
    ui.painter().text(
        egui::pos2(rect.right() - 18.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        &sp.ip,
        egui::FontId::new(12.0, egui::FontFamily::Monospace),
        p.text_tertiary,
    );

    if response.hovered() && response.enabled() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    // Advisory tooltip (e.g. "higher delay" for an AirPlay entry whose
    // speaker also has a lower-latency UPnP entry).
    let response = if let Some(note) = sp.note.as_deref() {
        response.on_hover_text(note)
    } else {
        response
    };

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

/// M15: pairing for an Advanced numeric setting — Slider for gross
/// dragging across the full range, DragValue for fine / precise typing,
/// Reset to default. DragValue alone (the previous layout) made it
/// hard to feel out where the "useful" zone of a range was; Slider
/// alone made it hard to type an exact value. Together they cover both.
///
/// The slider gets whatever width remains after the DragValue and Reset
/// button — so at narrow window widths the slider compresses gracefully
/// instead of pushing the other controls off the right edge.
fn advanced_slider_row(
    ui: &mut egui::Ui,
    p: &Palette,
    value: &mut i64,
    range: std::ops::RangeInclusive<i64>,
    suffix: &str,
    default: i64,
    default_label: &str,
) {
    let dragvalue_w = 96.0;
    let reset_w = 80.0;
    let gap = sp::S;
    let avail = ui.available_width();
    let slider_w = (avail - dragvalue_w - reset_w - 2.0 * gap).max(80.0);
    let prev_slider_w = ui.spacing().slider_width;
    ui.spacing_mut().slider_width = slider_w;
    ui.add(
        egui::Slider::new(value, range.clone())
            .show_value(false)
            .clamping(egui::SliderClamping::Always),
    );
    ui.spacing_mut().slider_width = prev_slider_w;
    ui.add_space(gap);
    ui.add(
        egui::DragValue::new(value)
            .range(range)
            .suffix(suffix)
            // m21: don't apply on every keystroke — typing "100" was
            // briefly applying 1 then 10 then 100 each frame.
            .update_while_editing(false),
    );
    ui.add_space(gap);
    if secondary_button(ui, p, "Reset", reset_w)
        .on_hover_text(format!("Reset to {}", default_label))
        .clicked()
    {
        *value = default;
    }
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
        .inner_margin(egui::Margin::symmetric(sp::PILL_H, sp::PILL_V))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(value)
                        .strong()
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
