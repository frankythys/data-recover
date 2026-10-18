//! "Partities" screen: per physical disk a bar and a list of its
//! partitions and unpartitioned space, operations on a selected partition
//! (see `partition_ops_view`), and a search of the unpartitioned space for
//! lost partitions, which can then be scanned for files.

use crate::cleanup::notice;
use crate::disk::{self, VolumeInfo};
use crate::health::{self, DiskHealth};
use crate::partition_ops::Guard;
use crate::partition_ops_view::{self, OpsPanel, Request, Selection};
use crate::partitions::{self, FoundPartition, Layout, PartInfo, PartKind, SearchProgress, Style};
use crate::{theme, ui};
use egui::{Rect, Sense, pos2, vec2};
use egui_phosphor::regular as icon;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

const BAR_H: f32 = 30.0;
/// Narrowest a segment is drawn, so tiny partitions stay visible.
const MIN_SEGMENT_W: f32 = 4.0;

/// A request to scan the file system at `start` bytes into `path`.
pub struct ScanRequest {
    pub path: String,
    pub start: u64,
    pub label: String,
}

struct SearchJob {
    disk: u32,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<SearchProgress>>,
    rx: Receiver<io::Result<Vec<FoundPartition>>>,
}

/// One stretch of the disk: a partition or unpartitioned space.
enum Segment<'a> {
    Part(&'a PartInfo),
    Free(u64, u64),
}

pub struct PartitionsView {
    layouts: Vec<Layout>,
    models: HashMap<u32, String>,
    rx: Option<Receiver<(Vec<Layout>, Vec<DiskHealth>)>>,
    loaded: bool,
    search: Option<SearchJob>,
    /// Search result per disk.
    results: HashMap<u32, Result<Vec<FoundPartition>, String>>,
    selected: Option<Selection>,
    ops: OpsPanel,
    /// Set when partitions changed, so the app rereads its drive list.
    changed: bool,
}

impl PartitionsView {
    pub fn new() -> Self {
        Self {
            layouts: Vec::new(),
            models: HashMap::new(),
            rx: None,
            loaded: false,
            search: None,
            results: HashMap::new(),
            selected: None,
            ops: OpsPanel::default(),
            changed: false,
        }
    }

