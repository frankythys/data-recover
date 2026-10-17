//! Design tokens (colours, fonts, spacing) and the global egui style.

use egui::{Color32, FontFamily, FontId, Rounding, Stroke, TextStyle};

// Palette — light theme only.
pub const ACCENT: Color32 = Color32::from_rgb(22, 119, 255);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(232, 241, 255);
pub const APP_BG: Color32 = Color32::from_rgb(245, 247, 250);
pub const SIDEBAR_BG: Color32 = Color32::from_rgb(255, 255, 255);
pub const CARD_BG: Color32 = Color32::from_rgb(255, 255, 255);
pub const CARD_BORDER: Color32 = Color32::from_rgb(229, 232, 238);
pub const SUBTLE_BG: Color32 = Color32::from_rgb(241, 243, 247);
pub const TRACK_BG: Color32 = Color32::from_rgb(232, 236, 242);
pub const TEXT_MAIN: Color32 = Color32::from_rgb(29, 35, 48);
pub const TEXT_DIM: Color32 = Color32::from_rgb(107, 115, 133);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(154, 161, 174);
pub const DANGER: Color32 = Color32::from_rgb(229, 72, 77);
pub const SUCCESS: Color32 = Color32::from_rgb(22, 163, 74);

// Per file-category accent colours.
pub const CAT_PHOTO: Color32 = Color32::from_rgb(245, 158, 11);
pub const CAT_VIDEO: Color32 = Color32::from_rgb(139, 92, 246);
pub const CAT_AUDIO: Color32 = Color32::from_rgb(236, 72, 153);
pub const CAT_DOC: Color32 = Color32::from_rgb(22, 119, 255);
pub const CAT_ARCHIVE: Color32 = Color32::from_rgb(20, 184, 166);
pub const CAT_OTHER: Color32 = Color32::from_rgb(107, 115, 133);

pub const RADIUS: f32 = 10.0;

/// Font family for medium-weight text (buttons, card titles).
pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

/// Font family that contains only the Phosphor icons. Icons must use this
/// family explicitly: Inter has its own glyphs in the same private-use
/// code points, so a plain fallback would show the wrong symbols.
pub fn icons() -> FontFamily {
    FontFamily::Name("icons".into())
}

pub fn icon(size: f32) -> FontId {
    FontId::new(size, icons())
}

/// Font family for semibold text (headings).
pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn font_medium(size: f32) -> FontId {
    FontId::new(size, medium())
}

pub fn font_semibold(size: f32) -> FontId {
    FontId::new(size, semibold())
}

/// A colour blended towards white, for tinted backgrounds.
pub fn tint(color: Color32, amount: f32) -> Color32 {
    let mix = |c: u8| (255.0 - (255.0 - c as f32) * amount).round() as u8;
    Color32::from_rgb(mix(color.r()), mix(color.g()), mix(color.b()))
}

/// Subtle drop shadow used by cards.
pub fn card_shadow() -> egui::epaint::Shadow {
    egui::epaint::Shadow {
        offset: egui::vec2(0.0, 1.0),
        blur: 6.0,
        spread: 0.0,
        color: Color32::from_black_alpha(14),
    }
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let add = |fonts: &mut egui::FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), egui::FontData::from_static(bytes));
    };
    add(&mut fonts, "Inter-Regular", include_bytes!("../assets/fonts/Inter-Regular.ttf"));
    add(&mut fonts, "Inter-Medium", include_bytes!("../assets/fonts/Inter-Medium.ttf"));
    add(&mut fonts, "Inter-SemiBold", include_bytes!("../assets/fonts/Inter-SemiBold.ttf"));
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    // Text fallbacks: egui's bundled fonts (Hack covers ← ▸ ▾, the emoji
    // fonts cover the rest). Phosphor is deliberately not in here.
    let fallbacks = ["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font", "Hack"];
    let family = |primary: &str| -> Vec<String> {
        std::iter::once(primary.to_owned())
            .chain(fallbacks.iter().map(|f| (*f).to_owned()))
            .collect()
    };
    fonts.families.insert(FontFamily::Proportional, family("Inter-Regular"));
    fonts.families.insert(medium(), family("Inter-Medium"));
    fonts.families.insert(semibold(), family("Inter-SemiBold"));
    fonts.families.insert(icons(), vec!["phosphor".to_owned()]);
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);

    // The app is designed for light mode only. Pin it, so a dark Windows
    // theme doesn't swap in egui's default dark style.
    ctx.set_theme(egui::Theme::Light);

    let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
    let mut visuals = egui::Visuals::light();

    visuals.override_text_color = Some(TEXT_MAIN);
    visuals.panel_fill = APP_BG;
    visuals.window_fill = CARD_BG;
    visuals.extreme_bg_color = CARD_BG;
    visuals.faint_bg_color = SUBTLE_BG;
    visuals.hyperlink_color = ACCENT;

    let r = Rounding::same(8.0);
    visuals.widgets.noninteractive.bg_fill = APP_BG;
    visuals.widgets.noninteractive.weak_bg_fill = APP_BG;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    visuals.widgets.noninteractive.rounding = r;

    visuals.widgets.inactive.bg_fill = CARD_BG;
    visuals.widgets.inactive.weak_bg_fill = CARD_BG;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT_MAIN);
    visuals.widgets.inactive.rounding = r;

    visuals.widgets.hovered.bg_fill = SUBTLE_BG;
    visuals.widgets.hovered.weak_bg_fill = SUBTLE_BG;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(208, 213, 221));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT_MAIN);
    visuals.widgets.hovered.rounding = r;
    visuals.widgets.hovered.expansion = 0.0;

    // `active` also sets the colour of `strong()` text, so it must stay dark.
    visuals.widgets.active.bg_fill = TRACK_BG;
    visuals.widgets.active.weak_bg_fill = TRACK_BG;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT_MAIN);
    visuals.widgets.active.rounding = r;
    visuals.widgets.active.expansion = 0.0;

    visuals.selection.bg_fill = ACCENT_SOFT;
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.window_rounding = Rounding::same(12.0);
    visuals.menu_rounding = Rounding::same(8.0);
    visuals.popup_shadow = card_shadow();

    style.visuals = visuals;
    style.text_styles = [
        (TextStyle::Heading, font_semibold(20.0)),
        (TextStyle::Body, font(14.0)),
        (TextStyle::Button, font_medium(14.0)),
        (TextStyle::Small, font(12.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 7.0);
    style.spacing.interact_size.y = 34.0;

    ctx.set_style_of(egui::Theme::Light, style);
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0_f32, CARD_BORDER))
        .rounding(Rounding::same(RADIUS))
        .shadow(card_shadow())
        .inner_margin(egui::Margin::same(16.0))
}
