//! The partition operations on the "Partities" screen: action buttons for
//! the selected partition or free space, a dialog per operation, and a
//! queue of pending operations that is applied in one go after
//! confirmation, as in common partition managers.

use crate::cleanup::{dialog, dialog_title, dim_background, notice};
use crate::disk::{self, VolumeInfo};
use crate::partition_ops::{self, Fs, Guard, PartOp, Target};
use crate::partitions::{Layout, PartInfo};
use crate::{theme, ui};
use egui::vec2;
use egui_phosphor::regular as icon;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

const MIB: u64 = 1 << 20;
const GIB: f64 = (1u64 << 30) as f64;

/// What is selected on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// An action button that was clicked.
pub enum Request {
    Resize(Target, u64),
    Create { disk: u32, start: u64, end: u64 },
    Letter(Target),
    Label(Target, String),
    Check(Target),
    Optimize(Target),
    Format(Target),
    Delete(Target),
}

enum Dialog {
    Resize { target: Target, from: u64, rx: Option<Receiver<Result<(u64, u64), String>>>, range: Option<Result<(u64, u64), String>>, gb: f64 },
    Create { disk: u32, offset: u64, max: u64, gb: f64, fs: Fs, label: String, letter: Option<char> },
    Letter { target: Target, letter: Option<char> },
    Label { target: Target, label: String },
    Format { target: Target, fs: Fs, label: String },
    Delete { target: Target },
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
    /// Set after operations ran, so the layout and drive letters are reread.
    changed: bool,
}

/// Name of a partition for the user: its drive letter and label if it has
/// one, otherwise its number.
pub fn part_name(p: &PartInfo, disk: u32, volume: Option<&VolumeInfo>) -> String {
    volume.map_or_else(|| format!("Partitie {} (schijf {disk})", p.number), VolumeInfo::display_name)
}

/// The buttons for `sel`. `system` partitions get none.
pub fn action_buttons(ui: &mut egui::Ui, sel: Selection, layout: &Layout, volumes: &[VolumeInfo], busy: bool) -> Option<Request> {
    let mut request = None;
    ui.add_enabled_ui(!busy, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            match sel {
                Selection::Free { disk, start, end } => {
                    if ui.add(ui::primary_icon_button(icon::PLUS, "Nieuwe partitie…")).clicked() {
                        request = Some(Request::Create { disk, start, end });
                    }
                }
                Selection::Part { disk, number } => {
                    let Some(p) = layout.partitions.iter().find(|p| p.number == number) else { return };
                    if p.kind.is_system() {
                        ui.label(egui::RichText::new("Systeempartitie: Windows heeft deze nodig, daarom kun je hem hier niet aanpassen.").color(theme::TEXT_DIM));
                        return;
                    }
                    let volume = volumes.iter().find(|v| v.disk_index == Some(disk) && v.disk_offset == Some(p.start));
                    let target = Target { disk, part: number, name: part_name(p, disk, volume) };
                    let has_volume = volume.is_some();
                    let mut button = |ui: &mut egui::Ui, glyph: &str, text: &str, enabled: bool, req: Request| {
                        if ui.add_enabled(enabled, ui::secondary_icon_button(glyph, text)).on_disabled_hover_text("Deze partitie heeft geen stationsletter").clicked() {
                            request = Some(req);
                        }
                    };
                    button(ui, icon::ARROWS_OUT_LINE_HORIZONTAL, "Vergroten/verkleinen…", true, Request::Resize(target.clone(), p.size));
                    button(ui, icon::HASH, "Stationsletter…", true, Request::Letter(target.clone()));
                    let label = volume.map(|v| v.label.clone()).unwrap_or_default();
                    button(ui, icon::TAG, "Label…", has_volume, Request::Label(target.clone(), label));
                    button(ui, icon::STETHOSCOPE, "Controleren", has_volume, Request::Check(target.clone()));
                    button(ui, icon::LIGHTNING, "Optimaliseren", has_volume, Request::Optimize(target.clone()));
                    if ui.add(ui::danger_icon_button(icon::ERASER, "Formatteren…")).clicked() {
                        request = Some(Request::Format(target.clone()));
                    }
                    if ui.add(ui::danger_icon_button(icon::TRASH, "Verwijderen…")).clicked() {
                        request = Some(Request::Delete(target));
                    }
                }
            }
        });
    });
    request
}

