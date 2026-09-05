//! Egui's antialiased meshes composited into the presentation buffer.
//! The decoded remote framebuffer is never a render target. Keeping this
//! backend in software also keeps session windows usable without an OpenGL
//! context (including remote and virtual displays).
use eframe::egui::{self, epaint, Color32, TextureId};
use std::collections::HashMap;

#[derive(Default)]
pub struct Renderer {
    textures: HashMap<TextureId, Texture>,
}
struct Texture {
    size: [usize; 2],
    pixels: Vec<Color32>,
}
impl Renderer {
    pub fn paint(
        &mut self,
        ctx: &egui::Context,
        output: egui::FullOutput,
        dst: &mut [u32],
        w: u32,
        h: u32,
    ) {
        if dst.len() < w as usize * h as usize {
            return;
        }
        for (id, delta) in output.textures_delta.set {
            let size = delta.image.size();
            let pixels: Vec<_> = match delta.image {
                egui::ImageData::Color(image) => image.pixels.clone(),
                egui::ImageData::Font(image) => image.srgba_pixels(None).collect(),
            };
            if let Some([x, y]) = delta.pos {
                if let Some(t) = self.textures.get_mut(&id) {
                    for row in 0..size[1] {
                        let start = (y + row) * t.size[0] + x;
                        t.pixels[start..start + size[0]]
                            .copy_from_slice(&pixels[row * size[0]..(row + 1) * size[0]]);
                    }
                }
            } else {
                self.textures.insert(id, Texture { size, pixels });
            }
        }
        let scale = output.pixels_per_point;
        for primitive in ctx.tessellate(output.shapes, scale) {
            let epaint::Primitive::Mesh(mesh) = primitive.primitive else {
                continue;
            };
            let Some(texture) = self.textures.get(&mesh.texture_id) else {
                continue;
            };
            let clip = primitive.clip_rect * scale;
            for tri in mesh.indices.chunks_exact(3) {
                let v = [
                    mesh.vertices[tri[0] as usize],
                    mesh.vertices[tri[1] as usize],
                    mesh.vertices[tri[2] as usize],
                ];
                triangle(dst, [w, h], clip, v, scale, texture);
            }
        }
        for id in output.textures_delta.free {
            self.textures.remove(&id);
        }
    }
}
fn edge(a: egui::Pos2, b: egui::Pos2, p: egui::Pos2) -> f32 {
    (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x)
}
fn triangle(
    dst: &mut [u32],
    size: [u32; 2],
    clip: egui::Rect,
    mut v: [epaint::Vertex; 3],
    scale: f32,
    texture: &Texture,
) {
    for vertex in &mut v {
        vertex.pos = vertex.pos * scale;
    }
    let mut area = edge(v[0].pos, v[1].pos, v[2].pos);
    if area == 0.0 {
        return;
    }
    if area < 0.0 {
        v.swap(1, 2);
        area = -area;
    }
    let bounds = egui::Rect::from_points(&[v[0].pos, v[1].pos, v[2].pos]).intersect(clip);
    let x0 = bounds.min.x.max(0.0).floor() as u32;
    let y0 = bounds.min.y.max(0.0).floor() as u32;
    let x1 = bounds.max.x.min(size[0] as f32).ceil() as u32;
    let y1 = bounds.max.y.min(size[1] as f32).ceil() as u32;
    let edges = [
        (v[1].pos, v[2].pos),
        (v[2].pos, v[0].pos),
        (v[0].pos, v[1].pos),
    ];
    // Half-open edges ensure a shared diagonal is blended exactly once.
    let inclusive = edges.map(|(a, b)| b.y < a.y || (b.y == a.y && b.x > a.x));
    for y in y0..y1 {
        for x in x0..x1 {
            let p = egui::pos2(x as f32 + 0.5, y as f32 + 0.5);
            if !clip.contains(p) {
                continue;
            }
            let e = edges.map(|(a, b)| edge(a, b, p));
            if (0..3).any(|i| e[i] < 0.0 || (e[i] == 0.0 && !inclusive[i])) {
                continue;
            }
            let weights = e.map(|e| e / area);
            let uv = v[0].uv.to_vec2() * weights[0]
                + v[1].uv.to_vec2() * weights[1]
                + v[2].uv.to_vec2() * weights[2];
            let tex = sample(texture, uv);
            let mut rgba = [0.0; 4];
            for i in 0..4 {
                let color: f32 = (0..3).map(|j| v[j].color[i] as f32 * weights[j]).sum();
                rgba[i] = tex[i] * color / 255.0;
            }
            let pixel = &mut dst[(y * size[0] + x) as usize];
            let mut result = 0;
            for (i, shift) in [16, 8, 0].into_iter().enumerate() {
                let old = ((*pixel >> shift) & 255) as f32;
                result |= ((rgba[i] + old * (1.0 - rgba[3] / 255.0))
                    .round()
                    .clamp(0.0, 255.0) as u32)
                    << shift;
            }
            *pixel = result;
        }
    }
}
fn sample(t: &Texture, uv: egui::Vec2) -> [f32; 4] {
    let x = (uv.x * t.size[0] as f32 - 0.5).clamp(0.0, (t.size[0] - 1) as f32);
    let y = (uv.y * t.size[1] as f32 - 0.5).clamp(0.0, (t.size[1] - 1) as f32);
    let (x0, y0) = (x as usize, y as usize);
    let (x1, y1) = ((x0 + 1).min(t.size[0] - 1), (y0 + 1).min(t.size[1] - 1));
    let (fx, fy) = (x.fract(), y.fract());
    std::array::from_fn(|i| {
        let a = t.pixels[y0 * t.size[0] + x0][i] as f32 * (1.0 - fx)
            + t.pixels[y0 * t.size[0] + x1][i] as f32 * fx;
        let b = t.pixels[y1 * t.size[0] + x0][i] as f32 * (1.0 - fx)
            + t.pixels[y1 * t.size[0] + x1][i] as f32 * fx;
        a * (1.0 - fy) + b * fy
    })
}

