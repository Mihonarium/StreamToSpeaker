//! System tray icon + context menu.
//!
//! `muda`'s `MenuItem` is `!Send` (it holds an `Rc` internally), so the
//! menu items themselves live on the GUI's main thread. `pump()` runs
//! from the egui update loop and keeps the status label / check states
//! synced.
//!
//! Tray + menu *events* are drained by a dedicated background thread,
//! not by `pump()`. This is the fix for the "Show window from tray
//! does nothing after minimize-to-tray" bug: egui's `update()` only
//! runs in response to `WM_PAINT`, and a hidden window doesn't
//! generate `WM_PAINT` — so if event draining lives in `pump()`, the
//! Show-window menu click sits in the channel forever. The bg thread
//! is unaffected by paint state and just calls `ShowWindow` directly
//! when the click arrives.

#![cfg(windows)]

use anyhow::{anyhow, Result};
use eframe::egui;
use log::{info, warn};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};

use crate::app::App;

/// Owns the tray icon and the menu items. Dropping it removes the
/// icon; the menu items go with it.
pub struct TrayHandle {
    _icon: tray_icon::TrayIcon,

    // Menu items are !Send, so they live on the GUI thread.
    status_item: MenuItem,
    enable_toggle: CheckMenuItem,
    open_web_item: MenuItem,

    last_label: String,
    last_enabled: bool,
    last_web_enabled: bool,

    /// HWND of the GUI window. Shared with the bg event thread; that
    /// thread calls raw Win32 `ShowWindow` on it when the user clicks
    /// the tray icon. Populated by `gui::run` once eframe has actually
    /// created the OS window (zero means "not set yet" — the very
    /// first frame is always visible, so the bg thread has nothing to
    /// do until the window is hidden, which happens later).
    hwnd: Arc<AtomicIsize>,
}

#[derive(Clone)]
struct TrayIds {
    switch_speaker: MenuId,
    trim_25: MenuId,
    trim_100: MenuId,
    pad_25: MenuId,
    pad_100: MenuId,
    resync: MenuId,
    enable: MenuId,
    show_window: MenuId,
    open_web: MenuId,
    quit: MenuId,
}

/// Build and install the tray icon. The handle must live as long as
/// the GUI process (drop it to remove the icon).
pub fn spawn(app: Arc<App>, egui_ctx: egui::Context) -> Result<TrayHandle> {
    let menu = Menu::new();

    let status_item = MenuItem::new("(no speaker)", false, None);
    menu.append(&status_item)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let switch_speaker = MenuItem::new("Switch speaker…", true, None);
    let trim_25 = MenuItem::new("Trim 25 ms", true, None);
    let trim_100 = MenuItem::new("Trim 100 ms", true, None);
    let pad_25 = MenuItem::new("Pad 25 ms", true, None);
    let pad_100 = MenuItem::new("Pad 100 ms", true, None);
    let resync = MenuItem::new("Resync (hard reset)", true, None);
    menu.append(&switch_speaker)?;
    menu.append(&trim_25)?;
    menu.append(&trim_100)?;
    menu.append(&pad_25)?;
    menu.append(&pad_100)?;
    menu.append(&resync)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let enable_toggle = CheckMenuItem::new(
        "Streaming enabled",
        true,
        app.is_streaming_enabled(),
        None,
    );
    menu.append(&enable_toggle)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let show_window = MenuItem::new("Show window", true, None);
    let web_on = app.is_web_ui_enabled();
    let open_web = MenuItem::new(
        if web_on {
            "Open web UI"
        } else {
            "Open web UI (disabled — enable in window)"
        },
        web_on,
        None,
    );
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append(&show_window)?;
    menu.append(&open_web)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let ids = TrayIds {
        switch_speaker: switch_speaker.id().clone(),
        trim_25: trim_25.id().clone(),
        trim_100: trim_100.id().clone(),
        pad_25: pad_25.id().clone(),
        pad_100: pad_100.id().clone(),
        resync: resync.id().clone(),
        enable: enable_toggle.id().clone(),
        show_window: show_window.id().clone(),
        open_web: open_web.id().clone(),
        quit: quit_item.id().clone(),
    };

    let icon = build_icon()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(crate::PRODUCT_NAME)
        .with_icon(icon)
        .build()
        .map_err(|e| anyhow!("tray-icon build: {}", e))?;

    let hwnd = Arc::new(AtomicIsize::new(0));

    // Spawn the bg event handler thread. It owns clones of the App
    // Arc, the egui Context, the TrayIds (MenuId is Clone), and the
    // shared HWND atomic. Runs until process exit.
    {
        let app = app.clone();
        let ctx = egui_ctx.clone();
        let ids = ids.clone();
        let hwnd = hwnd.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-tray-events".to_string())
            .spawn(move || event_loop(app, ctx, ids, hwnd))
            .ok();
    }

    Ok(TrayHandle {
        _icon: tray,
        status_item,
        enable_toggle,
        open_web_item: open_web,
        last_label: String::new(),
        last_enabled: app.is_streaming_enabled(),
        last_web_enabled: web_on,
        hwnd,
    })
}

