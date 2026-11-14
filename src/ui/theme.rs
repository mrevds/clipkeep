//! Look of the window: a deep green-black background, soft white text and the leaf green of the
//! clipkeep logo (a leaf whose vein is a paper clip) as the accent.

use eframe::egui::{
    self, epaint::Shadow, Color32, CornerRadius, FontId, Margin, Stroke, TextStyle,
};

pub const ACCENT: Color32 = Color32::from_rgb(0x1F, 0x8F, 0x55);
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(0x3D, 0xBE, 0x74);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x17, 0x40, 0x2C);
pub const NIGHT: Color32 = Color32::from_rgb(0x0F, 0x16, 0x13);
pub const RAISED: Color32 = Color32::from_rgb(0x18, 0x21, 0x1D);
pub const RAISED_HI: Color32 = Color32::from_rgb(0x22, 0x30, 0x2A);
pub const BORDER: Color32 = Color32::from_rgb(0x2B, 0x3A, 0x33);
pub const PAPER: Color32 = Color32::from_rgb(0xF1, 0xF7, 0xF2);
pub const MUTED: Color32 = Color32::from_rgb(0x93, 0xA6, 0x9B);
pub const ERR: Color32 = Color32::from_rgb(0xF0, 0x6B, 0x74);

const LOGO_PNG: &[u8] = include_bytes!("../../assets/logo-256.png");

/// The logo as RGBA pixels (for the window icon and the in-window picture).
pub fn logo_image() -> Option<egui::ColorImage> {
    let (w, h, rgba) = crate::store::media::decode_png(LOGO_PNG).ok()?;
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &rgba,
    ))
}

pub fn window_icon() -> Option<egui::IconData> {
    let (w, h, rgba) = crate::store::media::decode_png(LOGO_PNG).ok()?;
    Some(egui::IconData {
        rgba,
        width: w,
        height: h,
    })
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(PAPER);
        v.panel_fill = NIGHT;
        v.window_fill = NIGHT;
        v.extreme_bg_color = Color32::from_rgb(0x0A, 0x10, 0x0D); // text fields
        v.faint_bg_color = RAISED;
        v.hyperlink_color = Color32::from_rgb(0x6F, 0xD6, 0x9A);
        v.selection.bg_fill = ACCENT_SOFT;
        v.selection.stroke = Stroke::new(1.0, PAPER);
        v.window_shadow = Shadow::NONE;
        v.popup_shadow = Shadow::NONE;
        let r = CornerRadius::same(8);
        v.widgets.noninteractive.corner_radius = r;
        v.widgets.inactive.corner_radius = r;
        v.widgets.hovered.corner_radius = r;
        v.widgets.active.corner_radius = r;
        v.widgets.open.corner_radius = r;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, PAPER);
        v.widgets.inactive.bg_fill = RAISED;
        v.widgets.inactive.weak_bg_fill = RAISED;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, PAPER);
        v.widgets.hovered.bg_fill = RAISED_HI;
        v.widgets.hovered.weak_bg_fill = RAISED_HI;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x46, 0x5E, 0x52));
        v.widgets.hovered.fg_stroke = Stroke::new(1.0, PAPER);
        v.widgets.active.bg_fill = ACCENT_SOFT;
        v.widgets.active.weak_bg_fill = ACCENT_SOFT;
        v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
        v.widgets.active.fg_stroke = Stroke::new(1.0, PAPER);

        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.text_edit_width = 280.0;
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(14.5));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(14.5));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(12.0));
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(22.0));
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(14.0));
    });
}

/// Panel frame of the whole window: night background and a hairline border.
pub fn window_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(NIGHT)
        .stroke(Stroke::new(1.0, BORDER))
        .inner_margin(Margin::same(14))
}

/// The main call-to-action button: filled leaf green.
pub fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).color(PAPER).strong())
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .min_size(egui::vec2(0.0, 36.0))
}

pub fn muted(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).color(MUTED)
}
