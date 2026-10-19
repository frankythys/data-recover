//! Shared pieces for lists where several files can be ticked and removed in
//! one go: the "Alles selecteren" bar, the confirmation dialog and the
//! progress window that shows which file is being removed.

use crate::i18n::t;
use crate::tf;
use crate::cleanup::{dialog, dialog_title, dim_background, file_name, notice};
use crate::deleter::{DeleteJob, DeleteProgress, Mode};
use crate::largefiles::{self, LargeFile};
use crate::{disk, theme, ui};
use egui::vec2;
use egui_phosphor::regular as icon;

/// Names listed in the confirmation dialog before "en N meer".
const LISTED: usize = 6;

/// "Alles selecteren" checkbox with the count and size of what is ticked.
pub fn select_bar(ui: &mut egui::Ui, files: &[LargeFile], checked: &mut [bool]) {
    let ticked: Vec<&LargeFile> = files.iter().zip(checked.iter()).filter(|&(_, &c)| c).map(|(f, _)| f).collect();
    let all_on = !files.is_empty() && ticked.len() == files.len();
    ui.horizontal(|ui| {
        if ui::checkbox_label(ui, all_on, t("Alles selecteren", "Select all")) {
            checked.iter_mut().for_each(|c| *c = !all_on);
        }
        if !ticked.is_empty() {
            let bytes: u64 = ticked.iter().map(|f| f.size).sum();
            ui.label(
                egui::RichText::new(tf!("{} geselecteerd · {}", "{} selected · {}", ui::fmt_files(ticked.len()), disk::format_size(bytes)))
                    .color(theme::TEXT_DIM),
            );
        }
    });
}

/// The ticked files of a list.
pub fn ticked(files: &[LargeFile], checked: &[bool]) -> Vec<LargeFile> {
    files.iter().zip(checked).filter(|&(_, &c)| c).map(|(f, _)| f.clone()).collect()
}

/// Bottom bar text for a list with ticked files.
pub fn selection_label(ui: &mut egui::Ui, files: &[LargeFile], empty_hint: &str) {
    if files.is_empty() {
        ui.label(egui::RichText::new(empty_hint).color(theme::TEXT_DIM));
        return;
    }
    let title = if files.len() == 1 { file_name(&files[0].path) } else { tf!("{} geselecteerd", "{} selected", ui::fmt_files(files.len())) };
    ui.label(egui::RichText::new(title).font(theme::font_semibold(14.0)));
    let bytes: u64 = files.iter().map(|f| f.size).sum();
    ui.label(egui::RichText::new(disk::format_size(bytes)).color(theme::TEXT_DIM));
}

/// Confirmation and progress for removing a batch of files.
#[derive(Default)]
pub struct BulkDelete {
    confirm: Option<Vec<LargeFile>>,
    job: Option<DeleteJob>,
}

impl BulkDelete {
    /// Asks to remove `files` (the caller checked they may be removed).
    pub fn ask(&mut self, files: Vec<LargeFile>) {
        if !files.is_empty() {
            self.confirm = Some(files);
        }
    }

    pub fn busy(&self) -> bool {
        self.confirm.is_some() || self.job.is_some()
    }

    /// Draws the open dialog, if any. Returns the outcome once the user
    /// closes the summary after the files were removed.
    pub fn draw(&mut self, ctx: &egui::Context) -> Option<(Mode, DeleteProgress)> {
        if let Some(files) = self.confirm.clone() {
            self.draw_confirm(ctx, files);
            return None;
        }
        let job = self.job.as_ref()?;
        let progress = job.progress();
        let mode = job.mode;
        let mut close = false;
        let mut stop = false;
        dim_background(ctx);
        dialog("bulk_delete_progress", ctx, |ui| {
            if progress.finished {
                close = draw_summary(ui, mode, &progress);
            } else {
                stop = draw_running(ui, mode, &progress);
            }
        });
        if stop {
            job.stop();
        }
        if close {
            self.job = None;
            return Some((mode, progress));
        }
        None
    }

