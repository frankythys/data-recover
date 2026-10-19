//! Reusable, painted widgets that make up the app's look. Everything is drawn
//! at the space it is given, so nothing overflows when panels get narrow.

use crate::disk::{self, DriveKind, VolumeInfo};
use crate::i18n::{english, t};
use crate::tf;
use crate::ntfs::{self, RecoveredFile};
use crate::theme;
use egui::{Color32, FontId, Rect, Response, Sense, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as icon;
use std::sync::Arc;

/// Width reserved for a tree row's disclosure caret (also the indent step).
pub const ARROW_W: f32 = 18.0;

const MONTHS_SHORT: [&str; 12] = [
    "jan", "feb", "mrt", "apr", "mei", "jun", "jul", "aug", "sep", "okt", "nov", "dec",
];
const MONTHS_SHORT_EN: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Icon and accent colour for a file category.
pub fn category_style(category: &str) -> (&'static str, Color32) {
    match category {
        "Fotos" => (icon::IMAGE, theme::CAT_PHOTO),
        "Videos" => (icon::FILM_STRIP, theme::CAT_VIDEO),
        "Audio" => (icon::MUSIC_NOTES, theme::CAT_AUDIO),
        "Documenten" => (icon::FILE_TEXT, theme::CAT_DOC),
        "Archieven" => (icon::FILE_ZIP, theme::CAT_ARCHIVE),
        _ => (icon::FILE, theme::CAT_OTHER),
    }
}

/// 185856 -> "185.856" (English: "185,856").
pub fn fmt_thousands(n: u64) -> String {
    let separator = if english() { ',' } else { '.' };
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(separator);
        }
        out.push(c);
    }
    out
}

/// "1 bestand" / "1.234 bestanden".
pub fn fmt_files(n: usize) -> String {
    if n == 1 {
        t("1 bestand", "1 file").to_string()
    } else {
        let n = fmt_thousands(n as u64);
        tf!("{n} bestanden", "{n} files")
    }
}

pub fn fmt_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{} min {:02} s", secs / 60, secs % 60)
    }
}

/// "dec 2022", or "" when the date is unknown.
pub fn fmt_month(year: u16, month: u8) -> String {
    let months = if english() { MONTHS_SHORT_EN } else { MONTHS_SHORT };
    match (year, months.get(month.wrapping_sub(1) as usize)) {
        (0, _) | (_, None) => String::new(),
        (y, Some(m)) => format!("{m} {y}"),
    }
}

