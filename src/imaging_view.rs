//! "Schijfimage maken" screen: pick a disk or partition and a destination,
//! copy it into one `.img` file and scan that copy for deleted files.

use crate::cleanup::{file_name, location_chip, notice};
use crate::disk::{self, DriveKind, VolumeInfo};
use crate::health::{self, Bus, DiskHealth};
use crate::imaging::ImageJob;
use crate::{theme, ui};
use egui::{Sense, vec2};
use egui_phosphor::regular as icon;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};

/// FAT32 cannot hold files of 4 GB or more.
const FAT32_MAX_FILE: u64 = (4 << 30) - 1;

#[derive(Clone, PartialEq)]
enum Source {
    /// A whole physical disk, with all its partitions.
    Disk { index: u32, name: String, size: u64 },
    /// One partition, by drive letter.
    Volume { letter: String, name: String, size: u64, disk: Option<u32> },
}

impl Source {
    fn path(&self) -> String {
        match self {
            Source::Disk { index, .. } => format!(r"\\.\PhysicalDrive{index}"),
            Source::Volume { letter, .. } => format!(r"\\.\{letter}"),
        }
    }

    fn name(&self) -> &str {
        match self {
            Source::Disk { name, .. } | Source::Volume { name, .. } => name,
        }
    }

    fn size(&self) -> u64 {
        match self {
            Source::Disk { size, .. } | Source::Volume { size, .. } => *size,
        }
    }

    fn disk(&self) -> Option<u32> {
        match self {
            Source::Disk { index, .. } => Some(*index),
            Source::Volume { disk, .. } => *disk,
        }
    }

    /// File name suggested for the image.
    fn default_file(&self) -> String {
        let base = match self {
            Source::Disk { index, .. } => format!("Schijf{index}"),
            Source::Volume { letter, .. } => format!("Station_{}", letter.trim_end_matches(':')),
        };
        format!("{base}.img")
    }
}

pub struct ImagingView {
    disks: Vec<DiskHealth>,
    disks_rx: Option<Receiver<Vec<DiskHealth>>>,
    loaded: bool,
    source: Option<Source>,
    dest: Option<PathBuf>,
    job: Option<ImageJob>,
    /// Error from opening Explorer.
    message: Option<String>,
}

impl ImagingView {
    pub fn new() -> Self {
        Self { disks: Vec::new(), disks_rx: None, loaded: false, source: None, dest: None, job: None, message: None }
    }

    fn running(&self) -> bool {
        self.job.as_ref().is_some_and(ImageJob::running)
    }

