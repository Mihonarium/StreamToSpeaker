//! Line icons, drawn as vector shapes so they are sharp at every scale and
//! need no icon font: Windows 10 and 11 ship different ones.
//!
//! Each icon is drawn on a 24-unit grid with 1.5-unit round-capped lines,
//! in the style of Windows 11's own icons, and scaled to the size asked for.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

/// Every icon the parts and windows use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// One speaker.
    Speaker,
    /// A group of speakers playing together.
    Group,
    /// A HomePod stereo pair.
    Pair,
    /// An Apple TV or other TV.
    Tv,
    /// This PC (a laptop).
    Laptop,
    /// A desktop PC or a monitor's speakers.
    Monitor,
    Headphones,
    /// Music Assistant: a hub of linked devices.
    Hub,
    ChevronRight,
    ChevronLeft,
    ChevronDown,
    ChevronUp,
    /// Three dots: a menu of more actions.
    More,
    /// Settings.
    Gear,
    Volume,
    Mute,
    Check,
    /// A triangle with "!": needs a look.
    Warning,
    /// A circle with "!": something failed.
    Error,
    /// A circle with "×": not working.
    ErrorCross,
    Info,
    Close,
    Back,
    Search,
    Copy,
    Refresh,
    /// A counter-clockwise arrow: back to the speaker's own delay.
    Reset,
    Wifi,
    Bell,
    Phone,
    Lock,
    Shield,
    Play,
    Music,
    Clock,
    Timer,
    List,
    Wrench,
    User,
    Plus,
    Sliders,
    Flask,
    Help,
    External,
    Keyboard,
}