/// Lays out `text` on one line, elided with "…" to `max_w`. Returns the galley
/// and whether it had to be shortened.
pub fn elided(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> (Arc<egui::Galley>, bool) {
    let painter = ui.painter();
    let full = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    if full.size().x <= max_w {
        return (full, false);
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(8.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    (painter.layout_job(job), true)
}

/// Card background: soft shadow, white fill and a border that reacts to
/// hover/selection.
pub fn paint_card(ui: &Ui, rect: Rect, hovered: bool, selected: bool) {
    let p = ui.painter();
    p.add(theme::card_shadow().as_shape(rect, theme::RADIUS));
    p.rect_filled(rect, theme::RADIUS, theme::CARD_BG);
    let stroke = if selected {
        Stroke::new(1.5_f32, theme::ACCENT)
    } else if hovered {
        Stroke::new(1.0_f32, Color32::from_rgb(196, 203, 214))
    } else {
        Stroke::new(1.0_f32, theme::CARD_BORDER)
    };
    p.rect_stroke(rect, theme::RADIUS, stroke);
}

/// Rounded square with an icon, used as the leading visual of cards.
pub fn icon_tile(ui: &Ui, rect: Rect, glyph: &str, color: Color32, size: f32) {
    let p = ui.painter();
    p.rect_filled(rect, 10.0, theme::tint(color, 0.12));
    p.text(rect.center(), egui::Align2::CENTER_CENTER, glyph, theme::icon(size), color);
}

/// Small rounded label; returns its right edge.
pub fn badge(ui: &Ui, pos: egui::Pos2, text: &str, fg: Color32, bg: Color32) -> f32 {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::font_medium(11.0), fg);
    let rect = Rect::from_min_size(pos, vec2(galley.size().x + 14.0, 20.0));
    ui.painter().rect_filled(rect, 6.0, bg);
    ui.painter().galley(
        pos2(rect.left() + 7.0, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );
    rect.right()
}

/// Horizontal progress bar painted inside `rect`.
pub fn progress_bar(ui: &Ui, rect: Rect, fraction: f32, color: Color32) {
    let p = ui.painter();
    let r = rect.height() / 2.0;
    p.rect_filled(rect, r, theme::TRACK_BG);
    let f = fraction.clamp(0.0, 1.0);
    if f > 0.0 {
        let fill = Rect::from_min_size(rect.min, vec2((rect.width() * f).max(rect.height()), rect.height()));
        p.rect_filled(fill, r, color);
    }
}

/// Circular progress indicator with the percentage (or a check when done).
pub fn progress_ring(ui: &mut Ui, size: f32, fraction: f32, done: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    let radius = size / 2.0 - 4.0;
    let width: f32 = 5.0;
    let color = if done { theme::SUCCESS } else { theme::ACCENT };
    p.circle_stroke(c, radius, Stroke::new(width, theme::TRACK_BG));
    let f = if done { 1.0 } else { fraction.clamp(0.0, 1.0) };
    if f > 0.0 {
        let steps = (64.0 * f).ceil().max(2.0) as usize;
        let start = -std::f32::consts::FRAC_PI_2;
        let points: Vec<egui::Pos2> = (0..=steps)
            .map(|i| {
                let a = start + std::f32::consts::TAU * f * i as f32 / steps as f32;
                c + vec2(a.cos(), a.sin()) * radius
            })
            .collect();
        p.add(egui::Shape::line(points, Stroke::new(width, color)));
    }
    if done {
        p.text(c, egui::Align2::CENTER_CENTER, icon::CHECK, theme::icon(size * 0.42), color);
    } else {
        p.text(
            c,
            egui::Align2::CENTER_CENTER,
            format!("{:.0}%", f * 100.0),
            theme::font_semibold(size * 0.24),
            theme::TEXT_MAIN,
        );
    }
}

/// An icon followed by text on one line (icon from the icon family).
pub fn icon_text_job(glyph: &str, text: &str, size: f32, color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let fmt = |font_id: FontId| egui::TextFormat {
        font_id,
        color,
        valign: egui::Align::Center,
        ..Default::default()
    };
    job.append(glyph, 0.0, fmt(theme::icon(size + 2.0)));
    job.append(text, 7.0, fmt(theme::font_medium(size)));
    job
}

/// Filled primary button with a leading icon.
pub fn primary_icon_button(glyph: &str, text: &str) -> egui::Button<'static> {
    egui::Button::new(icon_text_job(glyph, text, 14.0, Color32::WHITE))
        .fill(theme::ACCENT)
        .stroke(Stroke::NONE)
        .rounding(egui::Rounding::same(8.0))
}

/// Filled red button with a leading icon, for removing things.
pub fn danger_icon_button(glyph: &str, text: &str) -> egui::Button<'static> {
    egui::Button::new(icon_text_job(glyph, text, 14.0, Color32::WHITE))
        .fill(theme::DANGER)
        .stroke(Stroke::NONE)
        .rounding(egui::Rounding::same(8.0))
}

/// White outlined button with a leading icon.
pub fn secondary_icon_button(glyph: &str, text: &str) -> egui::Button<'static> {
    egui::Button::new(icon_text_job(glyph, text, 14.0, theme::TEXT_MAIN))
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0_f32, theme::CARD_BORDER))
        .rounding(egui::Rounding::same(8.0))
}

/// Square icon-only button with a tooltip.
pub fn icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> Response {
    ui.add(
        egui::Button::new(egui::RichText::new(glyph).font(theme::icon(17.0)).color(theme::TEXT_MAIN))
            .fill(theme::CARD_BG)
            .stroke(Stroke::new(1.0_f32, theme::CARD_BORDER))
            .rounding(egui::Rounding::same(8.0))
            .min_size(vec2(34.0, 34.0)),
    )
    .on_hover_text(tooltip)
}

