//! Debug helper: `CLIPKEEP_SCREENSHOT=out.ppm clipkeep show` saves one frame and exits.

use super::*;

impl Popup {
    pub(super) fn debug_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot_to.clone() else {
            return;
        };
        ctx.request_repaint();
        if !self.loaded {
            return;
        }
        self.frames += 1;
        if self.frames == 3 {
            let c = ctx.clone();
            let callback = egui::ScreenshotCallback::new(move |img| {
                if let Err(e) = save_ppm(&path, &img) {
                    eprintln!("clipkeep: screenshot failed: {e}");
                }
                c.send_viewport_cmd(egui::ViewportCommand::Close);
                c.request_repaint();
            });
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(callback));
        }
    }
}

fn save_ppm(path: &std::path::Path, img: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = img.size;
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in &img.pixels {
        out.extend_from_slice(&[px.r(), px.g(), px.b()]);
    }
    std::fs::write(path, out)
}
