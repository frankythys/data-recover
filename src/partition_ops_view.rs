//! Pending partition operations on the "Partities" screen: a dialog per
//! operation, the queue ("Wachtende bewerkingen") and applying it in one go
//! after confirmation, with the outcome in a dialog.

use crate::cleanup::{dialog, dialog_title, dim_background, notice};
use crate::disk::{self, VolumeInfo};
use crate::partition_ops::{self, Fs, Guard, PartOp, Target};
use crate::partitions::PartInfo;
use crate::{theme, ui};
use egui::vec2;
use egui_phosphor::regular as icon;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

const MIB: u64 = 1 << 20;
const GIB: f64 = (1u64 << 30) as f64;

/// What is selected on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Selection {
    Part { disk: u32, number: u32 },
    Free { disk: u32, start: u64, end: u64 },
}

impl Selection {
    pub fn disk(&self) -> u32 {
        match self {
            Selection::Part { disk, .. } | Selection::Free { disk, .. } => *disk,
        }
    }
}

/// An operation the user picked.
pub enum Request {
    Resize(Target, u64),
    Create { disk: u32, start: u64, end: u64 },
    Letter(Target),
    Label(Target, String),
    SetActive(Target),
    Hide(Target, char),
    Show(Target),
    Check(Target),
    Optimize(Target),
    Format(Target),
    Wipe(Target, Fs),
    Delete(Target),
    /// Read-only details: title and (name, value) rows.
    Properties(String, Vec<(String, String)>),
}

enum Dialog {
    Resize { target: Target, from: u64, rx: Option<Receiver<Result<(u64, u64), String>>>, range: Option<Result<(u64, u64), String>>, gb: f64 },
    Create { disk: u32, offset: u64, max: u64, gb: f64, fs: Fs, label: String, letter: Option<char> },
    Letter { target: Target, letter: Option<char> },
    Label { target: Target, label: String },
    Format { target: Target, fs: Fs, label: String },
    Wipe { target: Target, fs: Fs },
    Delete { target: Target },
    Properties { title: String, rows: Vec<(String, String)> },
}

type Outcome = Vec<(String, Result<String, String>)>;

struct ApplyJob {
    total: usize,
    /// (operations done, the one running now)
    progress: Arc<Mutex<(usize, String)>>,
    rx: Receiver<Outcome>,
}

#[derive(Default)]
pub struct OpsPanel {
    queue: Vec<PartOp>,
    dialog: Option<Dialog>,
    confirm_apply: bool,
    apply: Option<ApplyJob>,
    outcome: Option<Outcome>,
    /// Why the last picked operation was refused.
    refused: Option<String>,
    /// Set after operations ran, so the layout and drive letters are reread.
    changed: bool,
}

/// Name of a partition for the user: its drive letter and label if it has
/// one, otherwise its number.
pub fn part_name(p: &PartInfo, disk: u32, volume: Option<&VolumeInfo>) -> String {
    volume.map_or_else(|| format!("Partitie {} (schijf {disk})", p.number), VolumeInfo::display_name)
}

