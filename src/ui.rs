use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::Receiver,
};

use crate::barcode_reader::{ScanEvent, ScanResult};
use crate::discogs::Release;
use eframe::egui::{self, RichText};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Scan,
    Search,
}

pub struct RecordShelfApp {
    rx: Receiver<ScanEvent>,
    scan_results: Vec<ScanResult>,
    status: String,
    tab: Tab,
    stop: Arc<AtomicBool>,
}

impl RecordShelfApp {
    pub fn new(rx: Receiver<ScanEvent>, stop: Arc<AtomicBool>) -> Self {
        Self {
            rx,
            scan_results: Vec::new(),
            status: "Waiting for scans...".to_string(),
            tab: Tab::Scan,
            stop,
        }
    }

    fn drain_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.rx.try_recv() {
            changed = true;
            match ev {
                ScanEvent::Info(msg) => self.status = msg,
                ScanEvent::Result(res) => {
                    self.status = format!("Last scan {} ({:.0?})", res.barcode, res.elapsed);
                    self.scan_results.clear();
                    self.scan_results.push(res);
                }
            }
        }
        changed
    }

    fn ui_scan(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for result in &self.scan_results {
                    ui.group(|ui| {
                        if let Some(err) = &result.error {
                            ui.colored_label(egui::Color32::RED, format!("Error: {err}"));
                        } else if let Some(release) = &result.release {
                            render_release(ui, release);
                        } else {
                            ui.label("No data");
                        }
                    });
                    ui.add_space(6.0);
                }
            });
    }

    fn ui_search_placeholder(&mut self, ui: &mut egui::Ui) {
        ui.label("Search tab coming soon.");
    }
}

impl eframe::App for RecordShelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let updated = self.drain_events();
        if updated {
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("top-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RecordShelf");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Scan, "Scan");
                ui.selectable_value(&mut self.tab, Tab::Search, "Search");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::widgets::global_dark_light_mode_switch(ui);
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Scan => self.ui_scan(ui),
            Tab::Search => self.ui_search_placeholder(ui),
        });
    }
}

impl Drop for RecordShelfApp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn render_release(ui: &mut egui::Ui, release: &Release) {
    // Primary info (bold)
    if let Some(title) = &release.title {
        ui.label(RichText::new(format!("Title: {title}")).strong());
    }
    if let Some(artists) = &release.artists {
        if !artists.is_empty() {
            let names = artists
                .iter()
                .filter_map(|a| a.name.as_deref())
                .collect::<Vec<_>>()
                .join(", ");
            if !names.is_empty() {
                ui.label(RichText::new(format!("Artist: {names}")).strong());
            }
        }
    }
    if let Some(labels) = &release.labels {
        if !labels.is_empty() {
            let labels_str = labels
                .iter()
                .map(|l| {
                    let name = l.name.as_deref().unwrap_or("");
                    let catno = l.catno.as_deref().unwrap_or("");
                    if catno.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name} ({catno})")
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            if !labels_str.is_empty() {
                ui.label(RichText::new(format!("Label: {labels_str}")).strong());
            }
        }
    }
    if let Some(year) = release.year {
        if year > 0 {
            ui.label(RichText::new(format!("Year: {year}")).strong());
        }
    }

    ui.add_space(8.0);

    // Secondary info
    if let Some(country) = &release.country {
        if !country.is_empty() {
            ui.label(RichText::new(format!("Country: {country}")).small());
        }
    }
    if let Some(formats) = &release.formats {
        if !formats.is_empty() {
            let formats_str = formats
                .iter()
                .map(|f| {
                    let name = f.name.as_deref().unwrap_or("").to_string();
                    match f.descriptions.as_ref() {
                        Some(descs) if !descs.is_empty() => {
                            format!("{name} ({})", descs.join(", "))
                        }
                        _ => name,
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            if !formats_str.is_empty() {
                ui.label(RichText::new(format!("Format: {formats_str}")).small());
            }
        }
    }
    if let Some(genres) = &release.genres {
        if !genres.is_empty() {
            ui.label(RichText::new(format!("Genres: {}", genres.join(", "))).small());
        }
    }
    if let Some(styles) = &release.styles {
        if !styles.is_empty() {
            ui.label(RichText::new(format!("Styles: {}", styles.join(", "))).small());
        }
    }

    ui.add_space(8.0);

    // Tracklist
    if let Some(tracks) = &release.tracklist {
        if !tracks.is_empty() {
            ui.label(RichText::new("Tracklist:").strong());
            for track in tracks {
                let mut title = track.title.clone().unwrap_or_default();
                if let Some(duration) = &track.duration {
                    if !duration.is_empty() {
                        title.push_str(&format!(" [{duration}]"));
                    }
                }
                let pos = track.position.as_deref().unwrap_or("");
                ui.label(RichText::new(format!("  {pos} {title}")).small());
            }
        }
    }

    ui.add_space(8.0);

    if let Some(uri) = &release.uri {
        if !uri.is_empty() {
            ui.hyperlink(uri);
        }
    }
}
