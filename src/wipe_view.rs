//! "Veilig wissen" screen: wipe the free space of a drive, or shred chosen
//! files, so deleted data can no longer be recovered.

use crate::i18n::t;
use crate::tf;
use crate::cleanup::{dialog, dialog_title, dim_background, file_name, location_chip, notice};
use crate::disk::{self, DriveKind, VolumeInfo};
use crate::wipe::{self, WipeProgress};
use crate::{theme, ui};
use egui::{Sense, vec2};
use egui_phosphor::regular as icon;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Files listed before "en N meer".
const LISTED: usize = 8;

type ShredResults = Vec<(PathBuf, Result<u64, String>)>;

struct FreeJob {
    letter: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<WipeProgress>>,
    rx: Receiver<io::Result<WipeProgress>>,
    started_at: Instant,
    done: Option<Result<WipeProgress, String>>,
}

#[derive(PartialEq)]
enum Confirm {
    FreeSpace,
    Files,
}

pub struct WipeView {
    letter: Option<String>,
    free_job: Option<FreeJob>,
    files: Vec<PathBuf>,
    shred_rx: Option<Receiver<ShredResults>>,
    shredded: Option<ShredResults>,
    confirm: Option<Confirm>,
}

impl WipeView {
    pub fn new() -> Self {
        Self { letter: None, free_job: None, files: Vec::new(), shred_rx: None, shredded: None, confirm: None }
    }

    fn wiping(&self) -> bool {
        self.free_job.as_ref().is_some_and(|j| j.done.is_none())
    }

