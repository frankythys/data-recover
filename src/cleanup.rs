//! "Grote bestanden" screen: find what takes the most space on a drive and
//! open it in Explorer or move it to the Recycle Bin.

use crate::disk::{self, VolumeInfo};
use crate::largefiles::{self, CleanupProgress, CleanupResult, LargeFile};
use crate::{ntfs, theme, ui};
use egui::{Rect, Sense, pos2, vec2};
use egui_phosphor::regular as icon;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MIN_SIZES: [(&str, u64); 4] = [
    ("10 MB", 10 << 20),
    ("100 MB", 100 << 20),
    ("500 MB", 500 << 20),
    ("1 GB", 1 << 30),
];
const ROW_H: f32 = 60.0;
const ROW_GAP: f32 = 8.0;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Files,
    Folders,
}

struct CleanupScan {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<CleanupProgress>>,
    done_rx: Receiver<io::Result<CleanupResult>>,
    started_at: Instant,
    finished_after: Option<u64>,
    /// Used space of the scanned drive, when the whole drive is scanned.
    drive_used: Option<u64>,
}

pub struct CleanupScreen {
    root: String,
    min_size: usize,
    scan: Option<CleanupScan>,
    result: Option<CleanupResult>,
    tab: Tab,
    /// Folder shown in the "Mappen" tab (index into `CleanupResult::dirs`).
    folder: usize,
    /// Files directly inside `folder`, largest first.
    folder_files: Vec<LargeFile>,
    /// Index into the list of the active tab. In the "Mappen" tab the
    /// subfolders come first, then the files.
    selected: Option<usize>,
    confirm: Option<LargeFile>,
    /// (text, is_success) shown in the action bar.
    message: Option<(String, bool)>,
}

impl CleanupScreen {
    pub fn new() -> Self {
        Self {
            root: "C:\\".to_string(),
            min_size: 1,
            scan: None,
            result: None,
            tab: Tab::Files,
            folder: 0,
            folder_files: Vec::new(),
            selected: None,
            confirm: None,
            message: None,
        }
    }

    pub fn has_results(&self) -> bool {
        self.result.is_some()
    }

    fn is_scanning(&self) -> bool {
        self.scan.as_ref().is_some_and(|s| s.finished_after.is_none())
    }

    fn start_scan(&mut self, volumes: &[VolumeInfo], ctx: &egui::Context) {
        let root = PathBuf::from(self.root.trim());
        let min_size = MIN_SIZES[self.min_size].1;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(CleanupProgress::default()));
        let (tx, rx) = channel();

        let thread_cancel = cancel.clone();
        let thread_progress = progress.clone();
        let thread_ctx = ctx.clone();
        let thread_root = root.clone();
        std::thread::spawn(move || {
            let result = largefiles::scan(&thread_root, min_size, largefiles::MAX_FILE_RESULTS, &thread_cancel, |p| {
                *thread_progress.lock().unwrap() = p;
                thread_ctx.request_repaint();
            });
            let _ = tx.send(result);
            thread_ctx.request_repaint();
        });

