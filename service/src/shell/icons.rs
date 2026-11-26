//! The tray icon in each of its states: where the sound is (the arcs), a
//! problem badge, and whether the taskbar is light or dark. Raw 32 × 32
//! RGBA, rendered from `assets/brand/panel-tray-*.svg`.

use crate::model::state::{Arcs, Badge};

/// The icon's side in pixels.
pub const SIDE: u32 = 32;

macro_rules! icon {
    ($name:literal) => {
        include_bytes!(concat!("../../../assets/panel-tray/", $name, "-32.rgba"))
    };
}

/// The tray icon for `arcs` with `badge`, on a dark taskbar or a light one.
pub fn tray_icon(arcs: Arcs, badge: Badge, dark_taskbar: bool) -> &'static [u8] {
    use Arcs::*;
    use Badge::*;
    match (arcs, badge, dark_taskbar) {
        (Ink, None, false) => icon!("pc-none-light"),
        (Ink, Attention, false) => icon!("pc-attention-light"),
        (Green, None, false) => icon!("speaker-none-light"),
        (Green, Attention, false) => icon!("speaker-attention-light"),
        (Ink, None, true) => icon!("pc-none-dark"),
        (Ink, Attention, true) => icon!("pc-attention-dark"),
        (Green, None, true) => icon!("speaker-none-dark"),
        (Green, Attention, true) => icon!("speaker-attention-dark"),
    }
}

pub fn appearance(tray: &crate::model::by_clock::Tray) -> (Arcs, Badge) {
    (if tray.accent { Arcs::Green } else { Arcs::Ink },
     if tray.amber { Badge::Attention } else { Badge::None })
}

pub fn app_mark(ui: &egui::Ui) -> egui::TextureHandle {
    let id = egui::Id::new("popup-app-icon");
    if let Some(image) = ui.ctx().data(|d| d.get_temp(id)) { return image; }
    let pixels = tray_icon(Arcs::Ink, Badge::None, false).chunks_exact(4)
        .map(|rgba| egui::Color32::from_white_alpha(rgba[3])).collect();
    let image = egui::ColorImage { size: [SIDE as usize; 2], pixels };
    let texture = ui.ctx().load_texture("app-mark", image, egui::TextureOptions::LINEAR);
    ui.ctx().data_mut(|d| d.insert_temp(id, texture.clone()));
    texture
}

/// The tray's tooltip, cut to what Windows shows (127 characters).
pub fn tooltip(text: &str) -> String {
    const MAX: usize = 127;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_a_whole_icon_and_they_differ() {
        let mut seen = std::collections::HashSet::new();
        for arcs in [Arcs::Ink, Arcs::Green] {
            for badge in [Badge::None, Badge::Attention] {
                for dark in [false, true] {
                    let icon = tray_icon(arcs, badge, dark);
                    assert_eq!(icon.len(), (SIDE * SIDE * 4) as usize);
                    assert!(seen.insert(icon.as_ptr()), "{arcs:?} {badge:?} {dark}");
                }
            }
        }
    }

    #[test]
    fn a_long_tooltip_is_cut_to_what_windows_shows() {
        let long = format!("Stream To Speaker – {} · Playing", "Living Room ".repeat(20));
        let t = tooltip(&long);
        assert_eq!(t.chars().count(), 127);
        assert!(t.starts_with("Stream To Speaker – Living Room"));
        assert!(t.ends_with('…'));
        assert_eq!(tooltip("Stream To Speaker – Kitchen · Playing"), "Stream To Speaker – Kitchen · Playing");
    }
}
