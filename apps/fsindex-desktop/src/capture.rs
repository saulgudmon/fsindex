//! Optional native smoke-test capture. Excluded from normal builds.
use eframe::egui;

pub fn poll(ctx: &egui::Context, ready: bool) {
    let Ok(path) = std::env::var("FSINDEX_CAPTURE_TO") else {
        return;
    };
    let screenshot = ctx.input(|i| {
        i.events.iter().find_map(|event| {
            if let egui::Event::Screenshot { image, .. } = event {
                Some(image.clone())
            } else {
                None
            }
        })
    });
    if let Some(image) = screenshot {
        let bytes: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|p| p.to_srgba_unmultiplied())
            .collect();
        image::save_buffer(
            &path,
            &bytes,
            image.width() as u32,
            image.height() as u32,
            image::ColorType::Rgba8,
        )
        .expect("save smoke-test screenshot");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    } else if ready && ctx.cumulative_frame_nr() >= 3 {
        let id = egui::Id::new("native-capture-requested");
        let already = ctx.data_mut(|d| {
            let already = d.get_temp::<bool>(id).unwrap_or(false);
            d.insert_temp(id, true);
            already
        });
        if !already {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
    }
}
