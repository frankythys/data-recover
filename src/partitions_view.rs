//! "Partities" screen, laid out like a partition manager: a toolbar, an
//! operations panel with the pending operations, the volume table and the
//! disk map. Operations are queued and applied in one go (see
//! `partition_ops_view`); the unpartitioned space can be searched for lost
//! partitions, which can then be scanned for files.

use crate::i18n::t;
use crate::tf;
use crate::cleanup::{dialog, dialog_title, dim_background, notice};
use crate::disk::{self, VolumeInfo};
use crate::health::{self, DiskHealth};
use crate::partition_map::{self, DiskView, VolRow};
use crate::partition_ops::{Fs, Guard, Target};
use crate::partition_ops_view::{OpsPanel, Request, Selection, part_name};
use crate::partitions::{self, FoundPartition, Layout, SearchProgress, Style};
use crate::{theme, ui};
use egui::vec2;
use egui_phosphor::regular as icon;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

const SIDE_W: f32 = 300.0;
const MAP_DEFAULT_H: f32 = 220.0;

/// Something for the rest of the app to do.
pub enum Event {
    /// Scan for deleted files; `start` is a byte offset into `path` for a
    /// partition that is not a drive (lost or without letter).
    Scan { path: String, start: Option<u64>, label: String },
    /// Open "Schijfimage maken" with this drive letter as the source.
    Image(String),
    /// Open "Oppervlaktetest" with this disk selected.
    Surface(u32),
}

/// What an entry of the operations list or the right-click menu does.
enum Action {
    Op(Request),
    Explore(PathBuf),
    Search(u32),
    Event(Event),
}

/// An entry of the operations list.
struct OpItem {
    glyph: &'static str,
    text: &'static str,
    danger: bool,
    /// `Err` holds why it is not available.
    action: Result<Action, &'static str>,
}

struct SearchJob {
    disk: u32,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<SearchProgress>>,
    rx: Receiver<io::Result<Vec<FoundPartition>>>,
}

pub struct PartitionsView {
    layouts: Vec<Layout>,
    disks: HashMap<u32, DiskHealth>,
    rx: Option<Receiver<(Vec<Layout>, Vec<DiskHealth>)>>,
    loaded: bool,
    search: Option<SearchJob>,
    /// Finished search shown in a dialog: (disk, result).
    found: Option<(u32, Result<Vec<FoundPartition>, String>)>,
    selected: Option<Selection>,
    ops: OpsPanel,
    /// Set when partitions changed, so the app rereads its drive list.
    changed: bool,
    message: Option<String>,
}

impl PartitionsView {
    pub fn new() -> Self {
        Self {
            layouts: Vec::new(),
            disks: HashMap::new(),
            rx: None,
            loaded: false,
            search: None,
            found: None,
            selected: None,
            ops: OpsPanel::default(),
            changed: false,
            message: None,
        }
    }

    /// Selects partition `number` of `disk` (for `--screenshot`).
    pub fn select_partition(&mut self, disk: u32, number: u32) {
        self.selected = Some(Selection::Part { disk, number });
    }