    fn draw_confirm(&mut self, ctx: &egui::Context, files: Vec<LargeFile>) {
        let total: u64 = files.iter().map(|f| f.size).sum();
        let mut decision: Option<Option<Mode>> = None;
        dim_background(ctx);
        dialog("bulk_delete_confirm", ctx, |ui| {
            let title = if files.len() == 1 { t("Bestand verwijderen?", "Remove file?").to_string() } else { tf!("{} bestanden verwijderen?", "Remove {} files?", files.len()) };
            dialog_title(ui, icon::TRASH, theme::DANGER, &title, &tf!("Samen {}", "Together {}", disk::format_size(total)));
            for f in files.iter().take(LISTED) {
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(egui::RichText::new(file_name(&f.path)).font(theme::font_medium(13.0))).truncate());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(disk::format_size(f.size)).color(theme::TEXT_DIM));
                    });
                });
            }
            if files.len() > LISTED {
                ui.label(egui::RichText::new(tf!("en nog {} meer", "and {} more", ui::fmt_files(files.len() - LISTED))).color(theme::TEXT_DIM));
            }
            ui.add_space(4.0);
            if let Some(warning) = files.iter().find_map(|f| largefiles::removal_warning(&f.path)) {
                notice(ui, icon::WARNING_CIRCLE, theme::DANGER, warning);
            }
            notice(
                ui,
                icon::INFO,
                theme::ACCENT,
                t(
                    "Definitief verwijderen maakt de ruimte meteen vrij. Via de prullenbak kun je het nog terugzetten, \
                     maar komt de ruimte pas vrij als je de prullenbak leegt; bestanden die te groot zijn voor de \
                     prullenbak laat Windows je eerst bevestigen.",
                    "Deleting permanently frees the space right away. Via the Recycle Bin you can still restore it, \
                     but the space is only freed when you empty the Recycle Bin; files too large for it Windows \
                     asks you to confirm first.",
                ),
            );
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(ui::danger_icon_button(icon::TRASH, t("Definitief verwijderen", "Delete permanently")).min_size(vec2(0.0, 36.0))).clicked() {
                    decision = Some(Some(Mode::Permanent));
                }
                if ui.add(ui::secondary_icon_button(icon::RECYCLE, t("Naar prullenbak", "To Recycle Bin")).min_size(vec2(0.0, 36.0))).clicked() {
                    decision = Some(Some(Mode::RecycleBin));
                }
                if ui.add(ui::secondary_icon_button(icon::X, t("Annuleren", "Cancel")).min_size(vec2(0.0, 36.0))).clicked() {
                    decision = Some(None);
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            decision = Some(None);
        }
        if let Some(choice) = decision {
            self.confirm = None;
            if let Some(mode) = choice {
                self.job = Some(DeleteJob::start(files, mode, ctx));
            }
        }
    }
}

/// Progress while removing. Returns whether Stop was clicked.
fn draw_running(ui: &mut egui::Ui, mode: Mode, p: &DeleteProgress) -> bool {
    let title = match mode {
        Mode::Permanent => t("Bezig met verwijderen…", "Removing…"),
        Mode::RecycleBin => t("Bezig met verplaatsen naar de prullenbak…", "Moving to the Recycle Bin…"),
    };
    let sub = tf!(
        "{} van {} · {} van {}",
        "{} of {} · {} of {}",
        p.done.min(p.total),
        p.total,
        disk::format_size(p.bytes_done),
        disk::format_size(p.bytes_total)
    );
    dialog_title(ui, icon::TRASH, theme::DANGER, title, &sub);
    ui.add_space(4.0);
    let fraction = if p.bytes_total > 0 { p.bytes_done as f32 / p.bytes_total as f32 } else { 0.0 };
    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), egui::Sense::hover());
    ui::progress_bar(ui, bar, fraction, theme::ACCENT);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(14.0).color(theme::ACCENT));
        ui.add(egui::Label::new(egui::RichText::new(&p.current).size(12.5).color(theme::TEXT_DIM)).truncate());
    });
    ui.add_space(8.0);
    let mut stop = false;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        stop = ui.add(ui::secondary_icon_button(icon::STOP, t("Stoppen", "Stop")).min_size(vec2(120.0, 36.0))).clicked();
    });
    stop
}

/// Result after removing. Returns whether "Sluiten" was clicked.
fn draw_summary(ui: &mut egui::Ui, mode: Mode, p: &DeleteProgress) -> bool {
    let freed = p.bytes_removed;
    let (glyph, color, title) = if p.failed.is_empty() && !p.cancelled {
        (icon::CHECK_CIRCLE, theme::SUCCESS, t("Klaar", "Done"))
    } else if p.cancelled {
        (icon::STOP, theme::ACCENT, t("Gestopt", "Stopped"))
    } else {
        (icon::WARNING_CIRCLE, theme::DANGER, t("Klaar, niet alles is gelukt", "Done, not everything worked"))
    };
    let sub = match mode {
        Mode::Permanent => tf!("{} verwijderd · {} vrijgemaakt", "{} removed · {} freed", ui::fmt_files(p.removed.len()), disk::format_size(freed)),
        Mode::RecycleBin => tf!("{} naar de prullenbak · {}", "{} to the Recycle Bin · {}", ui::fmt_files(p.removed.len()), disk::format_size(freed)),
    };
    dialog_title(ui, glyph, color, title, &sub);
    if !p.failed.is_empty() {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(tf!("Niet verwijderd ({}):", "Not removed ({}):", p.failed.len())).font(theme::font_medium(13.0)));
        for (path, reason) in p.failed.iter().take(LISTED) {
            ui.add(
                egui::Label::new(egui::RichText::new(format!("{} — {reason}", file_name(path))).size(12.0).color(theme::TEXT_DIM))
                    .truncate(),
            );
        }
    }
    if mode == Mode::RecycleBin && !p.removed.is_empty() {
        notice(ui, icon::INFO, theme::ACCENT, t("Leeg de prullenbak (Systeem opschonen) om de ruimte vrij te maken.", "Empty the Recycle Bin (System cleanup) to free the space."));
    }
    ui.add_space(8.0);
    let mut close = false;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        close = ui.add(ui::primary_icon_button(icon::CHECK, t("Sluiten", "Close")).min_size(vec2(120.0, 36.0))).clicked();
    });
    close
}
