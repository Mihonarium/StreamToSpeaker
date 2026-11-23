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
        (Ink, Failure, false) => icon!("pc-failure-light"),
        (GreenFaint, None, false) => icon!("connecting-none-light"),
        (GreenFaint, Attention, false) => icon!("connecting-attention-light"),
        (GreenFaint, Failure, false) => icon!("connecting-failure-light"),
        (Green, None, false) => icon!("speaker-none-light"),
        (Green, Attention, false) => icon!("speaker-attention-light"),
        (Green, Failure, false) => icon!("speaker-failure-light"),
        (Ink, None, true) => icon!("pc-none-dark"),
        (Ink, Attention, true) => icon!("pc-attention-dark"),
        (Ink, Failure, true) => icon!("pc-failure-dark"),
        (GreenFaint, None, true) => icon!("connecting-none-dark"),
        (GreenFaint, Attention, true) => icon!("connecting-attention-dark"),
        (GreenFaint, Failure, true) => icon!("connecting-failure-dark"),
        (Green, None, true) => icon!("speaker-none-dark"),
        (Green, Attention, true) => icon!("speaker-attention-dark"),
        (Green, Failure, true) => icon!("speaker-failure-dark"),
    }
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
        for arcs in [Arcs::Ink, Arcs::GreenFaint, Arcs::Green] {
            for badge in [Badge::None, Badge::Attention, Badge::Failure] {
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
        let long = "Stream To Speaker – Playing on ".to_string() + &"Living Room ".repeat(20);
        let t = tooltip(&long);
        assert_eq!(t.chars().count(), 127);
        assert!(t.ends_with('…'));
        assert_eq!(tooltip("Stream To Speaker – Playing on Kitchen"), "Stream To Speaker – Playing on Kitchen");
    }
}
