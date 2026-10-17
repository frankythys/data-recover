#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod disk;
mod ntfs;
mod theme;

use disk::{DriveKind, VolumeInfo};
use ntfs::{RecoveredFile, ScanProgress};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

enum Screen {
    Home,
    Browse,
}

struct ScanHandle {
    source_label: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<ScanProgress>>,
    found: Arc<Mutex<Vec<RecoveredFile>>>,
    done_rx: Receiver<io::Result<()>>,
    started_at: Instant,
    finished: bool,
}

struct DataRecovererApp {
    volumes: Vec<VolumeInfo>,
    scan_source_path: Option<String>,
    screen: Screen,
    scan: Option<ScanHandle>,
    selected: Vec<bool>,
    open_category: Option<&'static str>,
    /// (text, is_success) shown at the bottom of a screen.
    message: Option<(String, bool)>,
}

impl Default for DataRecovererApp {
    fn default() -> Self {
        Self {
            volumes: disk::list_volumes(),
            scan_source_path: None,
            screen: Screen::Home,
            scan: None,
            selected: Vec::new(),
            open_category: None,
            message: None,
        }
    }
}

impl DataRecovererApp {
    fn start_scan(&mut self, path: String, label: String, ctx: &egui::Context) {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(ScanProgress {
            records_scanned: 0,
            total_records: 0,
        }));
        let found = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = channel();

        let thread_cancel = cancel.clone();
        let thread_progress = progress.clone();
        let thread_found = found.clone();
        let thread_ctx = ctx.clone();
        let thread_path = path.clone();

        std::thread::spawn(move || {
            let result = ntfs::scan_drive(&thread_path, &thread_cancel, &thread_found, |p| {
                *thread_progress.lock().unwrap() = p;
                thread_ctx.request_repaint();
            });
            let _ = tx.send(result);
            thread_ctx.request_repaint();
        });

