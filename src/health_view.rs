//! "Schijfgezondheid" screen: one card per physical disk with its SMART
//! verdict, the key values and, on request, every SMART value.

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
                ui.heading("Schijfgezondheid");
                ui.label(
                    egui::RichText::new("Wat je schijven zelf bijhouden over hun temperatuur, slijtage en fouten (SMART).")
                        .color(theme::TEXT_DIM),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.add_enabled_ui(!loading, |ui| {
                    refresh = ui::icon_button(ui, icon::ARROW_CLOCKWISE, "Opnieuw lezen").clicked();
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
                "Start Data Recoverer als administrator om alle gezondheidsgegevens te lezen.",
            );
            ui.add_space(10.0);
        }
        if self.disks.is_empty() {
            if !loading {
                ui::empty_card(ui, icon::INFO, "Geen schijven gevonden.");
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
        Some(Status::Good) => ("Goed", icon::CHECK_CIRCLE, theme::SUCCESS),
        Some(Status::Caution) => ("Let op", icon::WARNING, theme::WARNING),
        Some(Status::Bad) => ("Slecht", icon::X_CIRCLE, theme::DANGER),
        None => ("Onbekend", icon::QUESTION, theme::TEXT_FAINT),
    }
}

fn kind_label(d: &DiskHealth) -> &'static str {
    match (d.bus, d.ssd) {
        (Bus::Nvme, _) => "NVMe-SSD",
        (Bus::Sata, Some(true)) => "SATA-SSD",
        (Bus::Sata, Some(false)) => "Harde schijf (SATA)",
        (Bus::Sata, None) => "SATA-schijf",
        (Bus::Usb, _) => "USB-schijf",
        (Bus::Sd, _) => "Geheugenkaart",
        (Bus::Other, Some(true)) => "SSD",
        (Bus::Other, _) => "Schijf",
    }
}

fn disk_card(ui: &mut egui::Ui, d: &DiskHealth, volumes: &[VolumeInfo]) {
    let s = d.summary();
    let (label, glyph, color) = status_style(s.status);
    let letters: Vec<&str> = volumes.iter().filter(|v| v.disk_index == Some(d.index)).map(|v| v.letter.as_str()).collect();
    let mut sub = format!("{} · {} · Schijf {}", kind_label(d), disk::format_size(d.size), d.index);
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
                let model = if d.model.is_empty() { "Onbekende schijf" } else { &d.model };
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
                metric(ui, icon::THERMOMETER, "Temperatuur", &format!("{t} °C"));
            }
            if let Some(l) = s.life_left {
                metric(ui, icon::BATTERY_HIGH, "Levensduur", &format!("{l}%"));
            }
            if let Some(h) = s.power_on_hours {
                metric(ui, icon::CLOCK, "In gebruik", &format!("{} uur", ui::fmt_thousands(h)));
            }
            if let Some(n) = s.power_cycles {
                metric(ui, icon::POWER, "Aangezet", &format!("{} keer", ui::fmt_thousands(n)));
            }
            if let Some(b) = s.bytes_read {
                metric(ui, icon::DOWNLOAD, "Gelezen", &disk::format_size(b));
            }
            if let Some(b) = s.bytes_written {
                metric(ui, icon::UPLOAD, "Geschreven", &disk::format_size(b));
            }
        });

        match &d.smart {
            Smart::NeedsAdmin => {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("SMART niet gelezen: hiervoor zijn administratorrechten nodig.").color(theme::TEXT_DIM));
            }
            Smart::Unavailable => {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Deze schijf geeft geen SMART-gegevens door. Bij USB-schijven en geheugenkaarten is dat normaal.")
                        .color(theme::TEXT_DIM),
                );
            }
            Smart::Nvme(_) | Smart::Ata(_) => {
                ui.add_space(10.0);
                if s.issues.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon::CHECK).font(theme::icon(14.0)).color(theme::SUCCESS));
                        ui.label(egui::RichText::new("Geen problemen gevonden.").color(theme::SUCCESS));
                    });
                }
                for (status, text) in &s.issues {
                    let (_, glyph, color) = status_style(Some(*status));
                    notice(ui, glyph, color, text);
                }
                ui.add_space(4.0);
                egui::CollapsingHeader::new(egui::RichText::new("Alle SMART-waarden").color(theme::TEXT_DIM))
                    .id_salt(("smart_values", d.index))
                    .show(ui, |ui| smart_table(ui, d));
            }
        }
    });
}

/// Small tile with an icon, a caption and a value.
fn metric(ui: &mut egui::Ui, glyph: &str, caption: &str, value: &str) {
    egui::Frame::none()
        .fill(theme::SUBTLE_BG)
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(130.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(glyph).font(theme::icon(18.0)).color(theme::TEXT_DIM));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(egui::RichText::new(caption).size(11.5).color(theme::TEXT_DIM));
                    ui.label(egui::RichText::new(value).font(theme::font_semibold(15.0)));
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
                ("Kritieke waarschuwing", if log.critical_warning == 0 { "Geen".to_string() } else { format!("0x{:02X}", log.critical_warning) }),
                ("Temperatuur", format!("{} °C", i32::from(log.temperature_kelvin) - 273)),
                ("Reserve beschikbaar", format!("{}%", log.available_spare)),
                ("Grens reserve", format!("{}%", log.spare_threshold)),
                ("Levensduur gebruikt", format!("{}%", log.percentage_used)),
                ("Gelezen", disk::format_size(log.data_units_read.saturating_mul(512_000))),
                ("Geschreven", disk::format_size(log.data_units_written.saturating_mul(512_000))),
                ("Aantal keer aangezet", ui::fmt_thousands(log.power_cycles)),
                ("Uren in gebruik", ui::fmt_thousands(log.power_on_hours)),
                ("Onveilig uitgeschakeld", ui::fmt_thousands(log.unsafe_shutdowns)),
                ("Lees- en schrijffouten", ui::fmt_thousands(log.media_errors)),
                ("Foutmeldingen in logboek", ui::fmt_thousands(log.error_log_entries)),
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
                for head in ["ID", "Naam", "Huidig", "Slechtst", "Grens", "Ruwe waarde"] {
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