/// One drawn element of an icon, in 24-unit grid coordinates.
enum El {
    /// SVG path data (M, L, H, V, C, S, A, Z and their relative forms).
    Path(&'static str),
    Circle(f32, f32, f32),
    Dot(f32, f32, f32),
    Rect(f32, f32, f32, f32, f32),
    FilledRect(f32, f32, f32, f32, f32),
}

use El::*;

impl Icon {
    fn elements(self) -> &'static [El] {
        match self {
            Icon::Speaker => &[Rect(6.5, 3.0, 11.0, 18.0, 3.0), Circle(12.0, 14.2, 3.3), Circle(12.0, 7.4, 1.1)],
            Icon::Group => &[
                Rect(2.8, 5.0, 8.2, 14.0, 2.4),
                Circle(6.9, 14.0, 2.3),
                Rect(13.0, 5.0, 8.2, 14.0, 2.4),
                Circle(17.1, 14.0, 2.3),
            ],
            Icon::Pair => &[
                Path("M3 8.5c0-1.6 1.6-2.8 3.8-2.8s3.8 1.2 3.8 2.8v7c0 1.6-1.6 2.8-3.8 2.8S3 17.1 3 15.5z"),
                Path("M13.4 8.5c0-1.6 1.6-2.8 3.8-2.8S21 6.9 21 8.5v7c0 1.6-1.6 2.8-3.8 2.8s-3.8-1.2-3.8-2.8z"),
            ],
            Icon::Tv => &[Rect(3.0, 5.0, 18.0, 12.0, 2.0), Path("M8.5 20h7")],
            Icon::Laptop => &[Rect(5.0, 5.5, 14.0, 10.0, 1.5), Path("M3 18.5h18")],
            Icon::Monitor => &[Rect(3.0, 4.0, 18.0, 12.5, 1.5), Path("M12 16.5V20M8 20h8")],
            Icon::Headphones => &[
                Path("M4 15v-2.5a8 8 0 0 1 16 0V15"),
                Rect(3.5, 14.0, 4.0, 6.0, 1.5),
                Rect(16.5, 14.0, 4.0, 6.0, 1.5),
            ],
            Icon::Hub => &[
                Circle(12.0, 12.0, 2.6),
                Circle(12.0, 4.2, 1.8),
                Circle(5.2, 16.1, 1.8),
                Circle(18.8, 16.1, 1.8),
                Path("M12 6v3.4M6.8 15.2l3-1.7M17.2 15.2l-3-1.7"),
            ],
            Icon::ChevronRight => &[Path("M9.5 6l6 6-6 6")],
            Icon::ChevronLeft => &[Path("M14.5 6l-6 6 6 6")],
            Icon::ChevronDown => &[Path("M6 9.5l6 6 6-6")],
            Icon::ChevronUp => &[Path("M6 14.5l6-6 6 6")],
            Icon::More => &[Dot(6.0, 12.0, 1.2), Dot(12.0, 12.0, 1.2), Dot(18.0, 12.0, 1.2)],
            Icon::Gear => &[
                Circle(12.0, 12.0, 3.2),
                Path("M12 2.8v2.4M12 18.8v2.4M2.8 12h2.4M18.8 12h2.4M5.5 5.5l1.7 1.7M16.8 16.8l1.7 1.7M5.5 18.5l1.7-1.7M16.8 7.2l1.7-1.7"),
                Circle(12.0, 12.0, 6.6),
            ],
            Icon::Volume => &[Path("M4 9.5h3.5L12 5.5v13l-4.5-4H4z"), Path("M15.5 9a4.2 4.2 0 0 1 0 6M18 6.5a7.6 7.6 0 0 1 0 11")],
            Icon::Mute => &[Path("M4 9.5h3.5L12 5.5v13l-4.5-4H4z"), Path("M15.5 9.5l5 5M20.5 9.5l-5 5")],
            Icon::Check => &[Path("M5 12.5l4.5 4.5L19 7.5")],
            Icon::Warning => &[Path("M12 3.8L21.5 20H2.5z"), Path("M12 10v4.6"), Dot(12.0, 17.2, 0.9)],
            Icon::Error => &[Circle(12.0, 12.0, 9.0), Path("M12 7.5v5.5"), Dot(12.0, 16.3, 0.9)],
            Icon::ErrorCross => &[Circle(12.0, 12.0, 9.0), Path("M8.8 8.8l6.4 6.4M15.2 8.8l-6.4 6.4")],
            Icon::Info => &[Circle(12.0, 12.0, 9.0), Path("M12 11v5.5"), Dot(12.0, 7.8, 0.9)],
            Icon::Close => &[Path("M6 6l12 12M18 6L6 18")],
            Icon::Back => &[Path("M19 12H5M11 6l-6 6 6 6")],
            Icon::Search => &[Circle(10.5, 10.5, 6.0), Path("M15 15l5.5 5.5")],
            Icon::Copy => &[Rect(8.0, 8.0, 12.0, 12.0, 2.0), Path("M5 15.5V5.8C5 5.1 5.6 4.5 6.3 4.5H15")],
            Icon::Refresh => &[Path("M19.5 12a7.5 7.5 0 1 1-2.2-5.3"), Path("M19.5 4.5v4.5H15")],
            Icon::Reset => &[Path("M4.5 12a7.5 7.5 0 1 0 2.2-5.3"), Path("M4.5 4.5v4.5H9")],
            Icon::Wifi => &[
                Path("M2.5 9a14 14 0 0 1 19 0M5.5 12.2a9.5 9.5 0 0 1 13 0M8.5 15.3a5 5 0 0 1 7 0"),
                Dot(12.0, 18.6, 1.0),
            ],
            Icon::Bell => &[Path("M6 16.5V11a6 6 0 0 1 12 0v5.5l1.5 1.5h-15z"), Path("M10 20.5h4")],
            Icon::Phone => &[Rect(7.0, 2.8, 10.0, 18.4, 2.2), Path("M11 18.2h2")],
            Icon::Lock => &[Rect(5.0, 10.5, 14.0, 10.0, 2.0), Path("M8 10.5V8a4 4 0 0 1 8 0v2.5")],
            Icon::Shield => &[Path("M12 3l7.5 3v5.5c0 4.4-3.2 8.2-7.5 9.5-4.3-1.3-7.5-5.1-7.5-9.5V6z")],
            Icon::Play => &[Path("M8 5.5v13l10.5-6.5z")],
            Icon::Music => &[Path("M9 18V6l10-2v12"), Circle(6.5, 18.0, 2.5), Circle(16.5, 16.0, 2.5)],
            Icon::Clock => &[Circle(12.0, 12.0, 8.5), Path("M12 7v5l3.5 2")],
            Icon::Timer => &[Circle(12.0, 13.5, 7.5), Path("M12 9.5v4l2.5 1.5M9.5 2.8h5M12 2.8V6")],
            Icon::List => &[
                Path("M8 6.5h12M8 12h12M8 17.5h12"),
                Dot(4.2, 6.5, 0.9),
                Dot(4.2, 12.0, 0.9),
                Dot(4.2, 17.5, 0.9),
            ],
            Icon::Wrench => &[Path("M14.5 4.2a5 5 0 0 0-6 6.4L3.8 15.3a2 2 0 0 0 2.9 2.9l4.7-4.7a5 5 0 0 0 6.4-6l-3 3-2.6-.6-.6-2.6z")],
            Icon::User => &[Circle(12.0, 8.5, 4.0), Path("M4.5 20a7.5 7.5 0 0 1 15 0")],
            Icon::Plus => &[Path("M12 5v14M5 12h14")],
            Icon::Sliders => &[
                Path("M5 4v16M12 4v16M19 4v16"),
                FilledRect(3.0, 13.0, 4.0, 3.0, 1.0),
                FilledRect(10.0, 7.0, 4.0, 3.0, 1.0),
                FilledRect(17.0, 11.0, 4.0, 3.0, 1.0),
            ],
            Icon::Flask => &[
                Path("M9.5 3.5h5M10.5 3.5v6L5 19a1.3 1.3 0 0 0 1.1 2h11.8a1.3 1.3 0 0 0 1.1-2l-5.5-9.5v-6"),
                Path("M7.8 15h8.4"),
            ],
            Icon::Help => &[
                Circle(12.0, 12.0, 9.0),
                Path("M9.6 9.3a2.5 2.5 0 0 1 4.8.9c0 1.7-2.4 2.1-2.4 3.6"),
                Dot(12.0, 17.0, 0.9),
            ],
            Icon::External => &[
                Path("M14 4.5h5.5V10M19.5 4.5L11 13"),
                Path("M18 14v4.5a1.5 1.5 0 0 1-1.5 1.5h-11A1.5 1.5 0 0 1 4 18.5v-11A1.5 1.5 0 0 1 5.5 6H10"),
            ],
            Icon::Keyboard => &[Rect(2.5, 6.5, 19.0, 11.0, 2.0), Path("M7.5 14h9"), Dot(6.0, 10.0, 0.75), Dot(9.0, 10.0, 0.75), Dot(12.0, 10.0, 0.75), Dot(15.0, 10.0, 0.75), Dot(18.0, 10.0, 0.75)],
        }
    }

