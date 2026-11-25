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
    /// focus from it, or the click ended a double click that leaves it
    /// closed.
    Nothing,
    /// Leave it open and give it the focus again: the click ended a double
    /// click whose first click opened it (its second press may have taken
    /// the focus from it).
    Keep,
}

/// How soon after the panel lost the focus a press on the icon counts as
/// the press that took it, in seconds.
pub const PRESS_AFTER_BLUR: f64 = 0.5;

/// How long the second press of a double click keeps the panel from
/// closing when it loses the focus, at most, in seconds: until the button
/// comes up, unless that never reaches the icon.
pub const DOUBLE_CLICK_HOLD: f64 = 1.0;

/// Tells a click on the tray icon that should close the panel from one
/// that should open it. Pressing the icon takes the focus from a flyout,
/// which closes, before the button comes up; that click must leave it
/// closed, not open it again. A double click is one click: it ends with
/// the panel as its first click left it (open after a double click on a
/// closed panel, closed after one on an open panel), though its second
/// press takes the focus again.
#[derive(Clone, Copy, Debug, Default)]
pub struct Toggle {
    closed_by_blur_at: Option<f64>,
    /// The press of the click under way found the panel open (or just
    /// closed by it).
    press_found_open: Option<bool>,
    /// What the last click did.
    last: Option<TrayClick>,
    /// The second press of a double click came, at this time: whether the
    /// panel is to end up open.
    double: Option<(f64, bool)>,
}

impl Toggle {
    fn just_closed(&self, now: f64) -> bool {
        self.closed_by_blur_at.is_some_and(|at| now - at <= PRESS_AFTER_BLUR)
    }

    /// The double click under way, if its second press came lately: whether
    /// the panel is to end up open.
    fn double_under_way(&self, now: f64) -> Option<bool> {
        self.double.filter(|(at, _)| now - at <= DOUBLE_CLICK_HOLD).map(|(_, open)| open)
    }

    /// The panel lost the focus at `now` (seconds): whether that closes
    /// it. Not while a double click that leaves it open is under way.
    pub fn blur_closes(&mut self, now: f64) -> bool {
        if self.double_under_way(now) == Some(true) {
            return false;
        }
        self.closed_by_blur_at = Some(now);
        true
    }

    /// The left button went down on the icon.
    pub fn press(&mut self, open: bool, now: f64) {
        self.double = None;
        self.press_found_open = Some(open || self.just_closed(now));
    }

    /// The left button went down on the icon a second time, quickly: the
    /// second press of a double click (it comes instead of `press`).
    pub fn double_press(&mut self, open: bool, now: f64) {
        let first_opened = match self.last {
            Some(click) => click == TrayClick::Open || click == TrayClick::Keep,
            None => open,
        };
        self.double = Some((now, first_opened));
        self.press_found_open = None;
    }

