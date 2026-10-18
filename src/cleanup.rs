//! "Schijf opruimen" screen, in five tabs:
//! - "Systeem opschonen": caches and temporary files, cleaned in one go.
//! - "Programma's": big program data (see `apps_view`).
//! - "Grote bestanden": the user's own largest files.
//! - "Grote mappen": the user's own folders by size, to browse into, as a
//!   list or as a treemap (see `treemap`).
//! - "Duplicaten": files with the same contents (see `duplicates_view`).
//! Files and folders can be opened in Explorer or moved to the Recycle Bin.
//! Windows and program files and folders are never listed.

use crate::apps_view::AppsView;
use crate::bulk_view::{self, BulkDelete};
use crate::disk::{self, VolumeInfo};
use crate::duplicates_view::DuplicatesView;
use crate::junk::{self, CleanReport, Kind, Measure};
use crate::largefiles::{self, CleanupProgress, CleanupResult, LargeFile};
use crate::{ntfs, theme, treemap, ui};
use egui::{Rect, Sense, pos2, vec2};
use egui_phosphor::regular as icon;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MIN_SIZES: [(&str, u64); 4] = [
    ("10 MB", 10 << 20),
    ("100 MB", 100 << 20),
    ("500 MB", 500 << 20),
    ("1 GB", 1 << 30),
];
pub(crate) const ROW_H: f32 = 60.0;
pub(crate) const ROW_GAP: f32 = 8.0;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Junk,
    Programs,
    Large,
    Folders,
    Duplicates,
}

/// What is selected in the "Grote mappen" tab.
#[derive(PartialEq, Clone, Copy)]
enum FolderSel {
    /// Index into `CleanupResult::dirs`.
    Dir(usize),
    /// Index into `CleanupScreen::folder_files`.
    File(usize),
}

fn kind_icon(kind: Kind) -> &'static str {
    match kind {
        Kind::UserTemp => icon::FILE_DASHED,
        Kind::WindowsTemp => icon::GEAR,
        Kind::BrowserCache => icon::GLOBE,
        Kind::UpdateCache => icon::ARROWS_CLOCKWISE,
        Kind::CrashDumps => icon::BUG,
        Kind::RecycleBin => icon::TRASH,
    }
}

/// State of the "Systeem opschonen" tab.
struct JunkState {
    /// Per entry of [`junk::ALL`]; `None` while still measuring.
    measures: Vec<Option<Measure>>,
    checked: Vec<bool>,
    measure_rx: Option<Receiver<(usize, Measure)>>,
    clean_rx: Option<Receiver<Vec<CleanReport>>>,
    confirm: bool,
}

impl JunkState {
    fn new() -> Self {
        Self {
            measures: vec![None; junk::ALL.len()],
            // The Recycle Bin is left unticked: this is a recovery app, and
            // its contents may still be wanted.
            checked: junk::ALL.iter().map(|&k| k != Kind::RecycleBin).collect(),
            measure_rx: None,
            clean_rx: None,
            confirm: false,
        }
    }

    fn start_measure(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for (i, &kind) in junk::ALL.iter().enumerate() {
                if tx.send((i, junk::measure(kind))).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
        self.measures = vec![None; junk::ALL.len()];
        self.measure_rx = Some(rx);
    }

    /// A category can be cleaned once measured and readable.
    fn usable(&self, i: usize) -> bool {
        self.measures[i].is_some_and(|m| m.accessible)
    }

    /// Ticked categories that can actually be cleaned.
    fn selected(&self) -> Vec<usize> {
        (0..junk::ALL.len()).filter(|&i| self.checked[i] && self.usable(i)).collect()
    }

    /// Bytes that cleaning the ticked categories would free.
    fn selected_bytes(&self) -> u64 {
        self.selected().iter().filter_map(|&i| self.measures[i]).map(|m| m.bytes).sum()
    }
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
    tab: Tab,
    junk: JunkState,
    apps: AppsView,
    dups: DuplicatesView,
    root: String,
    min_size: usize,
    scan: Option<CleanupScan>,
    result: Option<CleanupResult>,
    /// Ticked rows of `result.files` in "Grote bestanden".
    checked: Vec<bool>,
    bulk: BulkDelete,
    /// Folder open in the "Grote mappen" tab; `None` is the overview.
    folder: Option<usize>,
    /// The user's files directly inside `folder`, largest first.
    folder_files: Vec<LargeFile>,
    folder_sel: Option<FolderSel>,
    /// Show "Grote mappen" as a treemap instead of a list.
    folder_map: bool,
    confirm: Option<LargeFile>,
    confirm_dir: Option<usize>,
    /// (text, is_success) shown on the screen.
    message: Option<(String, bool)>,
}

impl CleanupScreen {
    pub fn new() -> Self {
        Self {
            tab: Tab::Junk,
            junk: JunkState::new(),
            apps: AppsView::new(),
            dups: DuplicatesView::new(),
            root: "C:\\".to_string(),
            min_size: 1,
            scan: None,
            result: None,
            checked: Vec::new(),
            bulk: BulkDelete::default(),
            folder: None,
            folder_files: Vec::new(),
            folder_sel: None,
            folder_map: false,
            confirm: None,
            confirm_dir: None,
            message: None,
        }
    }