    /// Filled status mark with a contrasting symbol, for the compact flyout.
    pub(crate) fn paint_badge(self, p: &Painter, r: Rect, fill: Color32, ink: Color32) {
        let c = r.center();
        let k = r.width() / 16.0;
        let at = |x, y| c + Vec2::new(x, y) * k;
        let (stem, dot) = match self {
            Icon::Warning => {
                p.add(Shape::convex_polygon(vec![at(0.0, -6.7), at(7.4, 6.4), at(-7.4, 6.4)], fill, Stroke::NONE));
                ((-2.0, 1.5), 3.8)
            }
            Icon::Info => {
                p.circle_filled(c, 7.5 * k, fill);
                ((-0.4, 3.4), -3.1)
            }
            _ => return self.paint(p, r, fill),
        };
        stroke_polyline(p, vec![at(0.0, stem.0), at(0.0, stem.1)], false, Stroke::new(1.5*k, ink));
        p.circle_filled(at(0.0, dot), 0.9*k, ink);
    }

    /// The compact flyout speaker family, with both drivers visible on a pair.
    pub(crate) fn paint_flyout(self, p: &Painter, r: Rect, ink: Color32) {
        let k = r.width() / 20.0;
        let stroke = Stroke::new(1.15*k, ink);
        let speaker = |x, w, h| {
            let c = r.center() + Vec2::new(x*k, 0.0);
            p.rect_stroke(Rect::from_center_size(c, Vec2::new(w,h)*k), 2.0*k, stroke);
            p.circle_stroke(c + Vec2::new(0.0,1.8*k), (w*0.27)*k, stroke);
            p.circle_filled(c - Vec2::new(0.0,3.3*k), 1.2*k, ink);
        };
        match self {
            Icon::Speaker => speaker(0.0, 8.0, 13.2),
            Icon::Pair | Icon::Group => {
                speaker(-4.1, 6.6, 12.8);
                speaker(4.1, 6.6, 12.8);
            }
            Icon::Laptop => self.paint(p, r.shrink(k), ink),
            _ => self.paint(p, r, ink),
        }
    }

