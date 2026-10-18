//! "Duplicaten" tab of the cleanup screen: finds the user's own files with
//! the same contents and lets the user remove the extra copies. The last
//! copy of a group can never be removed.

use crate::bulk_view::{self, BulkDelete};
use crate::cleanup::{ROW_GAP, ROW_H, file_name, location_chip, notice, result_row};
use crate::deleter::Mode;
use crate::disk::{self, VolumeInfo};
use crate::duplicates::{self, DupGroup, DupProgress, DupResult, Phase};
use crate::largefiles::{self, LargeFile};
use crate::{ntfs, theme, ui};
use egui::{Rect, Sense, pos2, vec2};
use egui_phosphor::regular as icon;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

const MIN_SIZES: [(&str, u64); 4] = [
    ("100 KB", 100 << 10),
    ("1 MB", 1 << 20),
    ("10 MB", 10 << 20),
    ("100 MB", 100 << 20),
];
/// Space a file row is indented under its group header.
const INDENT: f32 = 28.0;

struct DupScan {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<DupProgress>>,
    done_rx: Receiver<io::Result<DupResult>>,
    started_at: Instant,
    finished_after: Option<u64>,
}

/// One line of the list: a group header or one of its files.
#[derive(Clone, Copy)]
enum Row {
    Group(usize),
    File(usize, usize),
}

pub struct DuplicatesView {
    root: String,
    min_size: usize,
    scan: Option<DupScan>,
    result: Option<DupResult>,
    /// Ticks per group, per file.
    checked: Vec<Vec<bool>>,
    bulk: BulkDelete,
    message: Option<(String, bool)>,
    /// Set when something went to the Recycle Bin, so its size is remeasured.
    recycled: bool,
}

impl DuplicatesView {
    pub fn new() -> Self {
        Self {
            root: "C:\\".to_string(),
            min_size: 1,
            scan: None,
            result: None,
            checked: Vec::new(),
            bulk: BulkDelete::default(),
            message: None,
            recycled: false,
        }
    }

    pub fn has_action_bar(&self) -> bool {
        self.result.is_some()
    }

    /// Whether something was moved to the Recycle Bin since the last call.
    pub fn take_recycled(&mut self) -> bool {
        std::mem::take(&mut self.recycled)
    }

    fn is_scanning(&self) -> bool {
        self.scan.as_ref().is_some_and(|s| s.finished_after.is_none())
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        self.poll();
        self.draw_location(ui, volumes);
        ui.add_space(12.0);
        if self.scan.is_some() {
            self.draw_status(ui);
            ui.add_space(12.0);
        }
        if self.result.is_some() {
            self.draw_groups(ui);
        } else if !self.is_scanning() {
            ui::empty_card(ui, icon::MAGNIFYING_GLASS, "Kies een schijf of map en klik op Scannen.");
        }
        if let Some((mode, done)) = self.bulk.draw(ui.ctx()) {
            if let Some(result) = self.result.as_mut() {
                result.remove_files(&done.removed);
                self.checked = unticked(result);
            }
            self.recycled |= mode == Mode::RecycleBin && !done.removed.is_empty();
        }
    }