    /// Whether the bottom action bar (for a selected file or folder) is shown.
    pub fn has_action_bar(&self) -> bool {
        match self.tab {
            Tab::Junk => false,
            Tab::Programs => self.apps.has_action_bar(),
            Tab::Large | Tab::Folders => self.result.is_some(),
            Tab::Duplicates => self.dups.has_action_bar(),
        }
    }

    fn is_scanning(&self) -> bool {
        self.scan.as_ref().is_some_and(|s| s.finished_after.is_none())
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        self.poll(ui.ctx());
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.heading("Schijf opruimen");
            ui.label(egui::RichText::new("Maak veilig ruimte vrij op je schijf.").color(theme::TEXT_DIM));
        });
        ui.add_space(14.0);

        let mut new_tab = None;
        const TABS: [Tab; 5] = [Tab::Junk, Tab::Programs, Tab::Large, Tab::Folders, Tab::Duplicates];
        ui.allocate_ui(vec2(960.0, 36.0), |ui| {
            let tabs = [
                (icon::SPARKLE, "Systeem opschonen", "Tijdelijke bestanden en caches verwijderen"),
                (icon::APP_WINDOW, "Programma's", "AI-modellen, emulators, Docker en caches van programma's"),
                (icon::FILE, "Grote bestanden", "Je eigen grootste bestanden bekijken"),
                (icon::FOLDER, "Grote mappen", "Je eigen mappen op grootte bekijken"),
                (icon::COPY, "Duplicaten", "Bestanden met dezelfde inhoud vinden en kopieën opruimen"),
            ];
            let active = TABS.iter().position(|&t| t == self.tab).unwrap_or(0);
            new_tab = ui::segmented_tabs(ui, tabs, active);
        });
        if let Some(i) = new_tab {
            self.tab = TABS[i];
            self.message = None;
        }
        ui.add_space(14.0);

