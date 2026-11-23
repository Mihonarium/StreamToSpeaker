//! The panel as a window: it opens hidden, is measured, put in its place
//! by the clock and only then shown, so it never appears at the wrong size
//! or jumps; it keeps that height while open; and it closes on Esc, on a
//! click on the tray icon, or (as a flyout) when it loses the focus. The
//! frames come from the shell; Windows' part goes through [`Desktop`].

use super::placement::{self, Placement, PxRect};
use super::{Desktop, PanelMode};
use crate::panel::{PanelState, WIDTH};

/// What a click on the tray icon does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayClick {
    Open,
    Close,
    /// Nothing: pressing the icon already closed the panel by taking the
    /// focus from it.
    Nothing,
}

/// How soon after the panel lost the focus a press on the icon counts as
/// the press that took it, in seconds.
pub const PRESS_AFTER_BLUR: f64 = 0.5;

/// Tells a click on the tray icon that should close the panel from one
/// that should open it. Pressing the icon takes the focus from a flyout,
/// which closes, before the button comes up; that click must leave it
/// closed, not open it again.
#[derive(Clone, Copy, Debug, Default)]
pub struct Toggle {
    closed_by_blur_at: Option<f64>,
    /// The press of the click under way found the panel open (or just
    /// closed by it).
    press_found_open: Option<bool>,
}

impl Toggle {
    fn just_closed(&self, now: f64) -> bool {
        self.closed_by_blur_at.is_some_and(|at| now - at <= PRESS_AFTER_BLUR)
    }

    /// The panel closed because it lost the focus, at `now` (seconds).
    pub fn closed_by_blur(&mut self, now: f64) {
        self.closed_by_blur_at = Some(now);
    }

    /// The left button went down on the icon.
    pub fn press(&mut self, open: bool, now: f64) {
        self.press_found_open = Some(open || self.just_closed(now));
    }

    /// The left button came up on the icon: what to do.
    pub fn release(&mut self, open: bool, now: f64) -> TrayClick {
        let found_open = self.press_found_open.take().unwrap_or_else(|| self.just_closed(now));
        self.closed_by_blur_at = None;
        if open {
            TrayClick::Close
        } else if found_open {
            TrayClick::Nothing
        } else {
            TrayClick::Open
        }
    }
}

/// Where the panel's window is in its life.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    /// Drawn hidden until the height it needs settles.
    Measuring { last: f32, stable: u32, frames: u32 },
    /// Hidden, being put in its place.
    Placing { place: Placement, tries: u32 },
    /// On screen.
    Shown { place: Placement, had_focus: bool },
}

/// What the shell does after a frame of the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Wait,
    /// Show the window and give it the focus.
    Show,
    /// Close it because it lost the focus.
    CloseOnBlur,
    /// Close it (Esc, its ×).
    Close,
}

/// What a frame of the panel's window saw.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Seen {
    /// The height the panel needs, in points.
    pub natural_height: f32,
    /// Esc, or the window's × (the small-window form).
    pub close: bool,
    /// The window's inner height in points, once Windows has said.
    pub inner_height: Option<f32>,
    /// Whether the window has the keyboard focus, once Windows has said.
    pub focused: Option<bool>,
}

/// How many frames to wait for the height to settle, and to see the window
/// in its place, before going on anyway.
const MEASURE_FRAMES: u32 = 12;
const PLACE_TRIES: u32 = 6;
/// When no monitor is known: the height the panel opens at.
const FALLBACK_HEIGHT: f32 = 480.0;

/// The open panel.
pub struct PanelWindow {
    pub state: PanelState,
    /// The views shown over the everyday content, the top one last.
    pub views: Vec<OpenView>,
    /// The tray icon it opened from, if it did.
    icon: Option<PxRect>,
    phase: Phase,
}

/// A view in the panel, and what its frame keeps.
pub type OpenView = (Box<dyn super::ViewContent>, crate::panel::view::ViewState);

