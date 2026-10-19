//! Drawing for the "Partities" screen: the volume table grouped per disk
//! and the disk map with one block per partition, as partition managers
//! show them. Both report what was clicked; right-clicking opens a menu
//! drawn by the caller.

use crate::disk::{self, VolumeInfo};
use crate::health::Status;
use crate::partition_ops_view::Selection;
use crate::partitions::{Layout, PartInfo};
use crate::{theme, ui};
use egui::{Color32, Rect, Sense, Stroke, pos2, vec2};
use egui_phosphor::regular as icon;

const TABLE_ROW_H: f32 = 26.0;
const MAP_ROW_H: f32 = 80.0;
const DISK_BOX_W: f32 = 132.0;
const BLOCK_MIN_W: f32 = 118.0;
const BLOCK_GAP: f32 = 6.0;
/// Right-hand table columns: (title, width).
const COLUMNS: [(&str, f32); 5] = [("Capaciteit", 100.0), ("Vrije ruimte", 100.0), ("Bestandssysteem", 135.0), ("Type", 95.0), ("Status", 230.0)];

const PRIMARY: Color32 = Color32::from_rgb(76, 175, 80);
const LOGICAL: Color32 = Color32::from_rgb(214, 158, 46);
const SYSTEM: Color32 = Color32::from_rgb(150, 158, 172);

/// One line of a disk: a partition or unpartitioned space.
pub struct VolRow<'a> {
    pub sel: Selection,
    pub part: Option<&'a PartInfo>,
    pub volume: Option<&'a VolumeInfo>,
    pub name: String,
    pub fs: String,
    pub size: u64,
    pub used: Option<u64>,
    /// "Primair", "Logisch", "GPT" or "Niet-toegewezen".
    pub kind: &'static str,
    pub status: String,
    /// The partition Windows runs from.
    pub windows: bool,
}

impl VolRow<'_> {
    pub fn is_free(&self) -> bool {
        self.part.is_none()
    }

    fn color(&self) -> Color32 {
        match self.part {
            None => theme::TRACK_BG,
            Some(p) if p.kind.is_system() => SYSTEM,
            Some(p) if p.logical => LOGICAL,
            Some(_) => PRIMARY,
        }
    }

    /// "C: (NTFS)" or the partition's name.
    fn map_title(&self) -> String {
        match self.volume {
            Some(v) if !v.filesystem.is_empty() => format!("{} ({})", self.name, v.filesystem),
            _ => self.name.clone(),
        }
    }
}

pub struct DiskView<'a> {
    pub layout: &'a Layout,
    pub model: String,
    /// "Basis GPT", "Basis MBR" or "Geen partitietabel".
    pub style: &'static str,
    pub health: Option<Status>,
    pub rows: Vec<VolRow<'a>>,
}

impl DiskView<'_> {
    pub fn health_text(&self) -> &'static str {
        health_text(self.health)
    }
}

pub fn health_text(health: Option<Status>) -> &'static str {
    match health {
        Some(Status::Good) => "Gezond",
        Some(Status::Caution) => "Let op",
        Some(Status::Bad) => "Slecht",
        None => "Onbekend",
    }
}

/// Widths of blocks for `sizes` in `total` points: each at least `min`
/// when there is room, the rest shared out by size.
pub fn block_widths(sizes: &[u64], total: f32, min: f32, gap: f32) -> Vec<f32> {
    let n = sizes.len();
    if n == 0 {
        return Vec::new();
    }
    let room = (total - gap * (n - 1) as f32).max(0.0);
    let base = min.min(room / n as f32);
    let extra = room - base * n as f32;
    let sum: u64 = sizes.iter().sum();
    sizes
        .iter()
        .map(|&s| base + if sum > 0 { extra * (s as f64 / sum as f64) as f32 } else { extra / n as f32 })
        .collect()
}