thread_local! {
    static PAINTER: std::cell::RefCell<(egui::Context, Renderer)> = std::cell::RefCell::new((egui::Context::default(), Renderer::default()));
}
/// Antialiased vector shapes and proportional text for non-interactive chrome.
pub fn paint(dst: &mut [u32], w: u32, h: u32, draw: impl Fn(&egui::Painter)) {
    PAINTER.with_borrow_mut(|(ctx, renderer)| {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(w as f32, h as f32),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            draw(&ctx.layer_painter(egui::LayerId::background()))
        });
        renderer.paint(ctx, output, dst, w, h);
    });
}
thread_local! {
    static METRICS: egui::Context = {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        ctx
    };
}
pub fn text_width(text: &str, size: f32) -> u32 {
    METRICS.with(|ctx| {
        ctx.fonts(|fonts| {
            fonts
                .layout_no_wrap(
                    text.to_owned(),
                    egui::FontId::proportional(size),
                    Color32::WHITE,
                )
                .size()
                .x
                .ceil() as u32
        })
    })
}

pub fn color(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn translucent_mesh_has_no_double_blended_diagonal_and_is_clipped() {
        let mut pixels = vec![0xffffff; 32 * 32];
        paint(&mut pixels, 32, 32, |p| {
            p.with_clip_rect(egui::Rect::from_min_max(
                egui::pos2(4.0, 4.0),
                egui::pos2(20.0, 20.0),
            ))
            .rect_filled(
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(30.0, 30.0)),
                0,
                Color32::from_black_alpha(128),
            );
        });
        for y in 0..32 {
            for x in 0..32 {
                assert_eq!(
                    pixels[y * 32 + x],
                    if (4..20).contains(&x) && (4..20).contains(&y) {
                        0x7f7f7f
                    } else {
                        0xffffff
                    }
                );
            }
        }
    }
}
