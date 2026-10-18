#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apps;
mod apps_view;
mod bulk_view;
mod cleanup;
mod deleter;
mod disk;
mod duplicates;
mod duplicates_view;
mod fat;
mod health;
mod health_view;
mod imaging;
mod imaging_view;
mod junk;
mod largefiles;
mod ntfs;
mod partition_ops;
mod partition_ops_view;
mod partitions;
mod partitions_view;
mod preview;
mod scope;
mod speed;
mod speed_view;
mod surface;
mod surface_view;
mod theme;
mod treemap;
mod ui;
mod wipe;
mod wipe_view;

use disk::{DriveKind, VolumeInfo};
use egui_phosphor::regular as icon;
use ntfs::{RecoveredFile, ScanProgress};
use scope::Scope;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use ui::ARROW_W;

enum Screen {
    Home,
    Browse,
    Cleanup,
    Health,
    Surface,
    Speed,
    Partitions,
    Imaging,
    Wipe,
}

#[derive(PartialEq, Clone, Copy)]
enum BrowseTab {
    Date,
    Folder,
}

/// Which node of the left tree is selected (drives the file grid filter).
#[derive(PartialEq, Clone)]
enum TreeSel {
    All,
    Year(u16),
    YearMonth(u16, u8),
    /// A folder and everything below it.
    Folder(String),
    /// Only the files directly in the volume root.
    RootFiles,
}

impl TreeSel {
    fn matches(&self, rf: &RecoveredFile) -> bool {
        match self {
            TreeSel::All => true,
            TreeSel::Year(y) => rf.year == *y,
            TreeSel::YearMonth(y, m) => rf.year == *y && rf.month == *m,
            TreeSel::Folder(p) => {
                rf.path == *p
                    || (rf.path.len() > p.len()
                        && rf.path.starts_with(p.as_str())
                        && rf.path.as_bytes()[p.len()] == b'\\')
            }
            TreeSel::RootFiles => rf.path == "\\",
        }
    }

    /// Short Dutch description of the active filter, for the breadcrumb.
    fn describe(&self) -> String {
        match self {
            TreeSel::All => "Alle bestanden".to_string(),
            TreeSel::Year(0) => "Onbekende datum".to_string(),
            TreeSel::Year(y) => y.to_string(),
            TreeSel::YearMonth(y, m) => {
                let month = MONTHS_NL.get(m.wrapping_sub(1) as usize).copied().unwrap_or("?");
                format!("{month} {y}")
            }
            TreeSel::Folder(p) => p.clone(),
            TreeSel::RootFiles => "Hoofdmap".to_string(),
        }
    }
}

const MONTHS_NL: [&str; 12] = [
    "januari", "februari", "maart", "april", "mei", "juni", "juli", "augustus", "september",
    "oktober", "november", "december",
];

struct ScanHandle {
    source_label: String,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    progress: Arc<Mutex<ScanProgress>>,
    found: Arc<Mutex<Vec<RecoveredFile>>>,
    done_rx: Receiver<io::Result<()>>,
    started_at: Instant,
    finished_after: Option<u64>,
    finished: bool,
}

struct DataRecovererApp {
    volumes: Vec<VolumeInfo>,
    scan_source_path: Option<String>,
    screen: Screen,
    scan: Option<ScanHandle>,
    selected: Vec<bool>,
    browse_tab: BrowseTab,
    tree_sel: TreeSel,
    expanded_years: HashSet<u16>,
    expanded_folders: HashSet<String>,
    open_category: Option<&'static str>,
    /// Image thumbnails, decoded in the background (keyed by file index).
    previews: preview::PreviewLoader,
    /// (text, is_success) shown as a banner / in the action bar.
    message: Option<(String, bool)>,
    /// "Grote bestanden" screen; keeps its scan when switching screens.
    cleanup: cleanup::CleanupScreen,
    health: health_view::HealthView,
    imaging: imaging_view::ImagingView,
    partitions: partitions_view::PartitionsView,
    surface: surface_view::SurfaceView,
    speed: speed_view::SpeedView,
    wipe: wipe_view::WipeView,
}

impl DataRecovererApp {
    fn new(ctx: &egui::Context) -> Self {
        Self {
            volumes: disk::list_volumes(),
            scan_source_path: None,
            screen: Screen::Home,
            scan: None,
            selected: Vec::new(),
            browse_tab: BrowseTab::Date,
            tree_sel: TreeSel::All,
            expanded_years: HashSet::new(),
            expanded_folders: HashSet::new(),
            open_category: None,
            previews: preview::PreviewLoader::new(ctx),
            message: None,
            cleanup: cleanup::CleanupScreen::new(),
            health: health_view::HealthView::new(),
            imaging: imaging_view::ImagingView::new(),
            partitions: partitions_view::PartitionsView::new(),
            surface: surface_view::SurfaceView::new(),
            speed: speed_view::SpeedView::new(),
            wipe: wipe_view::WipeView::new(),
        }
    }
}