    /// Whether partitions were changed since the last call.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    fn reload(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((partitions::read_layouts(), health::read_all()));
            ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.loaded = true;
    }

    fn poll(&mut self) {
        if let Some((layouts, disks)) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.layouts = layouts;
            self.disks = disks.into_iter().map(|d| (d.index, d)).collect();
            self.rx = None;
        }
        let Some(job) = &self.search else { return };
        if let Ok(result) = job.rx.try_recv() {
            let result = result.map_err(|e| match e.kind() {
                io::ErrorKind::PermissionDenied => t("Start DiskTools als administrator om de schijf te doorzoeken.", "Start DiskTools as administrator to search the disk.").to_string(),
                io::ErrorKind::Interrupted => t("Zoeken gestopt.", "Search stopped.").to_string(),
                _ => tf!("Zoeken mislukt: {e}", "Search failed: {e}"),
            });
            self.found = Some((job.disk, result));
            self.search = None;
        }
    }

    fn start_search(&mut self, index: u32, ctx: &egui::Context) {
        let Some(layout) = self.layouts.iter().find(|l| l.index == index) else { return };
        let (size, gaps) = (layout.size, layout.gaps());
        if gaps.is_empty() {
            self.message = Some(t("Alle ruimte op deze schijf is ingedeeld, dus er kunnen geen verloren partities zijn.", "All space on this disk is partitioned, so there can be no lost partitions.").to_string());
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(SearchProgress::default()));
        let (tx, rx) = channel();
        let (thread_cancel, thread_progress, ctx) = (cancel.clone(), progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = File::open(format!(r"\\.\PhysicalDrive{index}")).and_then(|mut disk| {
                partitions::search(&mut disk, &gaps, size, &thread_cancel, |p| {
                    *thread_progress.lock().unwrap() = p.clone();
                    ctx.request_repaint();
                })
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.search = Some(SearchJob { disk: index, cancel, progress, rx });
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) -> Option<Event> {
        if !self.loaded {
            self.reload(ui.ctx());
        }
        self.poll();
        if self.ops.take_changed() {
            self.reload(ui.ctx());
            self.selected = None;
            self.changed = true;
        }
        let guard = guard(&self.layouts, volumes);
        let views = disk_views(&self.layouts, &self.disks, volumes);
        let selected_row = self.selected.and_then(|s| views.iter().flat_map(|d| &d.rows).find(|r| r.sel == s));
        let selected_disk = self.selected.and_then(|s| views.iter().find(|d| d.layout.index == s.disk()));
        let busy = self.ops.busy();

        let mut action: Option<Action> = None;
        let toolbar = self.draw_toolbar(ui, selected_disk.map(|d| d.layout.index));
        ui.add_space(10.0);
        if let Some(text) = self.message.take() {
            notice(ui, icon::INFO, theme::ACCENT, &text);
            ui.add_space(8.0);
        }

        egui::SidePanel::left("partition_ops_panel")
            .exact_width(SIDE_W)
            .resizable(false)
            .frame(theme::card_frame().inner_margin(egui::Margin::same(12.0)))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("partition_ops_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    section(ui, icon::LIST_BULLETS, t("Bewerkingen", "Operations"));
                    for item in operations(selected_row, selected_disk, busy, &guard) {
                        if let Some(a) = op_entry(ui, item) {
                            action = Some(a);
                        }
                    }
                    if self.selected.is_none() {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(t("Selecteer een partitie in de tabel of de kaart.", "Select a partition in the table or the map.")).size(12.0).color(theme::TEXT_DIM));
                    }
                    ui.add_space(14.0);
                    section(ui, icon::HOURGLASS, &tf!("Wachtende bewerkingen ({})", "Pending operations ({})", self.ops.queued()));
                    self.ops.draw_pending(ui);
                });
            });

        let mut clicked = None;
        let mut menu = |ui: &mut egui::Ui, row: &VolRow| {
            let disk = views.iter().find(|d| d.layout.index == row.sel.disk());
            for item in operations(Some(row), disk, busy, &guard) {
                let enabled = item.action.is_ok();
                let text = egui::RichText::new(format!("{}  {}", item.glyph, item.text));
                let text = if item.danger { text.color(theme::DANGER) } else { text };
                if ui.add_enabled(enabled, egui::Button::new(text).frame(false)).clicked() {
                    action = item.action.ok();
                    ui.close_menu();
                }
            }
        };
        egui::TopBottomPanel::bottom("partition_map_panel")
            .resizable(true)
            .default_height(MAP_DEFAULT_H)
            .height_range(160.0..=600.0)
            .frame(egui::Frame::none().inner_margin(egui::Margin { left: 12.0, right: 0.0, top: 10.0, bottom: 0.0 }))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("partition_map_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    if let Some(sel) = partition_map::disk_map(ui, &views, self.selected, &mut menu) {
                        clicked = Some(sel);
                    }
                    partition_map::legend(ui);
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin { left: 12.0, right: 0.0, top: 0.0, bottom: 4.0 }))
            .show_inside(ui, |ui| {
                if views.is_empty() {
                    if self.rx.is_some() {
                        ui.add(egui::Spinner::new().size(22.0).color(theme::ACCENT));
                    } else {
                        ui::empty_card(ui, icon::INFO, t("Geen schijven gevonden.", "No disks found."));
                    }
                    return;
                }
                egui::ScrollArea::both().id_salt("partition_table_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    if let Some(sel) = partition_map::volume_table(ui, &views, self.selected, &mut menu) {
                        clicked = Some(sel);
                    }
                });
            });
        drop(views);

        if let Some(sel) = clicked {
            self.selected = Some(sel);
        }
        let mut event = None;
        match toolbar {
            Some(Toolbar::Apply) => self.ops.ask_apply(),
            Some(Toolbar::Undo) => self.ops.undo(),
            Some(Toolbar::Clear) => self.ops.clear(),
            Some(Toolbar::Reload) => self.reload(ui.ctx()),
            Some(Toolbar::Search(disk)) => self.start_search(disk, ui.ctx()),
            Some(Toolbar::StopSearch) => {
                if let Some(job) = &self.search {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            None => {}
        }
        match action {
            Some(Action::Op(request)) => self.ops.open(request, ui.ctx(), &guard),
            Some(Action::Explore(path)) => {
                if let Err(e) = crate::largefiles::show_in_explorer(&path) {
                    self.message = Some(tf!("Verkenner openen mislukt: {e}", "Could not open Explorer: {e}"));
                }
            }
            Some(Action::Search(disk)) => self.start_search(disk, ui.ctx()),
            Some(Action::Event(e)) => event = Some(e),
            None => {}
        }
        self.ops.draw_dialogs(ui.ctx(), &guard);
        if let Some(e) = self.draw_found(ui.ctx()) {
            event = Some(e);
        }
        event
    }

    fn draw_toolbar(&self, ui: &mut egui::Ui, disk: Option<u32>) -> Option<Toolbar> {
        let mut pressed = None;
        let queued = self.ops.queued();
        let busy = self.ops.busy();
        ui.horizontal(|ui| {
            ui.heading(t("Partities", "Partitions"));
            ui.add_space(16.0);
            let apply = ui::primary_icon_button(icon::CHECK, &tf!("Toepassen ({queued})", "Apply ({queued})"));
            if ui.add_enabled(queued > 0 && !busy, apply).on_hover_text(t("Alle wachtende bewerkingen uitvoeren", "Carry out all pending operations")).clicked() {
                pressed = Some(Toolbar::Apply);
            }
            if ui.add_enabled(queued > 0 && !busy, ui::secondary_icon_button(icon::ARROW_COUNTER_CLOCKWISE, t("Ongedaan maken", "Undo"))).clicked() {
                pressed = Some(Toolbar::Undo);
            }
            if ui.add_enabled(queued > 0 && !busy, ui::secondary_icon_button(icon::X, t("Alles annuleren", "Cancel all"))).clicked() {
                pressed = Some(Toolbar::Clear);
            }
            ui.separator();
            let loading = self.rx.is_some();
            if ui.add_enabled(!loading && !busy, ui::secondary_icon_button(icon::ARROW_CLOCKWISE, t("Opnieuw lezen", "Reload"))).clicked() {
                pressed = Some(Toolbar::Reload);
            }
            match &self.search {
                Some(job) => {
                    let p = job.progress.lock().unwrap().clone();
                    let pct = (p.done * 100).checked_div(p.total).unwrap_or(0);
                    ui.add(egui::Spinner::new().size(16.0).color(theme::ACCENT));
                    ui.label(egui::RichText::new(tf!("Schijf {} doorzoeken… {pct}% · {} gevonden", "Searching disk {}… {pct}% · {} found", job.disk, p.found)).color(theme::TEXT_DIM));
                    if ui::icon_button(ui, icon::STOP, t("Zoeken stoppen", "Stop search")).clicked() {
                        pressed = Some(Toolbar::StopSearch);
                    }
                }
                None => {
                    let button = ui::secondary_icon_button(icon::BINOCULARS, t("Verloren partities zoeken", "Find lost partitions"));
                    let resp = ui
                        .add_enabled(disk.is_some() && !busy, button)
                        .on_hover_text(t("Doorzoekt de niet-toegewezen ruimte van de geselecteerde schijf naar oude partities. Er wordt niets geschreven.", "Searches the unallocated space of the selected disk for old partitions. Nothing is written."))
                        .on_disabled_hover_text(t("Selecteer eerst iets op een schijf", "Select something on a disk first"));
                    if resp.clicked() {
                        pressed = disk.map(Toolbar::Search);
                    }
                }
            }
            if loading {
                ui.add(egui::Spinner::new().size(16.0).color(theme::ACCENT));
            }
        });
        pressed
    }

    /// Result of a lost-partition search. Returns a scan to start.
    fn draw_found(&mut self, ctx: &egui::Context) -> Option<Event> {
        let (disk, result) = self.found.as_ref()?;
        let mut event = None;
        let mut close = false;
        dim_background(ctx);
        dialog("partition_found", ctx, |ui| {
            dialog_title(ui, icon::BINOCULARS, theme::ACCENT, t("Verloren partities", "Lost partitions"), &tf!("Schijf {disk}", "Disk {disk}"));
            match result {
                Err(e) => notice(ui, icon::WARNING_CIRCLE, theme::DANGER, e),
                Ok(found) if found.is_empty() => notice(ui, icon::INFO, theme::ACCENT, t("Geen verloren partities gevonden in de niet-toegewezen ruimte.", "No lost partitions found in the unallocated space.")),
                Ok(found) => {
                    notice(ui, icon::INFO, theme::ACCENT, t("Je kunt een gevonden partitie doorzoeken op bestanden. De indeling van de schijf wordt niet aangepast.", "You can search a found partition for files. The disk layout is not changed."));
                    for f in found {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("{} · {}", f.fs.label(), disk::format_size(f.size))).font(theme::font_medium(13.5)));
                            let mut detail = tf!("begint op {}", "starts at {}", disk::format_size(f.start));
                            if f.from_backup {
                                detail.push_str(t(" · via de reservekopie van de bootsector", " · via the backup boot sector"));
                            }
                            ui.label(egui::RichText::new(detail).color(theme::TEXT_DIM));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(ui::primary_icon_button(icon::MAGNIFYING_GLASS, t("Bestanden zoeken", "Find files"))).clicked() {
                                    event = Some(Event::Scan {
                                        path: format!(r"\\.\PhysicalDrive{disk}"),
                                        start: Some(f.start),
                                        label: tf!("Verloren {}-partitie (schijf {disk})", "Lost {} partition (disk {disk})", f.fs.label()),
                                    });
                                }
                            });
                        });
                    }
                }
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.add(ui::secondary_icon_button(icon::X, t("Sluiten", "Close")).min_size(vec2(120.0, 36.0))).clicked();
            });
        });
        if close || event.is_some() || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.found = None;
        }
        event
    }
}

