//! "Programma's" tab of the cleanup screen: the big data folders of
//! installed programs, with advice per program, cache cleaning and a list
//! of model files that can go to the Recycle Bin.

use crate::apps::{self, Cleanup, Spot};
use crate::cleanup::{ROW_GAP, ROW_H, dialog, dialog_title, dim_background, file_name, notice, result_row};
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
    selected: Option<usize>,
    confirm_cache: Option<usize>,
    cache_rx: Option<Receiver<(usize, CleanReport, u64)>>,
    confirm_model: Option<LargeFile>,
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
            selected: None,
            confirm_cache: None,
            cache_rx: None,
            confirm_model: None,
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
                self.models = models;
                self.models_rx = None;
            }
        }
        if let Some(rx) = &self.cache_rx {
            if let Ok((i, report, size)) = rx.try_recv() {
                self.cache_rx = None;
                self.sizes[i] = Some(size);
                let mut text = format!("{}: {} vrijgemaakt.", self.spots[i].title, disk::format_size(report.freed));
                if report.skipped > 0 {
                    text.push_str(&format!(" {} in gebruik, overgeslagen.", ui::fmt_files(report.skipped as usize)));
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
        self.draw_model_confirm(ui.ctx());
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
                        format!("Bezig met meten… {} gevonden", disk::format_size(total))
                    } else {
                        format!("{} aan programmagegevens", disk::format_size(total))
                    };
                    ui.label(egui::RichText::new(title).font(theme::font_semibold(18.0)));
                    ui.label(
                        egui::RichText::new("AI-modellen, emulators, Docker en caches. Per programma staat hoe je het veilig opruimt.")
                            .color(theme::TEXT_DIM),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(!measuring, |ui| {
                        refresh = ui::icon_button(ui, icon::ARROW_CLOCKWISE, "Opnieuw meten").clicked();
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
                ui::section_header(ui, icon::STACK, "Overige programmagegevens", Some(others.len()));
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
                self.models_rx = Some(rx);
                self.selected = None;
                self.open = Some(i);
            }
            Some((i, SpotAction::ClearCache)) => self.confirm_cache = Some(i),
            None => {}
        }
    }

    fn draw_models(&mut self, ui: &mut egui::Ui, open: usize) {
        let mut back = false;
        ui.horizontal(|ui| {
            back = ui::icon_button(ui, icon::ARROW_LEFT, "Terug naar programma's").clicked();
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
            "Modellen zoeken…".to_string()
        } else {
            format!("{} · {}, grootste eerst", ui::fmt_files(self.models.len()), disk::format_size(total))
        };
        ui.label(egui::RichText::new(summary).size(12.5).color(theme::TEXT_DIM));
        ui.add_space(8.0);
        if loading {
            ui.add(egui::Spinner::new().size(24.0).color(theme::ACCENT));
            return;
        }
        if self.models.is_empty() {
            ui::empty_card(ui, icon::CHECK_CIRCLE, "Geen modelbestanden gevonden.");
            return;
        }
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
                    let row = result_row(ui, icon::BRAIN, theme::CAT_VIDEO, &file_name(&m.path), &folder, m.size, self.selected == Some(i));
                    if row.double_clicked() {
                        open_file = Some(i);
                    } else if row.clicked() {
                        clicked = Some(i);
                    }
                }
            });
        if let Some(i) = clicked.or(open_file) {
            self.selected = Some(i);
        }
        if let Some(i) = open_file {
            let path = self.models[i].path.clone();
            self.show_in_explorer(&path);
        }
    }

    pub fn draw_action_bar(&mut self, ui: &mut egui::Ui) {
        let selection = self.selected.and_then(|i| self.models.get(i)).cloned();
        let mut open = false;
        let mut recycle = false;
        ui.horizontal_centered(|ui| {
            match &selection {
                Some(m) => {
                    ui.label(egui::RichText::new(file_name(&m.path)).font(theme::font_semibold(14.0)));
                    ui.label(egui::RichText::new(disk::format_size(m.size)).color(theme::TEXT_DIM));
                }
                None => {
                    ui.label(egui::RichText::new("Selecteer een model").color(theme::TEXT_DIM));
                }
            }
            if let Some((text, success)) = &self.message {
                let color = if *success { theme::SUCCESS } else { theme::DANGER };
                ui.add_space(12.0);
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = ui::danger_icon_button(icon::TRASH, "Naar prullenbak").min_size(vec2(160.0, 38.0));
                recycle = ui.add_enabled(selection.is_some(), button).clicked();
                let button = ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, "Open in Verkenner").min_size(vec2(170.0, 38.0));
                open = ui.add_enabled(selection.is_some(), button).clicked();
            });
        });
        if let Some(m) = selection {
            if open {
                self.show_in_explorer(&m.path);
            }
            if recycle {
                self.confirm_model = Some(m);
            }
        }
    }

    fn show_in_explorer(&mut self, path: &std::path::Path) {
        if let Err(e) = largefiles::show_in_explorer(path) {
            self.message = Some((format!("Verkenner openen mislukt: {e}"), false));
        }
    }

    fn draw_cache_confirm(&mut self, ctx: &egui::Context) {
        let Some(i) = self.confirm_cache else { return };
        let spot = self.spots[i].clone();
        let size = self.sizes[i].unwrap_or(0);
        let mut decision = None;
        dim_background(ctx);
        dialog("apps_cache_confirm", ctx, |ui| {
            dialog_title(ui, icon::BROOM, theme::ACCENT, &format!("{} leegmaken?", spot.title), &format!("{} wordt vrijgemaakt", disk::format_size(size)));
            for f in &spot.folders {
                ui.label(egui::RichText::new(f.display().to_string()).size(12.0).color(theme::TEXT_DIM));
            }
            ui.add_space(4.0);
            notice(ui, icon::INFO, theme::ACCENT, spot.advice);
            notice(
                ui,
                icon::WARNING_CIRCLE,
                theme::DANGER,
                "De inhoud wordt definitief verwijderd (niet via de prullenbak). Sluit het programma eerst; \
                 bestanden die in gebruik zijn worden overgeslagen.",
            );
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(ui::primary_icon_button(icon::BROOM, "Leegmaken").min_size(vec2(140.0, 36.0))).clicked() {
                    decision = Some(true);
                }
                if ui.add(ui::secondary_icon_button(icon::X, "Annuleren").min_size(vec2(120.0, 36.0))).clicked() {
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
                self.message = Some((format!("{} wordt leeggemaakt…", self.spots[i].title), true));
            }
            Some(false) => self.confirm_cache = None,
            None => {}
        }
    }

    fn draw_model_confirm(&mut self, ctx: &egui::Context) {
        let Some(model) = self.confirm_model.clone() else { return };
        let mut decision = None;
        dim_background(ctx);
        dialog("apps_model_confirm", ctx, |ui| {
            let sub = format!("{} · {}", file_name(&model.path), disk::format_size(model.size));
            dialog_title(ui, icon::TRASH, theme::DANGER, "Model naar de prullenbak?", &sub);
            ui.add(egui::Label::new(egui::RichText::new(model.path.display().to_string()).size(12.0).color(theme::TEXT_DIM)).wrap());
            ui.add_space(4.0);
            notice(
                ui,
                icon::INFO,
                theme::ACCENT,
                "Gebruik je dit model nog in een workflow, dan downloadt het programma het later opnieuw. \
                 Grote modellen passen vaak niet in de prullenbak; Windows vraagt dan eerst of het definitief weg mag.",
            );
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(ui::danger_icon_button(icon::TRASH, "Naar prullenbak").min_size(vec2(150.0, 36.0))).clicked() {
                    decision = Some(true);
                }
                if ui.add(ui::secondary_icon_button(icon::X, "Annuleren").min_size(vec2(120.0, 36.0))).clicked() {
                    decision = Some(false);
                }
            });
        });
        if ctx.input(|inp| inp.key_pressed(egui::Key::Escape)) {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                self.confirm_model = None;
                let name = file_name(&model.path);
                match apps::recycle_model(&self.spots, &model.path) {
                    Ok(true) => {
                        self.models.retain(|m| m.path != model.path);
                        self.selected = None;
                        if let Some(size) = self.open.and_then(|i| self.sizes[i].as_mut()) {
                            *size = size.saturating_sub(model.size);
                        }
                        self.recycled = true;
                        self.message = Some((format!("{name} ({}) is verwijderd.", disk::format_size(model.size)), true));
                    }
                    Ok(false) => self.message = Some((format!("{name} is niet verwijderd."), false)),
                    Err(e) => self.message = Some((format!("{name}: {e}"), false)),
                }
            }
            Some(false) => self.confirm_model = None,
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
                            .on_hover_text("Opruimen via het programma zelf");
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
                        if ui.add(ui::primary_icon_button(icon::BRAIN, "Modellen bekijken")).clicked() {
                            action = Some(SpotAction::Models);
                        }
                    }
                    Cleanup::Cache => {
                        let can = !busy && size.is_some_and(|s| s > 0);
                        if ui.add_enabled(can, ui::primary_icon_button(icon::BROOM, "Leegmaken")).clicked() {
                            action = Some(SpotAction::ClearCache);
                        }
                    }
                    Cleanup::Advice => {}
                }
                if ui.add(ui::secondary_icon_button(icon::ARROW_SQUARE_OUT, "Open in Verkenner")).clicked() {
                    action = Some(SpotAction::Explorer);
                }
            });
        });
    });
    action
}