impl DataRecovererApp {
    /// Starts a scan of `path` (a volume or image) showing the deleted files
    /// in `scope`.
    fn start_scan(&mut self, path: String, label: String, scope: Scope, ctx: &egui::Context) {
        self.start_scan_at(path, None, label, scope, ctx);
    }

    /// [`Self::start_scan`] of only the file system that starts at byte
    /// `start` of `path`, when given.
    fn start_scan_at(&mut self, path: String, start: Option<u64>, label: String, scope: Scope, ctx: &egui::Context) {
        let cancel = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(ScanProgress {
            records_scanned: 0,
            total_records: 0,
        }));
        let found = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = channel();

        let thread_cancel = cancel.clone();
        let thread_paused = paused.clone();
        let thread_progress = progress.clone();
        let thread_found = found.clone();
        let thread_ctx = ctx.clone();
        let thread_path = path.clone();

        std::thread::spawn(move || {
            // The scanner reports every deleted file into `raw`; the collector
            // moves those in `scope` into the results on each progress report.
            let raw = Arc::new(Mutex::new(Vec::new()));
            let mut collector = scope::Collector::new(scope, thread_path.clone());
            let result = ntfs::scan_drive_at(&thread_path, start, &thread_cancel, &raw, |p| {
                // The scan reports progress frequently; block here while paused
                // so the worker stops touching the disk until resumed.
                while thread_paused.load(Ordering::Relaxed)
                    && !thread_cancel.load(Ordering::Relaxed)
                {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
                collector.drain(&mut raw.lock().unwrap(), &mut thread_found.lock().unwrap());
                *thread_progress.lock().unwrap() = p;
                thread_ctx.request_repaint();
            });
            {
                let mut found = thread_found.lock().unwrap();
                collector.drain(&mut raw.lock().unwrap(), &mut found);
                collector.finish(&mut found);
            }
            let _ = tx.send(result);
            thread_ctx.request_repaint();
        });

        self.message = None;
        self.selected.clear();
        self.open_category = None;
        self.previews.reset();
        self.tree_sel = TreeSel::All;
        self.browse_tab = BrowseTab::Date;
        self.expanded_years.clear();
        self.expanded_folders.clear();
        self.scan_source_path = Some(path);
        self.scan = Some(ScanHandle {
            source_label: label,
            cancel,
            paused,
            progress,
            found,
            done_rx: rx,
            started_at: Instant::now(),
            finished_after: None,
            finished: false,
        });
        self.screen = Screen::Browse;
    }

    /// Scans volume `i` when its file system is supported.
    fn scan_volume(&mut self, i: usize, label: String, scope: Scope, ctx: &egui::Context) {
        let vol = &self.volumes[i];
        let fs = vol.filesystem.to_uppercase();
        if !matches!(fs.as_str(), "" | "NTFS" | "FAT" | "FAT32" | "EXFAT") {
            self.message = Some((
                format!(
                    "{} gebruikt {}. Ondersteund worden NTFS, FAT (12/16/32) en exFAT.",
                    vol.display_name(),
                    vol.filesystem
                ),
                false,
            ));
            return;
        }
        let path = vol.scan_path.clone();
        self.start_scan(path, label, scope, ctx);
    }

    /// Scans the drive of `folder`, showing only the deleted files that were
    /// in that folder or below.
    fn scan_folder(&mut self, folder: &Path, label: String, ctx: &egui::Context) {
        let text = folder.to_string_lossy();
        let volume = text
            .get(..2)
            .and_then(|letter| self.volumes.iter().position(|v| v.letter.eq_ignore_ascii_case(letter)));
        let Some(i) = volume else {
            self.message = Some((format!("{} staat niet op een schijf van deze computer.", folder.display()), false));
            return;
        };
        let on_volume = text[2..].trim_end_matches('\\');
        let scope = if on_volume.is_empty() { Scope::Drive } else { Scope::Folder(on_volume.to_string()) };
        self.scan_volume(i, label, scope, ctx);
    }