impl PanelWindow {
    /// Open the panel from the tray icon at `icon`, or with no icon (from
    /// Start, a second launch, a notification).
    pub fn open(icon: Option<PxRect>) -> Self {
        PanelWindow { state: PanelState::opened(), views: Vec::new(), icon, phase: Phase::Measuring { last: 0.0, stable: 0, frames: 0 } }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn shown(&self) -> bool {
        matches!(self.phase, Phase::Shown { .. })
    }

    /// After a frame of the panel's window: move it on in its life.
    pub fn after_frame(&mut self, seen: Seen, pinned: bool, mode: PanelMode, desktop: &mut dyn Desktop) -> Step {
        if seen.close {
            return Step::Close;
        }
        if let Phase::Measuring { last, stable, frames } = self.phase {
            let (stable, last) = if (seen.natural_height - last).abs() < 0.5 { (stable + 1, last) } else { (0, seen.natural_height) };
            let frames = frames + 1;
            if stable < 2 && frames < MEASURE_FRAMES {
                self.phase = Phase::Measuring { last, stable, frames };
                return Step::Wait;
            }
            let wanted = if last > 1.0 { last } else { FALLBACK_HEIGHT };
            let place = placement::place(&desktop.monitors(), self.icon, (WIDTH, wanted)).unwrap_or(Placement {
                rect: PxRect::new(0, 0, WIDTH as i32, FALLBACK_HEIGHT as i32),
                scale: 1.0,
                max_height: FALLBACK_HEIGHT,
            });
            // It keeps this height while open: whatever opens later takes
            // its room from the list.
            self.state.height = Some(place.rect.height() as f32 / place.scale);
            self.phase = Phase::Placing { place, tries: 0 };
        }
        if let Phase::Placing { place, tries } = self.phase {
            if desktop.place_panel(place.rect, mode) || tries + 1 >= PLACE_TRIES {
                self.phase = Phase::Shown { place, had_focus: false };
                return Step::Show;
            }
            self.phase = Phase::Placing { place, tries: tries + 1 };
            return Step::Wait;
        }
        if let Phase::Shown { place, had_focus } = self.phase {
            // Draw at the height Windows gave (the small window's title bar
            // takes some of it).
            if let Some(h) = seen.inner_height.filter(|h| *h > 40.0) {
                self.state.height = Some(h);
            }
            let had_focus = had_focus || seen.focused == Some(true);
            self.phase = Phase::Shown { place, had_focus };
            if had_focus && seen.focused == Some(false) && mode.closes_on_blur(pinned) {
                return Step::CloseOnBlur;
            }
        }
        Step::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::super::placement::Monitor;
    use super::super::FakeDesktop;
    use super::*;

    fn desktop() -> FakeDesktop {
        let m = Monitor { bounds: PxRect::new(0, 0, 2880, 1620), work: PxRect::new(0, 0, 2880, 1548), scale: 1.5, primary: true };
        FakeDesktop { monitors: vec![m], ..Default::default() }
    }

    fn seen(natural: f32) -> Seen {
        Seen { natural_height: natural, ..Default::default() }
    }

    #[test]
    fn a_click_on_the_icon_closes_an_open_panel_and_opens_a_closed_one() {
        let mut t = Toggle::default();
        t.press(true, 1.0);
        assert_eq!(t.release(true, 1.1), TrayClick::Close);
        t.press(false, 2.0);
        assert_eq!(t.release(false, 2.1), TrayClick::Open);
    }

    #[test]
    fn the_click_that_took_the_focus_from_the_panel_does_not_reopen_it() {
        // The press reaches the shell after the panel closed on losing the
        // focus to the taskbar.
        let mut t = Toggle::default();
        t.closed_by_blur(10.0);
        t.press(false, 10.05);
        // However long the button is held.
        assert_eq!(t.release(false, 11.5), TrayClick::Nothing);
        // Or the press arrives first, while the panel is still open.
        t.press(true, 20.0);
        t.closed_by_blur(20.02);
        assert_eq!(t.release(false, 20.1), TrayClick::Nothing);
        // The next click opens it.
        t.press(false, 30.0);
        assert_eq!(t.release(false, 30.1), TrayClick::Open);
        // Losing the focus to another window long before: a click opens.
        t.closed_by_blur(40.0);
        t.press(false, 45.0);
        assert_eq!(t.release(false, 45.1), TrayClick::Open);
        // A release with no press seen (the keyboard): by the time alone.
        t.closed_by_blur(50.0);
        assert_eq!(t.release(false, 50.2), TrayClick::Nothing);
        assert_eq!(t.release(false, 60.0), TrayClick::Open);
    }

    #[test]
    fn the_panel_is_measured_then_placed_then_shown_at_the_height_it_needs() {
        let mut d = desktop();
        let mut p = PanelWindow::open(None);
        assert_eq!(p.after_frame(seen(300.0), false, PanelMode::Flyout, &mut d), Step::Wait);
        assert_eq!(p.after_frame(seen(420.0), false, PanelMode::Flyout, &mut d), Step::Wait);
        assert_eq!(p.after_frame(seen(420.0), false, PanelMode::Flyout, &mut d), Step::Wait);
        assert!(d.placed.is_empty(), "nothing is placed while the height still moves");
        assert_eq!(p.after_frame(seen(420.0), false, PanelMode::Flyout, &mut d), Step::Show);
        // 420 points at 150 %, 12 points in from the work area's corner.
        assert_eq!(d.placed, vec![PxRect::new(2880 - 18 - 540, 1548 - 18 - 630, 2880 - 18, 1548 - 18)]);
        assert_eq!(p.state.height, Some(420.0));
        assert!(p.shown());
    }

    #[test]
    fn a_window_windows_is_slow_to_move_is_shown_once_it_is_in_place() {
        let mut d = FakeDesktop { slow: 2, ..desktop() };
        let mut p = PanelWindow::open(None);
        // Two frames to see the height settle, then two failed moves.
        let steps: Vec<Step> = (0..5).map(|_| p.after_frame(seen(400.0), false, PanelMode::Flyout, &mut d)).collect();
        assert_eq!(steps, [Step::Wait, Step::Wait, Step::Wait, Step::Wait, Step::Show]);
        assert_eq!(d.placed.len(), 3);
    }

    #[test]
    fn a_panel_taller_than_the_display_is_capped_and_its_list_scrolls() {
        let mut d = desktop();
        let mut p = PanelWindow::open(None);
        while !p.shown() {
            p.after_frame(seen(2000.0), false, PanelMode::Flyout, &mut d);
        }
        // The work area is 1032 points; 12 above and below.
        assert_eq!(p.state.height, Some(1008.0));
    }

    #[test]
    fn the_flyout_closes_when_it_loses_the_focus_unless_pinned_and_the_window_does_not() {
        for (mode, pinned, expect) in [
            (PanelMode::Flyout, false, Step::CloseOnBlur),
            (PanelMode::Flyout, true, Step::Wait),
            (PanelMode::Window, false, Step::Wait),
        ] {
            let mut d = desktop();
            let mut p = PanelWindow::open(None);
            while !p.shown() {
                p.after_frame(seen(400.0), pinned, mode, &mut d);
            }
            // Not yet focused: Windows hasn't said; losing nothing.
            assert_eq!(p.after_frame(Seen { focused: Some(false), ..seen(400.0) }, pinned, mode, &mut d), Step::Wait);
            assert_eq!(p.after_frame(Seen { focused: Some(true), ..seen(400.0) }, pinned, mode, &mut d), Step::Wait);
            assert_eq!(p.after_frame(Seen { focused: Some(false), ..seen(400.0) }, pinned, mode, &mut d), expect, "{mode:?} {pinned}");
        }
    }

    #[test]
    fn esc_closes_in_every_form_and_the_window_draws_at_its_inner_height() {
        let mut d = desktop();
        let mut p = PanelWindow::open(None);
        while !p.shown() {
            p.after_frame(seen(400.0), true, PanelMode::Window, &mut d);
        }
        p.after_frame(Seen { inner_height: Some(368.0), ..seen(400.0) }, true, PanelMode::Window, &mut d);
        assert_eq!(p.state.height, Some(368.0));
        assert_eq!(p.after_frame(Seen { close: true, ..seen(400.0) }, true, PanelMode::Window, &mut d), Step::Close);
    }
}