    /// Whether partitions were changed since the last call.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    fn guard(&self, volumes: &[VolumeInfo]) -> Guard {
        let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        Guard {
            windows_disk: volumes.iter().find(|v| v.letter.eq_ignore_ascii_case(&system_drive)).and_then(|v| v.disk_index),
            system_parts: self
                .layouts
                .iter()
                .flat_map(|l| l.partitions.iter().filter(|p| p.kind.is_system()).map(move |p| (l.index, p.number)))
                .collect(),
            used_letters: volumes.iter().filter_map(|v| v.letter.chars().next()).collect(),
        }
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
            self.models = disks.into_iter().map(|d| (d.index, d.model)).collect();
            self.rx = None;
        }
        let Some(job) = &self.search else { return };
        if let Ok(result) = job.rx.try_recv() {
            let result = result.map_err(|e| match e.kind() {
                io::ErrorKind::PermissionDenied => "Start Data Recoverer als administrator om de schijf te doorzoeken.".to_string(),
                io::ErrorKind::Interrupted => "Zoeken gestopt.".to_string(),
                _ => format!("Zoeken mislukt: {e}"),
            });
            self.results.insert(job.disk, result);
            self.search = None;
        }
    }

    fn start_search(&mut self, layout: &Layout, ctx: &egui::Context) {
        let (index, size, gaps) = (layout.index, layout.size, layout.gaps());
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
        self.results.remove(&index);
        self.search = Some(SearchJob { disk: index, cancel, progress, rx });
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) -> Option<ScanRequest> {
        if !self.loaded {
            self.reload(ui.ctx());
        }
        self.poll();
        let loading = self.rx.is_some();

        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.heading("Partities");
                ui.label(
                    egui::RichText::new("Hoe je schijven zijn ingedeeld, en zoeken naar partities die uit de indeling zijn verdwenen.")
                        .color(theme::TEXT_DIM),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.add_enabled_ui(!loading && self.search.is_none(), |ui| {
                    refresh = ui::icon_button(ui, icon::ARROW_CLOCKWISE, "Opnieuw lezen").clicked();
                });
                if loading {
                    ui.add(egui::Spinner::new().size(20.0).color(theme::ACCENT));
                }
            });
        });
        if refresh {
            self.reload(ui.ctx());
        }
        ui.add_space(14.0);
        self.ops.draw_queue(ui);
        if self.ops.take_changed() {
            self.reload(ui.ctx());
            self.selected = None;
            self.changed = true;
        }
        ui.add_space(8.0);
        let guard = self.guard(volumes);
        self.ops.draw_dialogs(ui.ctx(), &guard);
        if self.layouts.is_empty() {
            if !loading {
                ui::empty_card(ui, icon::INFO, "Geen schijven gevonden.");
            }
            return None;
        }

        let mut search = None;
        let mut scan = None;
        let mut stop = false;
        let mut select = None;
        let mut request: Option<Request> = None;
        egui::ScrollArea::vertical().id_salt("partitions").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 12.0;
            for layout in &self.layouts {
                let model = self.models.get(&layout.index).map_or("Schijf", String::as_str);
                let action = self.disk_card(ui, layout, model, volumes);
                match action {
                    Some(CardAction::Search) => search = Some(layout.clone()),
                    Some(CardAction::Stop) => stop = true,
                    Some(CardAction::Select(sel)) => select = Some(sel),
                    Some(CardAction::Op(req)) => request = Some(req),
                    Some(CardAction::Scan(f)) => {
                        scan = Some(ScanRequest {
                            path: format!(r"\\.\PhysicalDrive{}", layout.index),
                            start: f.start,
                            label: format!("Verloren {}-partitie (schijf {})", f.fs.label(), layout.index),
                        })
                    }
                    None => {}
                }
            }
        });
        if let Some(layout) = search {
            self.start_search(&layout, ui.ctx());
        }
        if let Some(sel) = select {
            self.selected = Some(sel);
        }
        if let Some(req) = request {
            self.ops.open(req, ui.ctx());
        }
        if stop {
            if let Some(job) = &self.search {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        scan
    }

    fn disk_card(&self, ui: &mut egui::Ui, layout: &Layout, model: &str, volumes: &[VolumeInfo]) -> Option<CardAction> {
        let style = match layout.style {
            Style::Gpt => "GPT",
            Style::Mbr => "MBR",
            Style::Raw => "Geen partitietabel",
        };
        let sub = format!("{style} · {} · Schijf {}", disk::format_size(layout.size), layout.index);
        let segments = segments(layout);
        let volume_of = |p: &PartInfo| {
            volumes.iter().find(|v| v.disk_index == Some(layout.index) && v.disk_offset == Some(p.start))
        };
        let mut action = None;

        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 16.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 8.0);
            ui.horizontal(|ui| {
                let (tile, _) = ui.allocate_exact_size(vec2(46.0, 46.0), Sense::hover());
                ui::icon_tile(ui, tile, icon::HARD_DRIVE, theme::ACCENT, 24.0);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(egui::RichText::new(model).font(theme::font_semibold(16.0)));
                    ui.label(egui::RichText::new(&sub).color(theme::TEXT_DIM));
                });
            });
            ui.add_space(12.0);

            // The bar: each stretch as wide as its share of the disk.
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H), Sense::hover());
            let scale = bar.width() / layout.size.max(1) as f32;
            for (i, seg) in segments.iter().enumerate() {
                let (start, end) = seg.range();
                let x0 = bar.left() + start as f32 * scale;
                let x1 = (bar.left() + end as f32 * scale).max(x0 + MIN_SEGMENT_W).min(bar.right());
                let rect = Rect::from_min_max(pos2(x0, bar.top()), pos2(x1, bar.bottom())).shrink2(vec2(1.0, 0.0));
                let color = seg.color();
                ui.painter().rect_filled(rect, 4.0, color);
                let sel = seg.selection(layout.index);
                if self.selected == Some(sel) {
                    ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(2.5_f32, theme::TEXT_MAIN));
                } else if matches!(seg, Segment::Free(..)) {
                    ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.0_f32, theme::CARD_BORDER));
                }
                let name = seg.name(volume_of);
                let tip = format!("{name}\n{} · begint op {}\nKlik om te selecteren", disk::format_size(end - start), disk::format_size(start));
                let resp = ui.interact(rect, ui.id().with(("segment", layout.index, i)), Sense::click()).on_hover_text(tip);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    action = Some(CardAction::Select(sel));
                }
            }
            ui.add_space(10.0);

            egui::Grid::new(("partition_rows", layout.index)).spacing(vec2(20.0, 6.0)).show(ui, |ui| {
                for head in ["", "Partitie", "Soort", "Bestandssysteem", "Grootte"] {
                    ui.label(egui::RichText::new(head).font(theme::font_semibold(12.0)).color(theme::TEXT_DIM));
                }
                ui.end_row();
                for seg in &segments {
                    let (dot, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                    ui.painter().rect_filled(dot, 3.0, seg.color());
                    let (kind, fs) = match seg {
                        Segment::Part(p) => (p.kind.label(), volume_of(p).map_or("—", |v| v.filesystem.as_str())),
                        Segment::Free(..) => ("Niet-toegewezen", "—"),
                    };
                    let (start, end) = seg.range();
                    let sel = seg.selection(layout.index);
                    let name = egui::RichText::new(seg.name(volume_of)).font(theme::font_medium(13.0));
                    if ui.selectable_label(self.selected == Some(sel), name).clicked() {
                        action = Some(CardAction::Select(sel));
                    }
                    ui.label(egui::RichText::new(kind).color(theme::TEXT_DIM));
                    ui.label(egui::RichText::new(fs).color(theme::TEXT_DIM));
                    ui.label(disk::format_size(end - start));
                    ui.end_row();
                }
            });
            if let Some(sel) = self.selected.filter(|s| s.disk() == layout.index) {
                ui.add_space(8.0);
                if let Some(req) = partition_ops_view::action_buttons(ui, sel, layout, volumes, self.ops.busy()) {
                    action = Some(CardAction::Op(req));
                }
            }
            ui.add_space(10.0);
            if let Some(a) = self.draw_search(ui, layout) {
                action = Some(a);
            }
        });
        action
    }

    /// The lost-partition search of one disk: its button, progress or result.
    fn draw_search(&self, ui: &mut egui::Ui, layout: &Layout) -> Option<CardAction> {
        let mut action = None;
        let gaps = layout.gaps();
        if let Some(job) = self.search.as_ref().filter(|j| j.disk == layout.index) {
            let p = job.progress.lock().unwrap().clone();
            let fraction = if p.total > 0 { p.done as f32 / p.total as f32 } else { 0.0 };
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0).color(theme::ACCENT));
                ui.label(
                    egui::RichText::new(format!(
                        "Zoeken… {} van {} · {} gevonden",
                        disk::format_size(p.done),
                        disk::format_size(p.total),
                        p.found
                    ))
                    .color(theme::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui::icon_button(ui, icon::STOP, "Stoppen").clicked() {
                        action = Some(CardAction::Stop);
                    }
                });
            });
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
            ui::progress_bar(ui, bar, fraction, theme::ACCENT);
            return action;
        }

        let free: u64 = gaps.iter().map(|(s, e)| e - s).sum();
        ui.horizontal(|ui| {
            let button = ui::secondary_icon_button(icon::BINOCULARS, "Verloren partities zoeken");
            let resp = ui
                .add_enabled(self.search.is_none() && !gaps.is_empty(), button)
                .on_hover_text("Doorzoekt de niet-toegewezen ruimte naar oude NTFS-, FAT- en exFAT-partities. Er wordt niets geschreven.");
            if resp.clicked() {
                action = Some(CardAction::Search);
            }
            let hint = if gaps.is_empty() {
                "Alle ruimte is ingedeeld, dus er zijn geen verloren partities.".to_string()
            } else {
                format!("{} niet-toegewezen ruimte", disk::format_size(free))
            };
            ui.label(egui::RichText::new(hint).color(theme::TEXT_DIM));
        });
        match self.results.get(&layout.index) {
            Some(Err(e)) => {
                ui.add_space(6.0);
                notice(ui, icon::WARNING_CIRCLE, theme::DANGER, e);
            }
            Some(Ok(found)) if found.is_empty() => {
                ui.add_space(6.0);
                notice(ui, icon::INFO, theme::ACCENT, "Geen verloren partities gevonden.");
            }
            Some(Ok(found)) => {
                ui.add_space(6.0);
                notice(
                    ui,
                    icon::CHECK_CIRCLE,
                    theme::SUCCESS,
                    "Gevonden partities kun je doorzoeken op bestanden. De indeling van de schijf wordt niet aangepast.",
                );
                for f in found {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon::SQUARE_SPLIT_HORIZONTAL).font(theme::icon(16.0)).color(theme::ACCENT));
                        ui.label(egui::RichText::new(format!("{} · {}", f.fs.label(), disk::format_size(f.size))).font(theme::font_medium(13.5)));
                        let mut detail = format!("begint op {}", disk::format_size(f.start));
                        if f.from_backup {
                            detail.push_str(" · gevonden via de reservekopie van de bootsector");
                        }
                        ui.label(egui::RichText::new(detail).color(theme::TEXT_DIM));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(ui::primary_icon_button(icon::MAGNIFYING_GLASS, "Bestanden zoeken")).clicked() {
                                action = Some(CardAction::Scan(*f));
                            }
                        });
                    });
                }
            }
            None => {}
        }
        action
    }
}

