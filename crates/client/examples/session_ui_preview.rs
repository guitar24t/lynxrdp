//! Render the real session controls without needing a remote server.
//! cargo run -p lynxrdp-client --example session_ui_preview -- <output.png> [details]
use eframe::egui;
use lynxrdp_client::{gui_paint, overlay, transfer_panel};
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "session-ui.png".into());
    let details = args.next().as_deref() == Some("details");
    let (w, h) = (1280u32, 800u32);
    let mut pixels = vec![0x263c50; (w * h) as usize];
    gui_paint::paint(&mut pixels, w, h, |p| {
        p.rect_filled(
            egui::Rect::from_min_max(egui::pos2(80.0, 110.0), egui::pos2(1060.0, 690.0)),
            12,
            egui::Color32::from_rgb(245, 247, 250),
        );
        p.text(
            egui::pos2(116.0, 145.0),
            egui::Align2::LEFT_TOP,
            "Remote desktop",
            egui::FontId::proportional(24.0),
            egui::Color32::from_rgb(30, 50, 65),
        );
        p.text(
            egui::pos2(116.0, 200.0),
            egui::Align2::LEFT_TOP,
            "Keep working while your files copy.",
            egui::FontId::proportional(18.0),
            egui::Color32::from_gray(85),
        );
    });
    overlay::paint(
        &mut pixels,
        w,
        h,
        &overlay::bar_layout(
            w,
            2,
            &overlay::Status::new(
                "alex@workstation",
                (1920, 1080),
                Some(std::time::Duration::from_millis(18)),
            ),
        ),
        Some(0),
        None,
    );
    let ctx = egui::Context::default();
    lynxrdp_client::theme::apply(&ctx);
    ctx.set_theme(egui::Theme::Dark);
    let mut panel = transfer_panel::Panel::default();
    panel.open = details;
    let items = vec![
        transfer_panel::Transfer {
            id: 1,
            name: "Project presentation.pdf".into(),
            progress: Some((4_500_000, 12_000_000)),
        },
        transfer_panel::Transfer {
            id: 2,
            name: "Photos.zip".into(),
            progress: Some((18_000_000, 60_000_000)),
        },
    ];
    let mut renderer = gui_paint::Renderer::default();
    let background = pixels.clone();
    // Egui measures windows on their first frame and positions them on the next.
    for frame in 0..3 {
        pixels.copy_from_slice(&background);
        let output = ctx.run(
            egui::RawInput {
                time: Some(frame as f64 * 0.2),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(w as f32, h as f32),
                )),
                ..Default::default()
            },
            |ctx| {
                panel.show(ctx, 0, &items);
            },
        );
        renderer.paint(&ctx, output, &mut pixels, w, h);
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, w, h);
    encoder.set_color(png::ColorType::Rgb);
    let mut writer = encoder.write_header()?;
    let rgb: Vec<u8> = pixels
        .into_iter()
        .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, p as u8])
        .collect();
    writer.write_image_data(&rgb)?;
    Ok(())
}
