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
    // Most UI pixels are solid panel/row backgrounds. Egui represents these
    // with a single UV in its white texel; sampling four texels and interpolating
    // four identical vertex colours for every pixel made a Retina window cost
    // tens of milliseconds, on the same thread that handles pointer events.
    // Keep the general path for text/images and gradients, including AA fringes.
    let constant_tex =
        (v[0].uv == v[1].uv && v[0].uv == v[2].uv).then(|| sample(texture, v[0].uv.to_vec2()));
    let constant_rgba = constant_tex
        .filter(|_| v[0].color == v[1].color && v[0].color == v[2].color)
        .map(|tex| std::array::from_fn::<_, 4, _>(|i| tex[i] * v[0].color[i] as f32 / 255.0));
    if constant_rgba == Some([0.0; 4]) {
        return;
    }
    let opaque = constant_rgba.filter(|rgba| rgba[3] == 255.0).map(|rgba| {
        ((rgba[0].round() as u32) << 16) | ((rgba[1].round() as u32) << 8) | rgba[2].round() as u32
    });
    'rows: for y in y0..y1 {
        let py = y as f32 + 0.5;
        if py < clip.min.y || py > clip.max.y {
            continue;
        }
        // Intersect the triangle with this scanline. AA fringes and shadows
        // often form very thin diagonal triangles with enormous bounding boxes.
        // Only visit their covered span, not every pixel in that box.
        let (mut start, mut end) = (x0, x1);
        for (i, (a, b)) in edges.iter().enumerate() {
            let dy = b.y - a.y;
            if dy == 0.0 {
                let e = (b.x - a.x) * (py - a.y);
                if e < 0.0 || (e == 0.0 && !inclusive[i]) {
                    continue 'rows;
                }
            } else {
                let x = a.x + (py - a.y) * (b.x - a.x) / dy - 0.5;
                // Round outwards, then check the endpoints with the original
                // half-open edge test. This preserves shared-edge ownership.
                if dy < 0.0 {
                    start = start.max(x.floor().max(0.0) as u32);
                } else {
                    end = end.min((x.ceil().max(0.0) as u32).saturating_add(1));
                }
            }
        }
        let covered = |x| {
            let p = egui::pos2(x as f32 + 0.5, py);
            clip.contains(p)
                && edges.iter().enumerate().all(|(i, &(a, b))| {
                    let e = edge(a, b, p);
                    e > 0.0 || (e == 0.0 && inclusive[i])
                })
        };
        while start < end && !covered(start) {
            start += 1;
        }
        while start < end && !covered(end - 1) {
            end -= 1;
        }
        if start >= end {
            continue;
        }
        let row = y as usize * size[0] as usize;
        if let Some(color) = opaque {
            dst[row + start as usize..row + end as usize].fill(color);
            continue;
        }
        for x in start..end {
            let pixel = &mut dst[(y * size[0] + x) as usize];
            let rgba = constant_rgba.unwrap_or_else(|| {
                let p = egui::pos2(x as f32 + 0.5, py);
                let e = edges.map(|(a, b)| edge(a, b, p));
                let weights = e.map(|e| e / area);
                let tex = constant_tex.unwrap_or_else(|| {
                    let uv = v[0].uv.to_vec2() * weights[0]
                        + v[1].uv.to_vec2() * weights[1]
                        + v[2].uv.to_vec2() * weights[2];
                    sample(texture, uv)
                });
                std::array::from_fn(|i| {
                    let color: f32 = (0..3).map(|j| v[j].color[i] as f32 * weights[j]).sum();
                    tex[i] * color / 255.0
                })
            });
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

    // Deliberately simple bounding-box rasterizer, retained as a correctness
    // oracle for the scanline and constant-colour optimizations.
    fn reference_triangle(
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

    #[test]
    fn optimized_triangles_match_reference_coverage_and_blending() {
        let texture = Texture {
            size: [2, 2],
            pixels: vec![
                Color32::WHITE,
                Color32::RED,
                Color32::BLUE,
                Color32::TRANSPARENT,
            ],
        };
        let mut seed = 1234u32;
        let mut coordinate = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed % 960) as f32 / 16.0 - 8.0
        };
        for case in 0..600 {
            let mut vertices = std::array::from_fn(|_| epaint::Vertex {
                pos: egui::pos2(coordinate(), coordinate()),
                uv: egui::pos2(0.0, 0.0),
                color: Color32::from_rgb(27, 89, 147),
            });
            match case % 6 {
                0 => {}
                1 => vertices
                    .iter_mut()
                    .for_each(|v| v.color = Color32::from_black_alpha(128)),
                2 => vertices
                    .iter_mut()
                    .for_each(|v| v.uv = egui::pos2(0.5, 0.5)),
                3 => vertices
                    .iter_mut()
                    .for_each(|v| v.color = Color32::TRANSPARENT),
                4 => vertices[1].color = Color32::from_rgba_premultiplied(60, 30, 20, 90),
                _ => {
                    vertices[1].uv = egui::pos2(1.0, 0.0);
                    vertices[2].uv = egui::pos2(0.0, 1.0);
                }
            }
            let clip = egui::Rect::from_min_max(egui::pos2(3.2, 2.6), egui::pos2(59.7, 60.1));
            let scale = [1.0, 1.25, 2.0][case % 3];
            let mut expected = vec![0x739bc1; 64 * 64];
            let mut actual = expected.clone();
            reference_triangle(&mut expected, [64, 64], clip, vertices, scale, &texture);
            triangle(&mut actual, [64, 64], clip, vertices, scale, &texture);
            for (i, (&a, &b)) in actual.iter().zip(&expected).enumerate() {
                // Constant interpolation removes floating-point roundoff. It
                // may change a rounded channel by one, never coverage/opacity.
                for shift in [0, 8, 16] {
                    assert!(
                        ((a >> shift) & 255).abs_diff((b >> shift) & 255) <= 1,
                        "case {case}, pixel {i}: {a:06x} != {b:06x}"
                    );
                }
            }
        }
    }

    /// Repeatable CPU-only measurement at Retina resolution, without a display
    /// server or network connection. Run with --ignored --nocapture.
    #[test]
    #[ignore = "manual rendering benchmark"]
    fn hidpi_controls_benchmark() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        ctx.set_pixels_per_point(2.0);
        let mut renderer = Renderer::default();
        let mut panel = crate::transfer_panel::Panel::default();
        panel.open = true;
        let transfers = [crate::transfer_panel::Transfer {
            id: 1,
            name: "Project presentation.pdf".into(),
            progress: Some((4_500_000, 12_000_000)),
        }];
        let mut pixels = vec![0; 1760 * 1120];
        let mut elapsed = std::time::Duration::ZERO;
        for frame in 0..35 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(880.0, 560.0),
                    )),
                    time: Some(frame as f64 / 60.0),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.heading("Connections");
                        for row in 0..8 {
                            ui.label(format!("Linux workstation {}", row + 1));
                        }
                    });
                    panel.show(ctx, 0, &transfers);
                },
            );
            let start = std::time::Instant::now();
            renderer.paint(&ctx, output, &mut pixels, 1760, 1120);
            std::hint::black_box(&pixels);
            if frame >= 5 {
                elapsed += start.elapsed();
            }
        }
        eprintln!(
            "HiDPI controls: {:.2} ms/frame",
            elapsed.as_secs_f64() * 1000.0 / 30.0
        );
    }
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