enum Toolbar {
    Apply,
    Undo,
    Clear,
    Reload,
    Search(u32),
    StopSearch,
}

fn section(ui: &mut egui::Ui, glyph: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(glyph).font(theme::icon(15.0)).color(theme::ACCENT));
        ui.label(egui::RichText::new(title).font(theme::font_semibold(14.0)));
    });
    ui.add_space(4.0);
}

/// One line of the operations list. Returns its action when clicked.
fn op_entry(ui: &mut egui::Ui, item: OpItem) -> Option<Action> {
    let enabled = item.action.is_ok();
    let color = match (enabled, item.danger) {
        (false, _) => theme::TEXT_FAINT,
        (true, true) => theme::DANGER,
        (true, false) => theme::TEXT_MAIN,
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, 5.0, theme::SUBTLE_BG);
    }
    let icon_color = if enabled && !item.danger { theme::ACCENT } else { color };
    ui.painter().text(rect.left_center() + vec2(8.0, 0.0), egui::Align2::LEFT_CENTER, item.glyph, theme::icon(15.0), icon_color);
    ui.painter().text(rect.left_center() + vec2(32.0, 0.0), egui::Align2::LEFT_CENTER, item.text, theme::font(13.0), color);
    match item.action {
        Ok(action) => resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked().then_some(action),
        Err(why) => {
            resp.on_hover_text(why);
            None
        }
    }
}

