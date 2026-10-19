//! "Schijfgezondheid" screen: one card per physical disk with its SMART
//! verdict, the key values and, on request, every SMART value.

use crate::i18n::t;
use crate::tf;
use crate::cleanup::notice;
use crate::disk::{self, VolumeInfo};
use crate::health::{self, Bus, DiskHealth, Smart, Status};
use crate::{theme, ui};
use egui::{Sense, vec2};
use egui_phosphor::regular as icon;
use std::sync::mpsc::{Receiver, channel};

pub struct HealthView {
    disks: Vec<DiskHealth>,
    rx: Option<Receiver<Vec<DiskHealth>>>,
    loaded: bool,
}

impl HealthView {
    pub fn new() -> Self {
        Self { disks: Vec::new(), rx: None, loaded: false }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(health::read_all());
            ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.loaded = true;
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, volumes: &[VolumeInfo]) {
        if !self.loaded {
            self.start(ui.ctx());
        }
        if let Some(disks) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.disks = disks;
            self.rx = None;
        }
        let loading = self.rx.is_some();

        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.heading(t("Schijfgezondheid", "Disk health"));
                ui.label(
                    egui::RichText::new(t("Wat je schijven zelf bijhouden over hun temperatuur, slijtage en fouten (SMART).", "What your disks record about their temperature, wear and errors (SMART)."))
                        .color(theme::TEXT_DIM),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.add_enabled_ui(!loading, |ui| {
                    refresh = ui::icon_button(ui, icon::ARROW_CLOCKWISE, t("Opnieuw lezen", "Read again")).clicked();
                });
                if loading {
                    ui.add(egui::Spinner::new().size(20.0).color(theme::ACCENT));
                }
            });
        });
        if refresh {
            self.start(ui.ctx());
        }
        ui.add_space(14.0);

        if self.disks.iter().any(|d| d.smart == Smart::NeedsAdmin) {
            notice(
                ui,
                icon::SHIELD_WARNING,
                theme::ACCENT,
                t(
                    "Start DiskTools als administrator om alle gezondheidsgegevens te lezen.",
                    "Start DiskTools as administrator to read all health data.",
                ),
            );
            ui.add_space(10.0);
        }
        if self.disks.is_empty() {
            if !loading {
                ui::empty_card(ui, icon::INFO, t("Geen schijven gevonden.", "No disks found."));
            }
            return;
        }
        egui::ScrollArea::vertical().id_salt("health").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 12.0;
            for d in &self.disks {
                disk_card(ui, d, volumes);
            }
        });
    }
}

fn status_style(status: Option<Status>) -> (&'static str, &'static str, egui::Color32) {
    match status {
        Some(Status::Good) => (t("Goed", "Good"), icon::CHECK_CIRCLE, theme::SUCCESS),
        Some(Status::Caution) => (t("Let op", "Caution"), icon::WARNING, theme::WARNING),
        Some(Status::Bad) => (t("Slecht", "Bad"), icon::X_CIRCLE, theme::DANGER),
        None => (t("Onbekend", "Unknown"), icon::QUESTION, theme::TEXT_FAINT),
    }
}

fn kind_label(d: &DiskHealth) -> &'static str {
    match (d.bus, d.ssd) {
        (Bus::Nvme, _) => "NVMe-SSD",
        (Bus::Sata, Some(true)) => "SATA-SSD",
        (Bus::Sata, Some(false)) => t("Harde schijf (SATA)", "Hard disk (SATA)"),
        (Bus::Sata, None) => t("SATA-schijf", "SATA disk"),
        (Bus::Usb, _) => t("USB-schijf", "USB drive"),
        (Bus::Sd, _) => t("Geheugenkaart", "Memory card"),
        (Bus::Other, Some(true)) => "SSD",
        (Bus::Other, _) => t("Schijf", "Disk"),
    }
}

