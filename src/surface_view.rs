//! "Oppervlaktetest" screen: pick a disk, read it completely and watch a
//! map of its surface fill in.

use crate::cleanup::notice;
use crate::health::{self, DiskHealth};
use crate::health_view::disk_chips;
use crate::surface::{self, Cell, SurfaceProgress};
use crate::{disk, theme, ui};
use egui::{Rect, Sense, pos2, vec2};
use egui_phosphor::regular as icon;
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const COLUMNS: usize = 50;
const ROWS: usize = 20;
const CELL_H: f32 = 12.0;
const CELL_GAP: f32 = 2.0;
/// Unreadable spots listed under the map.
const LISTED_BAD: usize = 8;

struct SurfaceJob {
    name: String,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    progress: Arc<Mutex<SurfaceProgress>>,
    rx: Receiver<io::Result<SurfaceProgress>>,
    started_at: Instant,
    finished_after: Option<Duration>,
    error: Option<String>,
}

pub struct SurfaceView {
    disks: Vec<DiskHealth>,
    disks_rx: Option<Receiver<Vec<DiskHealth>>>,
    loaded: bool,
    selected: Option<u32>,
    job: Option<SurfaceJob>,
}

impl SurfaceView {
    pub fn new() -> Self {
        Self { disks: Vec::new(), disks_rx: None, loaded: false, selected: None, job: None }
    }

    fn running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.finished_after.is_none())
    }

    /// Selects disk `index`, unless a test is running.
    pub fn preselect(&mut self, index: u32) {
        if !self.running() {
            self.selected = Some(index);
        }
    }

    fn poll(&mut self) {
        if let Some(disks) = self.disks_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.disks = disks;
            self.disks_rx = None;
        }
        let Some(job) = self.job.as_mut() else { return };
        if job.finished_after.is_some() {
            return;
        }
        if let Ok(result) = job.rx.try_recv() {
            job.finished_after = Some(job.started_at.elapsed());
            if let Err(e) = result {
                job.error = Some(if e.kind() == io::ErrorKind::PermissionDenied {
                    "Toegang geweigerd. Start Data Recoverer als administrator.".to_string()
                } else {
                    format!("Test mislukt: {e}")
                });
            }
        }
    }

    fn start(&mut self, index: u32, ctx: &egui::Context) {
        let name = self.disks.iter().find(|d| d.index == index).map_or_else(|| format!("Schijf {index}"), |d| d.model.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(SurfaceProgress::default()));
        let (tx, rx) = channel();
        let (thread_cancel, thread_paused, thread_progress, ctx) = (cancel.clone(), paused.clone(), progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = File::open(format!(r"\\.\PhysicalDrive{index}")).and_then(|mut disk| {
                surface::test(&mut disk, COLUMNS * ROWS, &thread_cancel, &thread_paused, |p| {
                    *thread_progress.lock().unwrap() = p.clone();
                    ctx.request_repaint();
                })
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.job = Some(SurfaceJob { name, cancel, paused, progress, rx, started_at: Instant::now(), finished_after: None, error: None });
    }

    pub fn draw(&mut self, ui: &mut egui::Ui) {
        if !self.loaded {
            self.loaded = true;
            let (tx, rx) = channel();
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                let _ = tx.send(health::read_all());
                ctx.request_repaint();
            });
            self.disks_rx = Some(rx);
        }
        self.poll();

        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.heading("Oppervlaktetest");
            ui.label(
                egui::RichText::new("Leest de hele schijf één keer en laat zien welke delen goed, traag of niet te lezen zijn. Er wordt niets geschreven.")
                    .color(theme::TEXT_DIM),
            );
        });
        ui.add_space(14.0);

        egui::ScrollArea::vertical().id_salt("surface").auto_shrink([false, false]).show(ui, |ui| {
            ui::section_header(ui, icon::HARD_DRIVES, "Schijf", None);
            if self.disks_rx.is_some() {
                ui.add(egui::Spinner::new().size(18.0).color(theme::ACCENT));
            }
            let running = self.running();
            let picked = ui.add_enabled_ui(!running, |ui| disk_chips(ui, &self.disks, self.selected)).inner;
            if let Some(i) = picked {
                self.selected = Some(i);
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let button = ui::primary_icon_button(icon::PULSE, "Test starten").min_size(vec2(150.0, 36.0));
                if ui.add_enabled(self.selected.is_some() && !running, button).clicked() {
                    if let Some(i) = self.selected {
                        self.start(i, ui.ctx());
                    }
                }
                ui.label(
                    egui::RichText::new("Duurt bij een harde schijf enkele uren; je kunt pauzeren of stoppen.")
                        .size(12.5)
                        .color(theme::TEXT_DIM),
                );
            });
            ui.add_space(14.0);
            self.draw_job(ui);
        });
    }

    fn draw_job(&self, ui: &mut egui::Ui) {
        let Some(job) = &self.job else { return };
        let p = job.progress.lock().unwrap().clone();
        let running = job.finished_after.is_none();
        let paused = job.paused.load(Ordering::Relaxed);
        let elapsed = job.finished_after.unwrap_or_else(|| job.started_at.elapsed()).as_secs_f64();
        let speed = if elapsed > 0.5 { (p.done as f64 / elapsed) as u64 } else { 0 };

        let title = match (running, paused, &job.error) {
            (_, _, Some(e)) => e.clone(),
            (true, true, _) => "Gepauzeerd".to_string(),
            (true, false, _) => format!("{} testen…", job.name),
            (false, ..) if p.cancelled => "Test gestopt".to_string(),
            (false, ..) => "Test klaar".to_string(),
        };
        let mut detail = format!("{} van {} gelezen", disk::format_size(p.done), disk::format_size(p.total));
        if speed > 0 {
            detail.push_str(&format!(" · {}/s", disk::format_size(speed)));
            if running && !paused {
                detail.push_str(&format!(" · nog {}", ui::fmt_duration(p.total.saturating_sub(p.done) / speed.max(1))));
            }
        }

        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 8.0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(egui::RichText::new(&title).font(theme::font_semibold(16.0)));
                    ui.label(egui::RichText::new(&detail).color(theme::TEXT_DIM));
                });
                if running {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui::icon_button(ui, icon::STOP, "Stoppen").clicked() {
                            job.cancel.store(true, Ordering::Relaxed);
                        }
                        let (glyph, tip) = if paused { (icon::PLAY, "Hervatten") } else { (icon::PAUSE, "Pauzeren") };
                        if ui::icon_button(ui, glyph, tip).clicked() {
                            job.paused.store(!paused, Ordering::Relaxed);
                        }
                    });
                }
            });
            ui.add_space(12.0);
            draw_map(ui, &p.cells);
            ui.add_space(8.0);
            legend(ui);
        });

        if job.error.is_some() || p.total == 0 {
            return;
        }
        ui.add_space(10.0);
        if p.bad_bytes > 0 {
            notice(
                ui,
                icon::X_CIRCLE,
                theme::DANGER,
                &format!(
                    "{} kon niet gelezen worden. Bestanden op die plekken zijn beschadigd. Maak zo snel mogelijk een \
                     schijfimage en vervang de schijf.",
                    disk::format_size(p.bad_bytes)
                ),
            );
            for &at in p.bad_offsets.iter().take(LISTED_BAD) {
                ui.label(egui::RichText::new(format!("Onleesbaar op {}", disk::format_size(at))).size(12.5).color(theme::TEXT_DIM));
            }
        } else if p.slow_chunks > 0 {
            notice(
                ui,
                icon::WARNING,
                theme::WARNING,
                &format!(
                    "{} plekken lazen erg traag. Bij een harde schijf is dat vaak het eerste teken van slijtage; houd \
                     Schijfgezondheid in de gaten en maak een back-up.",
                    ui::fmt_thousands(p.slow_chunks)
                ),
            );
        } else if p.finished && !p.cancelled {
            notice(ui, icon::CHECK_CIRCLE, theme::SUCCESS, "De hele schijf is goed te lezen.");
        }
    }
}