    /// Compact cog with six blunt teeth, readable in a 15-point slot.
    pub(crate) fn paint_cog(p: &Painter, r: Rect, color: Color32) {
        let mut outline = Vec::new();
        for tooth in 0..6 {
            for (angle, radius) in [(-30.0, 0.32), (-16.0, 0.32), (-10.0, 0.43), (10.0, 0.43), (16.0, 0.32)] {
                let a = (tooth as f32 * 60.0 + angle).to_radians();
                outline.push(r.center() + Vec2::angled(a) * (radius * r.width()));
            }
        }
        let stroke = Stroke::new(1.0_f32, color);
        p.add(Shape::closed_line(outline, stroke));
        p.circle_stroke(r.center(), r.width() * 0.13, stroke);
    }

    /// Draw the icon filling the square `rect` with lines of `color`.
    pub fn paint(self, painter: &Painter, rect: Rect, color: Color32) {
        self.paint_weight(painter, rect, color, 1.5);
    }

    /// As [`Icon::paint`], with lines `weight` grid units wide (1.5 normally).
    pub fn paint_weight(self, painter: &Painter, rect: Rect, color: Color32, weight: f32) {
        let k = rect.width().min(rect.height()) / 24.0;
        let at = |x: f32, y: f32| rect.center() + Vec2::new((x - 12.0) * k, (y - 12.0) * k);
        let stroke = Stroke::new(weight * k, color);
        for el in self.elements() {
            match *el {
                Path(d) => {
                    for sub in parse_path(d) {
                        let pts: Vec<Pos2> = sub.points.iter().map(|p| at(p.x, p.y)).collect();
                        stroke_polyline(painter, pts, sub.closed, stroke);
                    }
                }
                Circle(x, y, r) => {
                    painter.circle_stroke(at(x, y), r * k, stroke);
                }
                Dot(x, y, r) => {
                    painter.circle_filled(at(x, y), r * k, color);
                }
                Rect(x, y, w, h, rx) => {
                    let r = egui::Rect::from_min_max(at(x, y), at(x + w, y + h));
                    painter.rect_stroke(r, rx * k, stroke);
                }
                FilledRect(x, y, w, h, rx) => {
                    let r = egui::Rect::from_min_max(at(x, y), at(x + w, y + h));
                    painter.rect_filled(r, rx * k, color);
                }
            }
        }
    }
}

