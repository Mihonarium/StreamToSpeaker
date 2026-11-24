//! Where the panel sits: next to the clock on the monitor whose taskbar was
//! clicked, inside that monitor's work area, the way Windows' own flyouts
//! sit, for a taskbar on any edge and any scale.
//!
//! Everything here is in physical pixels except the panel's size, which is
//! in points (pixels at 100 %).

/// A rectangle in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl PxRect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        PxRect { left, top, right, bottom }
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    fn center(&self) -> (i32, i32) {
        ((self.left + self.right) / 2, (self.top + self.bottom) / 2)
    }
}

/// For the log: `360x480 at 1548,552`.
impl std::fmt::Display for PxRect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{} at {},{}", self.width(), self.height(), self.left, self.top)
    }
}

/// One monitor: all of it, the part the taskbar leaves free, and its scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    pub bounds: PxRect,
    pub work: PxRect,
    /// 1.0 at 100 %, 1.5 at 150 %.
    pub scale: f32,
    pub primary: bool,
}

/// The edge of the monitor the taskbar is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Bottom,
    Top,
    Left,
    Right,
}

/// Where the taskbar is on `m`: the side where the work area is smaller
/// than the monitor (bottom when it hides itself and leaves no gap).
pub fn taskbar_edge(m: &Monitor) -> Edge {
    let gaps = [
        (m.bounds.bottom - m.work.bottom, Edge::Bottom),
        (m.work.top - m.bounds.top, Edge::Top),
        (m.work.left - m.bounds.left, Edge::Left),
        (m.bounds.right - m.work.right, Edge::Right),
    ];
    gaps.iter().filter(|(gap, _)| *gap > 0).max_by_key(|(gap, _)| *gap).map(|(_, e)| *e).unwrap_or(Edge::Bottom)
}

/// The gap between a flyout and the work area's edges, in points.
pub const GAP: f32 = 12.0;

/// Where the panel goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// The panel's rectangle, in physical pixels.
    pub rect: PxRect,
    /// The monitor's scale.
    pub scale: f32,
    /// The tallest the panel may be, in points: the work area less the gap
    /// above and below.
    pub max_height: f32,
}

/// The monitor a click at the tray icon `icon` was on, or the primary one
/// (the panel opened from Start or a second launch).
pub fn monitor_for(monitors: &[Monitor], icon: Option<PxRect>) -> Option<&Monitor> {
    let by_icon = icon.and_then(|r| {
        let (x, y) = r.center();
        monitors.iter().find(|m| m.bounds.contains(x, y))
    });
    by_icon.or_else(|| monitors.iter().find(|m| m.primary)).or_else(|| monitors.first())
}

/// Place a panel of `size` points (width, height wanted) next to the tray:
/// in the corner of the work area at the end of the taskbar where the clock
/// is, `GAP` points in from its edges; as tall as wanted up to the work
/// area's height, and never wider than the work area.
pub fn place(monitors: &[Monitor], icon: Option<PxRect>, size: (f32, f32)) -> Option<Placement> {
    let m = monitor_for(monitors, icon)?;
    let s = m.scale.max(0.5);
    let px = |points: f32| (points * s).round() as i32;
    let gap = px(GAP);
    let work = m.work;
    let max_h = (work.height() - 2 * gap).max(1);
    let max_w = (work.width() - 2 * gap).max(1);
    let w = px(size.0).min(max_w);
    let h = px(size.1).min(max_h);
    let (left, top) = match taskbar_edge(m) {
        Edge::Bottom => (work.right - gap - w, work.bottom - gap - h),
        Edge::Top => (work.right - gap - w, work.top + gap),
        Edge::Left => (work.left + gap, work.bottom - gap - h),
        Edge::Right => (work.right - gap - w, work.bottom - gap - h),
    };
    Some(Placement { rect: PxRect::new(left, top, left + w, top + h), scale: s, max_height: max_h as f32 / s })
}