fn disk_card(ui: &mut egui::Ui, d: &DiskHealth, volumes: &[VolumeInfo]) {
    let s = d.summary();
    let (label, glyph, color) = status_style(s.status);
    let letters: Vec<&str> = volumes.iter().filter(|v| v.disk_index == Some(d.index)).map(|v| v.letter.as_str()).collect();
    let mut sub = tf!("{} · {} · Schijf {}", "{} · {} · Disk {}", kind_label(d), disk::format_size(d.size), d.index);
    if !letters.is_empty() {
        sub.push_str(&format!(" · {}", letters.join(", ")));
    }

    theme::card_frame().inner_margin(egui::Margin::symmetric(18.0, 16.0)).show(ui, |ui| {
        ui.set_min_width(ui.available_width() - 8.0);
        ui.horizontal(|ui| {
            let (tile, _) = ui.allocate_exact_size(vec2(46.0, 46.0), Sense::hover());
            let drive = if d.bus == Bus::Usb { icon::USB } else { icon::HARD_DRIVE };
            ui::icon_tile(ui, tile, drive, color, 24.0);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let model = if d.model.is_empty() { t("Onbekende schijf", "Unknown disk") } else { &d.model };
                ui.label(egui::RichText::new(model).font(theme::font_semibold(16.0)));
                ui.label(egui::RichText::new(&sub).color(theme::TEXT_DIM));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::Frame::none()
                    .fill(theme::tint(color, 0.12))
                    .rounding(egui::Rounding::same(14.0))
                    .inner_margin(egui::Margin::symmetric(12.0, 6.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(glyph).font(theme::icon(16.0)).color(color));
                            ui.label(egui::RichText::new(label).font(theme::font_semibold(14.0)).color(color));
                        });
                    });
            });
        });
        ui.add_space(12.0);

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
            if let Some(t) = s.temperature {
                metric(ui, icon::THERMOMETER, crate::i18n::t("Temperatuur", "Temperature"), &format!("{t} °C"));
            }
            if let Some(l) = s.life_left {
                metric(ui, icon::BATTERY_HIGH, t("Levensduur", "Life left"), &format!("{l}%"));
            }
            if let Some(h) = s.power_on_hours {
                metric(ui, icon::CLOCK, t("In gebruik", "Powered on"), &tf!("{} uur", "{} hours", ui::fmt_thousands(h)));
            }
            if let Some(n) = s.power_cycles {
                metric(ui, icon::POWER, t("Aangezet", "Power cycles"), &tf!("{} keer", "{} times", ui::fmt_thousands(n)));
            }
            if let Some(b) = s.bytes_read {
                metric(ui, icon::DOWNLOAD, t("Gelezen", "Read"), &disk::format_size(b));
            }
            if let Some(b) = s.bytes_written {
                metric(ui, icon::UPLOAD, t("Geschreven", "Written"), &disk::format_size(b));
            }
        });

        match &d.smart {
            Smart::NeedsAdmin => {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(t("SMART niet gelezen: hiervoor zijn administratorrechten nodig.", "SMART not read: this needs administrator rights.")).color(theme::TEXT_DIM));
            }
            Smart::Unavailable => {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(t("Deze schijf geeft geen SMART-gegevens door. Bij USB-schijven en geheugenkaarten is dat normaal.", "This disk does not report SMART data. That is normal for USB drives and memory cards."))
                        .color(theme::TEXT_DIM),
                );
            }
            Smart::Nvme(_) | Smart::Ata(_) => {
                ui.add_space(10.0);
                if s.issues.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon::CHECK).font(theme::icon(14.0)).color(theme::SUCCESS));
                        ui.label(egui::RichText::new(t("Geen problemen gevonden.", "No problems found.")).color(theme::SUCCESS));
                    });
                }
                for (status, text) in &s.issues {
                    let (_, glyph, color) = status_style(Some(*status));
                    notice(ui, glyph, color, text);
                }
                ui.add_space(4.0);
                egui::CollapsingHeader::new(egui::RichText::new(t("Alle SMART-waarden", "All SMART values")).color(theme::TEXT_DIM))
                    .id_salt(("smart_values", d.index))
                    .show(ui, |ui| smart_table(ui, d));
            }
        }
    });
}

/// Small tile with an icon, a caption and a value.
fn metric(ui: &mut egui::Ui, glyph: &str, caption: &str, value: &str) {
    // A known size up front lets the row wrap; a bare frame only knows its
    // size after it is drawn.
    const SIZE: egui::Vec2 = egui::vec2(154.0, 50.0);
    ui.allocate_ui(SIZE, |ui| {
        egui::Frame::none()
            .fill(theme::SUBTLE_BG)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(12.0, 8.0))
            .show(ui, |ui| {
                ui.set_width(SIZE.x - 24.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(glyph).font(theme::icon(18.0)).color(theme::TEXT_DIM));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(egui::RichText::new(caption).size(11.5).color(theme::TEXT_DIM));
                        ui.label(egui::RichText::new(value).font(theme::font_semibold(15.0)));
                    });
                });
            });
    });
}