/// A line through `pts`, with round ends where it is open.
fn stroke_polyline(painter: &Painter, pts: Vec<Pos2>, closed: bool, stroke: Stroke) {
    if pts.len() < 2 {
        return;
    }
    if !closed {
        let r = stroke.width / 2.0;
        painter.circle_filled(pts[0], r, stroke.color);
        painter.circle_filled(*pts.last().unwrap(), r, stroke.color);
        painter.add(Shape::line(pts, stroke));
    } else {
        painter.add(Shape::closed_line(pts, stroke));
    }
}

/// A run of points from one move-to to the next.
#[derive(Debug, Default, PartialEq)]
struct Subpath {
    points: Vec<Pos2>,
    closed: bool,
}

/// The points of SVG path data, curves flattened into short lines.
/// Supports M, L, H, V, C, S, A and Z, absolute and relative.
fn parse_path(d: &str) -> Vec<Subpath> {
    let mut tokens = Tokens { s: d.as_bytes(), i: 0 };
    let mut out: Vec<Subpath> = Vec::new();
    let mut cur = Subpath::default();
    let mut p = Pos2::ZERO;
    let mut start = Pos2::ZERO;
    let mut last_ctrl: Option<Pos2> = None;
    let mut cmd = b'M';
    while let Some(c) = tokens.command_or_number() {
        if let Some(c) = c {
            cmd = c;
            if c == b'Z' || c == b'z' {
                cur.closed = true;
                p = start;
                out.push(std::mem::take(&mut cur));
                last_ctrl = None;
                continue;
            }
        }
        let rel = cmd.is_ascii_lowercase();
        let base = if rel { p.to_vec2() } else { Vec2::ZERO };
        let mut num = || tokens.number().unwrap_or(0.0);
        match cmd.to_ascii_uppercase() {
            b'M' => {
                if cur.points.len() > 1 {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur = Subpath::default();
                }
                p = Pos2::new(num(), num()) + base;
                start = p;
                cur.points.push(p);
                // Further pairs after a move are lines.
                cmd = if rel { b'l' } else { b'L' };
                last_ctrl = None;
            }
            b'L' => {
                p = Pos2::new(num(), num()) + base;
                cur.points.push(p);
                last_ctrl = None;
            }
            b'H' => {
                let x = num() + base.x;
                p = Pos2::new(x, p.y);
                cur.points.push(p);
                last_ctrl = None;
            }
            b'V' => {
                let y = num() + base.y;
                p = Pos2::new(p.x, y);
                cur.points.push(p);
                last_ctrl = None;
            }
            b'C' | b'S' => {
                let c1 = if cmd.to_ascii_uppercase() == b'C' {
                    Pos2::new(num(), num()) + base
                } else {
                    match last_ctrl {
                        Some(lc) => p + (p - lc),
                        None => p,
                    }
                };
                let c2 = Pos2::new(num(), num()) + base;
                let end = Pos2::new(num(), num()) + base;
                for i in 1..=10 {
                    let t = i as f32 / 10.0;
                    let u = 1.0 - t;
                    let q = p.to_vec2() * (u * u * u)
                        + c1.to_vec2() * (3.0 * u * u * t)
                        + c2.to_vec2() * (3.0 * u * t * t)
                        + end.to_vec2() * (t * t * t);
                    cur.points.push(q.to_pos2());
                }
                last_ctrl = Some(c2);
                p = end;
            }
            b'A' => {
                let (rx, ry, _rot) = (num(), num(), num());
                let (large, sweep) = (num() != 0.0, num() != 0.0);
                let end = Pos2::new(num(), num()) + base;
                arc_points(&mut cur.points, p, end, rx, ry, large, sweep);
                p = end;
                last_ctrl = None;
            }
            _ => {
                // Unsupported command: skip its number.
                let _ = num();
            }
        }
    }
    if cur.points.len() > 1 {
        out.push(cur);
    }
    out
}

