use eframe::egui::{self, Color32, FontId, Stroke, TextStyle};

pub const BACKGROUND: Color32 = Color32::from_rgb(24, 27, 29);
pub const PANEL: Color32 = Color32::from_rgb(39, 43, 47);
pub const TEXT: Color32 = Color32::from_rgb(231, 234, 237);
pub const MUTED: Color32 = Color32::from_rgb(159, 166, 173);
pub const BORDER: Color32 = Color32::from_rgb(61, 66, 71);
pub const ACCENT: Color32 = Color32::from_rgb(80, 173, 221);
pub const SELECTED: Color32 = Color32::from_rgb(37, 76, 98);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.global_style()).clone();
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(12.0));
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.scroll = egui::style::ScrollStyle::solid();
    style.spacing.scroll.bar_width = 9.0;
    style.spacing.scroll.bar_inner_margin = 2.0;
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.panel_fill = PANEL;
    style.visuals.window_fill = PANEL;
    style.visuals.extreme_bg_color = BACKGROUND;
    style.visuals.faint_bg_color = Color32::from_rgb(29, 33, 36);
    style.visuals.selection.bg_fill = SELECTED;
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(48, 53, 58);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(48, 53, 58);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(58, 66, 72);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(58, 66, 72);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    ctx.set_global_style(style);
}