/// The operations for the selected row, in partition-manager order.
fn operations(row: Option<&VolRow>, disk: Option<&DiskView>, busy: bool, guard: &Guard) -> Vec<OpItem> {
    let item = |glyph, text, danger, action| OpItem { glyph, text, danger, action };
    let nothing = t("Selecteer eerst een partitie", "Select a partition first");
    if busy {
        return vec![item(icon::HOURGLASS, t("Bezig met toepassen…", "Applying…"), false, Err(t("Wacht tot de bewerkingen klaar zijn", "Wait until the operations are done")))];
    }
    let (Some(row), Some(disk)) = (row, disk) else {
        return [
            (icon::ARROWS_OUT_LINE_HORIZONTAL, t("Vergroten/verkleinen", "Resize")),
            (icon::HASH, t("Stationsletter wijzigen", "Change drive letter")),
            (icon::TAG, t("Label wijzigen", "Change label")),
            (icon::STETHOSCOPE, t("Controleren", "Check")),
            (icon::ERASER, t("Formatteren", "Format")),
            (icon::TRASH, t("Verwijderen", "Delete")),
        ]
        .into_iter()
        .map(|(g, text)| item(g, text, false, Err(nothing)))
        .collect();
    };
    let index = disk.layout.index;
    let properties = Action::Op(Request::Properties(row.name.clone(), properties(row, disk)));
    let Some(part) = row.part else {
        let Selection::Free { start, end, .. } = row.sel else { return Vec::new() };
        return vec![
            item(icon::PLUS, t("Nieuwe partitie", "New partition"), false, Ok(Action::Op(Request::Create { disk: index, start, end }))),
            item(icon::BINOCULARS, t("Verloren partities zoeken", "Find lost partitions"), false, Ok(Action::Search(index))),
            item(icon::INFO, t("Eigenschappen", "Properties"), false, Ok(properties)),
        ];
    };

    let target = Target { disk: index, part: part.number, name: row.name.clone() };
    let volume = row.volume;
    let letter = volume.and_then(|v| v.letter.chars().next());
    let system_why = t("Systeempartitie: Windows heeft deze nodig", "System partition: Windows needs it");
    let no_letter = t("Deze partitie heeft geen stationsletter", "This partition has no drive letter");
    let windows_disk = t("Niet op de schijf waar Windows op staat", "Not on the disk Windows is on");
    let system = part.kind.is_system();
    let unless_system = |a: Action| if system { Err(system_why) } else { Ok(a) };
    // Same rule as the guard: when the Windows disk is unknown, none may be erased.
    let on_windows_disk = guard.windows_disk.is_none_or(|d| d == index);
    let erasing = |a: Action| if on_windows_disk { Err(windows_disk) } else { unless_system(a) };
    let with_volume = |a: Action| match volume {
        _ if system => Err(system_why),
        None => Err(no_letter),
        Some(_) => Ok(a),
    };
    let fs = match volume.map(|v| v.filesystem.to_uppercase()).as_deref() {
        Some("EXFAT") => Fs::ExFat,
        Some("FAT32") => Fs::Fat32,
        _ => Fs::Ntfs,
    };
    let active = if disk.layout.style != Style::Mbr {
        Err(t("Alleen bij schijven met MBR", "Only on disks with MBR"))
    } else if part.logical {
        Err(t("Een logische partitie kan niet actief zijn", "A logical partition cannot be active"))
    } else if part.active {
        Err(t("Deze partitie is al actief", "This partition is already active"))
    } else if on_windows_disk {
        Err(t("Op de Windows-schijf kan dit het opstarten breken", "On the Windows disk this can break startup"))
    } else {
        unless_system(Action::Op(Request::SetActive(target.clone())))
    };
    let hide = match letter {
        Some(_) if system => Err(system_why),
        Some(l) if guard.windows_letter.is_none_or(|w| w == l) => Err(t("De Windows-schijf kan niet verborgen worden", "The Windows drive cannot be hidden")),
        Some(l) => Ok(Action::Op(Request::Hide(target.clone(), l))),
        None => unless_system(Action::Op(Request::Show(target.clone()))),
    };
    let scan = match volume {
        Some(v) => Ok(Action::Event(Event::Scan { path: v.scan_path.clone(), start: None, label: v.display_name() })),
        None if system => Err(system_why),
        None => Ok(Action::Event(Event::Scan { path: format!(r"\\.\PhysicalDrive{index}"), start: Some(part.start), label: row.name.clone() })),
    };
    vec![
        item(icon::ARROWS_OUT_LINE_HORIZONTAL, t("Vergroten/verkleinen", "Resize"), false, unless_system(Action::Op(Request::Resize(target.clone(), part.size)))),
        item(icon::COPY, t("Kopiëren naar image", "Copy to image"), false, with_volume(Action::Event(Event::Image(volume.map(|v| v.letter.clone()).unwrap_or_default())))),
        item(icon::HASH, t("Stationsletter wijzigen", "Change drive letter"), false, unless_system(Action::Op(Request::Letter(target.clone())))),
        item(icon::TAG, t("Label wijzigen", "Change label"), false, with_volume(Action::Op(Request::Label(target.clone(), volume.map(|v| v.label.clone()).unwrap_or_default())))),
        item(icon::FLAG, t("Actief maken", "Mark active"), false, active),
        item(icon::STETHOSCOPE, t("Controleren", "Check"), false, with_volume(Action::Op(Request::Check(target.clone(), letter.unwrap_or('C'))))),
        item(icon::LIGHTNING, t("Defragmenteren / optimaliseren", "Defragment / optimize"), false, with_volume(Action::Op(Request::Optimize(target.clone(), letter.unwrap_or('C'))))),
        item(if letter.is_some() { icon::EYE_SLASH } else { icon::EYE }, if letter.is_some() { t("Verbergen", "Hide") } else { t("Zichtbaar maken", "Show") }, false, hide),
        item(icon::TRASH, t("Verwijderen", "Delete"), true, erasing(Action::Op(Request::Delete(target.clone())))),
        item(icon::ERASER, t("Formatteren", "Format"), true, erasing(Action::Op(Request::Format(target.clone())))),
        item(icon::BROOM, t("Partitie wissen", "Wipe partition"), true, erasing(Action::Op(Request::Wipe(target, fs)))),
        item(icon::PULSE, t("Oppervlaktetest", "Surface test"), false, Ok(Action::Event(Event::Surface(index)))),
        item(icon::FOLDER_OPEN, t("Verkennen", "Explore"), false, with_volume(Action::Explore(PathBuf::from(format!("{}\\", volume.map(|v| v.letter.as_str()).unwrap_or("")))))),
        item(icon::MAGNIFYING_GLASS, t("Verwijderde bestanden zoeken", "Find deleted files"), false, scan),
        item(icon::INFO, t("Eigenschappen", "Properties"), false, Ok(properties)),
    ]
}