/// The table of volumes grouped per disk. Returns the row clicked.
pub fn volume_table(
    ui: &mut egui::Ui,
    disks: &[DiskView],
    selected: Option<Selection>,
    menu: &mut dyn FnMut(&mut egui::Ui, &VolRow),
) -> Option<Selection> {
    let width = ui.available_width() - 8.0;
    let name_w = (width - COLUMNS.iter().map(|c| c.1).sum::<f32>()).max(160.0);
    let mut clicked = None;

    // Header.
    let (rect, _) = ui.allocate_exact_size(vec2(width, TABLE_ROW_H), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::SUBTLE_BG);
    let mut x = rect.left() + 8.0;
    for (title, w) in std::iter::once(("Volume", name_w)).chain(COLUMNS) {
        cell(ui, rect, x, w, title, theme::font_semibold(12.5), theme::TEXT_DIM);
        x += w;
    }

    for d in disks {
        let (rect, _) = ui.allocate_exact_size(vec2(width, TABLE_ROW_H + 2.0), Sense::hover());
        ui.painter().text(pos2(rect.left() + 8.0, rect.center().y), egui::Align2::LEFT_CENTER, icon::HARD_DRIVE, theme::icon(15.0), theme::TEXT_DIM);
        let title = format!("Schijf {} ({}, {}) · {}", d.layout.index, d.style, d.health_text(), d.model);
        cell(ui, rect, rect.left() + 30.0, width - 40.0, &title, theme::font_semibold(13.0), theme::TEXT_MAIN);

        for row in &d.rows {
            let (rect, resp) = ui.allocate_exact_size(vec2(width, TABLE_ROW_H), Sense::click());
            let is_sel = selected == Some(row.sel);
            if is_sel {
                ui.painter().rect_filled(rect, 4.0, theme::tint(theme::ACCENT, 0.16));
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 4.0, theme::SUBTLE_BG);
            }
            let dot = Rect::from_center_size(pos2(rect.left() + 30.0, rect.center().y), vec2(10.0, 10.0));
            ui.painter().rect_filled(dot, 2.0, row.color());
            let free = row.volume.map(|v| disk::format_size(v.total_bytes.saturating_sub(v.used_bytes)));
            let values = [
                disk::format_size(row.size),
                free.unwrap_or_else(|| "—".to_string()),
                row.fs.clone(),
                row.kind.to_string(),
                row.status.clone(),
            ];
            let color = if row.is_free() { theme::TEXT_DIM } else { theme::TEXT_MAIN };
            cell(ui, rect, rect.left() + 44.0, name_w - 36.0, &row.name, theme::font_medium(13.0), color);
            let mut x = rect.left() + 8.0 + name_w;
            for ((_, w), value) in COLUMNS.iter().zip(values) {
                cell(ui, rect, x, *w, &value, theme::font(13.0), color);
                x += w;
            }
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() || resp.secondary_clicked() {
                clicked = Some(row.sel);
            }
            resp.context_menu(|ui| menu(ui, row));
        }
        ui.add_space(4.0);
    }
    clicked
}

/// Text in a table cell, elided to its width.
fn cell(ui: &egui::Ui, row: Rect, x: f32, w: f32, text: &str, font: egui::FontId, color: Color32) {
    let (g, _) = ui::elided(ui, text, font, color, (w - 10.0).max(10.0));
    ui.painter().galley(pos2(x, row.center().y - g.size().y / 2.0), g, color);
}

/// One row per disk with a block per partition. Returns the block clicked.
pub fn disk_map(
    ui: &mut egui::Ui,
    disks: &[DiskView],
    selected: Option<Selection>,
    menu: &mut dyn FnMut(&mut egui::Ui, &VolRow),
) -> Option<Selection> {
    let mut clicked = None;
    for d in disks {
        let width = ui.available_width() - 8.0;
        let (row_rect, _) = ui.allocate_exact_size(vec2(width, MAP_ROW_H), Sense::hover());
        ui.painter().rect_filled(row_rect, 8.0, theme::SUBTLE_BG);

        // Disk box.
        let disk_box = Rect::from_min_size(row_rect.min + vec2(6.0, 6.0), vec2(DISK_BOX_W, MAP_ROW_H - 12.0));
        ui.painter().rect_filled(disk_box, 6.0, theme::CARD_BG);
        ui.painter().rect_stroke(disk_box, 6.0, Stroke::new(1.0_f32, theme::CARD_BORDER));
        let p = ui.painter();
        p.text(disk_box.min + vec2(10.0, 16.0), egui::Align2::LEFT_CENTER, icon::HARD_DRIVE, theme::icon(18.0), theme::ACCENT);
        p.text(disk_box.min + vec2(34.0, 16.0), egui::Align2::LEFT_CENTER, format!("Schijf {}", d.layout.index), theme::font_semibold(13.5), theme::TEXT_MAIN);
        p.text(disk_box.min + vec2(10.0, 38.0), egui::Align2::LEFT_CENTER, d.style, theme::font(12.0), theme::TEXT_DIM);
        p.text(disk_box.min + vec2(10.0, 54.0), egui::Align2::LEFT_CENTER, disk::format_size(d.layout.size), theme::font(12.0), theme::TEXT_DIM);

        // Partition blocks.
        let area_left = disk_box.right() + BLOCK_GAP;
        let sizes: Vec<u64> = d.rows.iter().map(|r| r.size).collect();
        let widths = block_widths(&sizes, row_rect.right() - 6.0 - area_left, BLOCK_MIN_W, BLOCK_GAP);
        let mut x = area_left;
        for (row, w) in d.rows.iter().zip(widths) {
            let rect = Rect::from_min_size(pos2(x, disk_box.top()), vec2(w, disk_box.height()));
            x += w + BLOCK_GAP;
            let resp = ui.interact(rect, ui.id().with(("block", row.sel)), Sense::click());
            draw_block(ui, rect, row, selected == Some(row.sel), resp.hovered());
            let tip = format!("{}\n{} · {}", row.map_title(), disk::format_size(row.size), row.kind);
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip);
            if resp.clicked() || resp.secondary_clicked() {
                clicked = Some(row.sel);
            }
            resp.context_menu(|ui| menu(ui, row));
        }
        ui.add_space(8.0);
    }
    clicked
}