        self.result = None;
        self.selected = None;
        self.message = None;
        self.scan = Some(CleanupScan {
            cancel,
            progress,
            done_rx: rx,
            started_at: Instant::now(),
            finished_after: None,
            drive_used: drive_used(&root, volumes),
        });
    }

    fn poll_scan(&mut self) {
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
                self.result = Some(r);
                self.enter_folder(0);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                self.message = Some(("Scan gestopt.".to_string(), false));
            }
            Err(e) => self.message = Some((format!("Scan mislukt: {e}"), false)),
        }
    }

    // -----------------------------------------------------------------------
    // Main panel
    // -----------------------------------------------------------------------

    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        self.poll_scan();
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.heading("Grote bestanden opruimen");
            ui.label(
                egui::RichText::new("Vind de bestanden en mappen die de meeste ruimte innemen.")
                    .color(theme::TEXT_DIM),
            );
        });
        ui.add_space(14.0);

        self.draw_location_card(ui, volumes);
        ui.add_space(12.0);
        if self.scan.is_some() {
            self.draw_status_card(ui);
            ui.add_space(12.0);
        }
        if self.result.is_some() {
            self.draw_results(ui);
        } else if !self.is_scanning() {
            ui::empty_card(ui, icon::BROOM, "Kies een schijf of map en klik op Scannen.");
        }
        self.draw_confirm(ui.ctx());
    }

    fn draw_location_card(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        let scanning = self.is_scanning();
        let mut start = false;
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
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!scanning, |ui| {
                    ui.label(egui::RichText::new(icon::FUNNEL).font(theme::icon(18.0)).color(theme::TEXT_DIM));
                    ui.label(egui::RichText::new("Bestanden en mappen vanaf").color(theme::TEXT_DIM));
                    egui::ComboBox::from_id_salt("cleanup_min_size")
                        .width(90.0)
                        .selected_text(MIN_SIZES[self.min_size].0)
                        .show_ui(ui, |ui| {
                            for (i, (label, _)) in MIN_SIZES.iter().enumerate() {
                                ui.selectable_value(&mut self.min_size, i, *label);
                            }
                        });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let button = ui::primary_icon_button(icon::MAGNIFYING_GLASS, "Scannen");
                    if ui.add_enabled(!scanning, button.min_size(vec2(120.0, 36.0))).clicked() {
                        start = true;
                    }
                });
            });
        });
        if start {
            self.start_scan(volumes, ui.ctx());
        }
    }

    fn draw_status_card(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        let progress = scan.progress.lock().unwrap().clone();
        let finished = scan.finished_after.is_some();
        let completed = finished && self.result.is_some();
        let elapsed = scan.finished_after.unwrap_or_else(|| scan.started_at.elapsed().as_secs());
        let (files, bytes) = match &self.result {
            Some(r) => (r.file_count, r.total_bytes),
            None => (progress.files_scanned, progress.bytes_counted),
        };
        let fraction = scan
            .drive_used
            .filter(|&used| used > 0)
            .map_or(0.0, |used| bytes as f32 / used as f32);
        let cancel = scan.cancel.clone();

        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui::progress_ring(ui, 54.0, fraction, completed);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let title = if completed {
                        "Scan voltooid"
                    } else if finished {
                        "Scan gestopt"
                    } else {
                        "Bezig met scannen…"
                    };
                    ui.label(egui::RichText::new(title).font(theme::font_semibold(16.0)));
                    let mut detail = format!(
                        "{} · {} · {}",
                        ui::fmt_files(files as usize),
                        disk::format_size(bytes),
                        ui::fmt_duration(elapsed)
                    );
                    if let Some(r) = self.result.as_ref().filter(|r| r.skipped_folders > 0) {
                        detail.push_str(&format!(" · {} mappen zonder toegang", ui::fmt_thousands(r.skipped_folders)));
                    }
                    ui.label(egui::RichText::new(detail).color(theme::TEXT_DIM));
                    if !finished {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&progress.current_folder).size(12.0).color(theme::TEXT_FAINT),
                            )
                            .truncate(),
                        );
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
            if !finished {
                ui.add_space(10.0);
                let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
                ui::progress_bar(ui, bar, fraction, theme::ACCENT);
            }
        });
    }

    fn draw_results(&mut self, ui: &mut egui::Ui) {
        let mut new_tab = None;
        ui.allocate_ui(vec2(340.0, 36.0), |ui| {
            let tabs = [
                (icon::FILE, "Bestanden", "Grootste losse bestanden"),
                (icon::FOLDER, "Mappen", "Door de mappen bladeren, grootste eerst"),
            ];
            let active = if self.tab == Tab::Files { 0 } else { 1 };
            new_tab = ui::segmented_tabs(ui, tabs, active);
        });
        if let Some(i) = new_tab {
            self.tab = if i == 0 { Tab::Files } else { Tab::Folders };
            self.selected = None;
        }
        ui.add_space(4.0);
        match self.tab {
            Tab::Files => self.draw_file_list(ui),
            Tab::Folders => self.draw_folder_browser(ui),
        }
    }

    fn draw_file_list(&mut self, ui: &mut egui::Ui) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let rows = result.files.len();
        let min = MIN_SIZES[self.min_size].0;
        ui.label(
            egui::RichText::new(format!("{} van minimaal {min}, grootste eerst", ui::fmt_files(rows)))
                .size(12.5)
                .color(theme::TEXT_DIM),
        );
        ui.add_space(8.0);
        if rows == 0 {
            ui::empty_card(ui, icon::CHECK_CIRCLE, "Niets gevonden boven deze grootte.");
            return;
        }
        let mut clicked = None;
        let mut open = None;
        egui::ScrollArea::vertical()
            .id_salt("cleanup_files")
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H + ROW_GAP, rows, |ui, range| {
                ui.spacing_mut().item_spacing.y = ROW_GAP;
                for i in range {
                    let f = &result.files[i];
                    let name = file_name(&f.path);
                    let (glyph, color) = ui::category_style(ntfs::category_for_name(&name));
                    let folder = f.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                    let row = result_row(ui, glyph, color, &name, &folder, f.size, self.selected == Some(i), false);
                    if row.double_clicked() {
                        open = Some(i);
                    } else if row.clicked() {
                        clicked = Some(i);
                    }
                }
            });
        if let Some(i) = clicked.or(open) {
            self.selected = Some(i);
        }
        if open.is_some() {
            self.open_selected();
        }
    }

    /// "Mappen" tab: breadcrumb plus the subfolders and files of the
    /// current folder, largest first. Clicking a folder opens it.
    fn draw_folder_browser(&mut self, ui: &mut egui::Ui) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let current = &result.dirs[self.folder];
        let children = current.children.clone();
        let folder_size = current.size;

        // Breadcrumb: up button, then every folder from the root down.
        let mut go_to = None;
        ui.horizontal(|ui| {
            let up = ui.add_enabled_ui(current.parent.is_some(), |ui| ui::icon_button(ui, icon::ARROW_UP, "Map omhoog"));
            if up.inner.clicked() {
                go_to = current.parent;
            }
            ui.add_space(4.0);
            let mut chain = vec![self.folder];
            while let Some(parent) = result.dirs[*chain.last().unwrap()].parent {
                chain.push(parent);
            }
            chain.reverse();
            for (n, &i) in chain.iter().enumerate() {
                if n > 0 {
                    ui.label(egui::RichText::new(icon::CARET_RIGHT).font(theme::icon(14.0)).color(theme::TEXT_FAINT));
                }
                let last = i == self.folder;
                let name = result.dirs[i].name.to_string_lossy();
                let text = egui::RichText::new(name.as_ref())
                    .font(theme::font_semibold(15.0))
                    .color(if last { theme::TEXT_MAIN } else { theme::ACCENT });
                let crumb = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                if !last && crumb.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    go_to = Some(i);
                }
            }
        });
        ui.label(
            egui::RichText::new(format!(
                "{} · {} submappen · {} in deze map",
                disk::format_size(folder_size),
                ui::fmt_thousands(children.len() as u64),
                ui::fmt_files(self.folder_files.len())
            ))
            .size(12.5)
            .color(theme::TEXT_DIM),
        );
        ui.add_space(8.0);

        let rows = children.len() + self.folder_files.len();
        if rows == 0 {
            ui::empty_card(ui, icon::FOLDER_OPEN, "Deze map is leeg.");
        } else {
            let mut clicked = None;
            let mut double = None;
            let percent = |size: u64| {
                let p = if folder_size > 0 { size as f64 * 100.0 / folder_size as f64 } else { 0.0 };
                format!("{p:.1}%").replace('.', ",")
            };
            egui::ScrollArea::vertical()
                .id_salt(("cleanup_folder", self.folder))
                .auto_shrink([false, false])
                .show_rows(ui, ROW_H + ROW_GAP, rows, |ui, range| {
                    ui.spacing_mut().item_spacing.y = ROW_GAP;
                    for i in range {
                        let selected = self.selected == Some(i);
                        let row = if let Some(&d) = children.get(i) {
                            let node = &result.dirs[d];
                            let sub = format!("{} · {}", percent(node.size), ui::fmt_files(node.files as usize));
                            let name = node.name.to_string_lossy();
                            result_row(ui, icon::FOLDER, theme::ACCENT, &name, &sub, node.size, selected, true)
                        } else {
                            let f = &self.folder_files[i - children.len()];
                            let name = file_name(&f.path);
                            let (glyph, color) = ui::category_style(ntfs::category_for_name(&name));
                            let sub = if largefiles::is_protected(&f.path) {
                                format!("{} van deze map · Systeembestand, niet verwijderen", percent(f.size))
                            } else {
                                format!("{} van deze map", percent(f.size))
                            };
                            result_row(ui, glyph, color, &name, &sub, f.size, selected, false)
                        };
                        if row.double_clicked() {
                            double = Some(i);
                        } else if row.clicked() {
                            clicked = Some(i);
                        }
                    }
                });
            match (clicked, double) {
                (Some(i), _) if i < children.len() => go_to = Some(children[i]),
                (_, Some(i)) if i >= children.len() => {
                    self.selected = Some(i);
                    self.open_selected();
                }
                (Some(i), _) => self.selected = Some(i),
                _ => {}
            }
        }
        if let Some(d) = go_to {
            self.enter_folder(d);
        }
    }

    /// Shows folder `index` in the "Mappen" tab and reads its files.
    fn enter_folder(&mut self, index: usize) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        self.folder = index;
        self.folder_files = largefiles::list_files(&result.dir_path(index));
        self.selected = None;
    }

    // -----------------------------------------------------------------------
    // Action bar and actions
    // -----------------------------------------------------------------------

    /// The selected file, in either tab.
    fn selected_file(&self) -> Option<&LargeFile> {
        let result = self.result.as_ref()?;
        let i = self.selected?;
        match self.tab {
            Tab::Files => result.files.get(i),
            Tab::Folders => {
                let subfolders = result.dirs[self.folder].children.len();
                i.checked_sub(subfolders).and_then(|f| self.folder_files.get(f))
            }
        }
    }

    /// What the action bar acts on: the selected file, or in the "Mappen"
    /// tab the folder that is open.
    fn selected_path(&self) -> Option<(PathBuf, u64)> {
        if let Some(f) = self.selected_file() {
            return Some((f.path.clone(), f.size));
        }
        let result = self.result.as_ref()?;
        (self.tab == Tab::Folders).then(|| (result.dir_path(self.folder), result.dirs[self.folder].size))
    }

    pub fn draw_action_bar(&mut self, ui: &mut egui::Ui) {
        let selection = self.selected_path();
        let mut open = false;
        let mut recycle = false;
        ui.horizontal_centered(|ui| {
            match &selection {
                Some((path, size)) => {
                    ui.label(egui::RichText::new(file_name(path)).font(theme::font_semibold(14.0)));
                    ui.label(egui::RichText::new(disk::format_size(*size)).color(theme::TEXT_DIM));
                }
                None => {
                    ui.label(egui::RichText::new("Selecteer een bestand of map").color(theme::TEXT_DIM));
                }
            }
            if let Some((text, success)) = &self.message {
                let (glyph, color) = if *success {
                    (icon::CHECK_CIRCLE, theme::SUCCESS)
                } else {
                    (icon::WARNING_CIRCLE, theme::DANGER)
                };
                ui.add_space(12.0);
                ui.label(egui::RichText::new(glyph).font(theme::icon(16.0)).color(color));
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let protected = self.selected_file().is_some_and(|f| largefiles::is_protected(&f.path));
                let can_recycle = self.selected_file().is_some() && !protected;
                let button = ui::danger_icon_button(icon::TRASH, "Naar prullenbak").min_size(vec2(160.0, 38.0));
                let resp = ui.add_enabled(can_recycle, button);
                if protected {
                    resp.clone().on_disabled_hover_text("Windows heeft dit bestand nodig; het kan niet naar de prullenbak.");
                } else if self.tab == Tab::Folders {
                    resp.clone().on_disabled_hover_text("Selecteer een bestand. Mappen gaan niet in één keer naar de prullenbak.");
                }
                recycle = resp.clicked();
                let button = ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, "Open in Verkenner").min_size(vec2(170.0, 38.0));
                open = ui.add_enabled(selection.is_some(), button).clicked();
            });
        });
        if open {
            self.open_selected();
        }
        if recycle {
            self.confirm = self.selected_file().filter(|f| !largefiles::is_protected(&f.path)).cloned();
        }
    }

    fn open_selected(&mut self) {
        if let Some((path, _)) = self.selected_path() {
            if let Err(e) = largefiles::show_in_explorer(&path) {
                self.message = Some((format!("Verkenner openen mislukt: {e}"), false));
            }
        }
    }

    fn draw_confirm(&mut self, ctx: &egui::Context) {
        let Some(file) = self.confirm.clone() else {
            return;
        };
        let screen = ctx.screen_rect();
        ctx.layer_painter(egui::LayerId::new(egui::Order::PanelResizeLine, egui::Id::new("cleanup_dim")))
            .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(70));

        let mut decision = None;
        egui::Window::new("Naar prullenbak")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .fixed_size(vec2(460.0, 0.0))
            .frame(theme::card_frame().inner_margin(egui::Margin::same(22.0)))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.horizontal(|ui| {
                    let (tile, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
                    ui::icon_tile(ui, tile, icon::TRASH, theme::DANGER, 23.0);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("Naar prullenbak verplaatsen?").font(theme::font_semibold(16.0)));
                        ui.label(
                            egui::RichText::new(format!("{} · {}", file_name(&file.path), disk::format_size(file.size)))
                                .font(theme::font_medium(13.5)),
                        );
                    });
                });
                ui.add(
                    egui::Label::new(egui::RichText::new(file.path.display().to_string()).size(12.0).color(theme::TEXT_DIM))
                        .wrap(),
                );
                ui.add_space(4.0);
                if let Some(warning) = largefiles::removal_warning(&file.path) {
                    notice(ui, icon::WARNING_CIRCLE, theme::DANGER, warning);
                }
                notice(
                    ui,
                    icon::INFO,
                    theme::ACCENT,
                    "De ruimte komt pas vrij als je de prullenbak leegt. Is het bestand te groot voor de \
                     prullenbak, dan vraagt Windows eerst of het definitief weg mag.",
                );
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(ui::danger_icon_button(icon::TRASH, "Naar prullenbak").min_size(vec2(150.0, 36.0))).clicked() {
                        decision = Some(true);
                    }
                    if ui.add(ui::secondary_icon_button(icon::X, "Annuleren").min_size(vec2(120.0, 36.0))).clicked() {
                        decision = Some(false);
                    }
                });
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                self.confirm = None;
                self.recycle(&file);
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn recycle(&mut self, file: &LargeFile) {
        let name = file_name(&file.path);
        match largefiles::move_to_recycle_bin(&file.path) {
            Ok(true) => {
                if let Some(result) = self.result.as_mut() {
                    result.files.retain(|f| f.path != file.path);
                    if let Some(dir) = file.path.parent().and_then(|p| find_dir(result, p)) {
                        result.subtract_file(dir, file.size);
                    }
                }
                self.folder_files.retain(|f| f.path != file.path);
                self.selected = None;
                self.message = Some((
                    format!(
                        "{name} ({}) staat in de prullenbak. Leeg de prullenbak om de ruimte vrij te maken.",
                        disk::format_size(file.size)
                    ),
                    true,
                ));
            }
            Ok(false) => self.message = Some((format!("{name} is niet verwijderd."), false)),
            Err(e) => self.message = Some((format!("{name}: {e}"), false)),
        }
    }
}