    /// Picks up the scan thread's result once it finishes.
    fn poll_scan(&mut self) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        if scan.finished {
            return;
        }
        if let Ok(result) = scan.done_rx.try_recv() {
            scan.finished = true;
            scan.finished_after = Some(scan.started_at.elapsed().as_secs());
            if let Err(e) = result {
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

    fn leave_browse(&mut self) {
        if let Some(scan) = &self.scan {
            scan.cancel.store(true, Ordering::Relaxed);
        }
        self.screen = Screen::Home;
        self.scan = None;
        self.message = None;
    }

    /// Whether file `rf` is part of the current view (tree filter, opened
    /// category, or everything).
    fn in_view(&self, rf: &RecoveredFile) -> bool {
        if self.tree_sel != TreeSel::All {
            self.tree_sel.matches(rf)
        } else if let Some(cat) = self.open_category {
            ntfs::category_for_name(&rf.name) == cat
        } else {
            true
        }
    }

    fn view_title(&self) -> String {
        if self.tree_sel != TreeSel::All {
            self.tree_sel.describe()
        } else if let Some(cat) = self.open_category {
            cat.to_string()
        } else {
            "Alle bestanden".to_string()
        }
    }

    // -----------------------------------------------------------------------
    // Sidebar
    // -----------------------------------------------------------------------

    fn draw_sidebar_home(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui::logo(ui);
        ui.add_space(6.0);
        ui::sidebar_caption(ui, "Herstel");
        let home = matches!(self.screen, Screen::Home);
        if ui::nav_item(ui, icon::HARD_DRIVES, "Schijven & locaties", home).clicked() {
            self.screen = Screen::Home;
        }
        ui.add_space(6.0);
        ui::sidebar_caption(ui, "Opruimen");
        let cleanup = matches!(self.screen, Screen::Cleanup);
        if ui::nav_item(ui, icon::BROOM, "Schijf opruimen", cleanup).clicked() {
            self.screen = Screen::Cleanup;
        }
        ui.add_space(6.0);
        ui::sidebar_caption(ui, "Schijftools");
        let health = matches!(self.screen, Screen::Health);
        if ui::nav_item(ui, icon::HEARTBEAT, "Schijfgezondheid", health).clicked() {
            self.screen = Screen::Health;
        }
        let surface = matches!(self.screen, Screen::Surface);
        if ui::nav_item(ui, icon::PULSE, "Oppervlaktetest", surface).clicked() {
            self.screen = Screen::Surface;
        }
        let speed = matches!(self.screen, Screen::Speed);
        if ui::nav_item(ui, icon::SPEEDOMETER, "Snelheidstest", speed).clicked() {
            self.screen = Screen::Speed;
        }
        let partitions = matches!(self.screen, Screen::Partitions);
        if ui::nav_item(ui, icon::SQUARE_SPLIT_HORIZONTAL, "Partities", partitions).clicked() {
            self.screen = Screen::Partitions;
        }
        let imaging = matches!(self.screen, Screen::Imaging);
        if ui::nav_item(ui, icon::DISC, "Schijfimage maken", imaging).clicked() {
            self.screen = Screen::Imaging;
        }
        let wipe = matches!(self.screen, Screen::Wipe);
        if ui::nav_item(ui, icon::ERASER, "Veilig wissen", wipe).clicked() {
            self.screen = Screen::Wipe;
        }
    }

    /// Left panel in Browse mode: the "Opnamedatum" / "Oorspronkelijke map"
    /// tabs and a tree that filters the file grid.
    fn draw_browse_tree(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui::logo(ui);
        ui.add_space(4.0);

        let tab = self.browse_tab;
        let Some(scan) = self.scan.as_ref() else {
            return;
        };

        // Aggregate counts up front, then release the lock before rendering.
        let mut years: BTreeMap<u16, (usize, BTreeMap<u8, usize>)> = BTreeMap::new();
        let mut unknown_date = 0usize;
        let mut folder_files: BTreeMap<String, usize> = BTreeMap::new();
        let total;
        {
            let files = scan.found.lock().unwrap();
            total = files.len();
            for rf in files.iter() {
                match tab {
                    BrowseTab::Date => {
                        if rf.year == 0 {
                            unknown_date += 1;
                        } else {
                            let e = years.entry(rf.year).or_default();
                            e.0 += 1;
                            *e.1.entry(rf.month).or_default() += 1;
                        }
                    }
                    BrowseTab::Folder => {
                        *folder_files.entry(rf.path.clone()).or_default() += 1;
                    }
                }
            }
        }
        let folders = FolderTree::build(&folder_files);

        let mut new_tab: Option<BrowseTab> = None;
        let mut new_sel: Option<TreeSel> = None;
        let mut toggle_year: Option<u16> = None;
        let mut toggle_folder: Option<String> = None;
        let current = self.tree_sel.clone();

        egui::Frame::none()
            .inner_margin(egui::Margin::symmetric(12.0, 0.0))
            .show(ui, |ui| {
                let active = if tab == BrowseTab::Date { 0 } else { 1 };
                let tabs = [
                    (icon::CALENDAR_BLANK, "Datum", "Groeperen op opnamedatum (laatst gewijzigd)"),
                    (icon::FOLDER, "Mappen", "Groeperen op oorspronkelijke map"),
                ];
                if let Some(i) = ui::segmented_tabs(ui, tabs, active) {
                    new_tab = Some(if i == 0 { BrowseTab::Date } else { BrowseTab::Folder });
                }
                ui.add_space(10.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let all = current == TreeSel::All;
                        if ui::tree_row(ui, ARROW_W, None, Some(icon::STACK), "Alle data", total, all).0 {
                            new_sel = Some(TreeSel::All);
                        }
                        match tab {
                            BrowseTab::Date => {
                                for (&y, (ycount, months)) in years.iter().rev() {
                                    let is_exp = self.expanded_years.contains(&y);
                                    let (clicked, arrow) = ui::tree_row(
                                        ui,
                                        0.0,
                                        Some(is_exp),
                                        Some(icon::CALENDAR_BLANK),
                                        &y.to_string(),
                                        *ycount,
                                        current == TreeSel::Year(y),
                                    );
                                    if arrow {
                                        toggle_year = Some(y);
                                    } else if clicked {
                                        new_sel = Some(TreeSel::Year(y));
                                    }
                                    if is_exp {
                                        for (&m, &mc) in months.iter().rev() {
                                            let label = MONTHS_NL
                                                .get(m.wrapping_sub(1) as usize)
                                                .copied()
                                                .unwrap_or("?");
                                            let sel = current == TreeSel::YearMonth(y, m);
                                            if ui::tree_row(ui, ARROW_W + 22.0, None, None, label, mc, sel).0 {
                                                new_sel = Some(TreeSel::YearMonth(y, m));
                                            }
                                        }
                                    }
                                }
                                if unknown_date > 0 {
                                    let sel = current == TreeSel::Year(0);
                                    let row = ui::tree_row(
                                        ui,
                                        ARROW_W,
                                        None,
                                        Some(icon::CLOCK),
                                        "Onbekende datum",
                                        unknown_date,
                                        sel,
                                    );
                                    if row.0 {
                                        new_sel = Some(TreeSel::Year(0));
                                    }
                                }
                            }
                            BrowseTab::Folder => {
                                if let Some(&n) = folder_files.get("\\") {
                                    let sel = current == TreeSel::RootFiles;
                                    let row = ui::tree_row(
                                        ui,
                                        ARROW_W,
                                        None,
                                        Some(icon::FOLDER_SIMPLE),
                                        "Hoofdmap (losse bestanden)",
                                        n,
                                        sel,
                                    );
                                    if row.0 {
                                        new_sel = Some(TreeSel::RootFiles);
                                    }
                                }
                                folders.render(
                                    ui,
                                    "\\",
                                    0,
                                    &self.expanded_folders,
                                    &current,
                                    &mut new_sel,
                                    &mut toggle_folder,
                                );
                            }
                        }
                    });
            });

        if let Some(t) = new_tab {
            if t != self.browse_tab {
                // A date filter means nothing in the folder view and vice
                // versa, so start the other tab from "Alle data".
                self.browse_tab = t;
                self.tree_sel = TreeSel::All;
                self.open_category = None;
            }
        }
        if let Some(y) = toggle_year {
            if !self.expanded_years.remove(&y) {
                self.expanded_years.insert(y);
            }
        }
        if let Some(p) = toggle_folder {
            if !self.expanded_folders.remove(&p) {
                self.expanded_folders.insert(p);
            }
        }
        if let Some(sel) = new_sel {
            self.tree_sel = sel;
            self.open_category = None;
        }
    }

    // -----------------------------------------------------------------------
    // Home
    // -----------------------------------------------------------------------

    fn draw_home(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.heading("Kies een locatie om het herstel te starten");
                ui.label(
                    egui::RichText::new("Selecteer een schijf of image om verwijderde bestanden te zoeken.")
                        .color(theme::TEXT_DIM),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if ui::icon_button(ui, icon::ARROW_CLOCKWISE, "Opnieuw zoeken naar schijven").clicked() {
                    self.volumes = disk::list_volumes();
                }
            });
        });
        ui.add_space(14.0);

        if let Some((text, success)) = self.message.clone() {
            let (glyph, color) = if success {
                (icon::CHECK_CIRCLE, theme::SUCCESS)
            } else {
                (icon::INFO, theme::ACCENT)
            };
            egui::Frame::none()
                .fill(theme::tint(color, 0.10))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(glyph).font(theme::icon(16.0)).color(color));
                        ui.label(egui::RichText::new(text).color(theme::TEXT_MAIN));
                    });
                });
            ui.add_space(14.0);
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(14.0, 14.0);
            let internal: Vec<usize> = (0..self.volumes.len())
                .filter(|&i| self.volumes[i].kind == DriveKind::Internal)
                .collect();
            let external: Vec<usize> = (0..self.volumes.len())
                .filter(|&i| self.volumes[i].kind == DriveKind::External)
                .collect();
            let mut to_scan: Option<usize> = None;
            let avail = ui.available_width() - 8.0;
            let (_, drive_w) = ui::grid_columns(avail, 320.0, 14.0);
            let (_, tile_w) = ui::grid_columns(avail, 280.0, 14.0);

            ui::section_header(ui, icon::HARD_DRIVES, "Harde schijven", Some(internal.len()));
            if internal.is_empty() {
                ui::empty_card(ui, icon::INFO, "Geen interne schijven gevonden. Start de app als administrator.");
            } else {
                ui.horizontal_wrapped(|ui| {
                    for &i in &internal {
                        if ui::volume_card(ui, drive_w, &self.volumes[i]) {
                            to_scan = Some(i);
                        }
                    }
                });
            }
            ui.add_space(8.0);

            ui::section_header(ui, icon::USB, "Externe schijven", Some(external.len()));
            if external.is_empty() {
                ui::empty_card(ui, icon::USB, "Geen externe schijven aangesloten.");
            } else {
                ui.horizontal_wrapped(|ui| {
                    for &i in &external {
                        if ui::volume_card(ui, drive_w, &self.volumes[i]) {
                            to_scan = Some(i);
                        }
                    }
                });
            }
            ui.add_space(8.0);

            ui::section_header(ui, icon::LIGHTNING, "Snelle toegang", None);
            let mut quick: Option<QuickAction> = None;
            ui.horizontal_wrapped(|ui| {
                if ui::quick_tile(ui, tile_w, icon::DISC, "Schijfimage", "Een .img- of .dd-bestand scannen", true) {
                    quick = Some(QuickAction::DiskImage);
                }
                if ui::quick_tile(ui, tile_w, icon::DESKTOP, "Desktop", "Verwijderd van het bureaublad", true) {
                    quick = Some(QuickAction::Desktop);
                }
                if ui::quick_tile(ui, tile_w, icon::FOLDER_OPEN, "Map selecteren", "Eén map doorzoeken", true) {
                    quick = Some(QuickAction::Folder);
                }
                if ui::quick_tile(ui, tile_w, icon::TRASH, "Prullenbak", "Geleegde prullenbak", true) {
                    quick = Some(QuickAction::RecycleBin);
                }
            });

            if let Some(i) = to_scan {
                let label = self.volumes[i].display_name();
                self.scan_volume(i, label, Scope::Drive, ctx);
            } else if let Some(action) = quick {
                self.handle_quick_action(action, ctx);
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
                    self.start_scan(path.to_string_lossy().into_owned(), label, Scope::Drive, ctx);
                }
            }
            QuickAction::Desktop => match disk::desktop_folder() {
                Some(dir) => self.scan_folder(&dir, "Bureaublad".to_string(), ctx),
                None => self.message = Some(("De bureaubladmap is niet gevonden.".to_string(), false)),
            },
            QuickAction::Folder => {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let label = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
                    self.scan_folder(&dir, label, ctx);
                }
            }
            QuickAction::RecycleBin => {
                // Each drive has its own Recycle Bin; this tile looks at the
                // one of the Windows drive. Scanning a whole drive also shows
                // files from its emptied bin, under their own names.
                let letter = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
                match self.volumes.iter().position(|v| v.letter.eq_ignore_ascii_case(&letter)) {
                    Some(i) => self.scan_volume(i, format!("Prullenbak ({letter})"), Scope::RecycleBin, ctx),
                    None => {
                        self.message = Some((
                            format!("Schijf {letter} niet gevonden. Start Data Recoverer als administrator."),
                            false,
                        ))
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Browse
    // -----------------------------------------------------------------------

    fn draw_browse(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        let cancel = scan.cancel.clone();
        let paused_flag = scan.paused.clone();
        let source_label = scan.source_label.clone();
        let finished = scan.finished;
        let elapsed_secs = scan
            .finished_after
            .unwrap_or_else(|| scan.started_at.elapsed().as_secs());
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
            self.selected.resize(found_snapshot.len(), false);
        }
        let total_found_size: u64 = found_snapshot.iter().map(|f| f.size).sum();
        let is_paused = paused_flag.load(Ordering::Relaxed);

        // Header: back + breadcrumb.
        let mut go_back = false;
        let mut go_root = false;
        ui.horizontal(|ui| {
            let filtered = self.tree_sel != TreeSel::All || self.open_category.is_some();
            let tip = if filtered { "Terug naar alle bestanden" } else { "Terug naar schijven" };
            if ui::icon_button(ui, icon::ARROW_LEFT, tip).clicked() {
                go_back = true;
            }
            ui.add_space(4.0);
            let crumb = ui.add(
                egui::Label::new(
                    egui::RichText::new(&source_label)
                        .font(theme::font_semibold(20.0))
                        .color(if filtered { theme::TEXT_DIM } else { theme::TEXT_MAIN }),
                )
                .sense(egui::Sense::click()),
            );
            if filtered {
                if crumb.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    go_root = true;
                }
                ui.label(egui::RichText::new(icon::CARET_RIGHT).font(theme::icon(16.0)).color(theme::TEXT_FAINT));
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(self.view_title()).font(theme::font_semibold(20.0)),
                    )
                    .truncate(),
                );
            }
        });
        if go_back {
            if self.tree_sel != TreeSel::All || self.open_category.is_some() {
                go_root = true;
            } else {
                self.leave_browse();
                return;
            }
        }
        if go_root {
            self.tree_sel = TreeSel::All;
            self.open_category = None;
        }
        ui.add_space(12.0);

        // Status card.
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui::progress_ring(ui, 54.0, fraction, finished);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let title = if finished {
                        "Scan voltooid"
                    } else if is_paused {
                        "Gepauzeerd"
                    } else {
                        "Bezig met scannen…"
                    };
                    ui.label(egui::RichText::new(title).font(theme::font_semibold(16.0)));
                    ui.label(
                        egui::RichText::new(format!(
                            "{} gevonden · {} · {} van {} records · {}",
                            ui::fmt_files(found_snapshot.len()),
                            disk::format_size(total_found_size),
                            ui::fmt_thousands(records_scanned),
                            ui::fmt_thousands(total_records),
                            ui::fmt_duration(elapsed_secs),
                        ))
                        .color(theme::TEXT_DIM),
                    );
                    if !finished {
                        let current = found_snapshot.last().map(|f| f.name.as_str()).unwrap_or("…");
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("Laatst gevonden: {current}"))
                                    .size(12.0)
                                    .color(theme::TEXT_FAINT),
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
                        let (glyph, tip) = if is_paused {
                            (icon::PLAY, "Hervatten")
                        } else {
                            (icon::PAUSE, "Pauzeren")
                        };
                        if ui::icon_button(ui, glyph, tip).clicked() {
                            paused_flag.store(!is_paused, Ordering::Relaxed);
                        }
                    });
                }
            });
            if !finished {
                ui.add_space(10.0);
                let (bar, _) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
                ui::progress_bar(ui, bar, fraction, theme::ACCENT);
            }
        });
        ui.add_space(12.0);

        // Toolbar: select all in view.
        let visible: Vec<usize> = (0..found_snapshot.len())
            .filter(|&i| self.in_view(&found_snapshot[i]))
            .collect();
        if !visible.is_empty() {
            ui.horizontal(|ui| {
                let all_on = visible.iter().all(|&i| self.selected[i]);
                if ui::checkbox_label(ui, all_on, "Alles selecteren") {
                    for &i in &visible {
                        self.selected[i] = !all_on;
                    }
                }
                ui.label(
                    egui::RichText::new(format!("{} in deze weergave", ui::fmt_files(visible.len())))
                        .color(theme::TEXT_DIM),
                );
            });
            ui.add_space(6.0);
        }

        // Results grid.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(14.0, 14.0);
                if found_snapshot.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        if !finished {
                            ui.add(egui::Spinner::new().size(28.0).color(theme::ACCENT));
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new("Zoeken naar verwijderde bestanden…").color(theme::TEXT_DIM));
                        } else if total_records == 0 {
                            ui.label(egui::RichText::new(icon::WARNING_CIRCLE).font(theme::icon(34.0)).color(theme::TEXT_FAINT));
                            ui.label(
                                egui::RichText::new(
                                    "Geen ondersteund bestandssysteem gevonden. \
                                     Ondersteund worden NTFS, FAT (12/16/32) en exFAT.",
                                )
                                .color(theme::TEXT_DIM),
                            );
                        } else {
                            ui.label(egui::RichText::new(icon::CHECK_CIRCLE).font(theme::icon(34.0)).color(theme::TEXT_FAINT));
                            ui.label(egui::RichText::new("Geen verwijderde bestanden gevonden.").color(theme::TEXT_DIM));
                        }
                    });
                } else if self.tree_sel == TreeSel::All && self.open_category.is_none() {
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
                    categories.sort_by(|a, b| b.1.cmp(&a.1));
                    let mut clicked_category = None;
                    let (_, width) = ui::grid_columns(ui.available_width() - 8.0, 250.0, 14.0);
                    ui.horizontal_wrapped(|ui| {
                        for (name, count, size) in &categories {
                            if ui::category_card(ui, width, name, *count, *size) {
                                clicked_category = Some(*name);
                            }
                        }
                    });
                    if let Some(name) = clicked_category {
                        self.open_category = Some(name);
                    }
                } else {
                    let source = self.scan_source_path.clone().unwrap_or_default();
                    let (_, width) = ui::grid_columns(ui.available_width() - 8.0, 160.0, 14.0);
                    self.previews.begin_frame(ui.ctx());
                    ui.horizontal_wrapped(|ui| {
                        for &i in &visible {
                            let found = &found_snapshot[i];
                            let is_image = ntfs::category_for_name(&found.name) == "Fotos";
                            let thumb = if is_image { self.previews.thumb(i) } else { preview::Thumb::None };
                            let on_screen = ui::file_card(ui, width, &mut self.selected[i], found, thumb);
                            if is_image && on_screen {
                                self.previews.request(i, &source, found);
                            }
                        }
                    });
                }
            });
    }

    /// Bottom bar in Browse mode: selection summary and the "Herstellen" action.
    fn draw_action_bar(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        let (count, bytes) = {
            let files = scan.found.lock().unwrap();
            files
                .iter()
                .enumerate()
                .filter(|(i, _)| self.selected.get(*i).copied().unwrap_or(false))
                .fold((0usize, 0u64), |(c, b), (_, f)| (c + 1, b + f.size))
        };

        let mut recover = false;
        ui.horizontal_centered(|ui| {
            if count == 0 {
                ui.label(egui::RichText::new("Selecteer bestanden om te herstellen").color(theme::TEXT_DIM));
            } else {
                ui.label(
                    egui::RichText::new(format!("{} geselecteerd", ui::fmt_thousands(count as u64)))
                        .font(theme::font_semibold(14.0)),
                );
                ui.label(egui::RichText::new(disk::format_size(bytes)).color(theme::TEXT_DIM));
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
                let button = ui::primary_icon_button(icon::DOWNLOAD_SIMPLE, "Herstellen");
                if ui
                    .add_enabled(count > 0, button.min_size(egui::vec2(140.0, 38.0)))
                    .clicked()
                {
                    recover = true;
                }
            });
        });

        if recover {
            let files = scan.found.lock().unwrap().clone();
            self.recover_selected(&files);
        }
    }

    fn recover_selected(&mut self, found_files: &[RecoveredFile]) {
        let Some(drive_path) = self.scan_source_path.clone() else {
            return;
        };

        let Some(out_dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };

        let mut count = 0usize;
        for (i, found) in found_files.iter().enumerate() {
            if !self.selected.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let Err(e) = ntfs::recover_file(&drive_path, found, &out_dir) {
                self.message = Some((format!("Herstel van {} mislukt: {e}", found.name), false));
                return;
            }
            count += 1;
        }
        self.message = Some((
            format!("{count} bestand(en) hersteld naar {}", out_dir.display()),
            true,
        ));
    }
}