/// Points along an elliptical arc from `a` to `b` (no rotation), after `a`.
fn arc_points(out: &mut Vec<Pos2>, a: Pos2, b: Pos2, rx: f32, ry: f32, large: bool, sweep: bool) {
    use std::f32::consts::TAU;
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 || a == b {
        out.push(b);
        return;
    }
    // Endpoint to centre parameterisation (SVG 1.1, appendix F.6.5), with
    // the x-axis rotation always 0 here.
    let d = (a - b) / 2.0;
    let lambda = (d.x * d.x) / (rx * rx) + (d.y * d.y) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = (rx * rx * ry * ry - rx * rx * d.y * d.y - ry * ry * d.x * d.x).max(0.0);
    let den = rx * rx * d.y * d.y + ry * ry * d.x * d.x;
    let mut coef = if den > 0.0 { (num / den).sqrt() } else { 0.0 };
    if large == sweep {
        coef = -coef;
    }
    let cp = Vec2::new(coef * rx * d.y / ry, -coef * ry * d.x / rx);
    let center = ((a.to_vec2() + b.to_vec2()) / 2.0 + cp).to_pos2();
    let angle = |v: Vec2| v.y.atan2(v.x);
    let t1 = angle(Vec2::new((d.x - cp.x) / rx, (d.y - cp.y) / ry));
    let t2 = angle(Vec2::new((-d.x - cp.x) / rx, (-d.y - cp.y) / ry));
    let mut dt = t2 - t1;
    if sweep && dt < 0.0 {
        dt += TAU;
    } else if !sweep && dt > 0.0 {
        dt -= TAU;
    }
    let steps = ((dt.abs() / TAU) * 48.0).ceil().max(2.0) as usize;
    for i in 1..=steps {
        let t = t1 + dt * i as f32 / steps as f32;
        out.push(center + Vec2::new(rx * t.cos(), ry * t.sin()));
    }
}

struct Tokens<'a> {
    s: &'a [u8],
    i: usize,
}

impl Tokens<'_> {
    fn skip_separators(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] == b' ' || self.s[self.i] == b',') {
            self.i += 1;
        }
    }

    /// `Some(Some(c))` for a command letter, `Some(None)` when a number
    /// follows (an implicit repeat of the last command), `None` at the end.
    fn command_or_number(&mut self) -> Option<Option<u8>> {
        self.skip_separators();
        let c = *self.s.get(self.i)?;
        if c.is_ascii_alphabetic() {
            self.i += 1;
            Some(Some(c))
        } else {
            Some(None)
        }
    }

    fn number(&mut self) -> Option<f32> {
        self.skip_separators();
        let start = self.i;
        let mut seen_dot = false;
        if self.i < self.s.len() && (self.s[self.i] == b'-' || self.s[self.i] == b'+') {
            self.i += 1;
        }
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if c.is_ascii_digit() {
                self.i += 1;
            } else if c == b'.' && !seen_dot {
                seen_dot = true;
                self.i += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }
}

/// The product mark: a laptop in `ink` sending three arcs of `arcs`.
/// `faint_outer` draws the two outer arcs faint (connecting).
pub fn paint_mark(painter: &Painter, rect: Rect, ink: Color32, arcs: Color32, faint_outer: bool) {
    let k = rect.width().min(rect.height()) / 256.0;
    let origin = rect.center() - Vec2::splat(128.0 * k);
    let at = |x: f32, y: f32| origin + Vec2::new(x * k, y * k);
    let ink_stroke = Stroke::new(13.0 * k, ink);
    painter.rect_stroke(egui::Rect::from_min_max(at(43.0, 133.0), at(137.0, 199.0)), 3.0 * k, ink_stroke);
    painter.add(Shape::closed_line(vec![at(43.0, 199.0), at(137.0, 199.0), at(149.0, 225.0), at(31.0, 225.0)], ink_stroke));
    let arc_paths = [
        ("M 106.9 89.7 A 71 71 0 0 1 167.1 148.9", 1.0),
        ("M 111.1 60.0 A 101 101 0 0 1 196.8 144.2", if faint_outer { 0.35 } else { 1.0 }),
        ("M 115.2 30.3 A 131 131 0 0 1 226.4 139.5", if faint_outer { 0.15 } else { 1.0 }),
    ];
    for (d, alpha) in arc_paths {
        let color = arcs.gamma_multiply(alpha);
        for sub in parse_path(d) {
            let pts: Vec<Pos2> = sub.points.iter().map(|p| at(p.x, p.y)).collect();
            stroke_polyline(painter, pts, false, Stroke::new(17.0 * k, color));
        }
    }
}