enum CardAction {
    Search,
    Stop,
    Scan(FoundPartition),
    Select(Selection),
    Op(Request),
}

impl Segment<'_> {
    fn selection(&self, disk: u32) -> Selection {
        match self {
            Segment::Part(p) => Selection::Part { disk, number: p.number },
            Segment::Free(start, end) => Selection::Free { disk, start: *start, end: *end },
        }
    }

    fn range(&self) -> (u64, u64) {
        match self {
            Segment::Part(p) => (p.start, p.start + p.size),
            Segment::Free(s, e) => (*s, *e),
        }
    }

    fn color(&self) -> egui::Color32 {
        match self {
            Segment::Free(..) => theme::TRACK_BG,
            Segment::Part(p) if p.kind.is_system() => theme::TEXT_FAINT,
            Segment::Part(p) if p.kind == PartKind::Data => theme::ACCENT,
            Segment::Part(_) => theme::CAT_VIDEO,
        }
    }

    /// Drive letter and label, the GPT name, or the kind.
    fn name<'v>(&self, volume_of: impl Fn(&PartInfo) -> Option<&'v VolumeInfo>) -> String {
        match self {
            Segment::Free(..) => "Niet-toegewezen".to_string(),
            Segment::Part(p) => match volume_of(p) {
                Some(v) => v.display_name(),
                None if !p.name.is_empty() && p.kind != PartKind::Data => p.name.clone(),
                None => format!("Partitie {}", p.number),
            },
        }
    }
}

/// The partitions in order with the unpartitioned space between them.
fn segments(layout: &Layout) -> Vec<Segment<'_>> {
    let mut out: Vec<Segment> = layout.partitions.iter().map(Segment::Part).collect();
    out.extend(layout.gaps().into_iter().map(|(s, e)| Segment::Free(s, e)));
    out.sort_by_key(|s| s.range().0);
    out
}
