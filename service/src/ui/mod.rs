//! The app's look and the parts every window is built from.
//!
//! [`theme`] holds the colours, type and spacing, light and dark. The parts
//! ([`button`], …) draw from plain values (words, numbers, flags) and return
//! what the person did; none of them reads or changes the app's state.
//! [`preview`] draws any of it to a picture without a window; [`scenes`]
//! lists everything there is a picture of.

pub mod button;
pub mod gallery;
pub mod icon;
pub mod look;
pub mod preview;
pub mod theme;

/// Every scene the preview can draw. Each surface adds its own here.
pub fn scenes() -> Vec<preview::Scene> {
    let mut all = Vec::new();
    all.extend(gallery::scenes());
    all
}