    fn poll(&mut self) {
        if let Some(job) = self.free_job.as_mut().filter(|j| j.done.is_none()) {
            if let Ok(result) = job.rx.try_recv() {
                job.done = Some(result.map_err(|e| match e.kind() {
                    io::ErrorKind::Interrupted => t("Wissen gestopt. De vrije ruimte is maar voor een deel gewist.", "Wipe stopped. Only part of the free space was wiped.").to_string(),
                    io::ErrorKind::PermissionDenied => tf!("Geen toegang tot {}.", "No access to {}.", job.letter),
                    _ => tf!("Wissen mislukt: {e}", "Wipe failed: {e}"),
                }));
            }
        }
        if let Some(results) = self.shred_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.files.clear();
            self.shredded = Some(results);
            self.shred_rx = None;
        }
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        self.poll();
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.heading(t("Veilig wissen", "Secure erase"));
            ui.label(
                egui::RichText::new(t("Zorg dat verwijderde bestanden niet meer te herstellen zijn, bijvoorbeeld voordat je een schijf weggeeft.", "Make sure deleted files can no longer be recovered, for example before you give a drive away."))
                    .color(theme::TEXT_DIM),
            );
        });
        ui.add_space(14.0);
        egui::ScrollArea::vertical().id_salt("wipe").auto_shrink([false, false]).show(ui, |ui| {
            self.draw_free_space(ui, volumes);
            ui.add_space(20.0);
            self.draw_files(ui);
            ui.add_space(14.0);
            notice(
                ui,
                icon::INFO,
                theme::ACCENT,
                t(
                    "Eén keer overschrijven is genoeg voor moderne schijven. Bij SSD's kan de schijf zelf oude kopieën \
                     bewaren die je niet kunt overschrijven; Windows wist verwijderde gegevens daar meestal al via TRIM.",
                    "Overwriting once is enough for modern disks. An SSD may keep old copies itself that cannot be \
                     overwritten; there Windows usually clears deleted data already via TRIM.",
                ),
            );
        });
        self.draw_confirm(ui.ctx(), volumes);
    }

    fn draw_free_space(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        ui::section_header(ui, icon::BROOM, t("Vrije ruimte wissen", "Wipe free space"), None);
        ui.label(
            egui::RichText::new(t("Overschrijft alle vrije ruimte met nullen. Je bestanden blijven staan; verwijderde bestanden zijn daarna weg.", "Overwrites all free space with zeros. Your files stay; deleted files are gone afterwards."))
                .color(theme::TEXT_DIM),
        );
        ui.add_space(8.0);
        let wiping = self.wiping();
        ui.add_enabled_ui(!wiping, |ui| {
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
        ui.add_space(10.0);
        let button = ui::danger_icon_button(icon::BROOM, t("Vrije ruimte wissen…", "Wipe free space…")).min_size(vec2(190.0, 36.0));
        if ui.add_enabled(self.letter.is_some() && !wiping, button).clicked() {
            self.confirm = Some(Confirm::FreeSpace);
        }

        let Some(job) = &self.free_job else { return };
        ui.add_space(10.0);
        let p = match &job.done {
            Some(Ok(p)) => p.clone(),
            _ => job.progress.lock().unwrap().clone(),
        };
        let fraction = if p.expected > 0 { (p.written as f32 / p.expected as f32).min(1.0) } else { 0.0 };
        let elapsed = job.started_at.elapsed().as_secs_f64();
        let speed = if elapsed > 0.5 { (p.written as f64 / elapsed) as u64 } else { 0 };
        let (title, color) = match &job.done {
            None => (tf!("Vrije ruimte van {} wissen…", "Wiping free space of {}…", job.letter), theme::ACCENT),
            Some(Ok(_)) => (tf!("Klaar: {} overschreven", "Done: {} overwritten", disk::format_size(p.written)), theme::SUCCESS),
            Some(Err(e)) => (e.clone(), theme::DANGER),
        };
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 8.0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(egui::RichText::new(&title).font(theme::font_semibold(15.0)).color(color));
                    if job.done.is_none() {
                        let mut detail = tf!("{} van ongeveer {}", "{} of about {}", disk::format_size(p.written), disk::format_size(p.expected));
                        if speed > 0 {
                            let left = p.expected.saturating_sub(p.written) / speed.max(1);
                            detail.push_str(&tf!(" · {}/s · nog {}", " · {}/s · {} left", disk::format_size(speed), ui::fmt_duration(left)));
                        }
                        ui.label(egui::RichText::new(detail).color(theme::TEXT_DIM));
                    }
                });
                if job.done.is_none() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui::icon_button(ui, icon::STOP, t("Stoppen", "Stop")).clicked() {
                            job.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                }
            });
            if job.done.is_none() {
                ui.add_space(10.0);
                let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
                ui::progress_bar(ui, bar, fraction, theme::DANGER);
            }
        });
    }

    fn draw_files(&mut self, ui: &mut egui::Ui) {
        ui::section_header(ui, icon::FILE_X, t("Bestanden veilig verwijderen", "Securely delete files"), None);
        ui.label(
            egui::RichText::new(t("Overschrijft de gekozen bestanden voordat ze verwijderd worden. Ze gaan niet naar de prullenbak.", "Overwrites the chosen files before they are deleted. They do not go to the Recycle Bin."))
                .color(theme::TEXT_DIM),
        );
        ui.add_space(8.0);
        let busy = self.shred_rx.is_some();
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, ui::secondary_icon_button(icon::FOLDER_OPEN, t("Bestanden kiezen…", "Choose files…"))).clicked() {
                if let Some(picked) = rfd::FileDialog::new().pick_files() {
                    self.files = picked.into_iter().filter(|p| !crate::largefiles::is_system_or_app(p)).collect();
                    self.shredded = None;
                }
            }
            let button = ui::danger_icon_button(icon::FILE_X, t("Veilig verwijderen…", "Securely delete…"));
            if ui.add_enabled(!busy && !self.files.is_empty(), button).clicked() {
                self.confirm = Some(Confirm::Files);
            }
            if busy {
                ui.add(egui::Spinner::new().size(18.0).color(theme::DANGER));
            }
        });
        for path in self.files.iter().take(LISTED) {
            ui.label(egui::RichText::new(path.display().to_string()).size(12.5).color(theme::TEXT_DIM));
        }
        if self.files.len() > LISTED {
            ui.label(egui::RichText::new(tf!("en nog {} meer", "and {} more", ui::fmt_files(self.files.len() - LISTED))).color(theme::TEXT_DIM));
        }
        if let Some(results) = &self.shredded {
            let ok: Vec<u64> = results.iter().filter_map(|(_, r)| r.as_ref().ok().copied()).collect();
            ui.add_space(6.0);
            notice(
                ui,
                icon::CHECK_CIRCLE,
                theme::SUCCESS,
                &tf!("{} veilig verwijderd ({}).", "{} securely deleted ({}).", ui::fmt_files(ok.len()), disk::format_size(ok.iter().sum())),
            );
            for (path, r) in results {
                if let Err(e) = r {
                    notice(ui, icon::WARNING_CIRCLE, theme::DANGER, &format!("{}: {e}", file_name(path)));
                }
            }
        }
    }

    fn draw_confirm(&mut self, ctx: &egui::Context, volumes: &[VolumeInfo]) {
        let Some(what) = &self.confirm else { return };
        let mut decision = None;
        dim_background(ctx);
        dialog("wipe_confirm", ctx, |ui| match what {
            Confirm::FreeSpace => {
                let letter = self.letter.clone().unwrap_or_default();
                dialog_title(ui, icon::BROOM, theme::DANGER, t("Vrije ruimte wissen?", "Wipe free space?"), &tf!("Schijf {letter}", "Drive {letter}"));
                notice(
                    ui,
                    icon::WARNING_CIRCLE,
                    theme::DANGER,
                    &tf!(
                        "Verwijderde bestanden op {letter} zijn daarna voorgoed weg, ook voor deze app. Wil je nog iets \
                         herstellen van {letter}, doe dat dan eerst.",
                        "Deleted files on {letter} are gone for good afterwards, also for this app. If you still want to \
                         recover something from {letter}, do that first."
                    ),
                );
                notice(
                    ui,
                    icon::INFO,
                    theme::ACCENT,
                    t(
                        "De schijf is tijdens het wissen even helemaal vol. Sluit andere programma's; de ruimte komt na afloop vanzelf terug.",
                        "The drive is completely full for a while during the wipe. Close other programs; the space comes back afterwards.",
                    ),
                );
                decision = confirm_buttons(ui, t("Wissen", "Wipe"));
            }
            Confirm::Files => {
                let title = tf!("{} veilig verwijderen?", "Securely delete {}?", ui::fmt_files(self.files.len()));
                dialog_title(ui, icon::FILE_X, theme::DANGER, &title, t("Niet via de prullenbak", "Not via the Recycle Bin"));
                notice(ui, icon::WARNING_CIRCLE, theme::DANGER, t("Deze bestanden zijn daarna niet meer terug te halen, ook niet met deze app.", "These files can no longer be recovered afterwards, not even with this app."));
                decision = confirm_buttons(ui, t("Verwijderen", "Delete"));
            }
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                if self.confirm.take() == Some(Confirm::FreeSpace) {
                    self.start_free_space(volumes, ctx);
                } else {
                    self.start_shred(ctx);
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn start_free_space(&mut self, volumes: &[VolumeInfo], ctx: &egui::Context) {
        let Some(letter) = self.letter.clone() else { return };
        let expected = volumes.iter().find(|v| v.letter == letter).map_or(0, |v| v.total_bytes.saturating_sub(v.used_bytes));
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(WipeProgress { expected, ..Default::default() }));
        let (tx, rx) = channel();
        let (thread_cancel, thread_progress, ctx) = (cancel.clone(), progress.clone(), ctx.clone());
        let folder = disk::writable_folder(&letter);
        std::thread::spawn(move || {
            let result = wipe::wipe_free_space(&folder, expected, &thread_cancel, |p| {
                *thread_progress.lock().unwrap() = p.clone();
                ctx.request_repaint();
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.free_job = Some(FreeJob { letter, cancel, progress, rx, started_at: Instant::now(), done: None });
    }

    fn start_shred(&mut self, ctx: &egui::Context) {
        let files = self.files.clone();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let results = files.into_iter().map(|p| {
                let r = wipe::shred(&p).map_err(|e| e.to_string());
                (p, r)
            });
            let _ = tx.send(results.collect());
            ctx.request_repaint();
        });
        self.shred_rx = Some(rx);
    }
}

/// "Annuleren" and a red confirm button. Returns the choice once made.
fn confirm_buttons(ui: &mut egui::Ui, action: &str) -> Option<bool> {
    let mut decision = None;
    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui.add(ui::danger_icon_button(icon::TRASH, action).min_size(vec2(130.0, 36.0))).clicked() {
            decision = Some(true);
        }
        if ui.add(ui::secondary_icon_button(icon::X, t("Annuleren", "Cancel")).min_size(vec2(120.0, 36.0))).clicked() {
            decision = Some(false);
        }
    });
    decision
}