/// The busy indicator: three quarters of a ring turning once a second.
/// `time` is the clock in seconds (egui's input time), so a picture at a
/// given time is always the same.
pub fn paint_spinner(painter: &Painter, rect: Rect, color: Color32, time: f64) {
    let r = rect.width().min(rect.height()) / 2.0;
    spinner_arc(painter, rect, color, time, (r * 0.2).max(1.5), 0.75);
}

/// A short moving arc on a complete ring, readable beside small status text.
pub(crate) fn paint_spinner_on_track(painter: &Painter, rect: Rect, color: Color32, track: Color32, time: f64) {
    let width = 2.0;
    painter.circle_stroke(rect.center(), (rect.width() - width) / 2.0, Stroke::new(width, track));
    spinner_arc(painter, rect, color, time - 0.5, width, 0.25);
}

fn spinner_arc(painter: &Painter, rect: Rect, color: Color32, time: f64, width: f32, turns: f32) {
    use std::f32::consts::TAU;
    let r = rect.width().min(rect.height()) / 2.0;
    let radius = r - width / 2.0;
    let start = (time.fract() as f32) * TAU;
    let n = 36;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = start + turns * TAU * i as f32 / n as f32;
            rect.center() + radius * Vec2::new(a.cos(), a.sin())
        })
        .collect();
    stroke_polyline(painter, pts, false, Stroke::new(width, color));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Pos2, b: Pos2) -> bool {
        (a - b).length() < 0.01
    }

    #[test]
    fn lines_and_relative_moves() {
        let subs = parse_path("M12 2.8v2.4M2.8 12h2.4M5.5 5.5l1.7 1.7");
        assert_eq!(subs.len(), 3);
        assert!(close(subs[0].points[1], Pos2::new(12.0, 5.2)));
        assert!(close(subs[1].points[1], Pos2::new(5.2, 12.0)));
        assert!(close(subs[2].points[1], Pos2::new(7.2, 7.2)));
    }

    #[test]
    fn a_closed_path_ends_where_it_started() {
        let subs = parse_path("M12 3.8L21.5 20H2.5z");
        assert_eq!(subs.len(), 1);
        assert!(subs[0].closed);
        assert_eq!(subs[0].points.len(), 3);
    }

    #[test]
    fn an_arc_ends_at_its_endpoint_on_its_circle() {
        // A half circle of radius 8 around (12, 12.5), over the top.
        let subs = parse_path("M4 12.5a8 8 0 0 1 16 0");
        let pts = &subs[0].points;
        assert!(close(*pts.last().unwrap(), Pos2::new(20.0, 12.5)));
        for p in pts {
            assert!(((*p - Pos2::new(12.0, 12.5)).length() - 8.0).abs() < 0.01, "{p:?}");
            assert!(p.y <= 12.51, "goes over the top, not under: {p:?}");
        }
    }

    #[test]
    fn numbers_without_separators() {
        // "-1.2-2.8" is two numbers, ".6-.6" too.
        let subs = parse_path("M10 10l-1.2-2.8l.6-.6");
        assert!(close(subs[0].points[1], Pos2::new(8.8, 7.2)));
        assert!(close(subs[0].points[2], Pos2::new(9.4, 6.6)));
    }
}