impl OpsPanel {
    pub fn busy(&self) -> bool {
        self.apply.is_some()
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Takes the last operation off the queue.
    pub fn undo(&mut self) {
        self.queue.pop();
        self.refused = None;
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.refused = None;
    }

    pub fn ask_apply(&mut self) {
        if !self.queue.is_empty() && !self.busy() {
            self.confirm_apply = true;
        }
    }

    /// Whether operations ran since the last call.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// Letters taken by drives or by operations already in the queue.
    fn guard(&self, base: &Guard) -> Guard {
        let mut guard = base.clone();
        for op in &self.queue {
            match op {
                PartOp::SetLetter { letter, .. } | PartOp::Create { letter: Some(letter), .. } => guard.used_letters.push(*letter),
                _ => {}
            }
        }
        guard
    }

    /// Queues `op` unless it is not allowed.
    fn push(&mut self, op: PartOp, base: &Guard) {
        self.refused = self.guard(base).refuse(&op);
        if self.refused.is_none() {
            self.queue.push(op);
        }
    }

    pub fn open(&mut self, request: Request, ctx: &egui::Context, base: &Guard) {
        self.refused = None;
        self.dialog = Some(match request {
            Request::Resize(target, from) => {
                let (tx, rx) = channel();
                let (disk, part, ctx) = (target.disk, target.part, ctx.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(partition_ops::supported_size(disk, part));
                    ctx.request_repaint();
                });
                Dialog::Resize { target, from, rx: Some(rx), range: None, gb: from as f64 / GIB }
            }
            Request::Create { disk, start, end } => {
                // Partitions start on a MiB boundary.
                let offset = start.div_ceil(MIB) * MIB;
                let max = end.saturating_sub(offset) / MIB * MIB;
                Dialog::Create { disk, offset, max, gb: max as f64 / GIB, fs: Fs::Ntfs, label: String::new(), letter: None }
            }
            Request::Letter(target) => Dialog::Letter { target, letter: None },
            Request::Label(target, label) => Dialog::Label { target, label },
            Request::Format(target) => Dialog::Format { target, fs: Fs::Ntfs, label: String::new() },
            Request::Wipe(target, fs) => Dialog::Wipe { target, fs },
            Request::Delete(target) => Dialog::Delete { target },
            Request::Properties(title, rows) => Dialog::Properties { title, rows },
            Request::SetActive(target) => return self.push(PartOp::SetActive { target }, base),
            Request::Hide(target, letter) => return self.push(PartOp::Hide { target, letter }, base),
            Request::Show(target) => return self.push(PartOp::Show { target }, base),
            Request::Check(target) => return self.push(PartOp::Check { target }, base),
            Request::Optimize(target) => return self.push(PartOp::Optimize { target }, base),
        });
    }

    /// "Wachtende bewerkingen": the queue, for the side panel.
    pub fn draw_pending(&mut self, ui: &mut egui::Ui) {
        if let Some(why) = &self.refused {
            notice(ui, icon::WARNING_CIRCLE, theme::DANGER, why);
            ui.add_space(4.0);
        }
        if self.queue.is_empty() {
            ui.label(egui::RichText::new("Geen").color(theme::TEXT_FAINT));
            return;
        }
        let mut remove = None;
        for (i, op) in self.queue.iter().enumerate() {
            ui.horizontal(|ui| {
                let color = if op.is_destructive() { theme::DANGER } else { theme::TEXT_MAIN };
                let text = egui::RichText::new(format!("{}. {}", i + 1, op.describe())).size(12.5).color(color);
                ui.add(egui::Label::new(text).wrap());
            });
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                if ui.small_button("Weghalen").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.queue.remove(i);
        }
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Er verandert pas iets na Toepassen.").size(12.0).color(theme::TEXT_DIM));
    }

    /// The open dialog: an operation, the apply confirmation, the progress
    /// or the outcome.
    pub fn draw_dialogs(&mut self, ctx: &egui::Context, base: &Guard) {
        if let Some(outcome) = self.apply.as_ref().and_then(|j| j.rx.try_recv().ok()) {
            self.apply = None;
            self.outcome = Some(outcome);
            self.changed = true;
        }
        if self.apply.is_some() {
            self.draw_running(ctx);
        } else if self.outcome.is_some() {
            self.draw_outcome(ctx);
        } else if self.confirm_apply {
            self.draw_apply_confirm(ctx);
        } else if self.dialog.is_some() {
            self.draw_operation(ctx, base);
        }
    }