        match self.tab {
            Tab::Junk => self.draw_junk(ui),
            Tab::Programs => self.apps.draw(ui),
            Tab::Large | Tab::Folders => self.draw_large(ui, volumes),
            Tab::Duplicates => self.dups.draw(ui, volumes),
        }
        if self.apps.take_recycled() | self.dups.take_recycled() {
            // The Recycle Bin grew; remeasure it in "Systeem opschonen".
            self.junk.measures = vec![None; junk::ALL.len()];
        }
        self.draw_junk_confirm(ui.ctx());
        self.draw_recycle_confirm(ui.ctx());
        self.draw_folder_confirm(ui.ctx());
        if let Some((mode, done)) = self.bulk.draw(ui.ctx()) {
            self.apply_removed(&done.removed);
            if mode == crate::deleter::Mode::RecycleBin {
                self.junk.measures = vec![None; junk::ALL.len()];
            }
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        // Measurements arrive one category at a time.
        if let Some(rx) = &self.junk.measure_rx {
            loop {
                match rx.try_recv() {
                    Ok((i, m)) => self.junk.measures[i] = Some(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.junk.measure_rx = None;
                        break;
                    }
                }
            }
        }
        if let Some(rx) = &self.junk.clean_rx {
            if let Ok(reports) = rx.try_recv() {
                self.junk.clean_rx = None;
                let freed: u64 = reports.iter().map(|r| r.freed).sum();
                let skipped: u64 = reports.iter().map(|r| r.skipped).sum();
                let mut text = format!("{} vrijgemaakt.", disk::format_size(freed));
                if skipped > 0 {
                    text.push_str(&format!(
                        " {} overgeslagen (in gebruik of nog geen dag oud).",
                        ui::fmt_files(skipped as usize)
                    ));
                }
                self.message = Some((text, true));
                self.junk.start_measure(ctx);
            }
        }

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
                self.checked = vec![false; r.files.len()];
                self.result = Some(r);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                self.message = Some(("Scan gestopt.".to_string(), false));
            }
            Err(e) => self.message = Some((format!("Scan mislukt: {e}"), false)),
        }
    }

    // -----------------------------------------------------------------------
    // Systeem opschonen
    // -----------------------------------------------------------------------

    fn draw_junk(&mut self, ui: &mut egui::Ui) {
        let idle = self.junk.measure_rx.is_none() && self.junk.clean_rx.is_none();
        if idle && self.junk.measures.iter().all(Option::is_none) {
            self.junk.start_measure(ui.ctx());
        }
        let measuring = self.junk.measure_rx.is_some();
        let cleaning = self.junk.clean_rx.is_some();
        let selected = self.junk.selected_bytes();

        let mut clean = false;
        let mut remeasure = false;
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 16.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let (tile, _) = ui.allocate_exact_size(vec2(54.0, 54.0), Sense::hover());
                ui::icon_tile(ui, tile, icon::SPARKLE, theme::ACCENT, 28.0);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let title = if cleaning {
                        "Bezig met opschonen…".to_string()
                    } else if measuring {
                        "Bezig met meten…".to_string()
                    } else {
                        format!("{} kan worden opgeruimd", disk::format_size(selected))
                    };
                    ui.label(egui::RichText::new(title).font(theme::font_semibold(18.0)));
                    ui.label(
                        egui::RichText::new(
                            "Alleen bestanden die programma's zelf opnieuw aanmaken. Je eigen bestanden blijven staan.",
                        )
                        .color(theme::TEXT_DIM),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let button = ui::primary_icon_button(icon::SPARKLE, "Opschonen").min_size(vec2(150.0, 40.0));
                    let can_clean = !measuring && !cleaning && !self.junk.selected().is_empty();
                    clean = ui.add_enabled(can_clean, button).clicked();
                    ui.add_enabled_ui(!measuring && !cleaning, |ui| {
                        remeasure = ui::icon_button(ui, icon::ARROW_CLOCKWISE, "Opnieuw meten").clicked();
                    });
                    if measuring || cleaning {
                        ui.add(egui::Spinner::new().size(20.0).color(theme::ACCENT));
                    }
                });
            });
        });
        if clean {
            self.junk.confirm = true;
        }
        if remeasure {
            self.message = None;
            self.junk.start_measure(ui.ctx());
        }
        ui.add_space(10.0);
        self.draw_message(ui);

        egui::ScrollArea::vertical().id_salt("cleanup_junk").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = ROW_GAP;
            for (i, &kind) in junk::ALL.iter().enumerate() {
                let measure = self.junk.measures[i];
                let locked = measure.is_some_and(|m| !m.accessible);
                if junk_row(ui, kind, measure, self.junk.checked[i] && !locked, !cleaning && !locked) {
                    self.junk.checked[i] = !self.junk.checked[i];
                }
            }
        });
    }

    fn draw_junk_confirm(&mut self, ctx: &egui::Context) {
        if !self.junk.confirm {
            return;
        }
        let selected = self.junk.selected();
        let kinds: Vec<Kind> = selected.iter().map(|&i| junk::ALL[i]).collect();
        let total = self.junk.selected_bytes();
        let mut decision = None;
        dim_background(ctx);
        dialog("cleanup_junk_confirm", ctx, |ui| {
            let sub = format!("{} wordt vrijgemaakt", disk::format_size(total));
            dialog_title(ui, icon::SPARKLE, theme::ACCENT, "Opschonen?", &sub);
            for &i in &selected {
                let size = self.junk.measures[i].map(|m| disk::format_size(m.bytes)).unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(icon::CHECK).font(theme::icon(14.0)).color(theme::SUCCESS));
                    ui.label(egui::RichText::new(junk::ALL[i].title()).font(theme::font_medium(13.5)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(size).color(theme::TEXT_DIM));
                    });
                });
            }
            ui.add_space(4.0);
            notice(
                ui,
                icon::INFO,
                theme::ACCENT,
                "Deze bestanden worden definitief verwijderd. Programma's maken ze vanzelf opnieuw aan. \
                 Bestanden die in gebruik zijn, worden overgeslagen.",
            );
            if kinds.contains(&Kind::RecycleBin) {
                notice(
                    ui,
                    icon::WARNING_CIRCLE,
                    theme::DANGER,
                    "De prullenbak wordt geleegd. Die bestanden kun je daarna niet meer terugzetten.",
                );
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(ui::primary_icon_button(icon::SPARKLE, "Opschonen").min_size(vec2(140.0, 36.0))).clicked() {
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
                self.junk.confirm = false;
                self.message = None;
                let (tx, rx) = channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let reports = kinds.into_iter().map(junk::clean).collect();
                    let _ = tx.send(reports);
                    ctx.request_repaint();
                });
                self.junk.clean_rx = Some(rx);
            }
            Some(false) => self.junk.confirm = false,
            None => {}
        }
    }

    // -----------------------------------------------------------------------
    // Grote bestanden
    // -----------------------------------------------------------------------

    fn draw_large(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        self.draw_location(ui, volumes);
        ui.add_space(12.0);
        if self.scan.is_some() {
            self.draw_status_card(ui);
            ui.add_space(12.0);
        }
        if self.result.is_some() {
            match self.tab {
                Tab::Folders => self.draw_folders(ui),
                _ => self.draw_file_list(ui),
            }
        } else if !self.is_scanning() {
            ui::empty_card(ui, icon::MAGNIFYING_GLASS, "Kies een schijf of map en klik op Scannen.");
        }
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
        self.checked.clear();
        self.folder = None;
        self.folder_files.clear();
        self.folder_sel = None;
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
                    let what = if self.tab == Tab::Folders { "Mappen en bestanden vanaf" } else { "Bestanden vanaf" };
                    ui.label(egui::RichText::new(what).color(theme::TEXT_DIM));
                    egui::ComboBox::from_id_salt("cleanup_min_size")
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
                        egui::RichText::new("Windows- en programmabestanden worden niet getoond")
                            .size(12.5)
                            .color(theme::TEXT_DIM),
                    );
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

        // Once done, one line is enough; the space goes to the results.
        if completed {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon::CHECK_CIRCLE).font(theme::icon(16.0)).color(theme::SUCCESS));
                ui.label(egui::RichText::new("Scan voltooid").font(theme::font_semibold(14.0)));
                ui.label(
                    egui::RichText::new(format!(
                        "{} doorzocht · {} · {}",
                        ui::fmt_files(files as usize),
                        disk::format_size(bytes),
                        ui::fmt_duration(elapsed)
                    ))
                    .color(theme::TEXT_DIM),
                );
            });
            return;
        }

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
                    let detail = format!(
                        "{} doorzocht · {} · {}",
                        ui::fmt_files(files as usize),
                        disk::format_size(bytes),
                        ui::fmt_duration(elapsed)
                    );
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
            ui::empty_card(ui, icon::CHECK_CIRCLE, "Geen eigen bestanden gevonden boven deze grootte.");
            return;
        }
        bulk_view::select_bar(ui, &result.files, &mut self.checked);
        ui.add_space(6.0);
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
                    let row = result_row(ui, glyph, color, &name, &folder, f.size, false, Some(self.checked[i]));
                    if row.double_clicked() {
                        open = Some(i);
                    } else if row.clicked() {
                        clicked = Some(i);
                    }
                }
            });
        if let Some(i) = clicked {
            self.checked[i] = !self.checked[i];
        }
        if let Some(path) = open.map(|i| result.files[i].path.clone()) {
            self.show_in_explorer(&path);
        }
    }

    /// Takes removed files out of the results and clears the ticks.
    fn apply_removed(&mut self, removed: &[PathBuf]) {
        let Some(result) = self.result.as_mut() else {
            return;
        };
        for path in removed {
            let Some(file) = result.files.iter().find(|f| &f.path == path).cloned() else { continue };
            let folder = path.parent().and_then(|p| result.find_dir(p));
            result.remove_file(folder, &file);
        }
        self.folder_files.retain(|f| !removed.contains(&f.path));
        self.checked = vec![false; result.files.len()];
    }

    fn show_in_explorer(&mut self, path: &Path) {
        if let Err(e) = largefiles::show_in_explorer(path) {
            self.message = Some((format!("Verkenner openen mislukt: {e}"), false));
        }
    }

    /// "Grote mappen": the overview of the user's own folders, or the
    /// subfolders and files of the folder that is open.
    fn draw_folders(&mut self, ui: &mut egui::Ui) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let min_size = MIN_SIZES[self.min_size].1;
        let min = MIN_SIZES[self.min_size].0;
        let dirs: Vec<usize> = match self.folder {
            None => result.top_folders(),
            Some(f) => result.dirs[f].children.clone(),
        }
        .into_iter()
        .filter(|&d| result.dirs[d].size >= min_size)
        .collect();
        let files: Vec<usize> = (0..self.folder_files.len()).filter(|&i| self.folder_files[i].size >= min_size).collect();

        // Breadcrumb: up button, "Alle mappen" and the open folder's path.
        let mut go_to: Option<Option<usize>> = None;
        ui.horizontal(|ui| {
            let up = ui.add_enabled_ui(self.folder.is_some(), |ui| ui::icon_button(ui, icon::ARROW_UP, "Map omhoog"));
            if up.inner.clicked() {
                go_to = Some(self.folder.and_then(|f| result.dirs[f].parent).filter(|&p| !is_top(result, p)));
            }
            ui.add_space(4.0);
            let mut chain = Vec::new();
            let mut cur = self.folder;
            while let Some(i) = cur.filter(|&i| !is_top(result, i)) {
                chain.push(i);
                cur = result.dirs[i].parent;
            }
            chain.reverse();
            if crumb(ui, "Alle mappen", self.folder.is_none()) {
                go_to = Some(None);
            }
            for &i in &chain {
                ui.label(egui::RichText::new(icon::CARET_RIGHT).font(theme::icon(14.0)).color(theme::TEXT_FAINT));
                if crumb(ui, &result.dirs[i].name.to_string_lossy(), Some(i) == self.folder) {
                    go_to = Some(Some(i));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let view = |glyph: &str| egui::RichText::new(glyph).font(theme::icon(16.0));
                if ui.selectable_label(self.folder_map, view(icon::SQUARES_FOUR)).on_hover_text("Als kaart").clicked() {
                    self.folder_map = true;
                }
                if ui.selectable_label(!self.folder_map, view(icon::LIST_BULLETS)).on_hover_text("Als lijst").clicked() {
                    self.folder_map = false;
                }
            });
        });
        let summary = match self.folder {
            None => format!("{} mappen van minimaal {min}, grootste eerst", ui::fmt_thousands(dirs.len() as u64)),
            Some(f) => format!(
                "{} · {} submappen en {} van minimaal {min}",
                disk::format_size(result.dirs[f].size),
                ui::fmt_thousands(dirs.len() as u64),
                ui::fmt_files(files.len())
            ),
        };
        ui.label(egui::RichText::new(summary).size(12.5).color(theme::TEXT_DIM));
        ui.add_space(8.0);

        let rows = dirs.len() + files.len();
        if rows == 0 {
            ui::empty_card(ui, icon::CHECK_CIRCLE, "Geen eigen mappen of bestanden boven deze grootte.");
        } else if self.folder_map {
            let mut items: Vec<treemap::Item> = dirs
                .iter()
                .map(|&d| treemap::Item {
                    label: result.dirs[d].name.to_string_lossy().into_owned(),
                    size: result.dirs[d].size,
                    color: theme::ACCENT,
                    folder: true,
                })
                .collect();
            items.extend(files.iter().map(|&i| {
                let name = file_name(&self.folder_files[i].path);
                let color = ui::category_style(ntfs::category_for_name(&name)).1;
                treemap::Item { label: name, size: self.folder_files[i].size, color, folder: false }
            }));
            if let Some(hit) = treemap::draw(ui, &items) {
                match dirs.get(hit.index) {
                    Some(&d) => go_to = Some(Some(d)),
                    None => {
                        self.folder_sel = Some(FolderSel::File(files[hit.index - dirs.len()]));
                        if hit.double {
                            self.open_selected();
                        }
                    }
                }
            }
        } else {
            let mut clicked = None;
            let mut opened = None;
            let parent_size = self.folder.map(|f| result.dirs[f].size);
            egui::ScrollArea::vertical()
                .id_salt(("cleanup_folders", self.folder))
                .auto_shrink([false, false])
                .show_rows(ui, ROW_H + ROW_GAP, rows, |ui, range| {
                    ui.spacing_mut().item_spacing.y = ROW_GAP;
                    for r in range {
                        let (sel, row) = if let Some(&d) = dirs.get(r) {
                            let node = &result.dirs[d];
                            let mut sub = ui::fmt_files(node.files as usize);
                            if let Some(total) = parent_size.filter(|&t| t > 0) {
                                sub = format!("{} · {sub}", percent(node.size, total));
                            }
                            if self.folder.is_none() {
                                sub = format!("{sub} · {}", result.dir_path(d).display());
                            }
                            let name = node.name.to_string_lossy();
                            let selected = self.folder_sel == Some(FolderSel::Dir(d));
                            (FolderSel::Dir(d), folder_row(ui, &name, &sub, node.size, selected))
                        } else {
                            let i = files[r - dirs.len()];
                            let f = &self.folder_files[i];
                            let name = file_name(&f.path);
                            let (glyph, color) = ui::category_style(ntfs::category_for_name(&name));
                            let sub = parent_size.filter(|&t| t > 0).map(|t| format!("{} van deze map", percent(f.size, t)));
                            let selected = self.folder_sel == Some(FolderSel::File(i));
                            let row = result_row(ui, glyph, color, &name, &sub.unwrap_or_default(), f.size, selected, None);
                            (FolderSel::File(i), (row, false))
                        };
                        let (resp, chevron) = row;
                        if resp.double_clicked() || chevron {
                            opened = Some(sel);
                        } else if resp.clicked() {
                            clicked = Some(sel);
                        }
                    }
                });
            if let Some(sel) = clicked {
                self.folder_sel = Some(sel);
            }
            match opened {
                Some(FolderSel::Dir(d)) => go_to = Some(Some(d)),
                Some(sel @ FolderSel::File(_)) => {
                    self.folder_sel = Some(sel);
                    self.open_selected();
                }
                None => {}
            }
        }
        if let Some(target) = go_to {
            self.enter_folder(target);
        }
    }

    /// Opens `folder` in the "Grote mappen" tab (`None` = overview).
    fn enter_folder(&mut self, folder: Option<usize>) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        self.folder_files = folder.map(|f| largefiles::list_files(&result.dir_path(f))).unwrap_or_default();
        self.folder = folder;
        self.folder_sel = None;
    }

    /// Path and size of what the action bar acts on in the active tab.
    fn selection(&self) -> Option<(PathBuf, u64)> {
        match self.tab {
            Tab::Folders => match self.folder_sel? {
                FolderSel::Dir(d) => {
                    let result = self.result.as_ref()?;
                    Some((result.dir_path(d), result.dirs[d].size))
                }
                FolderSel::File(i) => self.folder_files.get(i).map(|f| (f.path.clone(), f.size)),
            },
            _ => self.selected_file().map(|f| (f.path.clone(), f.size)),
        }
    }

    fn selected_file(&self) -> Option<&LargeFile> {
        match self.tab {
            Tab::Folders => match self.folder_sel? {
                FolderSel::File(i) => self.folder_files.get(i),
                FolderSel::Dir(_) => None,
            },
            _ => {
                let files = &self.result.as_ref()?.files;
                let mut ticked = files.iter().zip(&self.checked).filter(|&(_, &c)| c);
                match (ticked.next(), ticked.next()) {
                    (Some((f, _)), None) => Some(f),
                    _ => None,
                }
            }
        }
    }

    pub fn draw_action_bar(&mut self, ui: &mut egui::Ui) {
        if self.tab == Tab::Programs {
            self.apps.draw_action_bar(ui);
            return;
        }
        if self.tab == Tab::Large {
            self.draw_large_action_bar(ui);
            return;
        }
        if self.tab == Tab::Duplicates {
            self.dups.draw_action_bar(ui);
            return;
        }
        let selection = self.selection();
        let can_recycle = selection.as_ref().is_some_and(|(p, _)| {
            !largefiles::is_system_or_app(p) && !(p.is_dir() && largefiles::is_container(p))
        });
        let mut open = false;
        let mut recycle = false;
        ui.horizontal_centered(|ui| {
            match &selection {
                Some((path, size)) => {
                    ui.label(egui::RichText::new(file_name(path)).font(theme::font_semibold(14.0)));
                    ui.label(egui::RichText::new(disk::format_size(*size)).color(theme::TEXT_DIM));
                }
                None => {
                    let hint = if self.tab == Tab::Folders { "Selecteer een map of bestand" } else { "Selecteer een bestand" };
                    ui.label(egui::RichText::new(hint).color(theme::TEXT_DIM));
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
                let button = ui::danger_icon_button(icon::TRASH, "Naar prullenbak").min_size(vec2(160.0, 38.0));
                let resp = ui.add_enabled(can_recycle, button);
                if selection.is_some() && !can_recycle {
                    resp.clone().on_disabled_hover_text("Deze map bevat andere mappen en kan niet in één keer weg.");
                }
                recycle = resp.clicked();
                let button =
                    ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, "Open in Verkenner").min_size(vec2(170.0, 38.0));
                open = ui.add_enabled(selection.is_some(), button).clicked();
            });
        });
        if open {
            self.open_selected();
        }
        if recycle {
            match (self.tab, self.folder_sel) {
                (Tab::Folders, Some(FolderSel::Dir(d))) => self.confirm_dir = Some(d),
                _ => self.confirm = self.selected_file().cloned(),
            }
        }
    }

    fn draw_large_action_bar(&mut self, ui: &mut egui::Ui) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let ticked = bulk_view::ticked(&result.files, &self.checked);
        let mut open = false;
        let mut remove = false;
        ui.horizontal_centered(|ui| {
            bulk_view::selection_label(ui, &ticked, "Vink bestanden aan om ze te verwijderen");
            if let Some((text, success)) = &self.message {
                let color = if *success { theme::SUCCESS } else { theme::DANGER };
                ui.add_space(12.0);
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = ui::danger_icon_button(icon::TRASH, "Verwijderen…").min_size(vec2(150.0, 38.0));
                remove = ui.add_enabled(!ticked.is_empty() && !self.bulk.busy(), button).clicked();
                let button = ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, "Open in Verkenner").min_size(vec2(170.0, 38.0));
                open = ui
                    .add_enabled(ticked.len() == 1, button)
                    .on_disabled_hover_text("Vink precies één bestand aan")
                    .clicked();
            });
        });
        if open {
            self.show_in_explorer(&ticked[0].path);
        }
        if remove {
            // Only the user's own files are listed; check again to be sure.
            let allowed = ticked.into_iter().filter(|f| !largefiles::is_system_or_app(&f.path)).collect();
            self.bulk.ask(allowed);
        }
    }

    fn open_selected(&mut self) {
        if let Some((path, _)) = self.selection() {
            if let Err(e) = largefiles::show_in_explorer(&path) {
                self.message = Some((format!("Verkenner openen mislukt: {e}"), false));
            }
        }
    }

    fn draw_folder_confirm(&mut self, ctx: &egui::Context) {
        let (Some(d), Some(result)) = (self.confirm_dir, self.result.as_ref()) else {
            return;
        };
        let path = result.dir_path(d);
        let (size, files) = (result.dirs[d].size, result.dirs[d].files);
        let mut decision = None;
        dim_background(ctx);
        dialog("cleanup_folder_confirm", ctx, |ui| {
            let sub = format!("{} · {} · {}", file_name(&path), disk::format_size(size), ui::fmt_files(files as usize));
            dialog_title(ui, icon::TRASH, theme::DANGER, "Hele map naar de prullenbak?", &sub);
            ui.add(egui::Label::new(egui::RichText::new(path.display().to_string()).size(12.0).color(theme::TEXT_DIM)).wrap());
            ui.add_space(4.0);
            notice(
                ui,
                icon::WARNING_CIRCLE,
                theme::DANGER,
                "De map gaat met alle submappen en bestanden naar de prullenbak. Controleer eerst in Verkenner \
                 of er niets in staat dat je nog nodig hebt.",
            );
            notice(
                ui,
                icon::INFO,
                theme::ACCENT,
                "Je kunt de map terugzetten vanuit de prullenbak. De ruimte komt pas vrij als je de prullenbak \
                 leegt. Is de map te groot voor de prullenbak, dan vraagt Windows eerst of hij definitief weg mag.",
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
                self.confirm_dir = None;
                let name = file_name(&path);
                match largefiles::move_to_recycle_bin(&path) {
                    Ok(true) => {
                        if let Some(result) = self.result.as_mut() {
                            result.remove_dir(d);
                        }
                        self.folder_sel = None;
                        self.message = Some((format!("{name} ({}) staat in de prullenbak.", disk::format_size(size)), true));
                        self.junk.measures = vec![None; junk::ALL.len()];
                    }
                    Ok(false) => self.message = Some((format!("{name} is niet verwijderd."), false)),
                    Err(e) => self.message = Some((format!("{name}: {e}"), false)),
                }
            }
            Some(false) => self.confirm_dir = None,
            None => {}
        }
    }

    fn draw_recycle_confirm(&mut self, ctx: &egui::Context) {
        let Some(file) = self.confirm.clone() else {
            return;
        };
        let mut decision = None;
        dim_background(ctx);
        dialog("cleanup_recycle_confirm", ctx, |ui| {
            let sub = format!("{} · {}", file_name(&file.path), disk::format_size(file.size));
            dialog_title(ui, icon::TRASH, theme::DANGER, "Naar prullenbak verplaatsen?", &sub);
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
                "Je kunt het bestand terugzetten vanuit de prullenbak. De ruimte komt pas vrij als je de \
                 prullenbak leegt (via Systeem opschonen). Is het bestand te groot voor de prullenbak, dan \
                 vraagt Windows eerst of het definitief weg mag.",
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
                    let folder = file.path.parent().and_then(|p| result.find_dir(p));
                    result.remove_file(folder, file);
                }
                self.folder_files.retain(|f| f.path != file.path);
                self.checked = vec![false; self.result.as_ref().map_or(0, |r| r.files.len())];
                self.folder_sel = None;
                self.message = Some((format!("{name} ({}) staat in de prullenbak.", disk::format_size(file.size)), true));
                // The Recycle Bin grew; show its new size in the other tab.
                self.junk.measures = vec![None; junk::ALL.len()];
            }
            Ok(false) => self.message = Some((format!("{name} is niet verwijderd."), false)),
            Err(e) => self.message = Some((format!("{name}: {e}"), false)),
        }
    }

    fn draw_message(&self, ui: &mut egui::Ui) {
        let Some((text, success)) = &self.message else {
            return;
        };
        let (glyph, color) = if *success {
            (icon::CHECK_CIRCLE, theme::SUCCESS)
        } else {
            (icon::WARNING_CIRCLE, theme::DANGER)
        };
        notice(ui, glyph, color, text);
        ui.add_space(10.0);
    }
}