    fn poll(&mut self) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        if scan.finished_after.is_some() {
            return;
        }
        let Ok(result) = scan.done_rx.try_recv() else {
            return;
        };
        scan.finished_after = Some(scan.started_at.elapsed().as_secs());
        match result {
            Ok(r) => {
                self.checked = unticked(&r);
                self.result = Some(r);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                self.message = Some(("Scan gestopt.".to_string(), false));
            }
            Err(e) => self.message = Some((format!("Scan mislukt: {e}"), false)),
        }
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let root = PathBuf::from(self.root.trim());
        let min_size = MIN_SIZES[self.min_size].1;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(DupProgress::default()));
        let (tx, rx) = channel();
        let (thread_cancel, thread_progress, thread_ctx) = (cancel.clone(), progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = duplicates::scan(&root, min_size, &thread_cancel, |p| {
                *thread_progress.lock().unwrap() = p;
                thread_ctx.request_repaint();
            });
            let _ = tx.send(result);
            thread_ctx.request_repaint();
        });
        self.result = None;
        self.checked.clear();
        self.message = None;
        self.scan = Some(DupScan { cancel, progress, done_rx: rx, started_at: Instant::now(), finished_after: None });
    }

    fn draw_location(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        let scanning = self.is_scanning();
        ui::section_header(ui, icon::MAP_PIN, "Locatie", None);
        ui.add_enabled_ui(!scanning, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
                let mut is_drive = false;
                for vol in volumes {
                    let root = format!("{}\\", vol.letter);
                    let selected = self.root.eq_ignore_ascii_case(&root);
                    is_drive |= selected;
                    let glyph = if vol.kind == disk::DriveKind::External { icon::USB } else { icon::HARD_DRIVE };
                    let free = format!("{} vrij", disk::format_size(vol.total_bytes.saturating_sub(vol.used_bytes)));
                    if location_chip(ui, glyph, &vol.display_name(), &free, selected) {
                        self.root = root;
                    }
                }
                let (title, sub) = if is_drive {
                    ("Map kiezen…".to_string(), "Eén map doorzoeken".to_string())
                } else {
                    (file_name(Path::new(&self.root)), self.root.clone())
                };
                if location_chip(ui, icon::FOLDER_OPEN, &title, &sub, !is_drive) {
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&self.root).pick_folder() {
                        self.root = dir.display().to_string();
                    }
                }
            });
        });
        ui.add_space(12.0);
        let mut start = false;
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!scanning, |ui| {
                    ui.label(egui::RichText::new(icon::FUNNEL).font(theme::icon(18.0)).color(theme::TEXT_DIM));
                    ui.label(egui::RichText::new("Bestanden vanaf").color(theme::TEXT_DIM));
                    egui::ComboBox::from_id_salt("dups_min_size")
                        .width(90.0)
                        .selected_text(MIN_SIZES[self.min_size].0)
                        .show_ui(ui, |ui| {
                            for (i, (label, _)) in MIN_SIZES.iter().enumerate() {
                                ui.selectable_value(&mut self.min_size, i, *label);
                            }
                        });
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new(icon::SHIELD_CHECK).font(theme::icon(16.0)).color(theme::SUCCESS));
                    ui.label(
                        egui::RichText::new("Inhoud wordt vergeleken, niet de naam · Windows- en programmabestanden worden overgeslagen")
                            .size(12.5)
                            .color(theme::TEXT_DIM),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let button = ui::primary_icon_button(icon::MAGNIFYING_GLASS, "Scannen");
                    start = ui.add_enabled(!scanning, button.min_size(vec2(120.0, 36.0))).clicked();
                });
            });
        });
        if start {
            self.start_scan(ui.ctx());
        }
    }

    fn draw_status(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        let p = scan.progress.lock().unwrap().clone();
        let finished = scan.finished_after.is_some();
        let elapsed = scan.finished_after.unwrap_or_else(|| scan.started_at.elapsed().as_secs());
        let cancel = scan.cancel.clone();

        if let (true, Some(result)) = (finished, &self.result) {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon::CHECK_CIRCLE).font(theme::icon(16.0)).color(theme::SUCCESS));
                ui.label(egui::RichText::new("Scan voltooid").font(theme::font_semibold(14.0)));
                ui.label(
                    egui::RichText::new(format!(
                        "{} doorzocht · {}",
                        ui::fmt_files(result.files_scanned as usize),
                        ui::fmt_duration(elapsed)
                    ))
                    .color(theme::TEXT_DIM),
                );
            });
            return;
        }

        let fraction = if p.bytes_to_compare > 0 { p.bytes_compared as f32 / p.bytes_to_compare as f32 } else { 0.0 };
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui::progress_ring(ui, 54.0, fraction, false);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let title = match (finished, p.phase) {
                        (true, _) => "Scan gestopt",
                        (false, Phase::Listing) => "Bestanden zoeken…",
                        (false, Phase::Comparing) => "Inhoud vergelijken…",
                    };
                    ui.label(egui::RichText::new(title).font(theme::font_semibold(16.0)));
                    let detail = match p.phase {
                        Phase::Listing => format!("{} gevonden · {}", ui::fmt_files(p.files_scanned as usize), ui::fmt_duration(elapsed)),
                        Phase::Comparing => format!(
                            "{} van {} vergeleken · {}",
                            disk::format_size(p.bytes_compared),
                            disk::format_size(p.bytes_to_compare),
                            ui::fmt_duration(elapsed)
                        ),
                    };
                    ui.label(egui::RichText::new(detail).color(theme::TEXT_DIM));
                    if !finished {
                        ui.add(egui::Label::new(egui::RichText::new(&p.current).size(12.0).color(theme::TEXT_FAINT)).truncate());
                    }
                });
                if !finished {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui::icon_button(ui, icon::STOP, "Scan stoppen").clicked() {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    });
                }
            });
            if !finished && p.phase == Phase::Comparing {
                ui.add_space(10.0);
                let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
                ui::progress_bar(ui, bar, fraction, theme::ACCENT);
            }
        });
    }

    fn draw_groups(&mut self, ui: &mut egui::Ui) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let min = MIN_SIZES[self.min_size].0;
        ui.label(
            egui::RichText::new(format!(
                "{} groepen met dezelfde inhoud (vanaf {min}) · {} te winnen, meeste eerst",
                ui::fmt_thousands(result.groups.len() as u64),
                disk::format_size(result.wasted())
            ))
            .size(12.5)
            .color(theme::TEXT_DIM),
        );
        ui.add_space(8.0);
        if let Some((text, success)) = &self.message {
            let (glyph, color) = if *success { (icon::CHECK_CIRCLE, theme::SUCCESS) } else { (icon::WARNING_CIRCLE, theme::DANGER) };
            notice(ui, glyph, color, text);
            ui.add_space(8.0);
        }
        if result.groups.is_empty() {
            ui::empty_card(ui, icon::CHECK_CIRCLE, "Geen dubbele bestanden gevonden boven deze grootte.");
            return;
        }

        let mut auto = false;
        let mut clear = false;
        ui.horizontal(|ui| {
            auto = ui
                .add(ui::secondary_icon_button(icon::MAGIC_WAND, "Kopieën selecteren"))
                .on_hover_text("Vinkt in elke groep alles aan behalve het oudste bestand")
                .clicked();
            clear = ui.add(ui::secondary_icon_button(icon::X, "Niets selecteren")).clicked();
            let ticked = ticked_files(result, &self.checked);
            if !ticked.is_empty() {
                let bytes: u64 = ticked.iter().map(|f| f.size).sum();
                ui.label(
                    egui::RichText::new(format!("{} geselecteerd · {}", ui::fmt_files(ticked.len()), disk::format_size(bytes)))
                        .color(theme::TEXT_DIM),
                );
            }
        });
        ui.add_space(6.0);

        let rows: Vec<Row> = result
            .groups
            .iter()
            .enumerate()
            .flat_map(|(g, group)| std::iter::once(Row::Group(g)).chain((0..group.files.len()).map(move |i| Row::File(g, i))))
            .collect();
        let mut clicked = None;
        let mut open = None;
        egui::ScrollArea::vertical()
            .id_salt("cleanup_duplicates")
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H + ROW_GAP, rows.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = ROW_GAP;
                for r in range {
                    match rows[r] {
                        Row::Group(g) => group_row(ui, &result.groups[g], g == 0),
                        Row::File(g, i) => {
                            let f = &result.groups[g].files[i];
                            let resp = ui
                                .horizontal(|ui| {
                                    ui.add_space(INDENT);
                                    file_row(ui, f, self.checked[g][i])
                                })
                                .inner;
                            if resp.double_clicked() {
                                open = Some(f.path.clone());
                            } else if resp.clicked() {
                                clicked = Some((g, i));
                            }
                        }
                    }
                }
            });
        if auto {
            for row in &mut self.checked {
                for (i, c) in row.iter_mut().enumerate() {
                    *c = i > 0;
                }
            }
        }
        if clear {
            self.checked = unticked(result);
        }
        if let Some((g, i)) = clicked {
            self.checked[g][i] = !self.checked[g][i];
        }
        if let Some(path) = open {
            self.show_in_explorer(&path);
        }
    }

    pub fn draw_action_bar(&mut self, ui: &mut egui::Ui) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let ticked = ticked_files(result, &self.checked);
        let keeps_one = self.checked.iter().all(|row| row.iter().any(|&c| !c));
        let mut open = false;
        let mut remove = false;
        ui.horizontal_centered(|ui| {
            bulk_view::selection_label(ui, &ticked, "Vink kopieën aan om ze te verwijderen");
            if !keeps_one {
                ui.add_space(12.0);
                ui.label(egui::RichText::new(icon::WARNING_CIRCLE).font(theme::icon(16.0)).color(theme::DANGER));
                ui.label(egui::RichText::new("Laat in elke groep minstens één bestand staan").color(theme::DANGER));
            } else if let Some((text, success)) = &self.message {
                let color = if *success { theme::SUCCESS } else { theme::DANGER };
                ui.add_space(12.0);
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = ui::danger_icon_button(icon::TRASH, "Verwijderen…").min_size(vec2(150.0, 38.0));
                remove = ui
                    .add_enabled(keeps_one && !ticked.is_empty() && !self.bulk.busy(), button)
                    .on_disabled_hover_text("Vink kopieën aan; in elke groep moet minstens één bestand blijven")
                    .clicked();
                let button = ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, "Open in Verkenner").min_size(vec2(170.0, 38.0));
                open = ui.add_enabled(ticked.len() == 1, button).on_disabled_hover_text("Vink precies één bestand aan").clicked();
            });
        });
        if open {
            self.show_in_explorer(&ticked[0].path.clone());
        }
        if remove && keeps_one {
            // Only the user's own files are listed; check again to be sure.
            let allowed = ticked.into_iter().filter(|f| !largefiles::is_system_or_app(&f.path)).collect();
            self.bulk.ask(allowed);
        }
    }

    fn show_in_explorer(&mut self, path: &Path) {
        if let Err(e) = largefiles::show_in_explorer(path) {
            self.message = Some((format!("Verkenner openen mislukt: {e}"), false));
        }
    }
}