/// One result: icon, name, folder and size on the right.
fn result_row(
    ui: &mut egui::Ui,
    glyph: &str,
    color: egui::Color32,
    title: &str,
    subtitle: &str,
    size: u64,
    selected: bool,
    opens: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width() - 8.0, ROW_H), Sense::click());
    ui::paint_card(ui, rect, resp.hovered(), selected);
    let tile = Rect::from_min_size(rect.min + vec2(12.0, 11.0), vec2(38.0, 38.0));
    ui::icon_tile(ui, tile, glyph, color, 20.0);

    let size_galley = ui.painter().layout_no_wrap(disk::format_size(size), theme::font_semibold(14.0), theme::TEXT_MAIN);
    let right = if opens {
        ui.painter().text(
            pos2(rect.right() - 20.0, rect.center().y),
            egui::Align2::CENTER_CENTER,
            icon::CARET_RIGHT,
            theme::icon(16.0),
            if resp.hovered() { theme::ACCENT } else { theme::TEXT_FAINT },
        );
        rect.right() - 36.0
    } else {
        rect.right() - 16.0
    };
    let size_x = right - size_galley.size().x;
    ui.painter().galley(pos2(size_x, rect.center().y - size_galley.size().y / 2.0), size_galley, theme::TEXT_MAIN);

    let x = tile.right() + 12.0;
    let max_w = size_x - 16.0 - x;
    let (t, _) = ui::elided(ui, title, theme::font_medium(14.0), theme::TEXT_MAIN, max_w);
    let (s, _) = ui::elided(ui, subtitle, theme::font(12.0), theme::TEXT_DIM, max_w);
    ui.painter().galley(pos2(x, rect.top() + 12.0), t, theme::TEXT_MAIN);
    ui.painter().galley(pos2(x, rect.top() + 33.0), s, theme::TEXT_DIM);
    let hint = if opens { "Klik om de map te openen" } else { "Dubbelklik om te openen in Verkenner" };
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{subtitle}\n{hint}"))
}