    /// Draws the screen. Returns (image path, label) when the user wants to
    /// scan the finished image.
    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) -> Option<(String, String)> {
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
        if let Some(disks) = self.disks_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.disks = disks;
            self.disks_rx = None;
        }
        if let Some(job) = self.job.as_mut() {
            job.poll();
        }

        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.heading("Schijfimage maken");
            ui.label(
                egui::RichText::new(
                    "Een exacte kopie van een schijf in één bestand. Herstel daarna uit de kopie, zodat een zwakke schijf niet verder belast wordt.",
                )
                .color(theme::TEXT_DIM),
            );
        });
        ui.add_space(14.0);

        let mut scan = None;
        egui::ScrollArea::vertical().id_salt("imaging").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_enabled_ui(!self.running(), |ui| {
                self.draw_sources(ui, volumes);
                ui.add_space(14.0);
                self.draw_destination(ui, volumes);
            });
            ui.add_space(14.0);
            scan = self.draw_job(ui);
        });
        scan
    }

    fn draw_sources(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        ui::section_header(ui, icon::HARD_DRIVES, "Bron: hele schijf", None);
        if self.disks_rx.is_some() {
            ui.add(egui::Spinner::new().size(18.0).color(theme::ACCENT));
        }
        let mut picked = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
            for d in &self.disks {
                let source = Source::Disk { index: d.index, name: d.model.clone(), size: d.size };
                let glyph = if d.bus == Bus::Usb { icon::USB } else { icon::HARD_DRIVE };
                let sub = format!("Schijf {} · {}", d.index, disk::format_size(d.size));
                if location_chip(ui, glyph, &d.model, &sub, self.source.as_ref() == Some(&source)) {
                    picked = Some(source);
                }
            }
        });
        ui.add_space(10.0);
        ui::section_header(ui, icon::HARD_DRIVE, "Of één partitie", None);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
            for v in volumes {
                let source = Source::Volume { letter: v.letter.clone(), name: v.display_name(), size: v.total_bytes, disk: v.disk_index };
                let glyph = if v.kind == DriveKind::External { icon::USB } else { icon::HARD_DRIVE };
                let sub = format!("{} · {}", v.filesystem, disk::format_size(v.total_bytes));
                if location_chip(ui, glyph, &v.display_name(), &sub, self.source.as_ref() == Some(&source)) {
                    picked = Some(source);
                }
            }
        });
        if let Some(source) = picked {
            self.source = Some(source);
        }
    }

    fn draw_destination(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        ui::section_header(ui, icon::FLOPPY_DISK, "Bestemming", None);
        let mut choose = false;
        let mut start = false;
        let problem = self.problem(volumes);
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 8.0);
            ui.horizontal(|ui| {
                let text = match &self.dest {
                    Some(p) => p.display().to_string(),
                    None => "Nog geen bestand gekozen".to_string(),
                };
                let color = if self.dest.is_some() { theme::TEXT_MAIN } else { theme::TEXT_DIM };
                ui.label(egui::RichText::new(icon::FILE).font(theme::icon(18.0)).color(theme::TEXT_DIM));
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ready = self.source.is_some() && self.dest.is_some() && problem.is_none();
                    let button = ui::primary_icon_button(icon::DISC, "Image maken").min_size(vec2(150.0, 36.0));
                    start = ui.add_enabled(ready, button).clicked();
                    choose = ui
                        .add_enabled(self.source.is_some(), ui::secondary_icon_button(icon::FOLDER_OPEN, "Kiezen…"))
                        .on_disabled_hover_text("Kies eerst een bron")
                        .clicked();
                });
            });
            if let Some(source) = &self.source {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("Kopie van {} · ongeveer {} nodig", source.name(), disk::format_size(source.size())))
                        .size(12.5)
                        .color(theme::TEXT_DIM),
                );
            }
        });
        if let Some(text) = &problem {
            ui.add_space(8.0);
            notice(ui, icon::WARNING_CIRCLE, theme::DANGER, text);
        }
        ui.add_space(8.0);
        notice(
            ui,
            icon::INFO,
            theme::ACCENT,
            "Zet de image op een andere schijf dan de bron, het liefst een externe schijf. Er wordt alleen gelezen \
             van de bron. Onleesbare stukken worden met nullen gevuld, zodat de kopie toch afkomt.",
        );
        if choose {
            if let Some(source) = &self.source {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Schijfimage", &["img"])
                    .set_file_name(source.default_file())
                    .save_file()
                {
                    self.dest = Some(path);
                }
            }
        }
        if start {
            if let (Some(source), Some(dest)) = (&self.source, &self.dest) {
                self.job = Some(ImageJob::start(source.path(), source.name().to_string(), dest, ui.ctx()));
            }
        }
    }

    /// Why the chosen destination cannot be used, if it cannot.
    fn problem(&self, volumes: &[VolumeInfo]) -> Option<String> {
        let (source, dest) = (self.source.as_ref()?, self.dest.as_ref()?);
        let text = dest.to_string_lossy();
        let vol = text.get(..2).and_then(|l| volumes.iter().find(|v| v.letter.eq_ignore_ascii_case(l)))?;
        if source.disk().is_some() && source.disk() == vol.disk_index {
            return Some(
                "De image kan niet op dezelfde schijf als de bron: dan overschrijf je juist de bestanden die je wilt \
                 herstellen. Kies een andere schijf."
                    .to_string(),
            );
        }
        let free = vol.total_bytes.saturating_sub(vol.used_bytes);
        if free < source.size() {
            return Some(format!(
                "Niet genoeg ruimte op {}: nodig {}, vrij {}.",
                vol.display_name(),
                disk::format_size(source.size()),
                disk::format_size(free)
            ));
        }
        if vol.filesystem.eq_ignore_ascii_case("FAT32") && source.size() > FAT32_MAX_FILE {
            return Some(format!("{} gebruikt FAT32, dat geen bestanden groter dan 4 GB kan opslaan.", vol.display_name()));
        }
        None
    }

    /// Progress or result of the copy. Returns the image to scan when asked.
    fn draw_job(&mut self, ui: &mut egui::Ui) -> Option<(String, String)> {
        let job = self.job.as_ref()?;
        let p = job.progress.lock().unwrap().clone();
        let elapsed = job.finished_after.unwrap_or_else(|| job.started_at.elapsed()).as_secs_f64();
        let speed = if elapsed > 0.5 { (p.done as f64 / elapsed) as u64 } else { 0 };
        let fraction = if p.total > 0 { p.done as f32 / p.total as f32 } else { 0.0 };
        let paused = job.paused.load(Ordering::Relaxed);
        let (cancel, paused_flag) = (job.cancel.clone(), job.paused.clone());

        let (title, color) = match &job.result {
            None if paused => ("Gepauzeerd".to_string(), theme::ACCENT),
            None => (format!("Image maken van {}…", job.source_label), theme::ACCENT),
            Some(Ok(done)) if done.cancelled => ("Gestopt. De image is onvolledig.".to_string(), theme::WARNING),
            Some(Ok(_)) => ("Image klaar".to_string(), theme::SUCCESS),
            Some(Err(e)) => (format!("Image maken mislukt: {e}"), theme::DANGER),
        };
        let mut detail = format!("{} van {}", disk::format_size(p.done), disk::format_size(p.total));
        if speed > 0 {
            detail.push_str(&format!(" · {}/s", disk::format_size(speed)));
            if job.running() && !paused {
                let left = p.total.saturating_sub(p.done) / speed.max(1);
                detail.push_str(&format!(" · nog {}", ui::fmt_duration(left)));
            }
        }

        let mut scan = None;
        let mut open = false;
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 8.0);
            ui.horizontal(|ui| {
                ui::progress_ring(ui, 54.0, fraction, matches!(job.result, Some(Ok(ref d)) if !d.cancelled));
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(egui::RichText::new(&title).font(theme::font_semibold(16.0)).color(color));
                    ui.label(egui::RichText::new(&detail).color(theme::TEXT_DIM));
                    ui.add(
                        egui::Label::new(egui::RichText::new(job.dest.display().to_string()).size(12.0).color(theme::TEXT_FAINT))
                            .truncate(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if job.running() {
                        if ui::icon_button(ui, icon::STOP, "Stoppen").clicked() {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        let (glyph, tip) = if paused { (icon::PLAY, "Hervatten") } else { (icon::PAUSE, "Pauzeren") };
                        if ui::icon_button(ui, glyph, tip).clicked() {
                            paused_flag.store(!paused, Ordering::Relaxed);
                        }
                    } else if matches!(job.result, Some(Ok(_))) {
                        if ui.add(ui::primary_icon_button(icon::MAGNIFYING_GLASS, "Image scannen").min_size(vec2(150.0, 36.0))).clicked() {
                            scan = Some((job.dest.display().to_string(), file_name(&job.dest)));
                        }
                        open = ui.add(ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, "Open in Verkenner")).clicked();
                    }
                });
            });
            if job.running() {
                ui.add_space(10.0);
                let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
                ui::progress_bar(ui, bar, fraction, theme::ACCENT);
            }
        });
        if p.unreadable > 0 {
            ui.add_space(8.0);
            notice(
                ui,
                icon::WARNING_CIRCLE,
                theme::WARNING,
                &format!(
                    "{} kon niet gelezen worden en is met nullen gevuld. De schijf heeft beschadigde sectoren; \
                     herstel voortaan uit deze image.",
                    disk::format_size(p.unreadable)
                ),
            );
        }
        if let Some(text) = &self.message {
            ui.add_space(8.0);
            notice(ui, icon::WARNING_CIRCLE, theme::DANGER, text);
        }
        if open {
            let dest = job.dest.clone();
            self.message = crate::largefiles::show_in_explorer(&dest).err().map(|e| format!("Verkenner openen mislukt: {e}"));
        }
        scan
    }
}