/// Painted checkbox square.
pub fn paint_checkbox(ui: &Ui, rect: Rect, checked: bool) {
    let p = ui.painter();
    if checked {
        p.rect_filled(rect, 5.0, theme::ACCENT);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, icon::CHECK, theme::icon(13.0), Color32::WHITE);
    } else {
        p.rect_filled(rect, 5.0, Color32::from_white_alpha(235));
        p.rect_stroke(rect, 5.0, Stroke::new(1.5_f32, Color32::from_rgb(190, 197, 209)));
    }
}

/// Clickable checkbox with a label (for "Alles selecteren").
pub fn checkbox_label(ui: &mut Ui, checked: bool, text: &str) -> bool {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::font_medium(13.5), theme::TEXT_MAIN);
    let (rect, resp) = ui.allocate_exact_size(vec2(26.0 + galley.size().x, 34.0), Sense::click());
    let box_rect = Rect::from_center_size(pos2(rect.left() + 9.0, rect.center().y), vec2(18.0, 18.0));
    paint_checkbox(ui, box_rect, checked);
    ui.painter().galley(
        pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
        galley,
        theme::TEXT_MAIN,
    );
    resp.clicked()
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

/// App logo, name and tagline at the top of the sidebar.
pub fn logo(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 76.0), Sense::hover());
    let tile = Rect::from_min_size(rect.min + vec2(18.0, 20.0), vec2(36.0, 36.0));
    match logo_texture(ui.ctx()) {
        Some(texture) => {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            ui.painter().image(texture.id(), tile, uv, Color32::WHITE);
        }
        None => {
            ui.painter().rect_filled(tile, 10.0, theme::ACCENT);
            ui.painter().text(tile.center(), egui::Align2::CENTER_CENTER, icon::HARD_DRIVE, theme::icon(21.0), Color32::WHITE);
        }
    }
    let x = tile.right() + 12.0;
    let max_w = rect.right() - x - 12.0;
    let (title, _) = elided(ui, "DiskTools", theme::font_semibold(16.0), theme::TEXT_MAIN, max_w);
    let (sub, _) = elided(ui, t("Herstel en schijfbeheer", "Recovery and disk management"), theme::font(12.0), theme::TEXT_DIM, max_w);
    let p = ui.painter();
    p.galley(pos2(x, tile.top() - 1.0), title, theme::TEXT_MAIN);
    p.galley(pos2(x, tile.top() + 20.0), sub, theme::TEXT_DIM);
}

/// The app icon as a texture, decoded once.
fn logo_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let id = egui::Id::new("app_logo");
    if let Some(texture) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(texture);
    }
    let img = image::load_from_memory_with_format(crate::ICON_PNG, image::ImageFormat::Png).ok()?.into_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    // Mipmaps keep the 256 px icon sharp when drawn small.
    let options = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
    let texture = ctx.load_texture("app_logo", egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()), options);
    ctx.data_mut(|d| d.insert_temp(id, texture.clone()));
    Some(texture)
}

