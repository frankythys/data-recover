#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod disk;
mod fat;
mod ntfs;
mod theme;

use disk::{DriveKind, VolumeInfo};
use ntfs::{RecoveredFile, ScanProgress};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

enum Screen {
    Home,
    Browse,
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

    /// Short Dutch description of the active filter, for the heading.
    fn describe(&self) -> String {
        match self {
            TreeSel::All => String::new(),
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
    /// Lazily decoded image thumbnails, keyed by file index. `None` means a
    /// decode was attempted and failed (clusters reused, not an image, …).
    previews: HashMap<usize, Option<egui::TextureHandle>>,
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
            browse_tab: BrowseTab::Date,
            tree_sel: TreeSel::All,
            expanded_years: HashSet::new(),
            expanded_folders: HashSet::new(),
            open_category: None,
            previews: HashMap::new(),
            message: None,
        }
    }
}

impl DataRecovererApp {
    fn start_scan(&mut self, path: String, label: String, ctx: &egui::Context) {
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
            let result = ntfs::scan_drive(&thread_path, &thread_cancel, &thread_found, |p| {
                // The scan reports progress frequently; block here while paused
                // so the worker stops touching the disk until resumed.
                while thread_paused.load(Ordering::Relaxed)
                    && !thread_cancel.load(Ordering::Relaxed)
                {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
                *thread_progress.lock().unwrap() = p;
                thread_ctx.request_repaint();
            });
            let _ = tx.send(result);
            thread_ctx.request_repaint();
        });