    fn draw_operation(&mut self, ctx: &egui::Context, base: &Guard) {
        let guard = self.guard(base);
        let Some(state) = self.dialog.as_mut() else { return };
        let info_only = matches!(state, Dialog::Properties { .. });
        let mut decision: Option<Option<PartOp>> = None;
        dim_background(ctx);
        dialog("partition_op", ctx, |ui| {
            let candidate = draw_dialog(ui, state, &guard);
            let refusal = candidate.as_ref().and_then(|op| guard.refuse(op));
            if let Some(why) = &refusal {
                notice(ui, icon::WARNING_CIRCLE, theme::DANGER, why);
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if info_only {
                    if ui.add(ui::secondary_icon_button(icon::X, "Sluiten").min_size(vec2(120.0, 36.0))).clicked() {
                        decision = Some(None);
                    }
                    return;
                }
                let ok = candidate.is_some() && refusal.is_none();
                let destructive = candidate.as_ref().is_some_and(PartOp::is_destructive);
                let button = if destructive {
                    ui::danger_icon_button(icon::PLUS, "In de wachtrij")
                } else {
                    ui::primary_icon_button(icon::PLUS, "In de wachtrij")
                };
                if ui.add_enabled(ok, button.min_size(vec2(140.0, 36.0))).clicked() {
                    decision = Some(candidate.clone());
                }
                if ui.add(ui::secondary_icon_button(icon::X, "Annuleren").min_size(vec2(120.0, 36.0))).clicked() {
                    decision = Some(None);
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            decision = Some(None);
        }
        if let Some(choice) = decision {
            self.dialog = None;
            if let Some(op) = choice {
                self.queue.push(op);
            }
        }
    }

    fn draw_apply_confirm(&mut self, ctx: &egui::Context) {
        let mut decision = None;
        let destructive = self.queue.iter().any(PartOp::is_destructive);
        dim_background(ctx);
        dialog("partition_apply", ctx, |ui| {
            dialog_title(ui, icon::CHECK, theme::ACCENT, "Bewerkingen toepassen?", &format!("{} bewerkingen", self.queue.len()));
            for (i, op) in self.queue.iter().enumerate() {
                let color = if op.is_destructive() { theme::DANGER } else { theme::TEXT_MAIN };
                ui.label(egui::RichText::new(format!("{}. {}", i + 1, op.describe())).color(color));
            }
            ui.add_space(4.0);
            if destructive {
                notice(
                    ui,
                    icon::WARNING_CIRCLE,
                    theme::DANGER,
                    "De rode bewerkingen wissen alle bestanden op die partities. Wil je daar nog iets van herstellen, doe dat dan eerst.",
                );
            }
            notice(
                ui,
                icon::INFO,
                theme::ACCENT,
                "Windows voert de bewerkingen een voor een uit. Lukt er een niet, dan stopt het daar. Maak bij belangrijke gegevens eerst een back-up.",
            );
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = if destructive { ui::danger_icon_button(icon::CHECK, "Toepassen") } else { ui::primary_icon_button(icon::CHECK, "Toepassen") };
                if ui.add(button.min_size(vec2(130.0, 36.0))).clicked() {
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
                self.confirm_apply = false;
                self.start_apply(ctx);
            }
            Some(false) => self.confirm_apply = false,
            None => {}
        }
    }

    fn draw_running(&self, ctx: &egui::Context) {
        let Some(job) = &self.apply else { return };
        let (done, current) = job.progress.lock().unwrap().clone();
        dim_background(ctx);
        dialog("partition_running", ctx, |ui| {
            dialog_title(ui, icon::GEAR, theme::ACCENT, "Bewerkingen uitvoeren…", &format!("{} van {}", done + 1, job.total));
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(18.0).color(theme::ACCENT));
                ui.label(current);
            });
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), egui::Sense::hover());
            ui::progress_bar(ui, bar, done as f32 / job.total.max(1) as f32, theme::ACCENT);
            ui.label(egui::RichText::new("Sluit de app niet tot alles klaar is.").color(theme::TEXT_DIM));
        });
    }

    fn draw_outcome(&mut self, ctx: &egui::Context) {
        let Some(outcome) = &self.outcome else { return };
        let failed = outcome.iter().any(|(_, r)| r.is_err());
        let mut close = false;
        dim_background(ctx);
        dialog("partition_outcome", ctx, |ui| {
            let (glyph, color, title) = if failed {
                (icon::WARNING_CIRCLE, theme::DANGER, "Niet alles is gelukt")
            } else {
                (icon::CHECK_CIRCLE, theme::SUCCESS, "Klaar")
            };
            dialog_title(ui, glyph, color, title, &format!("{} bewerkingen", outcome.len()));
            for (text, result) in outcome {
                let (glyph, color, detail) = match result {
                    Ok(msg) => (icon::CHECK_CIRCLE, theme::SUCCESS, msg.as_str()),
                    Err(e) => (icon::X_CIRCLE, theme::DANGER, e.as_str()),
                };
                ui.horizontal_top(|ui| {
                    ui.label(egui::RichText::new(glyph).font(theme::icon(16.0)).color(color));
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(text).font(theme::font_medium(13.5)));
                        ui.add(egui::Label::new(egui::RichText::new(detail).size(12.5).color(color)).wrap());
                    });
                });
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.add(ui::primary_icon_button(icon::CHECK, "Sluiten").min_size(vec2(120.0, 36.0))).clicked();
            });
        });
        if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.outcome = None;
        }
    }

    fn start_apply(&mut self, ctx: &egui::Context) {
        let ops = std::mem::take(&mut self.queue);
        let total = ops.len();
        let progress = Arc::new(Mutex::new((0, String::new())));
        let (tx, rx) = channel();
        let (thread_progress, ctx) = (progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut outcome = Vec::new();
            let mut failed = false;
            for (i, op) in ops.iter().enumerate() {
                if failed {
                    outcome.push((op.describe(), Err("Niet uitgevoerd omdat een eerdere bewerking mislukte.".to_string())));
                    continue;
                }
                *thread_progress.lock().unwrap() = (i, op.describe());
                ctx.request_repaint();
                let result = op.run();
                failed = result.is_err();
                outcome.push((op.describe(), result));
            }
            let _ = tx.send(outcome);
            ctx.request_repaint();
        });
        self.apply = Some(ApplyJob { total, progress, rx });
    }
}