/// One cleanup category: checkbox, icon, title, description and size.
/// Returns whether the row was clicked (toggles the checkbox).
fn junk_row(ui: &mut egui::Ui, kind: Kind, measure: Option<Measure>, checked: bool, enabled: bool) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width() - 8.0, 68.0), sense);
    ui::paint_card(ui, rect, enabled && resp.hovered(), checked);
    let check = Rect::from_center_size(pos2(rect.left() + 26.0, rect.center().y), vec2(20.0, 20.0));
    ui::paint_checkbox(ui, check, checked);
    let tile = Rect::from_min_size(pos2(rect.left() + 48.0, rect.top() + 14.0), vec2(40.0, 40.0));
    let color = if kind == Kind::RecycleBin { theme::DANGER } else { theme::ACCENT };
    ui::icon_tile(ui, tile, kind_icon(kind), color, 21.0);

    let right_x = match measure {
        None => {
            let spin = Rect::from_center_size(pos2(rect.right() - 28.0, rect.center().y), vec2(18.0, 18.0));
            egui::Spinner::new().size(18.0).color(theme::ACCENT).paint_at(ui, spin);
            spin.left()
        }
        Some(m) => {
            let (text, color) = if m.accessible {
                (disk::format_size(m.bytes), theme::TEXT_MAIN)
            } else {
                ("Administrator nodig".to_string(), theme::TEXT_FAINT)
            };
            let g = ui.painter().layout_no_wrap(text, theme::font_semibold(14.0), color);
            let x = rect.right() - 16.0 - g.size().x;
            ui.painter().galley(pos2(x, rect.center().y - g.size().y / 2.0), g, color);
            x
        }
    };

    let x = tile.right() + 12.0;
    let max_w = right_x - 16.0 - x;
    let (t, _) = ui::elided(ui, kind.title(), theme::font_medium(14.5), theme::TEXT_MAIN, max_w);
    let (d, _) = ui::elided(ui, kind.description(), theme::font(12.0), theme::TEXT_DIM, max_w);
    ui.painter().galley(pos2(x, rect.top() + 15.0), t, theme::TEXT_MAIN);
    ui.painter().galley(pos2(x, rect.top() + 37.0), d, theme::TEXT_DIM);
    let tip = if measure.is_some_and(|m| !m.accessible) {
        "Start Data Recoverer als administrator om dit op te ruimen."
    } else {
        kind.description()
    };
    let resp = resp.on_hover_text(tip);
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// One large file: icon, name, folder and size on the right.
pub(crate) fn result_row(
    ui: &mut egui::Ui,
    glyph: &str,
    color: egui::Color32,
    title: &str,
    subtitle: &str,
    size: u64,
    selected: bool,
    checkbox: Option<bool>,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width() - 8.0, ROW_H), Sense::click());
    ui::paint_card(ui, rect, resp.hovered(), selected || checkbox == Some(true));
    let mut left = rect.left() + 12.0;
    if let Some(checked) = checkbox {
        let check = Rect::from_center_size(pos2(left + 10.0, rect.center().y), vec2(20.0, 20.0));
        ui::paint_checkbox(ui, check, checked);
        left += 32.0;
    }
    let tile = Rect::from_min_size(pos2(left, rect.top() + 11.0), vec2(38.0, 38.0));
    ui::icon_tile(ui, tile, glyph, color, 20.0);

    let size_galley = ui.painter().layout_no_wrap(disk::format_size(size), theme::font_semibold(14.0), theme::TEXT_MAIN);
    let size_x = rect.right() - 16.0 - size_galley.size().x;
    ui.painter().galley(pos2(size_x, rect.center().y - size_galley.size().y / 2.0), size_galley, theme::TEXT_MAIN);

    let x = tile.right() + 12.0;
    let max_w = size_x - 16.0 - x;
    let (t, _) = ui::elided(ui, title, theme::font_medium(14.0), theme::TEXT_MAIN, max_w);
    let (s, _) = ui::elided(ui, subtitle, theme::font(12.0), theme::TEXT_DIM, max_w);
    ui.painter().galley(pos2(x, rect.top() + 12.0), t, theme::TEXT_MAIN);
    ui.painter().galley(pos2(x, rect.top() + 33.0), s, theme::TEXT_DIM);
    let hint = if checkbox.is_some() {
        "Klik om aan of uit te vinken, dubbelklik om te openen in Verkenner"
    } else {
        "Dubbelklik om te openen in Verkenner"
    };
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{subtitle}\n{hint}"))
}