fn properties(row: &VolRow, disk: &DiskView) -> Vec<(String, String)> {
    let bytes = |n: u64| format!("{} ({} bytes)", disk::format_size(n), ui::fmt_thousands(n));
    let mut rows = vec![(t("Schijf", "Disk").to_string(), tf!("Schijf {} · {} · {}", "Disk {} · {} · {}", disk.layout.index, disk.model, disk.style))];
    let (start, end) = match row.sel {
        Selection::Free { start, end, .. } => (start, end),
        Selection::Part { .. } => row.part.map_or((0, 0), |p| (p.start, p.start + p.size)),
    };
    if let Some(p) = row.part {
        rows.push((t("Partitienummer", "Partition number").to_string(), p.number.to_string()));
        rows.push((t("Soort", "Kind").to_string(), p.kind.label().to_string()));
        rows.push(("Type".to_string(), row.kind.to_string()));
        if !p.name.is_empty() {
            rows.push((t("GPT-naam", "GPT name").to_string(), p.name.clone()));
        }
    }
    rows.push((t("Begin", "Start").to_string(), bytes(start)));
    rows.push((t("Grootte", "Size").to_string(), bytes(end - start)));
    if let Some(v) = row.volume {
        rows.push((t("Stationsletter", "Drive letter").to_string(), v.letter.clone()));
        rows.push(("Label".to_string(), if v.label.is_empty() { "—".to_string() } else { v.label.clone() }));
        rows.push((t("Bestandssysteem", "File system").to_string(), v.filesystem.clone()));
        rows.push((t("Gebruikt", "Used").to_string(), bytes(v.used_bytes)));
        rows.push((t("Vrij", "Free").to_string(), bytes(v.total_bytes.saturating_sub(v.used_bytes))));
    }
    rows.push(("Status".to_string(), row.status.clone()));
    rows
}