#[derive(Clone, Copy)]
enum QuickAction {
    DiskImage,
    Desktop,
    Folder,
    RecycleBin,
}

impl eframe::App for DataRecovererApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scan();

        // Drag the right edge to resize; the width is remembered between runs.
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(260.0)
            .width_range(236.0..=520.0)
            .frame(
                egui::Frame::none()
                    .fill(theme::SIDEBAR_BG)
                    .stroke(egui::Stroke::new(1.0_f32, theme::CARD_BORDER)),
            )
            .show(ctx, |ui| match self.screen {
                Screen::Home | Screen::Cleanup | Screen::Health | Screen::Surface | Screen::Speed | Screen::Partitions | Screen::Imaging | Screen::Wipe => self.draw_sidebar_home(ui),
                Screen::Browse => self.draw_browse_tree(ui),
            });

        if matches!(self.screen, Screen::Browse) {
            egui::TopBottomPanel::bottom("actions")
                .exact_height(64.0)
                .frame(
                    egui::Frame::none()
                        .fill(theme::CARD_BG)
                        .stroke(egui::Stroke::new(1.0_f32, theme::CARD_BORDER))
                        .inner_margin(egui::Margin::symmetric(24.0, 0.0)),
                )
                .show(ctx, |ui| self.draw_action_bar(ui));
        }
        if matches!(self.screen, Screen::Cleanup) && self.cleanup.has_action_bar() {
            egui::TopBottomPanel::bottom("cleanup_actions")
                .exact_height(64.0)
                .frame(
                    egui::Frame::none()
                        .fill(theme::CARD_BG)
                        .stroke(egui::Stroke::new(1.0_f32, theme::CARD_BORDER))
                        .inner_margin(egui::Margin::symmetric(24.0, 0.0)),
                )
                .show(ctx, |ui| self.cleanup.draw_action_bar(ui));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::APP_BG)
                    .inner_margin(egui::Margin::symmetric(28.0, 24.0)),
            )
            .show(ctx, |ui| match self.screen {
                Screen::Home => self.draw_home(ui, ctx),
                Screen::Browse => self.draw_browse(ui),
                Screen::Cleanup => self.cleanup.draw(ui, &self.volumes),
                Screen::Health => self.health.draw(ui, &self.volumes),
                Screen::Surface => self.surface.draw(ui),
                Screen::Speed => self.speed.draw(ui, &self.volumes),
                Screen::Wipe => self.wipe.draw(ui, &self.volumes),
                Screen::Partitions => {
                    if let Some(req) = self.partitions.draw(ui, &self.volumes) {
                        self.start_scan_at(req.path, Some(req.start), req.label, Scope::Drive, ctx);
                    }
                    if self.partitions.take_changed() {
                        self.volumes = disk::list_volumes();
                    }
                }
                Screen::Imaging => {
                    if let Some((path, label)) = self.imaging.draw(ui, &self.volumes) {
                        self.start_scan(path, label, Scope::Drive, ctx);
                    }
                }
            });
    }
}