/// Small uppercase caption above a group of sidebar items.
pub fn sidebar_caption(ui: &mut Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    ui.painter().text(
        pos2(rect.left() + 18.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text.to_uppercase(),
        theme::font_semibold(11.0),
        theme::TEXT_FAINT,
    );
}

/// Full-width navigation item with an icon.
pub fn nav_item(ui: &mut Ui, glyph: &str, label: &str, active: bool) -> Response {
    let (outer, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    let rect = outer.shrink2(vec2(10.0, 2.0));
    let p = ui.painter();
    let fg = if active {
        p.rect_filled(rect, 8.0, theme::ACCENT_SOFT);
        theme::ACCENT
    } else {
        if resp.hovered() {
            p.rect_filled(rect, 8.0, theme::SUBTLE_BG);
        }
        theme::TEXT_DIM
    };
    p.text(pos2(rect.left() + 14.0, rect.center().y), egui::Align2::LEFT_CENTER, glyph, theme::icon(18.0), fg);
    let max_w = rect.right() - rect.left() - 52.0;
    let (g, _) = elided(ui, label, theme::font_medium(14.0), fg, max_w);
    ui.painter().galley(pos2(rect.left() + 42.0, rect.center().y - g.size().y / 2.0), g, fg);
    resp
}

/// Pill toggle with one segment per item that fills the available width. Each segment is
/// (icon, label, tooltip) on a single line with a fixed height; when space
/// runs out the icon is dropped first, then the label is elided.
pub fn segmented_tabs<const N: usize>(ui: &mut Ui, items: [(&str, &str, &str); N], active: usize) -> Option<usize> {
    let width = ui.available_width();
    let half = width / N as f32;
    let height = 36.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    ui.painter().rect_filled(rect, 9.0, theme::SUBTLE_BG);

    let mut clicked = None;
    for (i, (glyph, label, tooltip)) in items.iter().enumerate() {
        let seg = Rect::from_min_size(pos2(rect.left() + i as f32 * half, rect.top()), vec2(half, height)).shrink(3.0);
        let resp = ui.interact(seg, ui.id().with(("segment", i)), Sense::click());
        let on = i == active;
        if on {
            ui.painter().add(theme::card_shadow().as_shape(seg, 7.0));
            ui.painter().rect_filled(seg, 7.0, theme::CARD_BG);
        }
        let color = if on {
            theme::ACCENT
        } else if resp.hovered() {
            theme::TEXT_MAIN
        } else {
            theme::TEXT_DIM
        };
        let room = seg.width() - 12.0;
        let with_icon = ui.painter().layout_job(icon_text_job(glyph, label, 13.0, color));
        let galley = if with_icon.size().x <= room {
            with_icon
        } else {
            elided(ui, label, theme::font_medium(13.0), color, room).0
        };
        ui.painter().galley(
            pos2(seg.center().x - galley.size().x / 2.0, seg.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(*tooltip);
        if resp.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// One tree row: optional disclosure caret (`arrow` = Some(expanded)),
/// optional icon, a label elided to fit and a right-aligned count.
/// Returns (row clicked, caret clicked).
pub fn tree_row(
    ui: &mut Ui,
    indent: f32,
    arrow: Option<bool>,
    glyph: Option<&str>,
    label: &str,
    count: usize,
    selected: bool,
) -> (bool, bool) {
    const ROW_H: f32 = 30.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let bg = rect.shrink2(vec2(0.0, 1.0));
    if selected {
        ui.painter().rect_filled(bg, 7.0, theme::ACCENT_SOFT);
    } else if resp.hovered() {
        ui.painter().rect_filled(bg, 7.0, theme::SUBTLE_BG);
    }

    let cy = rect.center().y;
    let mut x = rect.left() + 6.0 + indent;
    let mut arrow_clicked = false;
    if let Some(expanded) = arrow {
        let caret = if expanded { icon::CARET_DOWN } else { icon::CARET_RIGHT };
        ui.painter().text(pos2(x + ARROW_W / 2.0, cy), egui::Align2::CENTER_CENTER, caret, theme::icon(13.0), theme::TEXT_DIM);
        if resp.clicked() {
            arrow_clicked = resp.interact_pointer_pos().is_some_and(|p| p.x <= x + ARROW_W + 2.0);
        }
        x += ARROW_W;
    }

    let fg = if selected { theme::ACCENT } else { theme::TEXT_MAIN };
    if let Some(g) = glyph {
        let icon_color = if selected { theme::ACCENT } else { theme::TEXT_DIM };
        ui.painter().text(pos2(x + 1.0, cy), egui::Align2::LEFT_CENTER, g, theme::icon(15.0), icon_color);
        x += 22.0;
    }

    let count_color = if selected { theme::ACCENT } else { theme::TEXT_FAINT };
    let count_galley = ui.painter().layout_no_wrap(fmt_thousands(count as u64), theme::font(12.0), count_color);
    let count_x = rect.right() - 10.0 - count_galley.size().x;
    ui.painter().galley(pos2(count_x, cy - count_galley.size().y / 2.0), count_galley, count_color);

    let font = if selected { theme::font_medium(13.5) } else { theme::font(13.5) };
    let (galley, was_elided) = elided(ui, label, font, fg, count_x - 8.0 - x);
    ui.painter().galley(pos2(x, cy - galley.size().y / 2.0), galley, fg);

    let row_clicked = resp.clicked() && !arrow_clicked;
    if was_elided {
        resp.on_hover_text(label);
    }
    (row_clicked, arrow_clicked)
}

// ---------------------------------------------------------------------------
// Home screen
// ---------------------------------------------------------------------------

/// Section heading with an icon and an optional count.
pub fn section_header(ui: &mut Ui, glyph: &str, title: &str, count: Option<usize>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(egui::RichText::new(glyph).font(theme::icon(16.0)).color(theme::TEXT_DIM));
        ui.label(egui::RichText::new(title).font(theme::font_semibold(14.0)).color(theme::TEXT_MAIN));
        if let Some(n) = count {
            ui.label(egui::RichText::new(n.to_string()).font(theme::font_medium(12.0)).color(theme::TEXT_FAINT));
        }
    });
}

/// Placeholder card for an empty section.
pub fn empty_card(ui: &mut Ui, glyph: &str, text: &str) {
    theme::card_frame().inner_margin(egui::Margin::symmetric(16.0, 12.0)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(glyph).font(theme::icon(16.0)).color(theme::TEXT_FAINT));
            ui.label(egui::RichText::new(text).color(theme::TEXT_DIM));
        });
    });
}