        self.message = None;
        self.selected.clear();
        self.open_category = None;
        self.scan_source_path = Some(path);
        self.scan = Some(ScanHandle {
            source_label: label,
            cancel,
            progress,
            found,
            done_rx: rx,
            started_at: Instant::now(),
            finished: false,
        });
        self.screen = Screen::Browse;
    }

    fn draw_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 8.0, theme::ACCENT);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "D",
                egui::FontId::proportional(18.0),
                egui::Color32::WHITE,
            );
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Data Recoverer").size(16.0).strong());
        });

        ui.add_space(24.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(egui::RichText::new("HERSTEL").size(11.0).color(theme::TEXT_DIM));
        });
        ui.add_space(4.0);

        let frame = egui::Frame::none()
            .fill(theme::ACCENT_SOFT)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(16.0, 10.0));
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            frame.show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Schijven & locaties")
                        .color(theme::ACCENT)
                        .strong(),
                );
            });
        });
    }

    fn draw_home(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("Kies een locatie om het herstel te starten");
            ui.add_space(6.0);
            if ui
                .add(theme::outline_button("⟳"))
                .on_hover_text("Opnieuw zoeken naar schijven")
                .clicked()
            {
                self.volumes = disk::list_volumes();
            }
        });
        ui.add_space(18.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            let internal: Vec<usize> = self
                .volumes
                .iter()
                .enumerate()
                .filter(|(_, v)| v.kind == DriveKind::Internal)
                .map(|(i, _)| i)
                .collect();
            let external: Vec<usize> = self
                .volumes
                .iter()
                .enumerate()
                .filter(|(_, v)| v.kind == DriveKind::External)
                .map(|(i, _)| i)
                .collect();

            let mut to_scan: Option<usize> = None;

            section_label(ui, &format!("Harde schijven ({})", internal.len()));
            ui.add_space(8.0);
            if internal.is_empty() {
                empty_hint(ui, "Geen interne schijven gevonden. Start als administrator.");
            } else {
                ui.horizontal_wrapped(|ui| {
                    for &i in &internal {
                        if draw_volume_card(ui, &self.volumes[i]) {
                            to_scan = Some(i);
                        }
                    }
                });
            }

            ui.add_space(20.0);
            section_label(ui, &format!("Externe schijven ({})", external.len()));
            ui.add_space(8.0);
            if external.is_empty() {
                empty_hint(ui, "Geen externe schijven aangesloten.");
            } else {
                ui.horizontal_wrapped(|ui| {
                    for &i in &external {
                        if draw_volume_card(ui, &self.volumes[i]) {
                            to_scan = Some(i);
                        }
                    }
                });
            }

            ui.add_space(20.0);
            section_label(ui, "Snelle toegang (4)");
            ui.add_space(8.0);
            let mut quick: Option<QuickAction> = None;
            ui.horizontal_wrapped(|ui| {
                if draw_quick_tile(ui, "\u{1F5BF}", "Schijfimage") {
                    quick = Some(QuickAction::DiskImage);
                }
                if draw_quick_tile(ui, "\u{1F5A5}", "Desktop") {
                    quick = Some(QuickAction::Desktop);
                }
                if draw_quick_tile(ui, "\u{1F4C1}", "Map selecteren") {
                    quick = Some(QuickAction::SelectFolder);
                }
                if draw_quick_tile(ui, "\u{1F5D1}", "Prullenbak") {
                    quick = Some(QuickAction::RecycleBin);
                }
            });

            if let Some(i) = to_scan {
                let vol = &self.volumes[i];
                let path = vol.scan_path.clone();
                let label = vol.display_name();
                self.start_scan(path, label, ctx);
            } else if let Some(action) = quick {
                self.handle_quick_action(action, ctx);
            }

            if let Some((text, success)) = &self.message {
                ui.add_space(12.0);
                let color = if *success { theme::SUCCESS } else { theme::TEXT_DIM };
                ui.colored_label(color, text);
            }
        });
    }

    fn handle_quick_action(&mut self, action: QuickAction, ctx: &egui::Context) {
        match action {
            QuickAction::DiskImage => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Schijfimage", &["img", "dd", "bin", "raw", "iso", "vhd"])
                    .add_filter("Alle bestanden", &["*"])
                    .pick_file()
                {
                    let label = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("Schijfimage")
                        .to_string();
                    self.start_scan(path.to_string_lossy().into_owned(), label, ctx);
                }
            }
            QuickAction::Desktop | QuickAction::SelectFolder | QuickAction::RecycleBin => {
                self.message = Some((
                    "Deze locatie wordt in een volgende versie ondersteund. \
                     Kies voorlopig een schijf of een schijfimage."
                        .to_string(),
                    false,
                ));
            }
        }
    }

    fn draw_browse(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };

        if !scan.finished {
            if let Ok(result) = scan.done_rx.try_recv() {
                match result {
                    Ok(()) => scan.finished = true,
                    Err(e) => {
                        let msg = if e.kind() == io::ErrorKind::PermissionDenied {
                            "Toegang geweigerd. Start Data Recoverer als administrator \
                             (rechtermuisknop → Als administrator uitvoeren)."
                                .to_string()
                        } else {
                            format!("Scan mislukt: {e}")
                        };
                        self.message = Some((msg, false));
                    }
                }
            }
        }

        let cancel = scan.cancel.clone();
        let source_label = scan.source_label.clone();
        let finished = scan.finished;
        let elapsed_secs = scan.started_at.elapsed().as_secs();
        let found_snapshot = scan.found.lock().unwrap().clone();
        let (records_scanned, total_records, fraction) = {
            let progress = scan.progress.lock().unwrap();
            let fraction = if progress.total_records > 0 {
                progress.records_scanned as f32 / progress.total_records as f32
            } else {
                0.0
            };
            (progress.records_scanned, progress.total_records, fraction)
        };

        if self.selected.len() < found_snapshot.len() {
            self.selected.resize(found_snapshot.len(), true);
        }

        let total_found_size: u64 = found_snapshot.iter().map(|f| f.size).sum();

        ui.horizontal(|ui| {
            if ui.add(theme::outline_button("← Schijven")).clicked() {
                cancel.store(true, Ordering::Relaxed);
                self.screen = Screen::Home;
                self.scan = None;
                self.message = None;
            }
            ui.add_space(8.0);
            ui.heading(format!("Gevonden bestanden ({})", found_snapshot.len()));
        });
        if self.scan.is_none() {
            return;
        }
        ui.add_space(12.0);

        let status_height = 110.0;
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - status_height).max(0.0))
            .show(ui, |ui| {
                if found_snapshot.is_empty() {
                    ui.colored_label(theme::TEXT_DIM, "Nog geen bestanden gevonden…");
                } else if let Some(category) = self.open_category {
                    if ui.add(theme::outline_button(&format!("← {category}"))).clicked() {
                        self.open_category = None;
                    }
                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        for (i, found) in found_snapshot.iter().enumerate() {
                            if ntfs::category_for_name(&found.name) != category {
                                continue;
                            }
                            draw_file_card(ui, &mut self.selected[i], found);
                        }
                    });
                } else {
                    let mut categories: Vec<(&'static str, usize, u64)> = Vec::new();
                    for found in &found_snapshot {
                        let cat = ntfs::category_for_name(&found.name);
                        match categories.iter_mut().find(|(name, _, _)| *name == cat) {
                            Some(entry) => {
                                entry.1 += 1;
                                entry.2 += found.size;
                            }
                            None => categories.push((cat, 1, found.size)),
                        }
                    }

                    let mut clicked_category = None;
                    ui.horizontal_wrapped(|ui| {
                        for (name, count, size) in &categories {
                            if draw_folder_card(ui, name, *count, *size) {
                                clicked_category = Some(*name);
                            }
                        }
                    });
                    if let Some(name) = clicked_category {
                        self.open_category = Some(name);
                    }
                }
            });

        ui.add_space(12.0);
        theme::card_frame().show(ui, |ui| {
            draw_progress_track(ui, fraction, ui.available_width());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let (badge_rect, _) =
                    ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(badge_rect.center(), 22.0, theme::ACCENT);
                ui.painter().text(
                    badge_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{:.0}%", fraction * 100.0),
                    egui::FontId::proportional(12.0),
                    egui::Color32::WHITE,
                );
                ui.add_space(10.0);

                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        let status_text = if finished {
                            "Scan voltooid"
                        } else {
                            "Snel scannen"
                        };
                        ui.strong(status_text);
                        ui.label(", bestanden gevonden:");
                        ui.colored_label(theme::ACCENT, format!("{}", found_snapshot.len()));
                        ui.colored_label(
                            theme::TEXT_DIM,
                            format!("({})", disk::format_size(total_found_size)),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            theme::TEXT_DIM,
                            format!(
                                "{source_label} — {records_scanned}/{total_records} MFT-records — Duur: {elapsed_secs}s"
                            ),
                        );
                        if !finished && ui.add(theme::outline_button("Stop")).clicked() {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    });
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let any_selected = self.selected.iter().any(|s| *s);
                    if ui
                        .add_enabled(any_selected, theme::accent_button("Herstellen"))
                        .clicked()
                    {
                        self.recover_selected(&found_snapshot);
                    }
                });
            });
        });

        if let Some((text, success)) = &self.message {
            ui.add_space(8.0);
            let color = if *success { theme::SUCCESS } else { theme::DANGER };
            ui.colored_label(color, text);
        }
    }

    fn recover_selected(&mut self, found_files: &[RecoveredFile]) {
        let Some(drive_path) = self.scan_source_path.clone() else {
            return;
        };

        let Some(out_dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };

        for (i, found) in found_files.iter().enumerate() {
            if !self.selected.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let Err(e) = ntfs::recover_file(&drive_path, found, &out_dir) {
                self.message = Some((format!("Herstel van {} mislukt: {e}", found.name), false));
                return;
            }
        }

        let count = self.selected.iter().filter(|selected| **selected).count();
        self.message = Some((format!("{count} bestand(en) hersteld naar {out_dir:?}"), true));
    }
}

