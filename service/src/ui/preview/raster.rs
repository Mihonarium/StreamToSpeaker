//! Draws egui's triangles on the CPU the way the app's OpenGL renderer
//! (egui_glow) draws them on the GPU: vertex colours and texture texels
//! multiplied in gamma space, premultiplied alpha blended with
//! `ONE, ONE_MINUS_SRC_ALPHA`, bilinear texture filtering, a scissor per
//! clip rectangle, and the coverage of each pixel sampled at its centre.
//! egui's meshes carry their own anti-aliasing (a feathered edge one pixel
//! wide), so nothing more is needed to match the window's pixels closely.

use std::collections::HashMap;

use egui::epaint::{ClippedPrimitive, ImageData, ImageDelta, Primitive, TextureId, Vertex};
use egui::{Color32, TextureFilter};

/// A texture as the renderer holds it: premultiplied RGBA, 0–1, gamma space.
struct Texture {
    size: [usize; 2],
    texels: Vec<[f32; 4]>,
    filter: TextureFilter,
}

/// Every texture egui has asked for so far.
#[derive(Default)]
pub struct Textures {
    map: HashMap<TextureId, Texture>,
}

impl Textures {
    /// Apply one frame's changes, in order.
    pub fn update(&mut self, delta: &egui::TexturesDelta) {
        for (id, d) in &delta.set {
            self.set(*id, d);
        }
        for id in &delta.free {
            self.map.remove(id);
        }
    }

    fn set(&mut self, id: TextureId, delta: &ImageDelta) {
        let (size, texels): ([usize; 2], Vec<[f32; 4]>) = match &delta.image {
            ImageData::Color(image) => (image.size, image.pixels.iter().map(|c| unit(*c)).collect()),
            // The same coverage curve egui_glow uploads fonts with.
            ImageData::Font(image) => (image.size, image.srgba_pixels(None).map(unit).collect()),
        };
        let filter = delta.options.magnification;
        match delta.pos {
            None => {
                self.map.insert(id, Texture { size, texels, filter });
            }
            Some([x0, y0]) => {
                let Some(t) = self.map.get_mut(&id) else { return };
                for y in 0..size[1] {
                    for x in 0..size[0] {
                        let (tx, ty) = (x0 + x, y0 + y);
                        if tx < t.size[0] && ty < t.size[1] {
                            t.texels[ty * t.size[0] + tx] = texels[y * size[0] + x];
                        }
                    }
                }
            }
        }
    }
}

fn unit(c: Color32) -> [f32; 4] {
    [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, c.a() as f32 / 255.0]
}

impl Texture {
    fn texel(&self, x: i64, y: i64) -> [f32; 4] {
        let x = x.clamp(0, self.size[0] as i64 - 1) as usize;
        let y = y.clamp(0, self.size[1] as i64 - 1) as usize;
        self.texels[y * self.size[0] + x]
    }

    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        let (w, h) = (self.size[0] as f32, self.size[1] as f32);
        match self.filter {
            TextureFilter::Nearest => self.texel((u * w).floor() as i64, (v * h).floor() as i64),
            TextureFilter::Linear => {
                let (x, y) = (u * w - 0.5, v * h - 0.5);
                let (x0, y0) = (x.floor(), y.floor());
                let (fx, fy) = (x - x0, y - y0);
                let (x0, y0) = (x0 as i64, y0 as i64);
                let a = self.texel(x0, y0);
                let b = self.texel(x0 + 1, y0);
                let c = self.texel(x0, y0 + 1);
                let d = self.texel(x0 + 1, y0 + 1);
                let mut out = [0.0; 4];
                for i in 0..4 {
                    let top = a[i] + (b[i] - a[i]) * fx;
                    let bottom = c[i] + (d[i] - c[i]) * fx;
                    out[i] = top + (bottom - top) * fy;
                }
                out
            }
        }
    }
}

/// An RGBA picture, 8 bits per channel, rows top to bottom.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    px: Vec<[f32; 4]>,
}

impl Canvas {
    pub fn new(width: usize, height: usize, clear: Color32) -> Self {
        Canvas { width, height, px: vec![unit(clear); width * height] }
    }

    /// Draw `primitives` (egui's tessellated output, positions in points).
    pub fn draw(&mut self, primitives: &[ClippedPrimitive], textures: &Textures, pixels_per_point: f32) {
        for p in primitives {
            let Primitive::Mesh(mesh) = &p.primitive else { continue };
            let Some(tex) = textures.map.get(&mesh.texture_id) else { continue };
            // Scissor exactly as egui_glow computes it.
            let clip = p.clip_rect;
            let x0 = ((clip.min.x * pixels_per_point).round() as i64).clamp(0, self.width as i64);
            let y0 = ((clip.min.y * pixels_per_point).round() as i64).clamp(0, self.height as i64);
            let x1 = ((clip.max.x * pixels_per_point).round() as i64).clamp(x0, self.width as i64);
            let y1 = ((clip.max.y * pixels_per_point).round() as i64).clamp(y0, self.height as i64);
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            for tri in mesh.indices.chunks_exact(3) {
                let v = [&mesh.vertices[tri[0] as usize], &mesh.vertices[tri[1] as usize], &mesh.vertices[tri[2] as usize]];
                self.triangle(v, tex, pixels_per_point, [x0, y0, x1, y1]);
            }
        }
    }