/// Drive card with badges, usage bar and a "Scannen" action on hover.
/// Returns whether it was clicked.
pub fn volume_card(ui: &mut Ui, width: f32, vol: &VolumeInfo) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 132.0), Sense::click());
    let hovered = resp.hovered();
    paint_card(ui, rect, hovered, false);

    let glyph = if vol.kind == DriveKind::External { icon::USB } else { icon::HARD_DRIVE };
    let tile = Rect::from_min_size(rect.min + vec2(16.0, 16.0), vec2(44.0, 44.0));
    icon_tile(ui, tile, glyph, theme::ACCENT, 24.0);

    let x = tile.right() + 14.0;
    let (name, _) = elided(ui, &vol.display_name(), theme::font_semibold(15.0), theme::TEXT_MAIN, rect.right() - x - 16.0);
    ui.painter().galley(pos2(x, tile.top() + 1.0), name, theme::TEXT_MAIN);

    let mut bx = x;
    if !vol.filesystem.is_empty() {
        bx = badge(ui, pos2(bx, tile.top() + 25.0), &vol.filesystem, theme::TEXT_DIM, theme::SUBTLE_BG) + 6.0;
    }
    if let Some(idx) = vol.disk_index {
        badge(ui, pos2(bx, tile.top() + 25.0), &tf!("Schijf {idx}", "Disk {idx}"), theme::TEXT_DIM, theme::SUBTLE_BG);
    }

    let used = if vol.total_bytes > 0 { vol.used_bytes as f32 / vol.total_bytes as f32 } else { 0.0 };
    let bar_color = if used > 0.9 { theme::DANGER } else { theme::ACCENT };
    let bar = Rect::from_min_max(pos2(rect.left() + 16.0, rect.bottom() - 42.0), pos2(rect.right() - 16.0, rect.bottom() - 36.0));
    progress_bar(ui, bar, used, bar_color);
    let (used_size, total_size) = (disk::format_size(vol.used_bytes), disk::format_size(vol.total_bytes));
    let usage = tf!("{used_size} gebruikt van {total_size}", "{used_size} used of {total_size}");
    ui.painter().text(pos2(bar.left(), rect.bottom() - 18.0), egui::Align2::LEFT_CENTER, usage, theme::font(12.0), theme::TEXT_DIM);

    if hovered {
        let g = ui.painter().layout_job(icon_text_job(icon::MAGNIFYING_GLASS, t("Scannen", "Scan"), 13.0, Color32::WHITE));
        let pill = Rect::from_min_size(pos2(rect.right() - g.size().x - 34.0, rect.top() + 16.0), vec2(g.size().x + 20.0, 30.0));
        ui.painter().rect_filled(pill, 8.0, theme::ACCENT);
        ui.painter().galley(pos2(pill.left() + 10.0, pill.center().y - g.size().y / 2.0), g, Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Quick-access tile. Disabled tiles are dimmed with a "Binnenkort" badge and
/// never report a click.
pub fn quick_tile(ui: &mut Ui, width: f32, glyph: &str, title: &str, subtitle: &str, enabled: bool) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 76.0), sense);
    let hovered = enabled && resp.hovered();
    paint_card(ui, rect, hovered, false);
    let color = if enabled { theme::ACCENT } else { theme::TEXT_FAINT };
    let tile = Rect::from_min_size(rect.min + vec2(16.0, 18.0), vec2(40.0, 40.0));
    icon_tile(ui, tile, glyph, color, 21.0);
    let x = tile.right() + 14.0;
    let title_color = if enabled { theme::TEXT_MAIN } else { theme::TEXT_DIM };
    let mut right = rect.right() - 14.0;
    if !enabled {
        let g = ui.painter().layout_no_wrap(t("Binnenkort", "Coming soon").to_owned(), theme::font_medium(10.5), theme::TEXT_DIM);
        let w = g.size().x + 12.0;
        badge(ui, pos2(rect.right() - 14.0 - w, rect.top() + 12.0), t("Binnenkort", "Coming soon"), theme::TEXT_DIM, theme::SUBTLE_BG);
        right -= w + 6.0;
    }
    let (t, _) = elided(ui, title, theme::font_medium(14.5), title_color, right - x);
    let (s, _) = elided(ui, subtitle, theme::font(12.0), theme::TEXT_DIM, rect.right() - 14.0 - x);
    ui.painter().galley(pos2(x, rect.top() + 19.0), t, title_color);
    ui.painter().galley(pos2(x, rect.top() + 40.0), s, theme::TEXT_DIM);
    let resp = if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp.on_hover_text(crate::i18n::t("Komt in een volgende versie", "Coming in a later version"))
    };
    enabled && resp.clicked()
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Responsive grid: how many columns of at least `min_w` fit in `avail`,
/// and the card width that makes them fill the row exactly.
pub fn grid_columns(avail: f32, min_w: f32, gap: f32) -> (usize, f32) {
    let cols = (((avail + gap) / (min_w + gap)).floor() as usize).max(1);
    let width = ((avail - gap * (cols as f32 - 1.0)) / cols as f32).floor() - 0.5;
    (cols, width.max(min_w.min(avail)))
}

