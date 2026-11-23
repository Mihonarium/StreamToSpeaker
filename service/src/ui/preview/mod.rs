//! Pictures of the real UI code, made without a window or a GPU.
//!
//! A [`Scene`] is a name, a size and a function that draws. [`render`] runs
//! it through egui exactly as a window would, then draws egui's output on
//! the CPU ([`raster`]) the way the app's OpenGL renderer draws it, so the
//! picture shows what the window shows: the same layout, the same text
//! rasterised by the same font code, the same colours. Everything that
//! varies (light or dark, scale, the clock) is an input, so a scene gives
//! the same pixels every time.
//!
//! The example `ui_preview` writes scenes to PNG files; the surfaces add
//! their scenes to [`crate::ui::scenes`].

pub mod raster;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use egui::{Context, FontDefinitions, Pos2, Rect, Response, Shape, Vec2};

use super::look::{forced_id, Force, Forced};
use super::theme::{self, Mode, Theme};

/// A named picture of some UI.
#[derive(Clone, Copy)]
pub struct Scene {
    /// Lowercase words joined by hyphens, unique, such as `delay-states`.
    pub name: &'static str,
    /// Width and height in points (pixels at 100 % scale).
    pub size: [f32; 2],
    /// Draws the scene. It is called several times per picture (egui lays
    /// out in passes) and must draw the same thing each time.
    pub draw: fn(&mut egui::Ui, &mut Stage),
}

/// What a scene is drawn with, and how it asks for controls to be shown
/// pointed at, pressed or focused.
pub struct Stage {
    mode: Mode,
    time: f64,
    forced: Forced,
}

impl Stage {
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The clock, in seconds: spinners and other motion are drawn as they
    /// are at this time.
    pub fn time(&self) -> f64 {
        self.time
    }

    /// Show the control behind `r` as if the pointer were on it.
    pub fn hover(&mut self, r: &Response) {
        self.force(r, Force::Hover);
    }

    /// Show the control behind `r` as if it were being pressed.
    pub fn press(&mut self, r: &Response) {
        self.force(r, Force::Press);
    }

    /// Give the control behind `r` the keyboard focus (its ring shows; a
    /// text field shows its caret).
    pub fn focus(&mut self, r: &Response) {
        self.force(r, Force::Focus);
        if !r.has_focus() {
            r.request_focus();
        }
    }

    /// Show every control in `area` whose accessible name starts with
    /// `name` as pointed at: for controls inside a part, such as the delay
    /// control's *Remove 25 milliseconds of delay*.
    pub fn hover_named(&mut self, area: Rect, name: &str) {
        self.force_named(area, name, Force::Hover);
    }

    pub fn press_named(&mut self, area: Rect, name: &str) {
        self.force_named(area, name, Force::Press);
    }

    /// Draw the focus ring on the controls in `area` named so (it doesn't
    /// move the keyboard focus).
    pub fn focus_named(&mut self, area: Rect, name: &str) {
        self.force_named(area, name, Force::Focus);
    }

    fn force(&mut self, r: &Response, f: Force) {
        let list = self.forced.by_id.entry(r.id).or_default();
        if !list.contains(&f) {
            list.push(f);
        }
    }

    fn force_named(&mut self, area: Rect, name: &str, f: Force) {
        let entry = (area, name.to_owned(), f);
        if !self.forced.by_name.contains(&entry) {
            self.forced.by_name.push(entry);
        }
    }
}

/// How to draw a scene.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub mode: Mode,
    /// Pixels per point: 1.0, 1.25, 1.5, 2.0.
    pub scale: f32,
    /// The clock, in seconds.
    pub time: f64,
}

/// A finished picture: 8-bit RGBA, rows top to bottom.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The fonts for pictures: the app's own font stack (see [`crate::fonts`]),
/// loaded from `dir` by the Windows file names (`segoeui.ttf`,
/// `seguisb.ttf`, `seguisym.ttf`, `msyh.ttc` and the rest). On a machine
/// without Windows' fonts, `dir` can hold other fonts under those names.
/// Every font is given its Windows line metrics (`usWinAscent` and
/// `usWinDescent`) as the ones egui reads, as Windows itself spaces lines:
/// for Segoe UI they are the same, and a font made to stand in for it,
/// such as Selawik, then takes exactly Segoe UI's room.
pub fn fonts_from_dir(dir: &Path) -> FontDefinitions {
    let mut load = |file: &str| -> Option<&'static [u8]> {
        let mut bytes = std::fs::read(dir.join(file)).ok()?;
        use_windows_line_metrics(&mut bytes);
        Some(Box::leak(bytes.into_boxed_slice()))
    };
    crate::fonts::definitions(&crate::fonts::DEFAULT_HAN_ORDER, &mut load).0
}