fn draw_block(ui: &egui::Ui, rect: Rect, row: &VolRow, selected: bool, hovered: bool) {
    let p = ui.painter();
    let fill = if selected { theme::tint(theme::ACCENT, 0.12) } else { theme::CARD_BG };
    p.rect_filled(rect, 6.0, fill);
    if row.is_free() {
        // Hatching for unallocated space.
        let clip = p.with_clip_rect(rect.shrink(1.0));
        let mut x = rect.left() - rect.height();
        while x < rect.right() {
            clip.line_segment([pos2(x, rect.bottom()), pos2(x + rect.height(), rect.top())], Stroke::new(1.0_f32, theme::TRACK_BG));
            x += 9.0;
        }
    }
    let stroke = if selected {
        Stroke::new(2.0_f32, theme::ACCENT)
    } else if hovered {
        Stroke::new(1.0_f32, Color32::from_rgb(196, 203, 214))
    } else {
        Stroke::new(1.0_f32, theme::CARD_BORDER)
    };
    p.rect_stroke(rect, 6.0, stroke);

    let inner_w = rect.width() - 16.0;
    if !row.is_free() {
        // Usage bar with the percentage used.
        let bar = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(inner_w, 16.0));
        p.rect_filled(bar, 3.0, theme::CARD_BG);
        p.rect_stroke(bar, 3.0, Stroke::new(1.0_f32, theme::tint(row.color(), 0.6)));
        let fraction = row.used.map_or(1.0, |u| (u as f32 / row.size.max(1) as f32).clamp(0.0, 1.0));
        let filled = Rect::from_min_size(bar.min, vec2(bar.width() * fraction, bar.height()));
        p.rect_filled(filled, 3.0, theme::tint(row.color(), if row.used.is_some() { 0.75 } else { 0.35 }));
        if row.used.is_some() && inner_w > 40.0 {
            p.text(bar.center(), egui::Align2::CENTER_CENTER, format!("{:.0}%", fraction * 100.0), theme::font_medium(11.0), theme::TEXT_MAIN);
        }
    }
    let mut title_x = rect.left() + 8.0;
    if row.windows {
        p.text(pos2(title_x, rect.top() + 38.0), egui::Align2::LEFT_CENTER, icon::FLAG, theme::icon(13.0), theme::DANGER);
        title_x += 16.0;
    }
    let max_w = rect.right() - 8.0 - title_x;
    let (title, _) = ui::elided(ui, &row.map_title(), theme::font_semibold(12.5), theme::TEXT_MAIN, max_w.max(10.0));
    p.galley(pos2(title_x, rect.top() + 30.0), title, theme::TEXT_MAIN);
    let sub = format!("{} {}", disk::format_size(row.size), if row.is_free() { "" } else { row.kind });
    let (sub, _) = ui::elided(ui, sub.trim(), theme::font(11.5), theme::TEXT_DIM, inner_w.max(10.0));
    p.galley(pos2(rect.left() + 8.0, rect.top() + 48.0), sub, theme::TEXT_DIM);
}

/// Colour key under the map.
pub fn legend(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        for (color, text) in [(PRIMARY, "Primair / GPT"), (LOGICAL, "Logisch"), (SYSTEM, "Systeem"), (theme::TRACK_BG, "Niet-toegewezen")] {
            let (dot, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            ui.painter().rect_filled(dot, 2.0, color);
            ui.label(egui::RichText::new(text).size(12.5).color(theme::TEXT_DIM));
            ui.add_space(8.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_fill_the_row_with_a_minimum_and_share_the_rest_by_size() {
        let widths = block_widths(&[100, 300, 0], 600.0, 100.0, 10.0);

        assert!((widths.iter().sum::<f32>() + 20.0 - 600.0).abs() < 0.01);
        assert!(widths.iter().all(|&w| w >= 100.0));
        assert!((widths[1] - 100.0 - 3.0 * (widths[0] - 100.0)).abs() < 0.01);
    }

    #[test]
    fn blocks_shrink_evenly_when_there_is_no_room_for_the_minimum() {
        let widths = block_widths(&[1, 1000], 110.0, 100.0, 10.0);

        assert_eq!(widths, [50.0, 50.0]);
        assert!(block_widths(&[], 100.0, 10.0, 2.0).is_empty());
    }
}
