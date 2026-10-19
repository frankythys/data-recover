//! "Programma's" tab of the cleanup screen: the big data folders of
//! installed programs, with advice per program, cache cleaning and a list
//! of model files that can go to the Recycle Bin.

use crate::i18n::t;
use crate::tf;
use crate::apps::{self, Cleanup, Spot};
use crate::bulk_view::{self, BulkDelete};
use crate::cleanup::{ROW_GAP, ROW_H, dialog, dialog_title, dim_background, file_name, notice, result_row};
use crate::deleter::Mode;
use crate::junk::CleanReport;
use crate::largefiles::{self, LargeFile};
use crate::{disk, theme, ui};
use egui::{Sense, vec2};
use egui_phosphor::regular as icon;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

/// Other program folders smaller than this are not shown.
const MIN_OTHER_SIZE: u64 = 500 << 20;
const MAX_OTHERS: usize = 15;

enum Measured {
    Spots(Vec<Spot>, usize),
    Size(usize, u64),
}

pub struct AppsView {
    spots: Vec<Spot>,
    /// How many of `spots` are known programs; the rest are other folders.
    known: usize,
    sizes: Vec<Option<u64>>,
    measure_rx: Option<Receiver<Measured>>,
    /// Location whose model files are shown.
    open: Option<usize>,
    models: Vec<LargeFile>,
    models_rx: Option<Receiver<Vec<LargeFile>>>,
    /// Ticked rows of `models`.
    checked: Vec<bool>,
    bulk: BulkDelete,
    confirm_cache: Option<usize>,
    cache_rx: Option<Receiver<(usize, CleanReport, u64)>>,
    message: Option<(String, bool)>,
    /// Set when something went to the Recycle Bin, so its size is remeasured.
    recycled: bool,
}

impl AppsView {
    pub fn new() -> Self {
        Self {
            spots: Vec::new(),
            known: 0,
            sizes: Vec::new(),
            measure_rx: None,
            open: None,
            models: Vec::new(),
            models_rx: None,
            checked: Vec::new(),
            bulk: BulkDelete::default(),
            confirm_cache: None,
            cache_rx: None,
            message: None,
            recycled: false,
        }
    }

    pub fn has_action_bar(&self) -> bool {
        self.open.is_some()
    }

    /// Whether something was moved to the Recycle Bin since the last call.
    pub fn take_recycled(&mut self) -> bool {
        std::mem::take(&mut self.recycled)
    }

    fn start_measure(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut spots = apps::known_spots();
            let known = spots.len();
            spots.extend(apps::other_program_folders(&spots));
            if tx.send(Measured::Spots(spots.clone(), known)).is_err() {
                return;
            }
            for (i, spot) in spots.iter().enumerate() {
                if tx.send(Measured::Size(i, apps::measure(spot))).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
        self.measure_rx = Some(rx);
        self.spots.clear();
        self.sizes.clear();
        self.open = None;
        self.message = None;
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.measure_rx {
            loop {
                match rx.try_recv() {
                    Ok(Measured::Spots(spots, known)) => {
                        self.sizes = vec![None; spots.len()];
                        self.spots = spots;
                        self.known = known;
                    }
                    Ok(Measured::Size(i, size)) => self.sizes[i] = Some(size),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.measure_rx = None;
                        break;
                    }
                }
            }
        }
        if let Some(rx) = &self.models_rx {
            if let Ok(models) = rx.try_recv() {
                self.checked = vec![false; models.len()];
                self.models = models;
                self.models_rx = None;
            }
        }
        if let Some(rx) = &self.cache_rx {
            if let Ok((i, report, size)) = rx.try_recv() {
                self.cache_rx = None;
                self.sizes[i] = Some(size);
                let mut text = tf!("{}: {} vrijgemaakt.", "{}: {} freed.", self.spots[i].title, disk::format_size(report.freed));
                if report.skipped > 0 {
                    text.push_str(&tf!(" {} in gebruik, overgeslagen.", " {} in use, skipped.", ui::fmt_files(report.skipped as usize)));
                }
                self.message = Some((text, true));
            }
        }
    }