fn unticked(result: &DupResult) -> Vec<Vec<bool>> {
    result.groups.iter().map(|g| vec![false; g.files.len()]).collect()
}

fn ticked_files(result: &DupResult, checked: &[Vec<bool>]) -> Vec<LargeFile> {
    result.groups.iter().zip(checked).flat_map(|(g, row)| bulk_view::ticked(&g.files, row)).collect()
}

/// Group header: file name, number of copies and the space to win.
fn group_row(ui: &mut egui::Ui, group: &DupGroup, first: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width() - 8.0, ROW_H), Sense::hover());
    if !first {
        let y = rect.top() + 4.0;
        ui.painter().hline(rect.x_range(), y, egui::Stroke::new(1.0_f32, theme::CARD_BORDER));
    }
    let name = file_name(&group.files[0].path);
    let (glyph, color) = ui::category_style(ntfs::category_for_name(&name));
    let tile = Rect::from_min_size(pos2(rect.left() + 4.0, rect.top() + 16.0), vec2(34.0, 34.0));
    ui::icon_tile(ui, tile, glyph, color, 18.0);

    let win = format!("{} te winnen", disk::format_size(group.wasted()));
    let win = ui.painter().layout_no_wrap(win, theme::font_semibold(13.5), theme::ACCENT);
    let win_x = rect.right() - 16.0 - win.size().x;
    ui.painter().galley(pos2(win_x, tile.center().y - win.size().y / 2.0), win, theme::ACCENT);

    let x = tile.right() + 12.0;
    let max_w = win_x - 16.0 - x;
    let sub = format!("{} identieke bestanden van {}", group.files.len(), disk::format_size(group.size));
    let (t, _) = ui::elided(ui, &name, theme::font_semibold(14.0), theme::TEXT_MAIN, max_w);
    let (s, _) = ui::elided(ui, &sub, theme::font(12.0), theme::TEXT_DIM, max_w);
    ui.painter().galley(pos2(x, rect.top() + 15.0), t, theme::TEXT_MAIN);
    ui.painter().galley(pos2(x, rect.top() + 35.0), s, theme::TEXT_DIM);
}

/// One copy: its folder, name and date, with a checkbox.
fn file_row(ui: &mut egui::Ui, f: &LargeFile, checked: bool) -> egui::Response {
    let name = file_name(&f.path);
    let (glyph, color) = ui::category_style(ntfs::category_for_name(&name));
    let folder = f.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let date = f
        .modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| {
            let (y, m) = ntfs::ymd_from_unix(d.as_secs() as i64);
            ui::fmt_month(y, m)
        })
        .filter(|s| !s.is_empty())
        .map_or(name.clone(), |d| format!("{name} · gewijzigd {d}"));
    result_row(ui, glyph, color, &folder, &date, f.size, false, Some(checked))
}