/// Draws the fields of a dialog. Returns the operation they describe, once
/// complete.
fn draw_dialog(ui: &mut egui::Ui, state: &mut Dialog, guard: &Guard) -> Option<PartOp> {
    match state {
        Dialog::Resize { target, from, rx, range, gb } => {
            dialog_title(ui, icon::ARROWS_OUT_LINE_HORIZONTAL, theme::ACCENT, "Vergroten of verkleinen", &target.name);
            if let Some(r) = rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                *range = Some(r);
                *rx = None;
            }
            match range {
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0).color(theme::ACCENT));
                        ui.label("Windows berekent wat kan…");
                    });
                    None
                }
                Some(Err(e)) => {
                    notice(ui, icon::WARNING_CIRCLE, theme::DANGER, e);
                    None
                }
                Some(Ok((min, max))) => {
                    let (min, max) = (*min, *max);
                    ui.label(format!("Nu {} · kan van {} tot {}", disk::format_size(*from), disk::format_size(min), disk::format_size(max)));
                    ui.add(egui::Slider::new(gb, min as f64 / GIB..=max as f64 / GIB).suffix(" GB").max_decimals(2));
                    let to = if *gb >= max as f64 / GIB - 0.005 { max } else { ((*gb * GIB) as u64 / MIB * MIB).clamp(min, max) };
                    notice(ui, icon::INFO, theme::ACCENT, "Vergroten kan alleen in vrije ruimte direct achter de partitie. De bestanden blijven staan.");
                    Some(PartOp::Resize { target: target.clone(), from: *from, to })
                }
            }
        }
        Dialog::Create { disk, offset, max, gb, fs, label, letter } => {
            dialog_title(ui, icon::PLUS, theme::ACCENT, "Nieuwe partitie", &format!("Schijf {disk} · {} vrij", disk::format_size(*max)));
            ui.add(egui::Slider::new(gb, 0.01..=*max as f64 / GIB).text("Grootte").suffix(" GB").max_decimals(2));
            fs_combo(ui, "create_fs", fs);
            label_edit(ui, label);
            letter_combo(ui, "create_letter", letter, guard, true);
            let size = if *gb >= *max as f64 / GIB - 0.005 { *max } else { ((*gb * GIB) as u64 / MIB * MIB).min(*max) };
            (size >= MIB).then(|| PartOp::Create { disk: *disk, offset: *offset, size, fs: *fs, label: label.clone(), letter: *letter })
        }
        Dialog::Letter { target, letter } => {
            dialog_title(ui, icon::HASH, theme::ACCENT, "Stationsletter wijzigen", &target.name);
            letter_combo(ui, "set_letter", letter, guard, false);
            notice(ui, icon::INFO, theme::ACCENT, "Programma's die naar de oude letter verwijzen, vinden hun bestanden daarna mogelijk niet meer.");
            letter.map(|l| PartOp::SetLetter { target: target.clone(), letter: l })
        }
        Dialog::Label { target, label } => {
            dialog_title(ui, icon::TAG, theme::ACCENT, "Label wijzigen", &target.name);
            label_edit(ui, label);
            Some(PartOp::SetLabel { target: target.clone(), label: label.clone() })
        }
        Dialog::Format { target, fs, label } => {
            dialog_title(ui, icon::ERASER, theme::DANGER, "Formatteren", &target.name);
            fs_combo(ui, "format_fs", fs);
            label_edit(ui, label);
            notice(ui, icon::WARNING_CIRCLE, theme::DANGER, "Alle bestanden op deze partitie worden gewist.");
            Some(PartOp::Format { target: target.clone(), fs: *fs, label: label.clone() })
        }
        Dialog::Wipe { target, fs } => {
            dialog_title(ui, icon::BROOM, theme::DANGER, "Partitie wissen", &target.name);
            fs_combo(ui, "wipe_fs", fs);
            notice(
                ui,
                icon::WARNING_CIRCLE,
                theme::DANGER,
                "Elke sector van de partitie wordt met nullen overschreven (volledig formatteren). Bestanden zijn daarna \
                 ook met deze app niet meer te herstellen. Dit kan lang duren.",
            );
            Some(PartOp::Wipe { target: target.clone(), fs: *fs })
        }
        Dialog::Delete { target } => {
            dialog_title(ui, icon::TRASH, theme::DANGER, "Partitie verwijderen", &target.name);
            notice(ui, icon::WARNING_CIRCLE, theme::DANGER, "De partitie en alle bestanden erop verdwijnen. De ruimte wordt niet-toegewezen.");
            Some(PartOp::Delete { target: target.clone() })
        }
        Dialog::Properties { title, rows } => {
            dialog_title(ui, icon::INFO, theme::ACCENT, "Eigenschappen", title);
            egui::Grid::new("partition_properties").striped(true).spacing(vec2(24.0, 6.0)).show(ui, |ui| {
                for (name, value) in rows.iter() {
                    ui.label(egui::RichText::new(name).color(theme::TEXT_DIM));
                    ui.label(value);
                    ui.end_row();
                }
            });
            None
        }
    }
}