/// One folder row with a chevron that opens it. Returns the row response
/// and whether the chevron was clicked.
fn folder_row(ui: &mut egui::Ui, title: &str, subtitle: &str, size: u64, selected: bool) -> (egui::Response, bool) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width() - 8.0, ROW_H), Sense::click());
    ui::paint_card(ui, rect, resp.hovered(), selected);
    let tile = Rect::from_min_size(rect.min + vec2(12.0, 11.0), vec2(38.0, 38.0));
    ui::icon_tile(ui, tile, icon::FOLDER, theme::ACCENT, 20.0);

    let chevron = Rect::from_center_size(pos2(rect.right() - 26.0, rect.center().y), vec2(36.0, 36.0));
    let on_chevron = resp.hover_pos().is_some_and(|p| chevron.contains(p));
    if on_chevron {
        ui.painter().rect_filled(chevron, 8.0, theme::ACCENT_SOFT);
    }
    ui.painter().text(
        chevron.center(),
        egui::Align2::CENTER_CENTER,
        icon::CARET_RIGHT,
        theme::icon(16.0),
        if resp.hovered() { theme::ACCENT } else { theme::TEXT_FAINT },
    );
    let size_galley = ui.painter().layout_no_wrap(disk::format_size(size), theme::font_semibold(14.0), theme::TEXT_MAIN);
    let size_x = chevron.left() - 8.0 - size_galley.size().x;
    ui.painter().galley(pos2(size_x, rect.center().y - size_galley.size().y / 2.0), size_galley, theme::TEXT_MAIN);

    let x = tile.right() + 12.0;
    let max_w = size_x - 16.0 - x;
    let (t, _) = ui::elided(ui, title, theme::font_medium(14.0), theme::TEXT_MAIN, max_w);
    let (s, _) = ui::elided(ui, subtitle, theme::font(12.0), theme::TEXT_DIM, max_w);
    ui.painter().galley(pos2(x, rect.top() + 12.0), t, theme::TEXT_MAIN);
    ui.painter().galley(pos2(x, rect.top() + 33.0), s, theme::TEXT_DIM);
    let chevron_clicked = resp.clicked() && resp.interact_pointer_pos().is_some_and(|p| chevron.contains(p));
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Klik om te selecteren, dubbelklik of klik op › om de map te openen");
    (resp, chevron_clicked)
}