    pub fn draw(&mut self, ui: &mut egui::Ui) {
        if self.measure_rx.is_none() && self.spots.is_empty() {
            self.start_measure(ui.ctx());
        }
        self.poll();
        match self.open {
            Some(i) => self.draw_models(ui, i),
            None => self.draw_spots(ui),
        }
        self.draw_cache_confirm(ui.ctx());
        if let Some((mode, done)) = self.bulk.draw(ui.ctx()) {
            self.models.retain(|m| !done.removed.contains(&m.path));
            self.checked = vec![false; self.models.len()];
            if let Some(size) = self.open.and_then(|i| self.sizes[i].as_mut()) {
                *size = size.saturating_sub(done.bytes_removed);
            }
            self.recycled |= mode == Mode::RecycleBin && !done.removed.is_empty();
        }
    }

    fn draw_spots(&mut self, ui: &mut egui::Ui) {
        let measuring = self.measure_rx.is_some();
        let total: u64 = self.sizes.iter().flatten().sum();
        let mut refresh = false;
        theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 16.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let (tile, _) = ui.allocate_exact_size(vec2(54.0, 54.0), Sense::hover());
                ui::icon_tile(ui, tile, icon::APP_WINDOW, theme::ACCENT, 28.0);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let title = if measuring {
                        tf!("Bezig met meten… {} gevonden", "Measuring… {} found", disk::format_size(total))
                    } else {
                        tf!("{} aan programmagegevens", "{} of program data", disk::format_size(total))
                    };
                    ui.label(egui::RichText::new(title).font(theme::font_semibold(18.0)));
                    ui.label(
                        egui::RichText::new(t("AI-modellen, emulators, Docker en caches. Per programma staat hoe je het veilig opruimt.", "AI models, emulators, Docker and caches. Each program says how to clean it up safely."))
                            .color(theme::TEXT_DIM),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(!measuring, |ui| {
                        refresh = ui::icon_button(ui, icon::ARROW_CLOCKWISE, t("Opnieuw meten", "Measure again")).clicked();
                    });
                    if measuring {
                        ui.add(egui::Spinner::new().size(20.0).color(theme::ACCENT));
                    }
                });
            });
        });
        if refresh {
            self.start_measure(ui.ctx());
        }
        ui.add_space(10.0);
        if let Some((text, success)) = &self.message {
            let (glyph, color) = if *success { (icon::CHECK_CIRCLE, theme::SUCCESS) } else { (icon::WARNING_CIRCLE, theme::DANGER) };
            notice(ui, glyph, color, text);
            ui.add_space(10.0);
        }

        let by_size = |range: std::ops::Range<usize>| {
            let mut v: Vec<usize> = range.collect();
            v.sort_by_key(|&i| std::cmp::Reverse(self.sizes.get(i).copied().flatten().unwrap_or(0)));
            v
        };
        let known = by_size(0..self.known);
        let others: Vec<usize> = by_size(self.known..self.spots.len())
            .into_iter()
            .filter(|&i| self.sizes[i].is_some_and(|s| s >= MIN_OTHER_SIZE))
            .take(MAX_OTHERS)
            .collect();

        let mut action = None;
        egui::ScrollArea::vertical().id_salt("cleanup_apps").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = ROW_GAP;
            for &i in &known {
                if let Some(a) = spot_card(ui, &self.spots[i], self.sizes[i], self.cache_rx.is_some()) {
                    action = Some((i, a));
                }
            }
            if !others.is_empty() {
                ui.add_space(8.0);
                ui::section_header(ui, icon::STACK, t("Overige programmagegevens", "Other program data"), Some(others.len()));
                for &i in &others {
                    if let Some(a) = spot_card(ui, &self.spots[i], self.sizes[i], self.cache_rx.is_some()) {
                        action = Some((i, a));
                    }
                }
            }
        });
        match action {
            Some((i, SpotAction::Explorer)) => self.show_in_explorer(&self.spots[i].folders[0].clone()),
            Some((i, SpotAction::Models)) => {
                let (tx, rx) = channel();
                let spot = self.spots[i].clone();
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let _ = tx.send(apps::model_files(&spot));
                    ctx.request_repaint();
                });
                self.models.clear();
                self.checked.clear();
                self.models_rx = Some(rx);
                self.open = Some(i);
            }
            Some((i, SpotAction::ClearCache)) => self.confirm_cache = Some(i),
            None => {}
        }
    }

    fn draw_models(&mut self, ui: &mut egui::Ui, open: usize) {
        let mut back = false;
        ui.horizontal(|ui| {
            back = ui::icon_button(ui, icon::ARROW_LEFT, t("Terug naar programma's", "Back to programs")).clicked();
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&self.spots[open].title).font(theme::font_semibold(18.0)));
        });
        if back {
            self.open = None;
            return;
        }
        let loading = self.models_rx.is_some();
        let total: u64 = self.models.iter().map(|m| m.size).sum();
        let summary = if loading {
            t("Modellen zoeken…", "Looking for models…").to_string()
        } else {
            tf!("{} · {}, grootste eerst", "{} · {}, largest first", ui::fmt_files(self.models.len()), disk::format_size(total))
        };
        ui.label(egui::RichText::new(summary).size(12.5).color(theme::TEXT_DIM));
        ui.add_space(8.0);
        if loading {
            ui.add(egui::Spinner::new().size(24.0).color(theme::ACCENT));
            return;
        }
        if self.models.is_empty() {
            ui::empty_card(ui, icon::CHECK_CIRCLE, t("Geen modelbestanden gevonden.", "No model files found."));
            return;
        }
        bulk_view::select_bar(ui, &self.models, &mut self.checked);
        ui.add_space(6.0);
        let mut clicked = None;
        let mut open_file = None;
        egui::ScrollArea::vertical()
            .id_salt(("cleanup_models", open))
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H + ROW_GAP, self.models.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = ROW_GAP;
                for i in range {
                    let m = &self.models[i];
                    let folder = m.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                    let row = result_row(ui, icon::BRAIN, theme::CAT_VIDEO, &file_name(&m.path), &folder, m.size, false, Some(self.checked[i]));
                    if row.double_clicked() {
                        open_file = Some(i);
                    } else if row.clicked() {
                        clicked = Some(i);
                    }
                }
            });
        if let Some(i) = clicked {
            self.checked[i] = !self.checked[i];
        }
        if let Some(i) = open_file {
            let path = self.models[i].path.clone();
            self.show_in_explorer(&path);
        }
    }

    pub fn draw_action_bar(&mut self, ui: &mut egui::Ui) {
        let ticked = bulk_view::ticked(&self.models, &self.checked);
        let mut open = false;
        let mut remove = false;
        ui.horizontal_centered(|ui| {
            bulk_view::selection_label(ui, &ticked, t("Vink modellen aan om ze te verwijderen", "Tick models to remove them"));
            if let Some((text, success)) = &self.message {
                let color = if *success { theme::SUCCESS } else { theme::DANGER };
                ui.add_space(12.0);
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = ui::danger_icon_button(icon::TRASH, t("Verwijderen…", "Remove…")).min_size(vec2(150.0, 38.0));
                remove = ui.add_enabled(!ticked.is_empty() && !self.bulk.busy(), button).clicked();
                let button = ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, t("Open in Verkenner", "Show in Explorer")).min_size(vec2(170.0, 38.0));
                open = ui
                    .add_enabled(ticked.len() == 1, button)
                    .on_disabled_hover_text(t("Vink precies één model aan", "Tick exactly one model"))
                    .clicked();
            });
        });
        if open {
            self.show_in_explorer(&ticked[0].path.clone());
        }
        if remove {
            let allowed = ticked.into_iter().filter(|m| apps::may_remove_model(&self.spots, &m.path)).collect();
            self.bulk.ask(allowed);
        }
    }

    fn show_in_explorer(&mut self, path: &std::path::Path) {
        if let Err(e) = largefiles::show_in_explorer(path) {
            self.message = Some((tf!("Verkenner openen mislukt: {e}", "Could not open Explorer: {e}"), false));
        }
    }

    fn draw_cache_confirm(&mut self, ctx: &egui::Context) {
        let Some(i) = self.confirm_cache else { return };
        let spot = self.spots[i].clone();
        let size = self.sizes[i].unwrap_or(0);
        let mut decision = None;
        dim_background(ctx);
        dialog("apps_cache_confirm", ctx, |ui| {
            dialog_title(ui, icon::BROOM, theme::ACCENT, &tf!("{} leegmaken?", "Empty {}?", spot.title), &tf!("{} wordt vrijgemaakt", "{} will be freed", disk::format_size(size)));
            for f in &spot.folders {
                ui.label(egui::RichText::new(f.display().to_string()).size(12.0).color(theme::TEXT_DIM));
            }
            ui.add_space(4.0);
            notice(ui, icon::INFO, theme::ACCENT, spot.advice);
            notice(
                ui,
                icon::WARNING_CIRCLE,
                theme::DANGER,
                t(
                    "De inhoud wordt definitief verwijderd (niet via de prullenbak). Sluit het programma eerst; \
                     bestanden die in gebruik zijn worden overgeslagen.",
                    "The contents are deleted permanently (not via the Recycle Bin). Close the program first; \
                     files in use are skipped.",
                ),
            );
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(ui::primary_icon_button(icon::BROOM, t("Leegmaken", "Empty")).min_size(vec2(140.0, 36.0))).clicked() {
                    decision = Some(true);
                }
                if ui.add(ui::secondary_icon_button(icon::X, t("Annuleren", "Cancel")).min_size(vec2(120.0, 36.0))).clicked() {
                    decision = Some(false);
                }
            });
        });
        if ctx.input(|inp| inp.key_pressed(egui::Key::Escape)) {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                self.confirm_cache = None;
                let (tx, rx) = channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let report = apps::clear_cache(&spot);
                    let _ = tx.send((i, report, apps::measure(&spot)));
                    ctx.request_repaint();
                });
                self.cache_rx = Some(rx);
                self.message = Some((tf!("{} wordt leeggemaakt…", "Emptying {}…", self.spots[i].title), true));
            }
            Some(false) => self.confirm_cache = None,
            None => {}
        }
    }

}