fn fs_combo(ui: &mut egui::Ui, id: &str, fs: &mut Fs) {
    ui.horizontal(|ui| {
        ui.label("Bestandssysteem");
        egui::ComboBox::from_id_salt(id).selected_text(fs.label()).show_ui(ui, |ui| {
            for option in Fs::ALL {
                ui.selectable_value(fs, option, option.label());
            }
        });
    });
}

fn label_edit(ui: &mut egui::Ui, label: &mut String) {
    ui.horizontal(|ui| {
        ui.label("Label");
        ui.add(egui::TextEdit::singleline(label).desired_width(220.0).hint_text("bijvoorbeeld Gegevens"));
    });
}

/// Drive-letter choice from the free letters; `automatic` adds "Automatisch".
fn letter_combo(ui: &mut egui::Ui, id: &str, letter: &mut Option<char>, guard: &Guard, automatic: bool) {
    let free: Vec<char> = ('D'..='Z').filter(|l| !guard.used_letters.contains(l)).collect();
    let shown = letter.map_or_else(|| if automatic { "Automatisch".to_string() } else { "Kies…".to_string() }, |l| format!("{l}:"));
    ui.horizontal(|ui| {
        ui.label("Stationsletter");
        egui::ComboBox::from_id_salt(id).selected_text(shown).show_ui(ui, |ui| {
            if automatic {
                ui.selectable_value(letter, None, "Automatisch");
            }
            for l in free {
                ui.selectable_value(letter, Some(l), format!("{l}:"));
            }
        });
    });
}
