//! System tray icon + context menu.
//!
//! Owns a background thread that listens for menu-item clicks and
//! dispatches them to the shared `App`. Left-click on the tray icon
//! shows the main window. The speaker picker lives in the window —
//! the tray menu is intentionally a small set of quick actions.

#![cfg(windows)]

use anyhow::{anyhow, Result};
use eframe::egui;
use log::{debug, info, warn};
use std::sync::Arc;
use std::thread;

use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};

use crate::app::App;

/// Lives as long as the GUI process. Dropping it removes the tray icon
/// and stops the background event-loop thread.
pub struct TrayHandle {
    _icon: tray_icon::TrayIcon,
    _join: thread::JoinHandle<()>,
}

/// Build and install the tray icon. `egui_ctx` lets the tray-event
/// thread show/focus the main window in response to menu clicks.
pub fn spawn(app: Arc<App>, egui_ctx: egui::Context) -> Result<TrayHandle> {
    let menu = Menu::new();

    // --- Status label (disabled — display-only) ---
    let status_item = MenuItem::new("(no speaker)", false, None);
    menu.append(&status_item)?;
    menu.append(&PredefinedMenuItem::separator())?;

    // --- Quick actions ---
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

    // --- Enable / disable toggle ---
    let enabled_now = app.is_streaming_enabled();
    let enable_toggle = CheckMenuItem::new("Streaming enabled", true, enabled_now, None);
    menu.append(&enable_toggle)?;
    menu.append(&PredefinedMenuItem::separator())?;

    // --- Window + quit ---
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

    // Capture menu-item ids so MenuEvent dispatching is unambiguous.
    let switch_speaker_id = switch_speaker.id().clone();
    let trim_25_id = trim_25.id().clone();
    let trim_100_id = trim_100.id().clone();
    let pad_25_id = pad_25.id().clone();
    let pad_100_id = pad_100.id().clone();
    let resync_id = resync.id().clone();
    let enable_id = enable_toggle.id().clone();
    let show_window_id = show_window.id().clone();
    let open_web_id = open_web.id().clone();
    let quit_id = quit_item.id().clone();

    let icon = build_icon()?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(crate::PRODUCT_NAME)
        .with_icon(icon)
        .build()
        .map_err(|e| anyhow!("tray-icon build: {}", e))?;

    // Worker thread that drains the tray + menu event channels.
    let app_for_thread = app.clone();
    let ctx_for_thread = egui_ctx.clone();
    let status_item_for_thread = status_item.clone();
    let enable_toggle_for_thread = enable_toggle.clone();
    let menu_receiver = MenuEvent::receiver().clone();
    let tray_receiver = TrayIconEvent::receiver().clone();

    let join = thread::Builder::new()
        .name("stream-to-speaker-tray".to_string())
        .spawn(move || {
            loop {
                if app_for_thread.is_shutting_down() {
                    break;
                }

                // Sync the status label and check-state ~2 Hz.
                let label = match app_for_thread.current_renderer() {
                    Some(r) => format!("▶ {}", r.friendly_name),
                    None => "(no speaker)".to_string(),
                };
                status_item_for_thread.set_text(&label);
                let enabled_now = app_for_thread.is_streaming_enabled();
                if enable_toggle_for_thread.is_checked() != enabled_now {
                    enable_toggle_for_thread.set_checked(enabled_now);
                }

                // Drain menu / tray events for up to 500 ms then loop.
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
                    match menu_receiver.recv_timeout(remaining) {
                        Ok(MenuEvent { id }) => {
                            if id == trim_25_id {
                                app_for_thread.adjust_latency(25);
                            } else if id == trim_100_id {
                                app_for_thread.adjust_latency(100);
                            } else if id == pad_25_id {
                                app_for_thread.adjust_latency(-25);
                            } else if id == pad_100_id {
                                app_for_thread.adjust_latency(-100);
                            } else if id == resync_id {
                                if let Err(e) = app_for_thread.resync() {
                                    warn!("resync failed: {}", e);
                                }
                            } else if id == enable_id {
                                let new_state = !app_for_thread.is_streaming_enabled();
                                if let Err(e) = app_for_thread.set_streaming_enabled(new_state) {
                                    warn!("toggle failed: {}", e);
                                }
                            } else if id == switch_speaker_id || id == show_window_id {
                                show_main_window(&ctx_for_thread);
                            } else if id == open_web_id {
                                if app_for_thread.config.web_enabled {
                                    open_web_ui(
                                        &app_for_thread.config.advertise_ip,
                                        app_for_thread.config.bind.port(),
                                    );
                                }
                            } else if id == quit_id {
                                info!("tray: quit requested");
                                app_for_thread.request_shutdown();
                                ctx_for_thread.request_repaint();
                                return;
                            }
                            ctx_for_thread.request_repaint();
                        }
                        Err(_) => break, // timeout or disconnected
                    }
                }

                // Drain tray-icon click events. Left-click toggles window.
                while let Ok(ev) = tray_receiver.try_recv() {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        show_main_window(&ctx_for_thread);
                    }
                }
            }
            debug!("tray thread exiting");
        })
        .map_err(|e| anyhow!("spawn tray thread: {}", e))?;

    Ok(TrayHandle {
        _icon: tray,
        _join: join,
    })
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

    // Stylised speaker silhouette: a vertical box with a circular "cone".
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