/// Category card ("Fotos — 171 bestanden · 785 MB"). Returns whether clicked.
pub fn category_card(ui: &mut Ui, width: f32, name: &str, count: usize, size: u64) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 84.0), Sense::click());
    paint_card(ui, rect, resp.hovered(), false);
    let (glyph, color) = category_style(name);
    let tile = Rect::from_min_size(rect.min + vec2(16.0, 20.0), vec2(44.0, 44.0));
    icon_tile(ui, tile, glyph, color, 23.0);
    let x = tile.right() + 14.0;
    let max_w = rect.right() - x - 34.0;
    let (t, _) = elided(ui, ntfs::category_label(name), theme::font_semibold(15.0), theme::TEXT_MAIN, max_w);
    let meta = format!("{} · {}", fmt_files(count), disk::format_size(size));
    let (m, _) = elided(ui, &meta, theme::font(12.0), theme::TEXT_DIM, max_w);
    ui.painter().galley(pos2(x, rect.top() + 22.0), t, theme::TEXT_MAIN);
    ui.painter().galley(pos2(x, rect.top() + 45.0), m, theme::TEXT_DIM);
    ui.painter().text(
        pos2(rect.right() - 18.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        icon::CARET_RIGHT,
        theme::icon(16.0),
        if resp.hovered() { theme::ACCENT } else { theme::TEXT_FAINT },
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// File card with a thumbnail (or type icon), selection checkbox, name and
/// size/date. Clicking anywhere on the card toggles the selection.
pub fn file_card(
    ui: &mut Ui,
    width: f32,
    selected: &mut bool,
    rf: &RecoveredFile,
    thumb_state: crate::preview::Thumb<'_>,
) -> bool {
    use crate::preview::Thumb;
    let thumb_h = ((width - 16.0) * 0.70).round();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, thumb_h + 80.0), Sense::click());
    if resp.clicked() {
        *selected = !*selected;
    }
    paint_card(ui, rect, resp.hovered(), *selected);

    let category = ntfs::category_for_name(&rf.name);
    let (glyph, color) = category_style(category);
    let overwritten = matches!(thumb_state, Thumb::Failed);
    let thumb = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(rect.width() - 16.0, thumb_h));

    if let Thumb::Image(tex) = thumb_state {
        // Cover-fit: crop the texture to the thumbnail's aspect ratio.
        let ts = tex.size_vec2();
        let (ta, ra) = (ts.x / ts.y.max(1.0), thumb.width() / thumb.height());
        let uv = if ta > ra {
            let w = ra / ta;
            Rect::from_min_max(pos2((1.0 - w) / 2.0, 0.0), pos2((1.0 + w) / 2.0, 1.0))
        } else {
            let h = ta / ra;
            Rect::from_min_max(pos2(0.0, (1.0 - h) / 2.0), pos2(1.0, (1.0 + h) / 2.0))
        };
        egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), ts))
            .uv(uv)
            .rounding(egui::Rounding::same(7.0))
            .paint_at(ui, thumb);
    } else if overwritten {
        ui.painter().rect_filled(thumb, 7.0, theme::SUBTLE_BG);
        ui.painter().text(thumb.center() - vec2(0.0, 9.0), egui::Align2::CENTER_CENTER, icon::WARNING_CIRCLE, theme::icon(26.0), theme::TEXT_FAINT);
        ui.painter().text(thumb.center() + vec2(0.0, 18.0), egui::Align2::CENTER_CENTER, t("Geen voorbeeld", "No preview"), theme::font(11.0), theme::TEXT_DIM);
    } else if matches!(thumb_state, Thumb::Loading) {
        ui.painter().rect_filled(thumb, 7.0, theme::tint(color, 0.08));
        let spin = Rect::from_center_size(thumb.center(), vec2(22.0, 22.0));
        egui::Spinner::new().size(22.0).color(color).paint_at(ui, spin);
    } else {
        let ext = std::path::Path::new(&rf.name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_uppercase();
        ui.painter().rect_filled(thumb, 7.0, theme::tint(color, 0.08));
        ui.painter().text(thumb.center() - vec2(0.0, 8.0), egui::Align2::CENTER_CENTER, glyph, theme::icon(34.0), color);
        if !ext.is_empty() {
            ui.painter().text(thumb.center() + vec2(0.0, 24.0), egui::Align2::CENTER_CENTER, ext, theme::font_semibold(11.0), color);
        }
    }

    let check = Rect::from_min_size(thumb.min + vec2(7.0, 7.0), vec2(20.0, 20.0));
    if *selected || resp.hovered() {
        paint_checkbox(ui, check, *selected);
    }

    let x = rect.left() + 12.0;
    let max_w = rect.width() - 24.0;
    let (name, _) = elided(ui, &rf.name, theme::font_medium(13.0), theme::TEXT_MAIN, max_w);
    ui.painter().galley(pos2(x, thumb.bottom() + 12.0), name, theme::TEXT_MAIN);
    let date = fmt_month(rf.year, rf.month);
    let meta = if date.is_empty() {
        disk::format_size(rf.size)
    } else {
        format!("{} · {date}", disk::format_size(rf.size))
    };
    let (m, _) = elided(ui, &meta, theme::font(11.5), theme::TEXT_DIM, max_w);
    ui.painter().galley(pos2(x, thumb.bottom() + 33.0), m, theme::TEXT_DIM);

    let (name, path, size) = (&rf.name, &rf.path, disk::format_size(rf.size));
    let mut tip = tf!("{name}\nMap: {path}\nGrootte: {size}", "{name}\nFolder: {path}\nSize: {size}");
    if !date.is_empty() {
        tip.push_str(&tf!("\nGewijzigd: {date}", "\nModified: {date}"));
    }
    if overwritten {
        tip.push_str(t(
            "\n\nData mogelijk al overschreven – herstel kan onvolledig zijn.",
            "\n\nData may already be overwritten – recovery may be incomplete.",
        ));
    }
    let visible = ui.is_rect_visible(rect);
    resp.on_hover_text(tip);
    visible
}