#[derive(Clone, Copy)]
enum QuickAction {
    DiskImage,
    Desktop,
    SelectFolder,
    RecycleBin,
}

impl eframe::App for DataRecovererApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::SidePanel::left("sidebar")
            .exact_width(220.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme::SIDEBAR_BG))
            .show(ctx, |ui| {
                self.draw_sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::APP_BG)
                    .inner_margin(egui::Margin::same(24.0)),
            )
            .show(ctx, |ui| match self.screen {
                Screen::Home => self.draw_home(ui, ctx),
                Screen::Browse => self.draw_browse(ui),
            });
    }
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(14.0).color(theme::TEXT_DIM));
}

fn empty_hint(ui: &mut egui::Ui, text: &str) {
    theme::card_frame().show(ui, |ui| {
        ui.colored_label(theme::TEXT_DIM, text);
    });
}

/// Draws a Recoverit-style volume card (filesystem + disk badges, label,
/// usage, and a "Scannen" pill on hover) and returns whether it was clicked.
fn draw_volume_card(ui: &mut egui::Ui, vol: &VolumeInfo) -> bool {
    let card_size = egui::vec2(300.0, 104.0);
    let (rect, response) = ui.allocate_exact_size(card_size, egui::Sense::click());

    let bg = if response.hovered() {
        theme::ACCENT_SOFT
    } else {
        theme::CARD_BG
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 12.0, bg);
    painter.rect_stroke(rect, 12.0, egui::Stroke::new(1.0_f32, theme::CARD_BORDER));

    // Badges top-left.
    let mut badge_x = rect.min.x + 16.0;
    let badge_y = rect.min.y + 14.0;
    if !vol.filesystem.is_empty() {
        badge_x = draw_badge(ui, egui::pos2(badge_x, badge_y), &vol.filesystem) + 6.0;
    }
    if let Some(idx) = vol.disk_index {
        draw_badge(ui, egui::pos2(badge_x, badge_y), &format!("Schijf{idx}"));
    }

    // Disk icon.
    let painter = ui.painter();
    let icon_rect = egui::Rect::from_min_size(
        rect.min + egui::vec2(16.0, 44.0),
        egui::vec2(44.0, 44.0),
    );
    painter.rect_filled(icon_rect, 8.0, theme::ACCENT_SOFT);
    painter.text(
        icon_rect.center(),
        egui::Align2::CENTER_CENTER,
        "\u{1F5B4}",
        egui::FontId::proportional(22.0),
        theme::ACCENT,
    );

    // Name + usage.
    let text_x = rect.min.x + 72.0;
    painter.text(
        egui::pos2(text_x, rect.min.y + 48.0),
        egui::Align2::LEFT_TOP,
        vol.display_name(),
        egui::FontId::proportional(15.0),
        theme::TEXT_MAIN,
    );
    painter.text(
        egui::pos2(text_x, rect.min.y + 70.0),
        egui::Align2::LEFT_TOP,
        format!(
            "{} / {}",
            disk::format_size(vol.used_bytes),
            disk::format_size(vol.total_bytes)
        ),
        egui::FontId::proportional(12.0),
        theme::TEXT_DIM,
    );

    // "Scannen" pill on hover.
    if response.hovered() {
        let pill = egui::Rect::from_min_size(
            egui::pos2(rect.max.x - 92.0, rect.max.y - 34.0),
            egui::vec2(76.0, 24.0),
        );
        painter.rect_filled(pill, 12.0, theme::ACCENT);
        painter.text(
            pill.center(),
            egui::Align2::CENTER_CENTER,
            "Scannen",
            egui::FontId::proportional(12.0),
            egui::Color32::WHITE,
        );
    }

    ui.add_space(6.0);
    response.clicked()
}

