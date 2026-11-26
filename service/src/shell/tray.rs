//! The icon by the clock: its picture, tooltip and menu come from the
//! snapshot; a left click opens or closes the panel, a right click opens
//! the menu.

use std::sync::mpsc::Sender;

use anyhow::{anyhow, Result};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::icons;
use super::placement::PxRect;
use crate::model::command::Command;
use crate::model::state::{Arcs, Badge};
use crate::model::by_clock::{MenuEntry, Tray as TrayWords};

/// What happened at the icon, for the shell.
#[derive(Clone, Debug)]
pub enum TrayEvent {
    /// The left button went down on the icon.
    Press,
    /// The left button went down on the icon a second time, quickly: the
    /// second press of a double click (instead of `Press`).
    DoublePress,
    /// The left button came up on the icon, at `rect`.
    Release(PxRect),
    /// An item of the menu was chosen.
    Menu(MenuId),
}

fn rect_of(r: &tray_icon::Rect) -> PxRect {
    let (x, y) = (r.position.x.round() as i32, r.position.y.round() as i32);
    PxRect::new(x, y, x + r.size.width as i32, y + r.size.height as i32)
}

/// The tray icon. Make it on the thread that runs the windows' messages.
pub struct Tray {
    icon: TrayIcon,
    look: (Arcs, Badge, bool),
    tooltip: String,
    menu: Vec<MenuEntry>,
    commands: Vec<(MenuId, Command)>,
    /// The ticked entries: the place.
    ticked: Vec<CheckMenuItem>,
}

/// A menu's text: Windows reads `&` as the start of a shortcut letter.
fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

/// The native menu, the command of each entry that has one, and the ticked
/// entries. The place's entry is ticked and drawn as the others are, not
/// greyed: a click on it changes nothing.
fn build_menu(entries: &[MenuEntry]) -> Result<(Menu, Vec<(MenuId, Command)>, Vec<CheckMenuItem>)> {
    let menu = Menu::new();
    let mut commands = Vec::new();
    let mut ticked = Vec::new();
    for e in entries {
        let text = menu_text(&e.label);
        let enabled = e.command.is_some() || e.checked;
        let id = if e.checked {
            let item = CheckMenuItem::new(text, enabled, true, None);
            menu.append(&item)?;
            let id = item.id().clone();
            ticked.push(item);
            id
        } else {
            let item = MenuItem::new(text, enabled, None);
            menu.append(&item)?;
            item.id().clone()
        };
        if let Some(c) = &e.command {
            commands.push((id, c.clone()));
        }
    }
    Ok((menu, commands, ticked))
}

fn picture(arcs: Arcs, badge: Badge, dark: bool) -> Result<Icon> {
    Icon::from_rgba(icons::tray_icon(arcs, badge, dark).to_vec(), icons::SIDE, icons::SIDE).map_err(|e| anyhow!("tray icon: {e}"))
}

impl Tray {
    /// Put the icon by the clock. Its events go to `events`, and `wake` is
    /// called after each so the shell looks at them.
    pub fn new(words: &TrayWords, dark_taskbar: bool, events: Sender<TrayEvent>, wake: impl Fn() + Send + Sync + Clone + 'static) -> Result<Tray> {
        let (arcs, badge) = icons::appearance(words);
        let (menu, commands, ticked) = build_menu(&words.menu)?;
        let tooltip = icons::tooltip(&words.tooltip);
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(&tooltip)
            .with_icon(picture(arcs, badge, dark_taskbar)?)
            .build()
            .map_err(|e| anyhow!("tray icon: {e}"))?;
        {
            let (events, wake) = (events.clone(), wake.clone());
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                let ev = match e {
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Down, .. } => TrayEvent::Press,
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } => TrayEvent::Release(rect_of(&rect)),
                    TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => TrayEvent::DoublePress,
                    // The left ones are logged with what they did; these do
                    // nothing in the shell (the right button opens the menu).
                    TrayIconEvent::Click { button, button_state, .. } => {
                        log::info!("tray: {button:?} button {button_state:?} on the icon");
                        return;
                    }
                    TrayIconEvent::DoubleClick { button, .. } => {
                        log::info!("tray: {button:?} double click on the icon");
                        return;
                    }
                    _ => return,
                };
                let _ = events.send(ev);
                wake();
            }));
        }
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = events.send(TrayEvent::Menu(e.id));
            wake();
        }));
        Ok(Tray { icon, look: (arcs, badge, dark_taskbar), tooltip, menu: words.menu.clone(), commands, ticked })
    }

    /// Show what `words` says, changing only what changed.
    pub fn sync(&mut self, words: &TrayWords, dark_taskbar: bool) {
        let (arcs, badge) = icons::appearance(words);
        let look = (arcs, badge, dark_taskbar);
        if look != self.look {
            match picture(arcs, badge, dark_taskbar) {
                Ok(p) => {
                    if self.icon.set_icon(Some(p)).is_ok() {
                        self.look = look;
                    }
                }
                Err(e) => log::warn!("{e:#}"),
            }
        }
        let tooltip = icons::tooltip(&words.tooltip);
        if tooltip != self.tooltip && self.icon.set_tooltip(Some(&tooltip)).is_ok() {
            self.tooltip = tooltip;
        }
        if words.menu != self.menu {
            match build_menu(&words.menu) {
                Ok((menu, commands, ticked)) => {
                    self.icon.set_menu(Some(Box::new(menu)));
                    self.commands = commands;
                    self.ticked = ticked;
                    self.menu = words.menu.clone();
                }
                Err(e) => log::warn!("tray menu: {e:#}"),
            }
        }
    }

    /// The command of the menu item `id`. Windows unticks a ticked entry
    /// when it is clicked; the place's entry is ticked again.
    pub fn command(&self, id: &MenuId) -> Option<Command> {
        if let Some(item) = self.ticked.iter().find(|i| i.id() == id) {
            item.set_checked(true);
        }
        self.commands.iter().find(|(i, _)| i == id).map(|(_, c)| c.clone())
    }

    /// Where the icon is now.
    pub fn rect(&self) -> Option<PxRect> {
        self.icon.rect().map(|r| rect_of(&r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{fake::ScriptedEngine, Port};
    use tray_icon::menu::MenuItemKind;
    #[test]
    fn native_menu_has_the_views_labels_checks_and_commands() {
        let view = ScriptedEngine::new("kitchen-playing").snapshot();
        let (menu, commands, ticked) = build_menu(&view.tray.menu).unwrap();
        assert_eq!(ticked.len(), view.tray.menu.iter().filter(|e| e.checked).count());
        assert_eq!(menu.items().len(), view.tray.menu.len());
        for (item, entry) in menu.items().iter().zip(&view.tray.menu) {
            let (text, checked, enabled) = match item {
                MenuItemKind::MenuItem(i) => (i.text(), false, i.is_enabled()),
                MenuItemKind::Check(i) => (i.text(), i.is_checked(), i.is_enabled()),
                _ => panic!("unexpected native menu item"),
            };
            assert_eq!((text, checked, enabled), (entry.label.clone(), entry.checked, entry.command.is_some() || entry.checked));
            assert_eq!(commands.iter().find(|(id, _)| id == item.id()).map(|(_, c)| c), entry.command.as_ref());
        }
    }
}