        self.message = None;
        self.selected.clear();
        self.open_category = None;
        self.previews.clear();
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
            finished: false,
        });
        self.screen = Screen::Browse;
    }

    fn draw_sidebar(&mut self, ui: &mut egui::Ui) {
        draw_logo(ui);

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

    /// Left panel in Browse mode: the "Opnamedatum" / "Oorspronkelijke map"
    /// tabs and a tree that filters the file grid. Rows are painted at the
    /// panel's current width, so nothing overflows when the panel is narrow.
    fn draw_browse_tree(&mut self, ui: &mut egui::Ui) {
        draw_logo(ui);
        ui.add_space(14.0);

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
            .inner_margin(egui::Margin::symmetric(10.0, 0.0))
            .show(ui, |ui| {
                let active = if tab == BrowseTab::Date { 0 } else { 1 };
                if let Some(i) = segmented_tabs(ui, ["Opnamedatum", "Oorspronkelijke map"], active) {
                    new_tab = Some(if i == 0 { BrowseTab::Date } else { BrowseTab::Folder });
                }
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if tree_row(ui, ARROW_W, None, "Alle data", total, current == TreeSel::All).0 {
                            new_sel = Some(TreeSel::All);
                        }
                        match tab {
                            BrowseTab::Date => {
                                for (&y, (ycount, months)) in years.iter().rev() {
                                    let is_exp = self.expanded_years.contains(&y);
                                    let (clicked, arrow) = tree_row(
                                        ui,
                                        0.0,
                                        Some(is_exp),
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
                                            if tree_row(ui, 2.0 * ARROW_W, None, label, mc, sel).0 {
                                                new_sel = Some(TreeSel::YearMonth(y, m));
                                            }
                                        }
                                    }
                                }
                                if unknown_date > 0 {
                                    let sel = current == TreeSel::Year(0);
                                    if tree_row(ui, ARROW_W, None, "Onbekende datum", unknown_date, sel).0
                                    {
                                        new_sel = Some(TreeSel::Year(0));
                                    }
                                }
                            }
                            BrowseTab::Folder => {
                                if let Some(&n) = folder_files.get("\\") {
                                    let sel = current == TreeSel::RootFiles;
                                    if tree_row(ui, ARROW_W, None, "Hoofdmap (losse bestanden)", n, sel).0 {
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
                if draw_quick_tile(ui, "\u{1F4BF}", "Schijfimage") {
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
                let fs = vol.filesystem.to_uppercase();
                let supported =
                    matches!(fs.as_str(), "" | "NTFS" | "FAT" | "FAT32" | "EXFAT");
                if !supported {
                    self.message = Some((
                        format!(
                            "{} gebruikt {}. Ondersteund worden NTFS, FAT (12/16/32) en exFAT.",
                            vol.display_name(),
                            vol.filesystem
                        ),
                        false,
                    ));
                } else {
                    let path = vol.scan_path.clone();
                    let label = vol.display_name();
                    self.start_scan(path, label, ctx);
                }
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
        let paused_flag = scan.paused.clone();
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
            if self.tree_sel != TreeSel::All {
                let shown = found_snapshot.iter().filter(|f| self.tree_sel.matches(f)).count();
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("· {}: {shown}", self.tree_sel.describe()))
                            .size(14.0)
                            .color(theme::TEXT_DIM),
                    )
                    .truncate(),
                );
            }
        });
        if self.scan.is_none() {
            return;
        }
        ui.add_space(12.0);

        let status_height = 110.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .max_height((ui.available_height() - status_height).max(0.0))
            .show(ui, |ui| {
                if found_snapshot.is_empty() {
                    ui.colored_label(theme::TEXT_DIM, "Nog geen bestanden gevonden…");
                } else if self.tree_sel != TreeSel::All {
                    // A tree node is selected: show its files as a flat grid.
                    let sel = self.tree_sel.clone();
                    let source = self.scan_source_path.clone().unwrap_or_default();
                    ui.horizontal_wrapped(|ui| {
                        for (i, found) in found_snapshot.iter().enumerate() {
                            if !sel.matches(found) {
                                continue;
                            }
                            let is_images = ntfs::category_for_name(&found.name) == "Fotos";
                            let preview = if is_images {
                                if !self.previews.contains_key(&i) {
                                    let tex = load_preview(ui.ctx(), &source, found, i);
                                    self.previews.insert(i, tex);
                                }
                                self.previews.get(&i).and_then(|t| t.clone())
                            } else {
                                None
                            };
                            draw_file_card(
                                ui,
                                &mut self.selected[i],
                                found,
                                preview.as_ref(),
                                is_images,
                            );
                        }
                    });
                } else if let Some(category) = self.open_category {
                    if ui.add(theme::outline_button(&format!("← {category}"))).clicked() {
                        self.open_category = None;
                    }
                    ui.add_space(8.0);
                    let is_images = category == "Fotos";
                    let source = self.scan_source_path.clone().unwrap_or_default();
                    ui.horizontal_wrapped(|ui| {
                        for (i, found) in found_snapshot.iter().enumerate() {
                            if ntfs::category_for_name(&found.name) != category {
                                continue;
                            }
                            let preview = if is_images {
                                if !self.previews.contains_key(&i) {
                                    let tex = load_preview(ui.ctx(), &source, found, i);
                                    self.previews.insert(i, tex);
                                }
                                self.previews.get(&i).and_then(|t| t.clone())
                            } else {
                                None
                            };
                            draw_file_card(
                                ui,
                                &mut self.selected[i],
                                found,
                                preview.as_ref(),
                                is_images,
                            );
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
        let is_paused = paused_flag.load(Ordering::Relaxed);
        theme::card_frame().show(ui, |ui| {
            // Live ticker of the file currently being recovered.
            if !finished {
                let current = found_snapshot
                    .last()
                    .map(|f| f.name.as_str())
                    .unwrap_or("…");
                // Truncate to one clipped line so a long name never re-flows
                // the layout (which caused flicker during scanning).
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("Scannen: {current}")).color(theme::TEXT_DIM),
                    )
                    .truncate(),
                );
                ui.add_space(6.0);
            }
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
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let status_text = if finished {
                            "Scan voltooid"
                        } else if is_paused {
                            "Gepauzeerd"
                        } else {
                            "Bezig met scannen"
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
                                "{source_label} — {records_scanned}/{total_records} records — Duur: {elapsed_secs}s"
                            ),
                        );
                        if !finished {
                            let label = if is_paused { "▶ Hervatten" } else { "⏸ Pauze" };
                            if ui.add(theme::outline_button(label)).clicked() {
                                paused_flag.store(!is_paused, Ordering::Relaxed);
                            }
                            if ui.add(theme::outline_button("⏹ Stop")).clicked() {
                                cancel.store(true, Ordering::Relaxed);
                            }
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

        if finished && found_snapshot.is_empty() && total_records == 0 {
            ui.add_space(8.0);
            ui.colored_label(
                theme::TEXT_DIM,
                "Geen ondersteund bestandssysteem gevonden op deze bron. \
                 Ondersteund worden NTFS, FAT (12/16/32) en exFAT.",
            );
        }

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
        // Drag the right edge to resize; the width is remembered between runs.
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(260.0)
            .width_range(210.0..=520.0)
            .frame(egui::Frame::none().fill(theme::SIDEBAR_BG))
            .show(ctx, |ui| match self.screen {
                Screen::Home => self.draw_sidebar(ui),
                Screen::Browse => self.draw_browse_tree(ui),
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

/// Width reserved for a tree row's disclosure triangle (also the indent step).
const ARROW_W: f32 = 18.0;

/// App logo + name at the top of the sidebar.
fn draw_logo(ui: &mut egui::Ui) {
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
        ui.add(egui::Label::new(egui::RichText::new("Data Recoverer").size(16.0).strong()).truncate());
    });
}

/// Two-segment toggle that fills the available width. Labels wrap onto two
/// lines when the panel is narrow. Returns the clicked segment, if any.
fn segmented_tabs(ui: &mut egui::Ui, labels: [&str; 2], active: usize) -> Option<usize> {
    let width = ui.available_width();
    let half = width / 2.0;
    // Wrap between words only: if the longest word doesn't fit a segment,
    // shrink the font instead of breaking it mid-word.
    const BASE: f32 = 12.5;
    let inner = half - 12.0;
    let widest_word = labels
        .iter()
        .flat_map(|l| l.split(' '))
        .map(|w| {
            ui.painter()
                .layout_no_wrap(w.to_string(), egui::FontId::proportional(BASE), theme::TEXT_MAIN)
                .size()
                .x
        })
        .fold(0.0, f32::max);
    let size = if widest_word > inner {
        (BASE * inner / widest_word).max(9.5)
    } else {
        BASE
    };
    let font = egui::FontId::proportional(size);
    let text_h = labels
        .iter()
        .map(|l| {
            ui.painter()
                .layout(l.to_string(), font.clone(), theme::TEXT_MAIN, half - 12.0)
                .size()
                .y
        })
        .fold(0.0, f32::max);
    let height = (text_h + 16.0).max(34.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8.0, theme::APP_BG);

    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        let seg = egui::Rect::from_min_size(
            egui::pos2(rect.left() + i as f32 * half, rect.top()),
            egui::vec2(half, height),
        )
        .shrink(3.0);
        let resp = ui.interact(seg, ui.id().with(("browse_tab", i)), egui::Sense::click());
        let on = i == active;
        if on {
            painter.rect_filled(seg, 6.0, theme::ACCENT);
        } else if resp.hovered() {
            painter.rect_filled(seg, 6.0, theme::ACCENT_SOFT);
        }
        let color = if on { egui::Color32::WHITE } else { theme::TEXT_MAIN };
        let mut job = egui::text::LayoutJob::simple(label.to_string(), font.clone(), color, seg.width() - 6.0);
        job.halign = egui::Align::Center;
        let galley = painter.layout_job(job);
        painter.galley(
            egui::pos2(seg.center().x, seg.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        if resp.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// One tree row painted at the full available width: an optional disclosure
/// triangle (`arrow` = Some(expanded)), a label elided with "…" to fit, and a
/// right-aligned count. Returns (row clicked, triangle clicked).
fn tree_row(
    ui: &mut egui::Ui,
    indent: f32,
    arrow: Option<bool>,
    label: &str,
    count: usize,
    selected: bool,
) -> (bool, bool) {
    const ROW_H: f32 = 26.0;
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::click());
    let painter = ui.painter_at(rect);

    let bg = rect.shrink2(egui::vec2(0.0, 1.0));
    if selected {
        painter.rect_filled(bg, 6.0, theme::ACCENT_SOFT);
    } else if resp.hovered() {
        painter.rect_filled(bg, 6.0, theme::APP_BG);
    }

    let cy = rect.center().y;
    let mut x = rect.left() + 6.0 + indent;
    let mut arrow_clicked = false;
    if let Some(expanded) = arrow {
        let c = egui::pos2(x + ARROW_W / 2.0, cy);
        let s = 4.0;
        let pts = if expanded {
            vec![
                egui::pos2(c.x - s, c.y - s * 0.6),
                egui::pos2(c.x + s, c.y - s * 0.6),
                egui::pos2(c.x, c.y + s * 0.7),
            ]
        } else {
            vec![
                egui::pos2(c.x - s * 0.6, c.y - s),
                egui::pos2(c.x + s * 0.7, c.y),
                egui::pos2(c.x - s * 0.6, c.y + s),
            ]
        };
        painter.add(egui::Shape::convex_polygon(pts, theme::TEXT_DIM, egui::Stroke::NONE));
        if resp.clicked() {
            arrow_clicked = resp
                .interact_pointer_pos()
                .is_some_and(|p| p.x <= x + ARROW_W + 2.0);
        }
        x += ARROW_W;
    }

    let count_galley =
        painter.layout_no_wrap(count.to_string(), egui::FontId::proportional(12.0), theme::TEXT_DIM);
    let count_x = rect.right() - 8.0 - count_galley.size().x;
    painter.galley(
        egui::pos2(count_x, cy - count_galley.size().y / 2.0),
        count_galley,
        theme::TEXT_DIM,
    );

    let color = if selected { theme::ACCENT } else { theme::TEXT_MAIN };
    let font = egui::FontId::proportional(13.0);
    let max_w = (count_x - 8.0 - x).max(10.0);
    let elided = painter.layout_no_wrap(label.to_owned(), font.clone(), color).size().x > max_w;
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: max_w,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = painter.layout_job(job);
    painter.galley(egui::pos2(x, cy - galley.size().y / 2.0), galley, color);

    let row_clicked = resp.clicked() && !arrow_clicked;
    if elided {
        resp.on_hover_text(label);
    }
    (row_clicked, arrow_clicked)
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
            let (clicked, arrow) =
                tree_row(ui, indent, has_kids.then_some(is_exp), name, count, selected);
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

fn draw_file_card(
    ui: &mut egui::Ui,
    selected: &mut bool,
    found: &RecoveredFile,
    preview: Option<&egui::TextureHandle>,
    image_attempted: bool,
) {
    let extension = std::path::Path::new(&found.name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("?")
        .to_uppercase();

    // An image whose preview was attempted but failed: its data clusters
    // were most likely already reused, so it can't be shown or reliably
    // recovered. Mark it plainly instead of a badge that looks functional.
    let overwritten = image_attempted && preview.is_none();

    let card_size = egui::vec2(140.0, 146.0);
    ui.allocate_ui(card_size, |ui| {
        theme::card_frame().show(ui, |ui| {
            ui.set_width(108.0);
            ui.checkbox(selected, "");
            ui.vertical_centered(|ui| {
                if let Some(tex) = preview {
                    let src = egui::load::SizedTexture::new(tex.id(), tex.size_vec2());
                    ui.add(
                        egui::Image::new(src)
                            .max_size(egui::vec2(72.0, 56.0))
                            .rounding(egui::Rounding::same(6.0)),
                    );
                } else {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
                    let (bg, fg) = if overwritten {
                        (theme::TRACK_BG, theme::TEXT_DIM)
                    } else {
                        (theme::ACCENT_SOFT, theme::ACCENT)
                    };
                    ui.painter().rect_filled(rect, 8.0, bg);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        extension,
                        egui::FontId::proportional(13.0),
                        fg,
                    );
                }
                ui.add_space(4.0);
                ui.add(egui::Label::new(egui::RichText::new(&found.name).size(11.0)).truncate())
                    .on_hover_text(&found.name);
                ui.colored_label(theme::TEXT_DIM, disk::format_size(found.size));
                if overwritten {
                    ui.label(
                        egui::RichText::new("Geen voorbeeld")
                            .size(9.5)
                            .italics()
                            .color(theme::TEXT_DIM),
                    )
                    .on_hover_text("Data mogelijk al overschreven – herstel kan onvolledig zijn.");
                }
            });
        });
    });
}

/// Reads a recovered image into memory and uploads a small thumbnail texture.
/// Returns `None` when the file is too large, unreadable, or not a decodable
/// image (e.g. its clusters were already reused after deletion).
fn load_preview(
    ctx: &egui::Context,
    source: &str,
    rec: &RecoveredFile,
    idx: usize,
) -> Option<egui::TextureHandle> {
    const MAX_PREVIEW_BYTES: u64 = 16 * 1024 * 1024;
    if rec.size == 0 || rec.size > MAX_PREVIEW_BYTES {
        return None;
    }
    let bytes = ntfs::read_file_bytes(source, rec, MAX_PREVIEW_BYTES).ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    let thumb = image.thumbnail(80, 80).to_rgba8();
    let size = [thumb.width() as usize, thumb.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw());
    Some(ctx.load_texture(format!("preview_{idx}"), color, egui::TextureOptions::LINEAR))
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
            let mut app = DataRecovererApp::default();
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
                app.start_scan(path.clone(), label, &cc.egui_ctx);
            }
            Ok(Box::new(app))
        }),
    )
}