impl OpsPanel {
    pub fn busy(&self) -> bool {
        self.apply.is_some()
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

    pub fn open(&mut self, request: Request, ctx: &egui::Context) {
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
            Request::Delete(target) => Dialog::Delete { target },
            Request::Check(target) => {
                self.queue.push(PartOp::Check { target });
                return;
            }
            Request::Optimize(target) => {
                self.queue.push(PartOp::Optimize { target });
                return;
            }
        });
    }

    /// The queue, the running operations or their outcome.
    pub fn draw_queue(&mut self, ui: &mut egui::Ui) {
        if let Some(outcome) = self.apply.as_ref().and_then(|j| j.rx.try_recv().ok()) {
            self.apply = None;
            self.outcome = Some(outcome);
            self.changed = true;
        }
        if self.queue.is_empty() && self.apply.is_none() && self.outcome.is_none() {
            return;
        }
        let mut remove = None;
        let mut clear = false;
        let mut apply = false;
        let mut close = false;
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 8.0);
            if let Some(job) = &self.apply {
                let (done, current) = job.progress.lock().unwrap().clone();
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(18.0).color(theme::ACCENT));
                    ui.label(egui::RichText::new(format!("Bezig ({} van {}): {current}", done + 1, job.total)).font(theme::font_semibold(14.0)));
                });
                ui.label(egui::RichText::new("Sluit de app niet tot alles klaar is.").color(theme::TEXT_DIM));
                return;
            }
            if let Some(outcome) = &self.outcome {
                ui.label(egui::RichText::new("Uitgevoerde bewerkingen").font(theme::font_semibold(15.0)));
                for (text, result) in outcome {
                    let (glyph, color, detail) = match result {
                        Ok(msg) => (icon::CHECK_CIRCLE, theme::SUCCESS, msg.clone()),
                        Err(e) => (icon::X_CIRCLE, theme::DANGER, e.clone()),
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(glyph).font(theme::icon(16.0)).color(color));
                        ui.label(egui::RichText::new(text).font(theme::font_medium(13.5)));
                        ui.label(egui::RichText::new(detail).color(color));
                    });
                }
                close = ui.add(ui::secondary_icon_button(icon::X, "Sluiten")).clicked();
                return;
            }
            ui.label(egui::RichText::new(format!("Wachtende bewerkingen ({})", self.queue.len())).font(theme::font_semibold(15.0)));
            ui.label(egui::RichText::new("Er verandert pas iets als je op Toepassen klikt.").size(12.5).color(theme::TEXT_DIM));
            ui.add_space(4.0);
            for (i, op) in self.queue.iter().enumerate() {
                ui.horizontal(|ui| {
                    let color = if op.is_destructive() { theme::DANGER } else { theme::TEXT_MAIN };
                    ui.label(egui::RichText::new(format!("{}. {}", i + 1, op.describe())).color(color));
                    if ui::icon_button(ui, icon::X, "Uit de wachtrij halen").clicked() {
                        remove = Some(i);
                    }
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                apply = ui.add(ui::primary_icon_button(icon::CHECK, "Toepassen")).clicked();
                clear = ui.add(ui::secondary_icon_button(icon::X, "Alles annuleren")).clicked();
            });
        });
        if let Some(i) = remove {
            self.queue.remove(i);
        }
        if clear {
            self.queue.clear();
        }
        if apply {
            self.confirm_apply = true;
        }
        if close {
            self.outcome = None;
        }
    }

    /// The open operation dialog or the confirmation before applying.
    pub fn draw_dialogs(&mut self, ctx: &egui::Context, base: &Guard) {
        if self.confirm_apply {
            self.draw_apply_confirm(ctx);
            return;
        }
        let guard = self.guard(base);
        let Some(dialog_state) = self.dialog.as_mut() else { return };
        let mut decision: Option<Option<PartOp>> = None;
        dim_background(ctx);
        dialog("partition_op", ctx, |ui| {
            let candidate = draw_dialog(ui, dialog_state, &guard);
            let refusal = candidate.as_ref().and_then(|op| guard.refuse(op));
            if let Some(why) = &refusal {
                notice(ui, icon::WARNING_CIRCLE, theme::DANGER, why);
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ok = candidate.is_some() && refusal.is_none();
                let button = ui::primary_icon_button(icon::PLUS, "In de wachtrij").min_size(vec2(140.0, 36.0));
                if ui.add_enabled(ok, button).clicked() {
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
        let destructive: Vec<String> = self.queue.iter().filter(|o| o.is_destructive()).map(PartOp::describe).collect();
        dim_background(ctx);
        dialog("partition_apply", ctx, |ui| {
            dialog_title(ui, icon::CHECK, theme::ACCENT, "Bewerkingen toepassen?", &format!("{} bewerkingen", self.queue.len()));
            for op in &self.queue {
                let color = if op.is_destructive() { theme::DANGER } else { theme::TEXT_MAIN };
                ui.label(egui::RichText::new(op.describe()).color(color));
            }
            ui.add_space(4.0);
            if !destructive.is_empty() {
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
                let button = if destructive.is_empty() { ui::primary_icon_button(icon::CHECK, "Toepassen") } else { ui::danger_icon_button(icon::CHECK, "Toepassen") };
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
                    notice(
                        ui,
                        icon::INFO,
                        theme::ACCENT,
                        "Vergroten kan alleen in vrije ruimte direct achter de partitie. De bestanden blijven staan.",
                    );
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
        Dialog::Delete { target } => {
            dialog_title(ui, icon::TRASH, theme::DANGER, "Partitie verwijderen", &target.name);
            notice(ui, icon::WARNING_CIRCLE, theme::DANGER, "De partitie en alle bestanden erop verdwijnen. De ruimte wordt niet-toegewezen.");
            Some(PartOp::Delete { target: target.clone() })
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