/// Nested folder tree built from each file's original folder, with counts
/// that include all subfolders.
struct FolderTree {
    totals: HashMap<String, usize>,
    children: BTreeMap<String, BTreeSet<String>>,
}

impl FolderTree {
    fn build(files_per_folder: &BTreeMap<String, usize>) -> Self {
        let mut totals: HashMap<String, usize> = HashMap::new();
        let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (path, &count) in files_per_folder {
            let mut cur = path.clone();
            loop {
                *totals.entry(cur.clone()).or_default() += count;
                if cur == "\\" || cur.is_empty() {
                    break;
                }
                let parent = match cur.rfind('\\') {
                    Some(0) | None => "\\".to_string(),
                    Some(i) => cur[..i].to_string(),
                };
                children.entry(parent.clone()).or_default().insert(cur.clone());
                cur = parent;
            }
        }
        Self { totals, children }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &self,
        ui: &mut egui::Ui,
        parent: &str,
        depth: usize,
        expanded: &HashSet<String>,
        current: &TreeSel,
        new_sel: &mut Option<TreeSel>,
        toggle: &mut Option<String>,
    ) {
        let Some(kids) = self.children.get(parent) else {
            return;
        };
        for path in kids {
            let name = path.rsplit('\\').next().filter(|s| !s.is_empty()).unwrap_or(path);
            let has_kids = self.children.contains_key(path);
            let is_exp = expanded.contains(path);
            let indent = depth as f32 * ARROW_W + if has_kids { 0.0 } else { ARROW_W };
            let count = self.totals.get(path).copied().unwrap_or(0);
            let selected = matches!(current, TreeSel::Folder(p) if p == path);
            let glyph = if has_kids && is_exp { icon::FOLDER_OPEN } else { icon::FOLDER };
            let (clicked, arrow) =
                ui::tree_row(ui, indent, has_kids.then_some(is_exp), Some(glyph), name, count, selected);
            if arrow {
                *toggle = Some(path.clone());
            } else if clicked {
                *new_sel = Some(TreeSel::Folder(path.clone()));
            }
            if has_kids && is_exp {
                self.render(ui, path, depth + 1, expanded, current, new_sel, toggle);
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Data Recoverer")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([920.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Data Recoverer",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            let mut app = DataRecovererApp::new(&cc.egui_ctx);
            // `data-recoverer --scan <pad>` opent meteen een scan van een schijf
            // of image, zonder bestandsdialoog (handig om te testen).
            let args: Vec<String> = std::env::args().collect();
            if let Some(path) = args
                .iter()
                .position(|a| a == "--scan")
                .and_then(|i| args.get(i + 1))
            {
                let label = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path)
                    .to_string();
                app.start_scan(path.clone(), label, Scope::Drive, &cc.egui_ctx);
            }
            Ok(Box::new(app))
        }),
    )
}
