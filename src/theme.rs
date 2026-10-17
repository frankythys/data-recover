use egui::{Color32, Rounding, Stroke};

pub const ACCENT: Color32 = Color32::from_rgb(28, 126, 214);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(232, 242, 255);
pub const SIDEBAR_BG: Color32 = Color32::from_rgb(255, 255, 255);
pub const APP_BG: Color32 = Color32::from_rgb(240, 242, 247);
pub const CARD_BG: Color32 = Color32::from_rgb(255, 255, 255);
pub const CARD_BORDER: Color32 = Color32::from_rgb(224, 227, 234);
pub const TRACK_BG: Color32 = Color32::from_rgb(226, 230, 238);
pub const FOLDER_COLOR: Color32 = Color32::from_rgb(245, 166, 35);
pub const TEXT_MAIN: Color32 = Color32::from_rgb(31, 36, 48);
pub const TEXT_DIM: Color32 = Color32::from_rgb(120, 126, 140);
pub const DANGER: Color32 = Color32::from_rgb(214, 61, 61);
pub const SUCCESS: Color32 = Color32::from_rgb(40, 160, 110);

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::light();

    visuals.override_text_color = Some(TEXT_MAIN);
    visuals.panel_fill = APP_BG;
    visuals.window_fill = CARD_BG;
    visuals.extreme_bg_color = APP_BG;
    visuals.faint_bg_color = ACCENT_SOFT;

    visuals.widgets.noninteractive.bg_fill = APP_BG;
    visuals.widgets.noninteractive.weak_bg_fill = APP_BG;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);

    visuals.widgets.inactive.bg_fill = CARD_BG;
    visuals.widgets.inactive.weak_bg_fill = CARD_BG;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    visuals.widgets.inactive.rounding = Rounding::same(10.0);

    visuals.widgets.hovered.bg_fill = ACCENT_SOFT;
    visuals.widgets.hovered.weak_bg_fill = ACCENT_SOFT;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.hovered.rounding = Rounding::same(10.0);

    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.active.weak_bg_fill = ACCENT;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.active.rounding = Rounding::same(10.0);

    visuals.selection.bg_fill = ACCENT_SOFT;
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    visuals.window_rounding = Rounding::same(12.0);

    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);

    ctx.set_style(style);
}

pub fn accent_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).color(Color32::WHITE).strong())
        .fill(ACCENT)
        .rounding(Rounding::same(8.0))
}

pub fn outline_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).color(TEXT_MAIN))
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0_f32, CARD_BORDER))
        .rounding(Rounding::same(8.0))
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0_f32, CARD_BORDER))
        .rounding(Rounding::same(10.0))
        .inner_margin(egui::Margin::same(16.0))
}
