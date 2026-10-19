//! "Snelheidstest" screen: pick a drive and a test size, confirm, and see
//! the four speeds fill in.

use crate::i18n::t;
use crate::tf;
use crate::cleanup::{dialog, dialog_title, dim_background, location_chip, notice};
use crate::disk::{self, DriveKind, VolumeInfo};
use crate::speed::{self, Phase, Rate, SpeedProgress};
use crate::{theme, ui};
use egui::vec2;
use egui_phosphor::regular as icon;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SIZES: [(&str, u64); 3] = [("256 MB", 256 << 20), ("1 GB", 1 << 30), ("4 GB", 4 << 30)];
const RANDOM_TIME: Duration = Duration::from_secs(5);

struct SpeedJob {
    letter: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<SpeedProgress>>,
    rx: Receiver<io::Result<SpeedProgress>>,
    done: Option<Result<SpeedProgress, String>>,
}

pub struct SpeedView {
    letter: Option<String>,
    size: usize,
    confirm: bool,
    job: Option<SpeedJob>,
}

impl SpeedView {
    pub fn new() -> Self {
        Self { letter: None, size: 1, confirm: false, job: None }
    }

    fn running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.done.is_none())
    }

    fn start(&mut self, letter: String, ctx: &egui::Context) {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(SpeedProgress::default()));
        let (tx, rx) = channel();
        let (thread_cancel, thread_progress, ctx) = (cancel.clone(), progress.clone(), ctx.clone());
        let folder = disk::writable_folder(&letter);
        let size = SIZES[self.size].1;
        std::thread::spawn(move || {
            let result = speed::run(&folder, size, RANDOM_TIME, &thread_cancel, |p| {
                *thread_progress.lock().unwrap() = p.clone();
                ctx.request_repaint();
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.job = Some(SpeedJob { letter, cancel, progress, rx, done: None });
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        if let Some(job) = self.job.as_mut() {
            if job.done.is_none() {
                if let Ok(result) = job.rx.try_recv() {
                    job.done = Some(result.map_err(|e| match e.kind() {
                        io::ErrorKind::Interrupted => t("Test gestopt.", "Test stopped.").to_string(),
                        io::ErrorKind::PermissionDenied => tf!("Geen toegang tot {}. Kies een andere schijf.", "No access to {}. Choose another drive.", job.letter),
                        _ => tf!("Test mislukt: {e}", "Test failed: {e}"),
                    }));
                }
            }
        }
        let running = self.running();

        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.heading(t("Snelheidstest", "Speed test"));
            ui.label(
                egui::RichText::new(t("Meet hoe snel een schijf leest en schrijft, in grote stukken en in kleine stukjes op willekeurige plekken.", "Measures how fast a drive reads and writes, in large blocks and in small pieces at random places."))
                    .color(theme::TEXT_DIM),
            );
        });
        ui.add_space(14.0);

        ui::section_header(ui, icon::HARD_DRIVE, t("Schijf", "Drive"), None);
        ui.add_enabled_ui(!running, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
                for v in volumes {
                    let glyph = if v.kind == DriveKind::External { icon::USB } else { icon::HARD_DRIVE };
                    let free = tf!("{} vrij", "{} free", disk::format_size(v.total_bytes.saturating_sub(v.used_bytes)));
                    if location_chip(ui, glyph, &v.display_name(), &free, self.letter.as_deref() == Some(v.letter.as_str())) {
                        self.letter = Some(v.letter.clone());
                    }
                }
            });
        });
        ui.add_space(12.0);

        let free = self
            .letter
            .as_ref()
            .and_then(|l| volumes.iter().find(|v| &v.letter == l))
            .map(|v| v.total_bytes.saturating_sub(v.used_bytes));
        let enough = free.is_some_and(|f| f > SIZES[self.size].1 * 2);
        let mut start = false;
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 8.0);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!running, |ui| {
                    ui.label(egui::RichText::new(t("Grootte testbestand", "Test file size")).color(theme::TEXT_DIM));
                    egui::ComboBox::from_id_salt("speed_size").width(90.0).selected_text(SIZES[self.size].0).show_ui(ui, |ui| {
                        for (i, (label, _)) in SIZES.iter().enumerate() {
                            ui.selectable_value(&mut self.size, i, *label);
                        }
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let button = ui::primary_icon_button(icon::SPEEDOMETER, t("Test starten", "Start test")).min_size(vec2(150.0, 36.0));
                    start = ui
                        .add_enabled(!running && enough, button)
                        .on_disabled_hover_text(t("Kies een schijf met minstens twee keer de testgrootte vrij", "Choose a drive with at least twice the test size free"))
                        .clicked();
                    if running {
                        if ui::icon_button(ui, icon::STOP, t("Stoppen", "Stop")).clicked() {
                            if let Some(job) = &self.job {
                                job.cancel.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                });
            });
        });
        if start {
            self.confirm = true;
        }
        ui.add_space(12.0);
        self.draw_results(ui);
        self.draw_confirm(ui.ctx());
    }

    fn draw_results(&self, ui: &mut egui::Ui) {
        let Some(job) = &self.job else { return };
        let p = match &job.done {
            Some(Ok(p)) => p.clone(),
            _ => job.progress.lock().unwrap().clone(),
        };
        if let Some(Err(e)) = &job.done {
            notice(ui, icon::WARNING_CIRCLE, theme::DANGER, e);
            ui.add_space(8.0);
        }
        let running = job.done.is_none();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
            let tiles = [
                (Phase::SeqRead, t("Sequentieel lezen", "Sequential read"), p.seq_read),
                (Phase::SeqWrite, t("Sequentieel schrijven", "Sequential write"), p.seq_write),
                (Phase::RandomRead, t("Willekeurig 4K lezen", "Random 4K read"), p.random_read),
                (Phase::RandomWrite, t("Willekeurig 4K schrijven", "Random 4K write"), p.random_write),
            ];
            for (phase, title, rate) in tiles {
                let active = running && p.phase == Some(phase);
                result_tile(ui, title, rate, active, if active { p.fraction } else { 0.0 }, phase);
            }
        });
        if matches!(job.done, Some(Ok(_))) {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    t(
                        "Sequentieel: grote bestanden kopiëren. Willekeurig 4K: opstarten en programma's openen, waar SSD's veel sneller zijn dan harde schijven.",
                        "Sequential: copying large files. Random 4K: starting Windows and opening programs, where SSDs are much faster than hard disks.",
                    ),
                )
                .size(12.5)
                .color(theme::TEXT_DIM),
            );
        }
    }

    fn draw_confirm(&mut self, ctx: &egui::Context) {
        if !self.confirm {
            return;
        }
        let Some(letter) = self.letter.clone() else {
            self.confirm = false;
            return;
        };
        let mut decision = None;
        dim_background(ctx);
        dialog("speed_confirm", ctx, |ui| {
            dialog_title(ui, icon::SPEEDOMETER, theme::ACCENT, t("Snelheidstest starten?", "Start speed test?"), &tf!("Schijf {letter}", "Drive {letter}"));
            notice(
                ui,
                icon::WARNING_CIRCLE,
                theme::DANGER,
                &tf!(
                    "De test schrijft tijdelijk {} naar {letter}. Daardoor kunnen verwijderde bestanden op deze schijf \
                     overschreven worden. Wil je nog bestanden herstellen van {letter}, doe dat dan eerst.",
                    "The test temporarily writes {} to {letter}. This can overwrite deleted files on this drive. \
                     If you still want to recover files from {letter}, do that first.",
                    SIZES[self.size].0
                ),
            );
            notice(ui, icon::INFO, theme::ACCENT, t("Het testbestand wordt na afloop altijd verwijderd. Je eigen bestanden blijven staan.", "The test file is always removed afterwards. Your own files stay."));
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(ui::primary_icon_button(icon::SPEEDOMETER, t("Starten", "Start")).min_size(vec2(130.0, 36.0))).clicked() {
                    decision = Some(true);
                }
                if ui.add(ui::secondary_icon_button(icon::X, t("Annuleren", "Cancel")).min_size(vec2(120.0, 36.0))).clicked() {
                    decision = Some(false);
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                self.confirm = false;
                self.start(letter, ctx);
            }
            Some(false) => self.confirm = false,
            None => {}
        }
    }
}