    fn triangle(&mut self, v: [&Vertex; 3], tex: &Texture, ppp: f32, clip: [i64; 4]) {
        let p: [(f64, f64); 3] = [0, 1, 2].map(|i| ((v[i].pos.x * ppp) as f64, (v[i].pos.y * ppp) as f64));
        let edge = |a: (f64, f64), b: (f64, f64), c: (f64, f64)| (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
        let area = edge(p[0], p[1], p[2]);
        if area == 0.0 || !area.is_finite() {
            return;
        }
        let sign = area.signum();
        // A pixel centre exactly on an edge belongs to one of the two
        // triangles that share it: the one for which the edge (in the
        // triangle's own winding, made positive) runs downwards, or
        // leftwards when level.
        let owns = |a: (f64, f64), b: (f64, f64)| {
            let (dx, dy) = ((b.0 - a.0) * sign, (b.1 - a.1) * sign);
            dy > 0.0 || (dy == 0.0 && dx < 0.0)
        };
        let own = [owns(p[1], p[2]), owns(p[2], p[0]), owns(p[0], p[1])];
        let min_x = p.iter().map(|q| q.0).fold(f64::INFINITY, f64::min).floor() as i64;
        let max_x = p.iter().map(|q| q.0).fold(f64::NEG_INFINITY, f64::max).ceil() as i64;
        let min_y = p.iter().map(|q| q.1).fold(f64::INFINITY, f64::min).floor() as i64;
        let max_y = p.iter().map(|q| q.1).fold(f64::NEG_INFINITY, f64::max).ceil() as i64;
        let (x0, x1) = (min_x.max(clip[0]), max_x.min(clip[2]));
        let (y0, y1) = (min_y.max(clip[1]), max_y.min(clip[3]));
        let col = [0, 1, 2].map(|i| unit(v[i].color));
        let uv = [0, 1, 2].map(|i| (v[i].uv.x, v[i].uv.y));
        for y in y0..y1 {
            let py = y as f64 + 0.5;
            for x in x0..x1 {
                let c = (x as f64 + 0.5, py);
                let w = [edge(p[1], p[2], c) * sign, edge(p[2], p[0], c) * sign, edge(p[0], p[1], c) * sign];
                if (0..3).any(|i| w[i] < 0.0 || (w[i] == 0.0 && !own[i])) {
                    continue;
                }
                let total = area.abs();
                let b = [(w[0] / total) as f32, (w[1] / total) as f32, (w[2] / total) as f32];
                let u = uv[0].0 * b[0] + uv[1].0 * b[1] + uv[2].0 * b[2];
                let vv = uv[0].1 * b[0] + uv[1].1 * b[1] + uv[2].1 * b[2];
                let t = tex.sample(u, vv);
                let dst = &mut self.px[y as usize * self.width + x as usize];
                let mut src = [0.0f32; 4];
                for i in 0..4 {
                    src[i] = (col[0][i] * b[0] + col[1][i] * b[1] + col[2][i] * b[2]) * t[i];
                }
                let keep = 1.0 - src[3];
                for i in 0..4 {
                    dst[i] = src[i] + dst[i] * keep;
                }
            }
        }
    }

    /// The picture as 8-bit RGBA.
    pub fn into_rgba(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.px.len() * 4);
        for p in self.px {
            for c in p {
                out.push((c.clamp(0.0, 1.0) * 255.0).round() as u8);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::Mesh;
    use egui::{Pos2, Rect};

    fn white_texture() -> Textures {
        let mut t = Textures::default();
        let image = egui::ColorImage::new([1, 1], Color32::WHITE);
        let delta = ImageDelta::full(image, egui::TextureOptions::LINEAR);
        t.update(&egui::TexturesDelta { set: vec![(TextureId::default(), delta)], free: vec![] });
        t
    }

    #[test]
    fn two_triangles_sharing_an_edge_cover_each_pixel_once() {
        // A half-transparent square made of two triangles: a pixel drawn
        // twice would come out darker along the diagonal.
        let mut mesh = Mesh::default();
        mesh.add_colored_rect(Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(8.0, 8.0)), Color32::from_black_alpha(128));
        let prim = ClippedPrimitive { clip_rect: Rect::EVERYTHING, primitive: Primitive::Mesh(mesh) };
        let mut canvas = Canvas::new(8, 8, Color32::WHITE);
        canvas.draw(&[prim], &white_texture(), 1.0);
        let rgba = canvas.into_rgba();
        let first = rgba[0];
        assert!(rgba.chunks(4).all(|p| p[0] == first), "uneven: {:?}", rgba.chunks(4).map(|p| p[0]).collect::<Vec<_>>());
        assert_eq!(first, 127);
    }

    #[test]
    fn the_clip_rectangle_is_respected() {
        let mut mesh = Mesh::default();
        mesh.add_colored_rect(Rect::from_min_max(Pos2::ZERO, Pos2::new(8.0, 8.0)), Color32::BLACK);
        let clip = Rect::from_min_max(Pos2::ZERO, Pos2::new(4.0, 8.0));
        let prim = ClippedPrimitive { clip_rect: clip, primitive: Primitive::Mesh(mesh) };
        let mut canvas = Canvas::new(8, 8, Color32::WHITE);
        canvas.draw(&[prim], &white_texture(), 1.0);
        let rgba = canvas.into_rgba();
        assert_eq!(rgba[3 * 4], 0, "inside the clip");
        assert_eq!(rgba[4 * 4], 255, "outside the clip");
    }
}