    /// The left button came up on the icon: what to do.
    pub fn release(&mut self, open: bool, now: f64) -> TrayClick {
        let click = match self.double.take().filter(|(at, _)| now - at <= DOUBLE_CLICK_HOLD) {
            Some((_, true)) if open => TrayClick::Keep,
            Some((_, true)) => TrayClick::Open,
            Some((_, false)) if open => TrayClick::Close,
            Some((_, false)) => TrayClick::Nothing,
            None => {
                let found_open = self.press_found_open.take().unwrap_or_else(|| self.just_closed(now));
                if open {
                    TrayClick::Close
                } else if found_open {
                    TrayClick::Nothing
                } else {
                    TrayClick::Open
                }
            }
        };
        self.press_found_open = None;
        self.closed_by_blur_at = None;
        self.last = Some(click);
        click
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

/// What the shell tells the panel's window after a frame that ended in
/// `step`: to show itself and take the focus once it is in its place.
pub fn commands(step: Step) -> Vec<egui::ViewportCommand> {
    match step {
        Step::Show => vec![egui::ViewportCommand::Visible(true), egui::ViewportCommand::Focus],
        _ => Vec::new(),
    }
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
    /// What the last frame saw of the focus, to log its changes.
    focused: Option<bool>,
    /// The log already said that its window is off screen while it is open.
    hidden_told: bool,
    /// The extended style bits of its window the log last named.
    style: Option<u32>,
}

/// For the log: what a frame saw of the focus.
fn focus_word(focused: Option<bool>) -> &'static str {
    match focused {
        None => "unknown",
        Some(true) => "yes",
        Some(false) => "no",
    }
}

/// A view in the panel, and what its frame keeps.
pub type OpenView = (Box<dyn super::ViewContent>, crate::panel::view::ViewState);

impl PanelWindow {
    /// Open the panel from the tray icon at `icon`, or with no icon (from
    /// Start, a second launch, a notification).
    pub fn open(icon: Option<PxRect>) -> Self {
        PanelWindow {
            state: PanelState::opened(),
            views: Vec::new(),
            icon,
            phase: Phase::Measuring { last: 0.0, stable: 0, frames: 0 },
            focused: None,
            hidden_told: false,
            style: None,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn shown(&self) -> bool {
        matches!(self.phase, Phase::Shown { .. })
    }

    /// Its window's builder for this frame: hidden until it is shown, then
    /// shown. eframe applies a window's builder again every frame, and
    /// egui 0.29's comparison sends its `visible` every frame (it compares
    /// it with `active`): a builder that kept saying "hidden" hid the panel
    /// the frame after it was shown. Saying what is wanted now holds with
    /// that comparison and with one that sends only changes.
    pub fn builder(&self, mode: PanelMode) -> egui::ViewportBuilder {
        super::panel_builder(mode).with_visible(self.shown()).with_icon(super::app_icon())
    }

    /// It stays open whatever the focus did so far: losing the focus closes
    /// it again only after it has had the focus once more (after a double
    /// click on the icon, whose second press took the focus).
    pub fn forget_focus(&mut self) {
        if let Phase::Shown { place, .. } = self.phase {
            self.phase = Phase::Shown { place, had_focus: false };
        }
    }

    /// Where it is on screen, once shown.
    pub fn rect(&self) -> Option<PxRect> {
        match self.phase {
            Phase::Shown { place, .. } => Some(place.rect),
            _ => None,
        }
    }

    /// After a frame of the panel's window: move it on in its life.
    pub fn after_frame(&mut self, seen: Seen, pinned: bool, mode: PanelMode, desktop: &mut dyn Desktop) -> Step {
        if self.shown() && !self.hidden_told && desktop.panel_window_shown() == Some(false) {
            self.hidden_told = true;
            log::warn!("panel: its window is off screen while the panel is open");
        }
        if self.shown() {
            // The style it was shown with, and any change after: whether
            // it stays out of Alt+Tab.
            if let Some(ex) = desktop.panel_ex_style().filter(|ex| Some(*ex) != self.style) {
                log::info!(
                    "panel: its window's style is now {} (was {})",
                    super::ex_style_words(ex),
                    self.style.map_or_else(|| "not seen".to_string(), super::ex_style_words)
                );
                self.style = Some(ex);
            }
        }
        if seen.focused != self.focused {
            if seen.focused == Some(false) && self.shown() {
                let window = match desktop.panel_window_shown() {
                    Some(true) => "on screen",
                    Some(false) => "off screen",
                    None => "not found",
                };
                log::info!(
                    "panel: focus {} -> no (its window {window}; the foreground: {})",
                    focus_word(self.focused),
                    desktop.foreground()
                );
            } else {
                log::info!("panel: focus {} -> {}", focus_word(self.focused), focus_word(seen.focused));
            }
            self.focused = seen.focused;
        }
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
            // The most it may take; its list scrolls beyond it.
            self.state.height = Some(place.max_height);
            log::info!("panel: measured at {wanted} points after {frames} frames; placing it at {}", place.rect);
            self.phase = Phase::Placing { place, tries: 0 };
        }
        if let Phase::Placing { place, tries } = self.phase {
            let placed = desktop.place_panel(place.rect, mode);
            if placed || tries + 1 >= PLACE_TRIES {
                self.style = desktop.panel_ex_style();
                let style = self.style.map_or_else(|| "not found".to_string(), super::ex_style_words);
                if placed {
                    log::info!("panel: showing it (its window's style: {style}; the foreground: {})", desktop.foreground());
                } else {
                    log::warn!(
                        "panel: showing it, though Windows didn't confirm its place after {} tries (its window's style: {style}; the foreground: {})",
                        tries + 1,
                        desktop.foreground()
                    );
                }
                self.phase = Phase::Shown { place, had_focus: false };
                return Step::Show;
            }
            self.phase = Phase::Placing { place, tries: tries + 1 };
            return Step::Wait;
        }
        if let Phase::Shown { mut place, had_focus } = self.phase {
            // The window follows what the panel needs, anchored at the edge
            // by the taskbar: what appears above the list moves the far
            // edge, never the rows. The small window's title bar is added
            // to what the panel needs.
            let outer = place.rect.height() as f32 / place.scale;
            let frame = seen.inner_height.filter(|h| *h > 40.0).map_or(0.0, |inner| (outer - inner).max(0.0));
            self.state.height = Some((place.max_height - frame).max(60.0));
            let want = (seen.natural_height + frame).min(place.max_height);
            if seen.natural_height > 1.0 && (want - outer).abs() > 0.5 {
                let next = placement::resize(&place, &desktop.monitors(), want);
                if desktop.place_panel(next.rect, mode) {
                    place = next;
                }
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
    use super::super::eframe_model::Window;
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
        assert!(t.blur_closes(10.0));
        t.press(false, 10.05);
        // However long the button is held.
        assert_eq!(t.release(false, 11.5), TrayClick::Nothing);
        // Or the press arrives first, while the panel is still open.
        t.press(true, 20.0);
        assert!(t.blur_closes(20.02));
        assert_eq!(t.release(false, 20.1), TrayClick::Nothing);
        // The next click opens it.
        t.press(false, 30.0);
        assert_eq!(t.release(false, 30.1), TrayClick::Open);
        // Losing the focus to another window long before: a click opens.
        assert!(t.blur_closes(40.0));
        t.press(false, 45.0);
        assert_eq!(t.release(false, 45.1), TrayClick::Open);
        // A release with no press seen (the keyboard): by the time alone.
        assert!(t.blur_closes(50.0));
        assert_eq!(t.release(false, 50.2), TrayClick::Nothing);
        assert_eq!(t.release(false, 60.0), TrayClick::Open);
    }

    /// What reaches the shell from the tray icon and the panel's window,
    /// at a time in seconds: the left button's press, the second press of a
    /// double click (tray-icon's `DoubleClick`, which Windows sends instead
    /// of the second press), its release, and the panel losing the focus.
    #[derive(Clone, Copy, Debug)]
    enum Ev {
        Down(f64),
        DoubleDown(f64),
        Up(f64),
        LostFocus(f64),
    }

    /// The panel's state after `events`, starting `open` or not, handled as
    /// the shell handles them.
    fn after(open: bool, events: &[Ev]) -> bool {
        let mut t = Toggle::default();
        let mut open = open;
        for e in events {
            match *e {
                Ev::Down(at) => t.press(open, at),
                Ev::DoubleDown(at) => t.double_press(open, at),
                Ev::Up(at) => match t.release(open, at) {
                    TrayClick::Open => open = true,
                    TrayClick::Close => open = false,
                    TrayClick::Nothing | TrayClick::Keep => {}
                },
                // The shell hears of it only while the panel is open.
                Ev::LostFocus(at) => {
                    if open && t.blur_closes(at) {
                        open = false;
                    }
                }
            }
        }
        open
    }

    #[test]
    fn a_single_click_toggles_and_a_double_click_ends_as_its_first_click_left_the_panel() {
        use Ev::*;
        // A single click opens a closed panel; on an open one its press
        // takes the focus, which closes it, before or after the shell hears
        // of the press, and it stays closed.
        assert!(after(false, &[Down(1.0), Up(1.1)]));
        assert!(!after(true, &[Down(1.0), LostFocus(1.01), Up(1.1)]));
        assert!(!after(true, &[LostFocus(1.0), Down(1.02), Up(1.1)]));
        // A double click on a closed panel opens it and leaves it open,
        // though its second press takes the focus from the panel that just
        // opened, whichever the shell hears of first.
        assert!(after(false, &[Down(1.0), Up(1.08), DoubleDown(1.25), LostFocus(1.26), Up(1.33)]));
        assert!(after(false, &[Down(1.0), Up(1.08), LostFocus(1.25), DoubleDown(1.26), Up(1.33)]));
        assert!(after(false, &[Down(1.0), Up(1.08), DoubleDown(1.25), Up(1.33)]));
        // A double click on an open panel closes it, once.
        assert!(!after(true, &[Down(5.0), LostFocus(5.01), Up(5.08), DoubleDown(5.25), Up(5.33)]));
        // Also as a small window, which doesn't close on losing the focus.
        assert!(!after(true, &[Down(5.0), Up(5.08), DoubleDown(5.25), Up(5.33)]));
        // A second press whose release never reaches the icon holds the
        // panel open for a moment only.
        assert!(!after(false, &[Down(1.0), Up(1.08), DoubleDown(1.25), LostFocus(3.0)]));
    }

    #[test]
    fn a_click_after_the_panel_closed_on_a_lost_focus_opens_it() {
        use Ev::*;
        // Someone clicked elsewhere a while ago.
        assert!(after(true, &[LostFocus(10.0), Down(12.0), Up(12.1)]));
        // And twice, quickly.
        assert!(after(true, &[LostFocus(10.0), Down(12.0), Up(12.08), DoubleDown(12.25), Up(12.33)]));
    }

    #[test]
    fn a_click_on_the_icon_among_the_hidden_icons_works_as_one_on_the_taskbar() {
        use Ev::*;
        // The click on the button that shows the hidden icons sends the app
        // nothing; the icon among them sends what it sends on the taskbar.
        assert!(after(false, &[Down(20.0), Up(20.1)]));
        assert!(after(false, &[Down(20.0), Up(20.08), DoubleDown(20.25), LostFocus(20.26), Up(20.33)]));
        // With the panel open, the click on that button takes the focus and
        // closes it; a click on the icon right after leaves it closed, as
        // the click that closed it, and a later one opens it.
        assert!(!after(true, &[LostFocus(30.0), Down(30.4), Up(30.5)]));
        assert!(after(true, &[LostFocus(30.0), Down(30.4), Up(30.5), Down(31.5), Up(31.6)]));
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
        // The most it may take before its list scrolls: the work area.
        assert_eq!(p.state.height, Some(1008.0));
        assert!(p.shown());
    }

    #[test]
    fn the_window_follows_the_panel_anchored_at_the_taskbar_edge() {
        let mut d = desktop();
        let mut p = PanelWindow::open(None);
        while !p.shown() {
            p.after_frame(seen(400.0), false, PanelMode::Flyout, &mut d);
        }
        let first = *d.placed.last().unwrap();
        // The delay block appears: 120 points more, the top edge moves up,
        // the bottom (by the taskbar) stays.
        p.after_frame(seen(520.0), false, PanelMode::Flyout, &mut d);
        let grown = *d.placed.last().unwrap();
        assert_eq!(grown.bottom, first.bottom);
        assert_eq!(grown.height(), 780);
        // Nothing changes: nothing is moved.
        let n = d.placed.len();
        p.after_frame(seen(520.0), false, PanelMode::Flyout, &mut d);
        assert_eq!(d.placed.len(), n);
        // It shrinks back the same way, and never past the work area.
        p.after_frame(seen(400.0), false, PanelMode::Flyout, &mut d);
        assert_eq!(*d.placed.last().unwrap(), first);
        p.after_frame(seen(5000.0), false, PanelMode::Flyout, &mut d);
        let capped = *d.placed.last().unwrap();
        assert_eq!((capped.top, capped.bottom), (18, first.bottom));
        // A taskbar at the top: the panel grows downward from it.
        let top = Monitor { bounds: PxRect::new(0, 0, 1920, 1080), work: PxRect::new(0, 48, 1920, 1080), scale: 1.0, primary: true };
        let mut d = FakeDesktop { monitors: vec![top], ..Default::default() };
        let mut p = PanelWindow::open(None);
        while !p.shown() {
            p.after_frame(seen(400.0), false, PanelMode::Flyout, &mut d);
        }
        let first = *d.placed.last().unwrap();
        p.after_frame(seen(450.0), false, PanelMode::Flyout, &mut d);
        assert_eq!(d.placed.last().unwrap().top, first.top);
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

    /// The panel's window frame after frame as eframe drives it, through
    /// measuring, placing and shown; the frame it was shown in.
    fn panel_as_eframe_drives_it(mode: PanelMode, right_comparison: bool) -> (Window, usize) {
        let mut d = desktop();
        let mut p = PanelWindow::open(None);
        let mut window: Option<Window> = None;
        let mut shown_in = None;
        for frame in 0..30 {
            // As the shell does: the builder before the frame, the commands
            // after it.
            let builder = p.builder(mode);
            let step = p.after_frame(seen(400.0), false, mode, &mut d);
            let first = window.is_none();
            let w = window.get_or_insert_with(|| Window::made(builder.clone(), right_comparison));
            w.frame(&builder, commands(step), first);
            if step == Step::Show {
                shown_in = Some(frame);
            }
        }
        (window.unwrap(), shown_in.expect("the panel was never shown"))
    }

    #[test]
    fn once_the_panel_is_shown_nothing_hides_its_window() {
        // egui 0.29.1's patch compares `visible` with `active`, and so sends
        // a builder's `visible` again every frame; a library that compares
        // it with `visible` sends it once. The window stays shown either way.
        for mode in [PanelMode::Flyout, PanelMode::Window] {
            for right_comparison in [false, true] {
                let (w, shown_in) = panel_as_eframe_drives_it(mode, right_comparison);
                assert!(shown_in >= 2, "it is measured and placed first");
                for (frame, ran) in w.frames.iter().enumerate() {
                    let hides = ran.iter().any(|c| matches!(c, egui::ViewportCommand::Visible(false)));
                    assert!(frame <= shown_in || !hides, "{mode:?} {right_comparison}: frame {frame} hides it: {ran:?}");
                }
                assert!(w.visible, "{mode:?} {right_comparison}");
                // And nothing shows it before it is in its place.
                let early = w.frames[..shown_in].iter().flatten().any(|c| matches!(c, egui::ViewportCommand::Visible(true)));
                assert!(!early, "{mode:?} {right_comparison}");
            }
        }
    }

    #[test]
    fn a_window_gets_its_icon_when_it_is_made_and_not_again_every_frame() {
        use super::super::popup::{popup_builder, PopupKind};
        let icon_commands = |w: &Window| w.frames.iter().flatten().filter(|c| matches!(c, egui::ViewportCommand::Icon(_))).count();
        for right_comparison in [false, true] {
            let (panel, _) = panel_as_eframe_drives_it(PanelMode::Flyout, right_comparison);
            assert_eq!(icon_commands(&panel), 0, "{right_comparison}");
            let size = egui::Vec2::new(360.0, 96.0);
            let builder = || popup_builder(PopupKind::Notification, size, true).with_icon(super::super::app_icon());
            let mut popup = Window::made(builder(), right_comparison);
            for k in 0..10 {
                popup.frame(&builder(), Vec::new(), k == 0);
            }
            assert_eq!(icon_commands(&popup), 0, "{right_comparison}");
        }
    }

    #[test]
    fn once_a_popup_is_shown_nothing_hides_its_window() {
        use super::super::popup::{popup_builder, PopupKind};
        for right_comparison in [false, true] {
            let size = egui::Vec2::new(280.0, 72.0);
            // As the shell does: placed in its second frame, then shown.
            let builder = |placed: bool| popup_builder(PopupKind::Confirmation, size, placed).with_icon(super::super::app_icon());
            let mut w = Window::made(builder(false), right_comparison);
            w.frame(&builder(false), Vec::new(), true);
            assert!(!w.visible);
            w.frame(&builder(true), vec![egui::ViewportCommand::Visible(true)], false);
            for _ in 0..20 {
                w.frame(&builder(true), Vec::new(), false);
                assert!(w.visible, "{right_comparison}: {:?}", w.frames.last());
            }
        }
    }

    #[test]
    fn eframe_never_shows_or_hides_the_root_window_by_itself() {
        use super::super::{root_builder, RootMode};
        for mode in [RootMode::Hidden, RootMode::ToolWindow] {
            for right_comparison in [false, true] {
                // eframe patches the root's window with what egui holds for
                // the root: its default builder.
                let mut w = Window::made(root_builder(mode).with_icon(super::super::app_icon()), right_comparison);
                for k in 0..20 {
                    w.frame(&egui::ViewportBuilder::default(), Vec::new(), k == 0);
                }
                let visible = w.frames.iter().flatten().filter(|c| matches!(c, egui::ViewportCommand::Visible(_))).count();
                assert_eq!(visible, 0, "{mode:?} {right_comparison}");
            }
        }
    }

    #[test]
    fn esc_closes_in_every_form_and_the_window_draws_at_its_inner_height() {
        let mut d = desktop();
        let mut p = PanelWindow::open(None);
        while !p.shown() {
            p.after_frame(seen(400.0), true, PanelMode::Window, &mut d);
        }
        // Windows gave 32 points to the title bar: the window grows by them
        // so the panel has the 400 it needs.
        let before = d.placed.len();
        p.after_frame(Seen { inner_height: Some(368.0), ..seen(400.0) }, true, PanelMode::Window, &mut d);
        assert_eq!(d.placed.len(), before + 1);
        assert_eq!(d.placed.last().unwrap().height(), (432.0 * 1.5) as i32);
        assert_eq!(p.after_frame(Seen { close: true, ..seen(400.0) }, true, PanelMode::Window, &mut d), Step::Close);
    }
}