/// Tinted info line with an icon, wrapped to the available width.
fn notice(ui: &mut egui::Ui, glyph: &str, color: egui::Color32, text: &str) {
    egui::Frame::none()
        .fill(theme::tint(color, 0.10))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(egui::RichText::new(glyph).font(theme::icon(16.0)).color(color));
                ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(theme::TEXT_MAIN)).wrap());
            });
        });
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Index of the scanned folder at `path`, found by walking down from the root.
fn find_dir(result: &CleanupResult, path: &Path) -> Option<usize> {
    let rest = path.strip_prefix(result.dir_path(0)).ok()?;
    let mut node = 0;
    for part in rest.components() {
        node = *result.dirs[node].children.iter().find(|&&c| result.dirs[c].name == part.as_os_str())?;
    }
    Some(node)
}

/// The volume whose drive letter `path` starts with.
fn volume_for<'a>(path: &Path, volumes: &'a [VolumeInfo]) -> Option<&'a VolumeInfo> {
    let text = path.to_string_lossy();
    let letter = text.get(..2)?;
    volumes.iter().find(|v| v.letter.eq_ignore_ascii_case(letter))
}

/// Used bytes of the drive when `root` is a whole drive (`C:\`), so the
/// progress ring can show a percentage.
fn drive_used(root: &Path, volumes: &[VolumeInfo]) -> Option<u64> {
    let text = root.to_string_lossy();
    if text.trim_end_matches('\\').len() != 2 {
        return None;
    }
    volume_for(root, volumes).map(|v| v.used_bytes)
}

/// Selectable location card (a drive or "Map kiezen…"). Returns whether clicked.
fn location_chip(ui: &mut egui::Ui, glyph: &str, title: &str, subtitle: &str, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(250.0, 64.0), Sense::click());
    let enabled = ui.is_enabled();
    ui::paint_card(ui, rect, enabled && resp.hovered(), selected);
    let color = if selected { theme::ACCENT } else { theme::TEXT_DIM };
    let tile = Rect::from_min_size(rect.min + vec2(12.0, 12.0), vec2(40.0, 40.0));
    ui::icon_tile(ui, tile, glyph, color, 21.0);
    let x = tile.right() + 12.0;
    let max_w = rect.right() - 14.0 - x;
    let fg = if selected { theme::ACCENT } else { theme::TEXT_MAIN };
    let (t, _) = ui::elided(ui, title, theme::font_medium(14.0), fg, max_w);
    let (s, _) = ui::elided(ui, subtitle, theme::font(12.0), theme::TEXT_DIM, max_w);
    ui.painter().galley(pos2(x, rect.top() + 13.0), t, fg);
    ui.painter().galley(pos2(x, rect.top() + 34.0), s, theme::TEXT_DIM);
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(subtitle).clicked()
}