/// The same panel grown or shrunk to `height` points, keeping the edge
/// that faces the taskbar where it is (the bottom for a taskbar at the
/// bottom), so the panel stays against the clock.
pub fn resize(p: &Placement, monitors: &[Monitor], height: f32) -> Placement {
    let s = p.scale;
    let h = ((height * s).round() as i32).min((p.max_height * s).round() as i32).max(1);
    let m = monitors.iter().find(|m| m.bounds.contains(p.rect.left, p.rect.top) || m.bounds.contains(p.rect.right - 1, p.rect.bottom - 1));
    let top_anchored = m.is_some_and(|m| taskbar_edge(m) == Edge::Top);
    let rect = if top_anchored {
        PxRect::new(p.rect.left, p.rect.top, p.rect.right, p.rect.top + h)
    } else {
        PxRect::new(p.rect.left, p.rect.bottom - h, p.rect.right, p.rect.bottom)
    };
    Placement { rect, ..*p }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920 × 1080 monitor at `scale` with a 48-point taskbar on `edge`,
    /// its left top corner at (x, y).
    fn monitor(x: i32, y: i32, scale: f32, edge: Edge, primary: bool) -> Monitor {
        let (w, h) = ((1920.0 * scale) as i32, (1080.0 * scale) as i32);
        let bar = (48.0 * scale) as i32;
        let bounds = PxRect::new(x, y, x + w, y + h);
        let work = match edge {
            Edge::Bottom => PxRect::new(x, y, x + w, y + h - bar),
            Edge::Top => PxRect::new(x, y + bar, x + w, y + h),
            Edge::Left => PxRect::new(x + bar, y, x + w, y + h),
            Edge::Right => PxRect::new(x, y, x + w - bar, y + h),
        };
        Monitor { bounds, work, scale, primary }
    }

    #[test]
    fn the_taskbars_edge_is_where_the_work_area_is_cut() {
        for edge in [Edge::Bottom, Edge::Top, Edge::Left, Edge::Right] {
            assert_eq!(taskbar_edge(&monitor(0, 0, 1.0, edge, true)), edge);
        }
        let hidden = Monitor { work: PxRect::new(0, 0, 1920, 1080), ..monitor(0, 0, 1.0, Edge::Bottom, true) };
        assert_eq!(taskbar_edge(&hidden), Edge::Bottom, "a taskbar that hides itself");
    }

    #[test]
    fn the_panel_sits_in_the_corner_by_the_clock_for_each_edge_and_scale() {
        // (edge, scale) → the panel's rectangle for 360 × 500 points.
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let gap = (12.0 * scale).round() as i32;
            let (w, h) = ((360.0 * scale).round() as i32, (500.0 * scale).round() as i32);
            for edge in [Edge::Bottom, Edge::Top, Edge::Left, Edge::Right] {
                let m = monitor(0, 0, scale, edge, true);
                let p = place(&[m], None, (360.0, 500.0)).unwrap();
                assert_eq!((p.rect.width(), p.rect.height()), (w, h), "{edge:?} {scale}");
                let expect = match edge {
                    Edge::Bottom => (m.work.right - gap - w, m.work.bottom - gap - h),
                    Edge::Top => (m.work.right - gap - w, m.work.top + gap),
                    Edge::Left => (m.work.left + gap, m.work.bottom - gap - h),
                    Edge::Right => (m.work.right - gap - w, m.work.bottom - gap - h),
                };
                assert_eq!((p.rect.left, p.rect.top), expect, "{edge:?} {scale}");
                assert!(m.work.contains(p.rect.left, p.rect.top) && m.work.contains(p.rect.right - 1, p.rect.bottom - 1));
            }
        }
    }

    #[test]
    fn the_monitor_is_the_one_whose_tray_was_clicked() {
        // The primary on the right at 100 %, a second on the left at 150 %,
        // further up (negative coordinates).
        let primary = monitor(0, 0, 1.0, Edge::Bottom, true);
        let second = monitor(-2880, -300, 1.5, Edge::Bottom, false);
        let all = [primary, second];
        let icon_on_second = PxRect::new(-200, 1250, -170, 1290);
        let p = place(&all, Some(icon_on_second), (360.0, 500.0)).unwrap();
        assert_eq!(p.scale, 1.5);
        assert!(second.work.contains(p.rect.left, p.rect.top));
        assert_eq!(p.rect.width(), 540);
        // From Start or a second launch: the primary monitor.
        let p = place(&all, None, (360.0, 500.0)).unwrap();
        assert_eq!(p.scale, 1.0);
        assert!(primary.work.contains(p.rect.left, p.rect.top));
    }

    #[test]
    fn a_display_too_small_caps_the_panel_to_the_work_area() {
        // 1366 × 768 at 200 %: 683 × 384 points, less the taskbar.
        let m = Monitor { bounds: PxRect::new(0, 0, 1366, 768), work: PxRect::new(0, 0, 1366, 672), scale: 2.0, primary: true };
        let p = place(&[m], None, (360.0, 900.0)).unwrap();
        assert_eq!(p.rect.height(), 672 - 2 * 24);
        assert_eq!(p.rect.top, 24);
        assert!((p.max_height - 312.0).abs() < 0.01);
        // Narrower than the panel: as wide as the work area allows.
        let tiny = Monitor { bounds: PxRect::new(0, 0, 640, 480), work: PxRect::new(0, 0, 640, 440), scale: 2.0, primary: true };
        let p = place(&[tiny], None, (360.0, 300.0)).unwrap();
        assert_eq!(p.rect.width(), 640 - 48);
        assert!(p.rect.left >= 0);
    }

    #[test]
    fn resizing_keeps_the_edge_by_the_taskbar() {
        let m = monitor(0, 0, 1.5, Edge::Bottom, true);
        let p = place(&[m], None, (360.0, 400.0)).unwrap();
        let taller = resize(&p, &[m], 500.0);
        assert_eq!(taller.rect.bottom, p.rect.bottom);
        assert_eq!(taller.rect.height(), 750);
        let t = monitor(0, 0, 1.0, Edge::Top, true);
        let p = place(&[t], None, (360.0, 400.0)).unwrap();
        let taller = resize(&p, &[t], 500.0);
        assert_eq!(taller.rect.top, p.rect.top);
        // Never past the work area.
        let huge = resize(&p, &[t], 5000.0);
        assert!(huge.rect.bottom <= t.work.bottom);
    }
}