fn smart_table(ui: &mut egui::Ui, d: &DiskHealth) {
    let cell = |ui: &mut egui::Ui, text: String, color: egui::Color32| {
        ui.label(egui::RichText::new(text).size(12.5).color(color));
    };
    match &d.smart {
        Smart::Nvme(log) => {
            let rows = [
                (t("Kritieke waarschuwing", "Critical warning"), if log.critical_warning == 0 { t("Geen", "None").to_string() } else { format!("0x{:02X}", log.critical_warning) }),
                (t("Temperatuur", "Temperature"), format!("{} °C", i32::from(log.temperature_kelvin) - 273)),
                (t("Reserve beschikbaar", "Available spare"), format!("{}%", log.available_spare)),
                (t("Grens reserve", "Spare threshold"), format!("{}%", log.spare_threshold)),
                (t("Levensduur gebruikt", "Percentage used"), format!("{}%", log.percentage_used)),
                (t("Gelezen", "Data read"), disk::format_size(log.data_units_read.saturating_mul(512_000))),
                (t("Geschreven", "Data written"), disk::format_size(log.data_units_written.saturating_mul(512_000))),
                (t("Aantal keer aangezet", "Power cycles"), ui::fmt_thousands(log.power_cycles)),
                (t("Uren in gebruik", "Power-on hours"), ui::fmt_thousands(log.power_on_hours)),
                (t("Onveilig uitgeschakeld", "Unsafe shutdowns"), ui::fmt_thousands(log.unsafe_shutdowns)),
                (t("Lees- en schrijffouten", "Media errors"), ui::fmt_thousands(log.media_errors)),
                (t("Foutmeldingen in logboek", "Error log entries"), ui::fmt_thousands(log.error_log_entries)),
            ];
            egui::Grid::new(("nvme_grid", d.index)).striped(true).spacing(vec2(24.0, 6.0)).show(ui, |ui| {
                for (name, value) in rows {
                    cell(ui, name.to_string(), theme::TEXT_DIM);
                    cell(ui, value, theme::TEXT_MAIN);
                    ui.end_row();
                }
            });
        }
        Smart::Ata(attrs) => {
            egui::Grid::new(("ata_grid", d.index)).striped(true).spacing(vec2(18.0, 6.0)).show(ui, |ui| {
                for head in ["ID", t("Naam", "Name"), t("Huidig", "Current"), t("Slechtst", "Worst"), t("Grens", "Threshold"), t("Ruwe waarde", "Raw value")] {
                    ui.label(egui::RichText::new(head).size(12.0).font(theme::font_semibold(12.0)).color(theme::TEXT_DIM));
                }
                ui.end_row();
                for a in attrs {
                    let failing = a.threshold > 0 && a.current > 0 && a.current <= a.threshold;
                    let color = if failing { theme::DANGER } else { theme::TEXT_MAIN };
                    cell(ui, format!("{:02X}", a.id), theme::TEXT_DIM);
                    cell(ui, health::attribute_name(a.id).to_string(), color);
                    cell(ui, a.current.to_string(), color);
                    cell(ui, a.worst.to_string(), color);
                    cell(ui, a.threshold.to_string(), theme::TEXT_DIM);
                    cell(ui, ui::fmt_thousands(a.raw), color);
                    ui.end_row();
                }
            });
        }
        Smart::NeedsAdmin | Smart::Unavailable => {}
    }
}

/// Selectable cards for the physical disks. Returns the disk clicked.
pub(crate) fn disk_chips(ui: &mut egui::Ui, disks: &[DiskHealth], selected: Option<u32>) -> Option<u32> {
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
        for d in disks {
            let glyph = if d.bus == Bus::Usb { icon::USB } else { icon::HARD_DRIVE };
            let sub = tf!("{} · {} · Schijf {}", "{} · {} · Disk {}", kind_label(d), disk::format_size(d.size), d.index);
            if crate::cleanup::location_chip(ui, glyph, &d.model, &sub, selected == Some(d.index)) {
                picked = Some(d.index);
            }
        }
    });
    picked
}