impl TrayHandle {
    /// Set the HWND the bg event thread uses to show the window when
    /// the user clicks the tray icon. Called by `gui::run` once eframe
    /// has actually created the OS window (HWND is stable for the
    /// lifetime of the window).
    pub fn set_hwnd(&mut self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Relaxed);
    }

    /// Update the visible bits of the menu (status label, check
    /// states) to reflect the App. Called from `gui::update()` once
    /// per frame; cheap when there's nothing to do. Event draining
    /// happens on the bg event thread now (see module-level docs).
    pub fn pump(&mut self, app: &Arc<App>, _ctx: &egui::Context) {
        // Sync the status label.
        let label = match app.current_renderer() {
            Some(r) => format!("▶ {}", r.friendly_name),
            None => "(no speaker)".to_string(),
        };
        if label != self.last_label {
            self.status_item.set_text(&label);
            self.last_label = label;
        }

        // Sync the enable checkbox.
        let enabled = app.is_streaming_enabled();
        if enabled != self.last_enabled {
            self.enable_toggle.set_checked(enabled);
            self.last_enabled = enabled;
        }

        // Sync the "Open web UI" item label + enabled state.
        let web_on = app.is_web_ui_enabled();
        if web_on != self.last_web_enabled {
            self.open_web_item.set_text(if web_on {
                "Open web UI"
            } else {
                "Open web UI (disabled — enable in window)"
            });
            self.open_web_item.set_enabled(web_on);
            self.last_web_enabled = web_on;
        }
    }
}

/// Background event loop. Drains MenuEvent + TrayIconEvent and acts
/// on them directly via the App Arc / raw Win32 calls. Runs from
/// `spawn()` until process exit; the process exit kills the thread
/// (we never explicitly join, mirroring the audio-thread pattern).
fn event_loop(
    app: Arc<App>,
    ctx: egui::Context,
    ids: TrayIds,
    hwnd: Arc<AtomicIsize>,
) {
    use crossbeam_channel::select;
    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();
    loop {
        select! {
            recv(menu_rx) -> ev => {
                match ev {
                    Ok(MenuEvent { id }) => handle_menu(&id, &app, &ids, &hwnd, &ctx),
                    Err(_) => return, // channel closed (shouldn't happen)
                }
            }
            recv(tray_rx) -> ev => {
                match ev {
                    Ok(TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    }) => {
                        show_window_win32(hwnd.load(Ordering::Relaxed));
                        ctx.request_repaint();
                    }
                    Ok(_) => {} // right/middle clicks, double-click, etc.
                    Err(_) => return,
                }
            }
        }
    }
}