enum SpotAction {
    Explorer,
    Models,
    ClearCache,
}

/// One program location: icon, name, size, advice and its actions.
fn spot_card(ui: &mut egui::Ui, spot: &Spot, size: Option<u64>, busy: bool) -> Option<SpotAction> {
    let (glyph, color) = match spot.cleanup {
        Cleanup::Models => (icon::BRAIN, theme::CAT_VIDEO),
        Cleanup::Cache => (icon::ARCHIVE, theme::CAT_ARCHIVE),
        Cleanup::Advice => (icon::PACKAGE, theme::ACCENT),
    };
    let mut action = None;
    theme::card_frame().inner_margin(egui::Margin::symmetric(16.0, 12.0)).show(ui, |ui| {
        ui.set_min_width(ui.available_width() - 8.0);
        ui.horizontal(|ui| {
            let (tile, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            ui::icon_tile(ui, tile, glyph, color, 21.0);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&spot.title).font(theme::font_medium(14.5)));
                    if spot.cleanup == Cleanup::Advice {
                        ui.label(egui::RichText::new(icon::LOCK_SIMPLE).font(theme::icon(13.0)).color(theme::TEXT_FAINT))
                            .on_hover_text(t("Opruimen via het programma zelf", "Clean up via the program itself"));
                    }
                });
                ui.add(egui::Label::new(egui::RichText::new(spot.advice).size(12.0).color(theme::TEXT_DIM)).wrap());
            });
        });
        ui.horizontal(|ui| {
            ui.add_space(52.0);
            match size {
                Some(s) => ui.label(egui::RichText::new(disk::format_size(s)).font(theme::font_semibold(14.0))),
                None => ui.add(egui::Spinner::new().size(14.0).color(theme::ACCENT)),
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match spot.cleanup {
                    Cleanup::Models => {
                        if ui.add(ui::primary_icon_button(icon::BRAIN, t("Modellen bekijken", "View models"))).clicked() {
                            action = Some(SpotAction::Models);
                        }
                    }
                    Cleanup::Cache => {
                        let can = !busy && size.is_some_and(|s| s > 0);
                        if ui.add_enabled(can, ui::primary_icon_button(icon::BROOM, t("Leegmaken", "Empty"))).clicked() {
                            action = Some(SpotAction::ClearCache);
                        }
                    }
                    Cleanup::Advice => {}
                }
                if ui.add(ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, t("Open in Verkenner", "Show in Explorer"))).clicked() {
                    action = Some(SpotAction::Explorer);
                }
            });
        });
    });
    action
}