/// Copy a single font file's `OS/2` Windows ascent and descent into its
/// `hhea` ascender, descender and line gap (0). Collections and files
/// without those tables are left as they are.
fn use_windows_line_metrics(font: &mut [u8]) {
    let be16 = |b: &[u8], at: usize| -> Option<u16> { Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?])) };
    let be32 = |b: &[u8], at: usize| -> Option<u32> { Some(u32::from_be_bytes([*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?])) };
    if font.get(0..4) == Some(b"ttcf") {
        return;
    }
    let Some(count) = be16(font, 4) else { return };
    let table = |tag: &[u8; 4]| -> Option<usize> {
        (0..count as usize).find_map(|i| {
            let rec = 12 + 16 * i;
            (font.get(rec..rec + 4)? == tag).then(|| be32(font, rec + 8).map(|o| o as usize)).flatten()
        })
    };
    let (Some(hhea), Some(os2)) = (table(b"hhea"), table(b"OS/2")) else { return };
    let (Some(win_ascent), Some(win_descent)) = (be16(font, os2 + 74), be16(font, os2 + 76)) else { return };
    if font.len() < hhea + 10 {
        return;
    }
    font[hhea + 4..hhea + 6].copy_from_slice(&(win_ascent as i16).to_be_bytes());
    font[hhea + 6..hhea + 8].copy_from_slice(&(-(win_descent as i16)).to_be_bytes());
    font[hhea + 8..hhea + 10].copy_from_slice(&0i16.to_be_bytes());
}

/// A context set up as a window of the app is: `fonts`, the theme in
/// `mode`. For tests of layout.
pub fn context(fonts: FontDefinitions, mode: Mode) -> Context {
    let ctx = Context::default();
    ctx.set_fonts(fonts);
    theme::apply(&ctx, mode);
    ctx
}

/// The input of one pass: a window of `size` points at `scale`.
pub fn input(size: Vec2, scale: f32, time: f64) -> egui::RawInput {
    let mut input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
        time: Some(time),
        focused: true,
        ..Default::default()
    };
    input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(scale);
    input
}

/// Draw `scene`. Fails, saying why, when the scene panics, draws past its
/// own size, or shows a character no font has (it would be a box).
pub fn render(scene: &Scene, fonts: &FontDefinitions, settings: Settings) -> Result<Picture, String> {
    let result = catch_unwind(AssertUnwindSafe(|| render_inner(scene, fonts, settings)));
    match result {
        Ok(r) => r,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "a panic".into());
            Err(format!("panicked: {msg}"))
        }
    }
}

fn render_inner(scene: &Scene, fonts: &FontDefinitions, s: Settings) -> Result<Picture, String> {
    let ctx = context(fonts.clone(), s.mode);
    // Pictures show where motion ends, never a frame of a fade.
    ctx.style_mut(|st| st.animation_time = 0.0);
    let theme = Theme::new(s.mode);
    let size = Vec2::from(scene.size);
    let mut textures = raster::Textures::default();
    let mut forced = Forced::default();
    let mut output = None;
    let mut content = Rect::NOTHING;
    // Pass 1 lays out and learns which controls the scene wants in which
    // state; later passes draw them so, until nothing changes.
    for pass in 0..6 {
        ctx.data_mut(|d| d.insert_temp(forced_id(), forced.clone()));
        let mut stage = Stage { mode: s.mode, time: s.time, forced: Forced::default() };
        let out = ctx.run(input(size, s.scale, s.time), |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::none().fill(theme.c.surface)).show(ctx, |ui| {
                content = ui.scope(|ui| (scene.draw)(ui, &mut stage)).response.rect;
            });
        });
        textures.update(&out.textures_delta);
        let settled = pass >= 2 && stage.forced == forced;
        forced = stage.forced;
        output = Some(out);
        if settled {
            break;
        }
    }
    let out = output.expect("at least one pass");
    let screen = Rect::from_min_size(Pos2::ZERO, size);
    if !screen.expand(0.5).contains_rect(content) {
        return Err(format!(
            "draws {:.0} × {:.0} points, past its size of {:.0} × {:.0}",
            content.max.x, content.max.y, size.x, size.y
        ));
    }
    let mut missing = String::new();
    for cs in &out.shapes {
        missing_glyphs(&ctx, &cs.shape, &mut missing);
    }
    if !missing.is_empty() {
        return Err(format!("no font has these characters, so they show as boxes: {missing}"));
    }
    let ppp = out.pixels_per_point;
    let primitives = ctx.tessellate(out.shapes, ppp);
    let (w, h) = ((size.x * ppp).round() as usize, (size.y * ppp).round() as usize);
    let mut canvas = raster::Canvas::new(w, h, theme.c.surface);
    canvas.draw(&primitives, &textures, ppp);
    Ok(Picture { width: w as u32, height: h as u32, rgba: canvas.into_rgba() })
}

