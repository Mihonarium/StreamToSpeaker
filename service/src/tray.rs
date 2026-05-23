//! System tray icon + context menu.
//!
//! `muda`'s `MenuItem` is `!Send` (it holds an `Rc` internally), so the
//! menu items live on the GUI's main thread. Tray + menu events are
//! drained from the egui `update()` loop via `TrayHandle::pump`. The
//! same loop also keeps the status label and check-state synced with
//! the shared `App`.

#![cfg(windows)]

use anyhow::{anyhow, Result};
use eframe::egui;
use log::{info, warn};
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

    // Ids used to route MenuEvents to actions.
    ids: TrayIds,

    last_label: String,
    last_enabled: bool,
}

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
pub fn spawn(app: Arc<App>, _egui_ctx: egui::Context) -> Result<TrayHandle> {
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
    let open_web = MenuItem::new(
        if app.config.web_enabled {
            "Open web UI"
        } else {
            "Open web UI (start with --web)"
        },
        app.config.web_enabled,
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

    Ok(TrayHandle {
        _icon: tray,
        status_item,
        enable_toggle,
        ids,
        last_label: String::new(),
        last_enabled: app.is_streaming_enabled(),
    })
}

impl TrayHandle {
    /// Drain pending tray and menu events, update the visible bits of
    /// the menu (status label, check state) to reflect the App. Called
    /// from `gui::update()` once per frame; cheap when there's nothing
    /// to do.
    pub fn pump(&mut self, app: &Arc<App>, ctx: &egui::Context) {
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

        // Drain menu events. The receiver is process-global; events
        // from any tray icon in this process flow through it.
        while let Ok(MenuEvent { id }) = MenuEvent::receiver().try_recv() {
            self.handle_menu_event(&id, app, ctx);
        }

        // Drain tray-icon events: left-click → show window.
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                show_main_window(ctx);
            }
        }
    }

    fn handle_menu_event(&self, id: &MenuId, app: &Arc<App>, ctx: &egui::Context) {
        if *id == self.ids.trim_25 {
            app.adjust_latency(25);
        } else if *id == self.ids.trim_100 {
            app.adjust_latency(100);
        } else if *id == self.ids.pad_25 {
            app.adjust_latency(-25);
        } else if *id == self.ids.pad_100 {
            app.adjust_latency(-100);
        } else if *id == self.ids.resync {
            if let Err(e) = app.resync() {
                warn!("resync failed: {}", e);
            }
        } else if *id == self.ids.enable {
            let new_state = !app.is_streaming_enabled();
            if let Err(e) = app.set_streaming_enabled(new_state) {
                warn!("toggle failed: {}", e);
            }
        } else if *id == self.ids.switch_speaker || *id == self.ids.show_window {
            show_main_window(ctx);
        } else if *id == self.ids.open_web {
            if app.config.web_enabled {
                open_web_ui(&app.config.advertise_ip, app.config.bind.port());
            }
        } else if *id == self.ids.quit {
            info!("tray: quit requested");
            app.request_shutdown();
            ctx.request_repaint();
        }
        ctx.request_repaint();
    }
}

fn show_main_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
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
    const W: u32 = 32;
    const H: u32 = 32;
    let mut pixels = vec![0u8; (W * H * 4) as usize];

    let fg = [60u8, 180, 100, 255];
    let bg = [16u8, 16, 16, 0]; // transparent

    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let xc = x as i32 - 16;
            let yc = y as i32 - 16;
            let r2 = xc * xc + yc * yc;
            let speaker_body = (xc.abs() <= 5 && yc.abs() <= 10)
                || (r2 < 14 * 14 && xc > 5)
                || (r2 < 6 * 6);
            let color = if speaker_body { fg } else { bg };
            pixels[i] = color[0];
            pixels[i + 1] = color[1];
            pixels[i + 2] = color[2];
            pixels[i + 3] = color[3];
        }
    }

    Icon::from_rgba(pixels, W, H).map_err(|e| anyhow!("icon: {}", e))
}