fn handle_menu(
    id: &MenuId,
    app: &Arc<App>,
    ids: &TrayIds,
    hwnd: &AtomicIsize,
    ctx: &egui::Context,
) {
    if *id == ids.trim_25 {
        app.adjust_latency(25);
    } else if *id == ids.trim_100 {
        app.adjust_latency(100);
    } else if *id == ids.pad_25 {
        app.adjust_latency(-25);
    } else if *id == ids.pad_100 {
        app.adjust_latency(-100);
    } else if *id == ids.resync {
        if let Err(e) = app.resync() {
            warn!("tray resync failed: {}", e);
        }
    } else if *id == ids.enable {
        let new_state = !app.is_streaming_enabled();
        if let Err(e) = app.set_streaming_enabled(new_state) {
            warn!("tray toggle failed: {}", e);
        }
    } else if *id == ids.switch_speaker || *id == ids.show_window {
        show_window_win32(hwnd.load(Ordering::Relaxed));
    } else if *id == ids.open_web {
        if app.is_web_ui_enabled() {
            open_web_ui(&app.config.advertise_ip, app.config.bind.port());
        }
    } else if *id == ids.quit {
        info!("tray: quit requested");
        app.request_shutdown();
        // Force the window visible so gui.rs::update() runs and
        // executes its shutdown sequence (drop tray, ViewportCommand
        // Close). Without this, a hidden window stays hidden and
        // update() never sees the shutdown flag.
        show_window_win32(hwnd.load(Ordering::Relaxed));
    }
    ctx.request_repaint();
}

/// Raw Win32 "bring this window to the foreground" sequence.
/// `SetWindowPos` with `SWP_SHOWWINDOW` flips visibility AND lifts
/// Z-order in one synchronous call, sidestepping the eframe
/// ViewportCommand queue (which doesn't drain on hidden windows —
/// emilk/egui#5229, #3655). `SW_RESTORE` handles the minimised case.
/// `ShowWindowAsync` is used for `SW_RESTORE` so the call is safe
/// from a non-owning thread (this fn runs on the tray bg thread).
fn show_window_win32(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, SetWindowPos, ShowWindowAsync,
        HWND_TOP, SW_RESTORE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };
    unsafe {
        let h = hwnd as _;
        SetWindowPos(h, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
        if IsIconic(h) != 0 {
            ShowWindowAsync(h, SW_RESTORE);
        }
        SetForegroundWindow(h);
    }
}

fn open_web_ui(advertise_ip: &str, port: u16) {
    let url = format!("http://{}:{}/", advertise_ip, port);
    if let Err(e) = std::process::Command::new("cmd")
        .args(["/C", "start", "", &url])
        .spawn()
    {
        warn!("failed to open browser to {}: {}", url, e);
    }
}

// -----------------------------------------------------------------------------
// Icon (procedurally drawn — no PNG/ICO to ship)
// -----------------------------------------------------------------------------

fn build_icon() -> Result<Icon> {
    // 32×32 — Windows downsamples to 16×16 / 24×24 / 20×20 depending
    // on DPI. Shapes need to be bold enough that downsampling doesn't
    // collapse them into a featureless blob (which is what the
    // previous design did, hence "it's a square").
    const W: u32 = 32;
    const H: u32 = 32;
    let mut pixels = vec![0u8; (W * H * 4) as usize];
    // Transparent background — all zeros (RGBA 0,0,0,0).
    let fg = [60u8, 180, 100, 255]; // accent green, opaque

    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let xi = x as i32;
            let yi = y as i32;

            // 1. Speaker body — solid vertical rectangle on the left.
            //    Width 7, height 11, centered vertically at y=16.
            let body = xi >= 5 && xi <= 11 && yi >= 11 && yi <= 21;

            // 2. Cone — trapezoid opening to the right, attaches to
            //    the body's right edge. At x=11 the cone matches the
            //    body's height; at x=22 it widens to fill 24 px
            //    vertically.
            //    half_height(dx) = 5 + dx * 7 / 11   (5 → 12, linear)
            let cone_dx = xi - 11;
            let half_h = 5 + (cone_dx * 7 + 5) / 11;
            let cone =
                cone_dx >= 0 && cone_dx <= 11 && (yi - 16).abs() <= half_h;

            if body || cone {
                pixels[i] = fg[0];
                pixels[i + 1] = fg[1];
                pixels[i + 2] = fg[2];
                pixels[i + 3] = fg[3];
            }
        }
    }

    Icon::from_rgba(pixels, W, H).map_err(|e| anyhow!("icon: {}", e))
}