/// Breadcrumb part; the current one is dark, the others are links.
/// Returns whether a link was clicked.
fn crumb(ui: &mut egui::Ui, text: &str, current: bool) -> bool {
    let color = if current { theme::TEXT_MAIN } else { theme::ACCENT };
    let label = egui::Label::new(egui::RichText::new(text).font(theme::font_semibold(15.0)).color(color));
    let resp = ui.add(label.sense(Sense::click()));
    !current && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Whether folder `index` is the root or a container shown only through
/// its contents (so it is skipped in the breadcrumb).
fn is_top(result: &CleanupResult, index: usize) -> bool {
    index == 0 || largefiles::is_container(&result.dir_path(index))
}

/// "12,5%" with a Dutch decimal comma.
fn percent(part: u64, total: u64) -> String {
    format!("{:.1}%", part as f64 * 100.0 / total as f64).replace('.', ",")
}

/// Selectable location card (a drive or "Map kiezen…"). Returns whether clicked.
pub(crate) fn location_chip(ui: &mut egui::Ui, glyph: &str, title: &str, subtitle: &str, selected: bool) -> bool {
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

/// Darkens the app behind a dialog.
pub(crate) fn dim_background(ctx: &egui::Context) {
    ctx.layer_painter(egui::LayerId::new(egui::Order::PanelResizeLine, egui::Id::new("cleanup_dim")))
        .rect_filled(ctx.screen_rect(), 0.0, egui::Color32::from_black_alpha(70));
}

/// Centered card-style dialog.
pub(crate) fn dialog(id: &str, ctx: &egui::Context, content: impl FnOnce(&mut egui::Ui)) {
    egui::Window::new(id)
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .fixed_size(vec2(540.0, 0.0))
        .frame(theme::card_frame().inner_margin(egui::Margin::same(22.0)))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            content(ui);
        });
}

pub(crate) fn dialog_title(ui: &mut egui::Ui, glyph: &str, color: egui::Color32, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
        ui::icon_tile(ui, tile, glyph, color, 23.0);
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).font(theme::font_semibold(16.0)));
            ui.label(egui::RichText::new(subtitle).font(theme::font_medium(13.5)));
        });
    });
}

/// Tinted info line with an icon, wrapped to the available width.
pub(crate) fn notice(ui: &mut egui::Ui, glyph: &str, color: egui::Color32, text: &str) {
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

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Used bytes of the drive when `root` is a whole drive (`C:\`), so the
/// progress ring can show a percentage.
fn drive_used(root: &Path, volumes: &[VolumeInfo]) -> Option<u64> {
    let text = root.to_string_lossy();
    if text.trim_end_matches('\\').len() != 2 {
        return None;
    }
    let letter = text.get(..2)?;
    volumes.iter().find(|v| v.letter.eq_ignore_ascii_case(letter)).map(|v| v.used_bytes)
}