/// What may not be changed on this computer.
fn guard(layouts: &[Layout], volumes: &[VolumeInfo]) -> Guard {
    let windows = windows_volume(volumes);
    Guard {
        windows_disk: windows.and_then(|v| v.disk_index),
        system_parts: layouts
            .iter()
            .flat_map(|l| l.partitions.iter().filter(|p| p.kind.is_system()).map(move |p| (l.index, p.number)))
            .collect(),
        used_letters: volumes.iter().filter_map(|v| v.letter.chars().next()).collect(),
        windows_letter: windows.and_then(|v| v.letter.chars().next()),
    }
}

fn windows_volume(volumes: &[VolumeInfo]) -> Option<&VolumeInfo> {
    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    volumes.iter().find(|v| v.letter.eq_ignore_ascii_case(&system_drive))
}

/// Table and map rows for every disk.
fn disk_views<'a>(layouts: &'a [Layout], disks: &HashMap<u32, DiskHealth>, volumes: &'a [VolumeInfo]) -> Vec<DiskView<'a>> {
    let windows = windows_volume(volumes);
    layouts
        .iter()
        .map(|layout| {
            let health = disks.get(&layout.index).and_then(|d| d.summary().status);
            let health_text = partition_map::health_text(health);
            let mut rows: Vec<VolRow> = layout
                .partitions
                .iter()
                .map(|p| {
                    let volume = volumes.iter().find(|v| v.disk_index == Some(layout.index) && v.disk_offset == Some(p.start));
                    let is_windows = volume.is_some() && volume.map(|v| &v.letter) == windows.map(|w| &w.letter);
                    let mut flags = Vec::new();
                    if p.active {
                        flags.push(t("Actief", "Active"));
                    }
                    if p.kind == partitions::PartKind::Efi || (p.active && layout.style == Style::Mbr) {
                        flags.push(t("Systeem", "System"));
                    }
                    if is_windows {
                        flags.push(t("Opstart", "Boot"));
                    }
                    let status = if flags.is_empty() { health_text.to_string() } else { format!("{health_text} ({})", flags.join(", ")) };
                    // GPT names are mostly generic ("Basic data partition"), so
                    // system partitions are named by their kind.
                    let name = if p.kind.is_system() { p.kind.label().to_string() } else { part_name(p, layout.index, volume) };
                    VolRow {
                        sel: Selection::Part { disk: layout.index, number: p.number },
                        part: Some(p),
                        volume,
                        name,
                        fs: volume.map_or_else(|| "—".to_string(), |v| v.filesystem.clone()),
                        size: p.size,
                        used: volume.map(|v| v.used_bytes),
                        kind: match (layout.style, p.logical) {
                            (Style::Gpt, _) => "GPT",
                            (_, true) => t("Logisch", "Logical"),
                            _ => t("Primair", "Primary"),
                        },
                        status,
                        windows: is_windows,
                    }
                })
                .collect();
            rows.extend(layout.gaps().into_iter().map(|(start, end)| VolRow {
                sel: Selection::Free { disk: layout.index, start, end },
                part: None,
                volume: None,
                name: t("Niet-toegewezen", "Unallocated").to_string(),
                fs: "—".to_string(),
                size: end - start,
                used: None,
                kind: t("Niet-toegewezen", "Unallocated"),
                status: "—".to_string(),
                windows: false,
            }));
            rows.sort_by_key(|r| match r.sel {
                Selection::Free { start, .. } => start,
                Selection::Part { .. } => r.part.map_or(0, |p| p.start),
            });
            DiskView {
                layout,
                model: disks.get(&layout.index).map_or_else(|| t("Onbekende schijf", "Unknown disk").to_string(), |d| d.model.clone()),
                style: match layout.style {
                    Style::Gpt => t("Basis GPT", "Basic GPT"),
                    Style::Mbr => t("Basis MBR", "Basic MBR"),
                    Style::Raw => t("Geen partitietabel", "No partition table"),
                },
                health,
                rows,
            }
        })
        .collect()
}