/// Draws a small rounded badge at `pos` and returns its right edge x.
fn draw_badge(ui: &mut egui::Ui, pos: egui::Pos2, text: &str) -> f32 {
    let font = egui::FontId::proportional(10.0);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font.clone(), theme::ACCENT);
    let pad = 7.0;
    let w = galley.size().x + pad * 2.0;
    let h = 17.0;
    let badge = egui::Rect::from_min_size(pos, egui::vec2(w, h));
    let painter = ui.painter();
    painter.rect_filled(badge, 5.0, theme::ACCENT_SOFT);
    painter.text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        text,
        font,
        theme::ACCENT,
    );
    badge.max.x
}

/// Draws a quick-access tile (icon + label) and returns whether clicked.
fn draw_quick_tile(ui: &mut egui::Ui, icon: &str, label: &str) -> bool {
    let size = egui::vec2(210.0, 84.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let bg = if response.hovered() {
        theme::ACCENT_SOFT
    } else {
        theme::CARD_BG
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 12.0, bg);
    painter.rect_stroke(rect, 12.0, egui::Stroke::new(1.0_f32, theme::CARD_BORDER));

    let icon_rect =
        egui::Rect::from_min_size(rect.min + egui::vec2(18.0, 22.0), egui::vec2(40.0, 40.0));
    painter.rect_filled(icon_rect, 8.0, theme::ACCENT_SOFT);
    painter.text(
        icon_rect.center(),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(20.0),
        theme::ACCENT,
    );
    painter.text(
        rect.min + egui::vec2(70.0, 42.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(15.0),
        theme::TEXT_MAIN,
    );

    ui.add_space(6.0);
    response.clicked()
}

fn draw_file_card(ui: &mut egui::Ui, selected: &mut bool, found: &RecoveredFile) {
    let extension = std::path::Path::new(&found.name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("?")
        .to_uppercase();

    let card_size = egui::vec2(140.0, 126.0);
    ui.allocate_ui(card_size, |ui| {
        theme::card_frame().show(ui, |ui| {
            ui.set_width(108.0);
            ui.checkbox(selected, "");
            ui.vertical_centered(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 8.0, theme::ACCENT_SOFT);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    extension,
                    egui::FontId::proportional(13.0),
                    theme::ACCENT,
                );
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&found.name).size(11.0))
                    .on_hover_text(&found.name);
                ui.colored_label(theme::TEXT_DIM, disk::format_size(found.size));
            });
        });
    });
}