fn missing_glyphs(ctx: &Context, shape: &Shape, out: &mut String) {
    match shape {
        Shape::Vec(shapes) => shapes.iter().for_each(|s| missing_glyphs(ctx, s, out)),
        Shape::Text(t) => {
            let job = &t.galley.job;
            for section in &job.sections {
                let text = &job.text[section.byte_range.clone()];
                for c in text.chars().filter(|c| !c.is_whitespace() && !c.is_control()) {
                    let font = &section.format.font_id;
                    if !ctx.fonts(|f| f.has_glyph(font, c)) && !out.contains(c) {
                        out.push(c);
                    }
                }
            }
        }
        _ => {}
    }
}

/// The scenes whose names match `patterns`: a name, or a prefix ending in
/// `*`. No patterns means every scene. A pattern that matches nothing is
/// an error that lists the names.
pub fn select<'a>(scenes: &'a [Scene], patterns: &[String]) -> Result<Vec<&'a Scene>, String> {
    let mut seen = std::collections::HashSet::new();
    for s in scenes {
        if !seen.insert(s.name) {
            return Err(format!("two scenes are named {:?}", s.name));
        }
    }
    if patterns.is_empty() {
        return Ok(scenes.iter().collect());
    }
    let mut out: Vec<&Scene> = Vec::new();
    for p in patterns {
        let matches: Vec<&Scene> = match p.strip_suffix('*') {
            Some(prefix) => scenes.iter().filter(|s| s.name.starts_with(prefix)).collect(),
            None => scenes.iter().filter(|s| s.name == p).collect(),
        };
        if matches.is_empty() {
            let names: Vec<&str> = scenes.iter().map(|s| s.name).collect();
            return Err(format!("no scene matches {p:?}; the scenes are: {}", names.join(", ")));
        }
        for m in matches {
            if !out.iter().any(|o| o.name == m.name) {
                out.push(m);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> FontDefinitions {
        crate::fonts::definitions(&[], &mut |_| None).0
    }

    fn settings(mode: Mode) -> Settings {
        Settings { mode, scale: 1.0, time: 0.0 }
    }

    const BOX: Scene = Scene {
        name: "test-box",
        size: [120.0, 40.0],
        draw: |ui, _| {
            ui.label("Kitchen");
        },
    };

    #[test]
    fn the_same_scene_gives_the_same_pixels() {
        let a = render(&BOX, &fonts(), settings(Mode::Light)).unwrap();
        let b = render(&BOX, &fonts(), settings(Mode::Light)).unwrap();
        assert_eq!((a.width, a.height), (120, 40));
        assert!(a.rgba == b.rgba);
        let dark = render(&BOX, &fonts(), settings(Mode::Dark)).unwrap();
        assert!(a.rgba != dark.rgba);
    }

    #[test]
    fn scale_sets_the_picture_size() {
        let p = render(&BOX, &fonts(), Settings { scale: 1.5, ..settings(Mode::Light) }).unwrap();
        assert_eq!((p.width, p.height), (180, 60));
    }

    #[test]
    fn a_scene_that_draws_past_its_size_fails() {
        let s = Scene {
            name: "too-small",
            size: [60.0, 20.0],
            draw: |ui, _| {
                ui.add_space(30.0);
                ui.label("text");
            },
        };
        let e = render(&s, &fonts(), settings(Mode::Light)).err().unwrap();
        assert!(e.contains("past its size"), "{e}");
    }

    #[test]
    fn a_panicking_scene_fails_with_its_message() {
        let s = Scene { name: "boom", size: [60.0, 20.0], draw: |_, _| panic!("kaput") };
        let e = render(&s, &fonts(), settings(Mode::Light)).err().unwrap();
        assert!(e.contains("kaput"), "{e}");
    }

    #[test]
    fn a_character_no_font_has_fails() {
        // egui's own fonts have no Chinese.
        let s = Scene { name: "boxes", size: [80.0, 30.0], draw: |ui, _| { ui.label("客厅"); } };
        let e = render(&s, &fonts(), settings(Mode::Light)).err().unwrap();
        assert!(e.contains("客") && e.contains("boxes"), "{e}");
    }

    #[test]
    fn selecting_by_name_and_prefix() {
        let mk = |name| Scene { name, size: [1.0, 1.0], draw: |_, _| {} };
        let scenes = [mk("delay-a"), mk("delay-b"), mk("row")];
        let names = |p: &[&str]| -> Vec<&str> {
            select(&scenes, &p.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap().iter().map(|s| s.name).collect()
        };
        assert_eq!(names(&["delay-*"]), ["delay-a", "delay-b"]);
        assert_eq!(names(&["row", "delay-a", "row"]), ["row", "delay-a"]);
        assert_eq!(names(&[]).len(), 3);
        assert!(select(&scenes, &["nope".into()]).err().unwrap().contains("delay-a"));
        let dup = [mk("x"), mk("x")];
        assert!(select(&dup, &[]).is_err());
    }
}