fn result_tile(ui: &mut egui::Ui, title: &str, rate: Option<Rate>, active: bool, fraction: f32, phase: Phase) {
    theme::card_frame().inner_margin(egui::Margin::symmetric(16.0, 14.0)).show(ui, |ui| {
        ui.set_width(210.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(egui::RichText::new(title).font(theme::font_medium(13.0)).color(theme::TEXT_DIM));
            match rate {
                Some(r) => {
                    ui.label(egui::RichText::new(format!("{}/s", disk::format_size(r.bytes_per_sec as u64))).font(theme::font_semibold(22.0)));
                    let detail = if matches!(phase, Phase::RandomRead | Phase::RandomWrite) {
                        format!("{} IOPS", ui::fmt_thousands(r.ops_per_sec as u64))
                    } else {
                        t("blokken van 1 MB", "1 MB blocks").to_string()
                    };
                    ui.label(egui::RichText::new(detail).size(12.0).color(theme::TEXT_FAINT));
                }
                None if active => {
                    ui.add(egui::Spinner::new().size(22.0).color(theme::ACCENT));
                    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), egui::Sense::hover());
                    ui::progress_bar(ui, bar, fraction, theme::ACCENT);
                }
                None => {
                    ui.label(egui::RichText::new("—").font(theme::font_semibold(22.0)).color(theme::TEXT_FAINT));
                    ui.label(egui::RichText::new(" ").size(12.0));
                }
            }
        });
    });
}