/// Draws a folder card for a file category and returns whether it was
/// clicked (to drill into that category's files).
fn draw_folder_card(ui: &mut egui::Ui, name: &str, count: usize, size: u64) -> bool {
    let card_size = egui::vec2(140.0, 126.0);
    let mut clicked = false;
    ui.allocate_ui(card_size, |ui| {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(124.0, 112.0), egui::Sense::click());
        let bg = if response.hovered() {
            theme::ACCENT_SOFT
        } else {
            theme::CARD_BG
        };
        ui.painter().rect_filled(rect, 10.0, bg);
        ui.painter()
            .rect_stroke(rect, 10.0, egui::Stroke::new(1.0_f32, theme::CARD_BORDER));

        let folder_center = rect.center() - egui::vec2(0.0, 14.0);
        let body = egui::Rect::from_center_size(folder_center, egui::vec2(52.0, 38.0));
        let tab = egui::Rect::from_min_size(
            egui::pos2(body.min.x + 4.0, body.min.y - 8.0),
            egui::vec2(22.0, 10.0),
        );
        ui.painter().rect_filled(tab, 3.0, theme::FOLDER_COLOR);
        ui.painter().rect_filled(body, 4.0, theme::FOLDER_COLOR);

        ui.painter().text(
            rect.center() + egui::vec2(0.0, 28.0),
            egui::Align2::CENTER_CENTER,
            name,
            egui::FontId::proportional(14.0),
            theme::TEXT_MAIN,
        );
        ui.painter().text(
            rect.center() + egui::vec2(0.0, 46.0),
            egui::Align2::CENTER_CENTER,
            format!("{count} • {}", disk::format_size(size)),
            egui::FontId::proportional(11.0),
            theme::TEXT_DIM,
        );

        if response.clicked() {
            clicked = true;
        }
    });
    clicked
}

fn draw_progress_track(ui: &mut egui::Ui, fraction: f32, width: f32) {
    let height = 8.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width.max(1.0), height), egui::Sense::hover());
    let rounding = height / 2.0;

    ui.painter().rect_filled(rect, rounding, theme::TRACK_BG);

    let filled_width = (rect.width() * fraction.clamp(0.0, 1.0)).max(height);
    let filled_rect =
        egui::Rect::from_min_size(rect.min, egui::vec2(filled_width.min(rect.width()), height));
    ui.painter().rect_filled(filled_rect, rounding, theme::ACCENT);
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([940.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Data Recoverer",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(DataRecovererApp::default()))
        }),
    )
}