fn cell_color(cell: Cell) -> egui::Color32 {
    match cell {
        Cell::Pending => theme::TRACK_BG,
        Cell::Good => theme::SUCCESS,
        Cell::Slow => theme::WARNING,
        Cell::Bad => theme::DANGER,
    }
}

/// The disk as a grid of areas, from the start (top left) to the end.
fn draw_map(ui: &mut egui::Ui, cells: &[Cell]) {
    let width = ui.available_width();
    let cell_w = (width - CELL_GAP * (COLUMNS - 1) as f32) / COLUMNS as f32;
    let height = ROWS as f32 * (CELL_H + CELL_GAP);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    for i in 0..COLUMNS * ROWS {
        let (row, col) = (i / COLUMNS, i % COLUMNS);
        let min = pos2(rect.left() + col as f32 * (cell_w + CELL_GAP), rect.top() + row as f32 * (CELL_H + CELL_GAP));
        let cell = cells.get(i).copied().unwrap_or(Cell::Pending);
        ui.painter().rect_filled(Rect::from_min_size(min, vec2(cell_w, CELL_H)), 2.0, cell_color(cell));
    }
}

fn legend(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        for (cell, text) in [(Cell::Good, "Goed"), (Cell::Slow, "Traag"), (Cell::Bad, "Onleesbaar"), (Cell::Pending, "Nog niet gelezen")] {
            let (dot, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            ui.painter().rect_filled(dot, 2.0, cell_color(cell));
            ui.label(egui::RichText::new(text).size(12.5).color(theme::TEXT_DIM));
            ui.add_space(8.0);
        }
    });
}
