use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::Receiver,
};

use crate::Args;
use crate::barcode_reader::{ScanEvent, ScanResult};
use crate::discogs::{
    CollectionFolder, CollectionInstance, DiscogsClient, Release, SearchItem, SearchQuery,
};
use eframe::egui::{self, RichText};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Scan,
    Search,
}

pub struct RecordShelfApp {
    _args: Args,
    discogs: DiscogsClient,
    rx: Receiver<ScanEvent>,
    scan_results: Vec<ScanResult>,
    status: String,
    tab: Tab,
    stop: Arc<AtomicBool>,
    username: String,
    folders: Vec<CollectionFolder>,
    folder_picker_open: bool,
    folder_selected: Option<usize>,
    collection_instance: Option<CollectionInstance>,
    collection_info: Option<String>,
    search_focus: bool,
    search_q: String,
    search_artist: String,
    search_title: String,
    search_catno: String,
    search_results: Vec<SearchItem>,
    selected_result: Option<usize>,
    selected_release: Option<Release>,
    search_error: Option<String>,
}

impl RecordShelfApp {
    pub fn new(
        args: Args,
        discogs: DiscogsClient,
        rx: Receiver<ScanEvent>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            _args: args.clone(),
            discogs,
            rx,
            scan_results: Vec::new(),
            status: "Waiting for scans...".to_string(),
            tab: Tab::Scan,
            stop,
            username: args.username.clone(),
            folders: Vec::new(),
            folder_picker_open: false,
            folder_selected: None,
            collection_instance: None,
            collection_info: None,
            search_focus: false,
            search_q: String::new(),
            search_artist: String::new(),
            search_title: String::new(),
            search_catno: String::new(),
            search_results: Vec::new(),
            selected_result: None,
            selected_release: None,
            search_error: None,
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

    fn ui_search(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut trigger_search = false;
        let mut search_fields_active = false;
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label("Query:");
                let resp = ui.text_edit_singleline(&mut self.search_q);
                if self.search_focus {
                    resp.request_focus();
                    self.search_focus = false;
                }
                if resp.has_focus() {
                    search_fields_active = true;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    trigger_search = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Artist:");
                let resp = ui.text_edit_singleline(&mut self.search_artist);
                if resp.has_focus() {
                    search_fields_active = true;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    trigger_search = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Release:");
                let resp = ui.text_edit_singleline(&mut self.search_title);
                if resp.has_focus() {
                    search_fields_active = true;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    trigger_search = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Catalog No:");
                let resp = ui.text_edit_singleline(&mut self.search_catno);
                if resp.has_focus() {
                    search_fields_active = true;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    trigger_search = true;
                }
            });

            if trigger_search {
                self.perform_search();
            }

            if let Some(err) = &self.search_error {
                ui.colored_label(egui::Color32::RED, err);
            }

            ui.separator();

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let up_pressed = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
                    let down_pressed = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
                    let esc_pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));
                    let a_pressed = ui.input(|i| i.key_pressed(egui::Key::A));

                    for (idx, item) in self.search_results.iter().enumerate() {
                        let title = item.title_display();
                        let artist = item.artist_display();
                        let label = item.label_display();
                        let formats = item.format_display();
                        let year = item.year.map(|y| y.to_string()).unwrap_or_default();
                        let country = item.country.clone().unwrap_or_default();
                        let line = format!(
                            "{title} | {artist} | {label} | {year} | {country} | {formats}"
                        );
                        let selected = self.selected_result == Some(idx);
                        if ui.selectable_label(selected, line).clicked() {
                            self.selected_result = Some(idx);
                        }
                    }

                    if a_pressed && !search_fields_active {
                        if self.selected_result.is_none() {
                            self.search_error =
                                Some("Select a result before choosing a folder".to_string());
                        } else if !self.folder_picker_open {
                            self.prepare_folder_picker();
                        } else {
                            self.add_selected_to_collection();
                        }
                    }

                    if self.folder_picker_open {
                        if up_pressed {
                            if let Some(sel) = self.folder_selected {
                                if sel > 0 {
                                    self.folder_selected = Some(sel - 1);
                                }
                            }
                        }
                        if down_pressed {
                            let len = self.folders.len();
                            if len > 0 {
                                let sel = self.folder_selected.unwrap_or(0);
                                if sel + 1 < len {
                                    self.folder_selected = Some(sel + 1);
                                }
                            }
                        }
                        if enter_pressed {
                            self.add_selected_to_collection();
                        }
                        if esc_pressed {
                            self.folder_picker_open = false;
                            self.collection_instance = None;
                            self.collection_info = None;
                        }
                    } else {
                        if up_pressed {
                            if let Some(sel) = self.selected_result {
                                if sel > 0 {
                                    self.selected_result = Some(sel - 1);
                                }
                            }
                        }
                        if down_pressed {
                            let len = self.search_results.len();
                            if len > 0 {
                                let sel = self.selected_result.unwrap_or(0);
                                if sel + 1 < len {
                                    self.selected_result = Some(sel + 1);
                                }
                            }
                        }
                        if enter_pressed {
                            self.load_selected_release();
                        }
                        if esc_pressed {
                            self.selected_result = None;
                            self.selected_release = None;
                            self.search_focus = true;
                        }
                    }
                });

            if self.folder_picker_open {
                let ctx = ui.ctx().clone();
                let esc_picker = ctx.input(|i| i.key_pressed(egui::Key::Escape));
                if esc_picker {
                    self.folder_picker_open = false;
                    self.collection_instance = None;
                    self.collection_info = None;
                } else {
                    let mut open = true;
                    egui::Window::new("Add to folder")
                        .open(&mut open)
                        .collapsible(false)
                        .show(&ctx, |ui| {
                            if let Some(info) = &self.collection_info {
                                ui.colored_label(egui::Color32::YELLOW, info);
                            }
                            for (idx, folder) in self.folders.iter().enumerate() {
                                let selected = self.folder_selected == Some(idx);
                                let label = format!(
                                    "{} ({} items)",
                                    folder.name,
                                    folder.count.unwrap_or(0)
                                );
                                if ui.selectable_label(selected, label).clicked() {
                                    self.folder_selected = Some(idx);
                                }
                            }
                        });
                    self.folder_picker_open = open;
                }
            }

            if !self.folder_picker_open && self.search_focus {
                // focus will be applied on next frame
            }

            ui.separator();

            if let Some(release) = &self.selected_release {
                render_release(ui, release);
            }
        });
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
            Tab::Search => self.ui_search(ui, ctx),
        });
    }
}

impl Drop for RecordShelfApp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl RecordShelfApp {
    fn perform_search(&mut self) {
        let query = SearchQuery {
            query: non_empty(&self.search_q),
            artist: non_empty(&self.search_artist),
            release_title: non_empty(&self.search_title),
            catno: non_empty(&self.search_catno),
            page: Some(1),
            per_page: Some(25),
        };

        match self.discogs.search(&query) {
            Ok(resp) => {
                self.search_results = resp.results;
                self.selected_result = if self.search_results.is_empty() {
                    None
                } else {
                    Some(0)
                };
                self.selected_release = None;
                self.search_error = None;
            }
            Err(err) => {
                self.search_error = Some(err.to_string());
            }
        }

        self.search_q.clear();
        self.search_artist.clear();
        self.search_title.clear();
        self.search_catno.clear();
        self.search_focus = self.search_results.is_empty();
    }

    fn load_selected_release(&mut self) {
        if let Some(idx) = self.selected_result {
            if let Some(item) = self.search_results.get(idx) {
                if let Some(id) = item.id {
                    match self.discogs.release(id) {
                        Ok(rel) => {
                            self.selected_release = Some(rel);
                            self.search_error = None;
                        }
                        Err(err) => {
                            self.search_error = Some(err.to_string());
                        }
                    }
                } else {
                    self.search_error = Some("Selected item missing id".to_string());
                }
            }
        }
    }

    fn ensure_folders_loaded(&mut self) -> bool {
        if self.username.trim().is_empty() {
            self.search_error = Some("Collection: username required".to_string());
            return false;
        }
        if self.folders.is_empty() {
            match self.discogs.collection_folders(&self.username) {
                Ok(resp) => {
                    self.folders = resp.folders;
                }
                Err(err) => {
                    self.search_error = Some(err.to_string());
                    return false;
                }
            }
        }
        if self.folders.is_empty() {
            self.search_error = Some("No folders found".to_string());
            return false;
        }
        if self.folder_selected.is_none() {
            self.folder_selected = Some(0);
        }
        true
    }

    fn prepare_folder_picker(&mut self) {
        if !self.ensure_folders_loaded() {
            return;
        }
        self.collection_instance = None;
        self.collection_info = None;
        if let Some(idx) = self.selected_result {
            if let Some(item) = self.search_results.get(idx) {
                if let Some(id) = item.id {
                    match self.discogs.collection_instances(&self.username, id) {
                        Ok(instances) => {
                            if let Some(inst) = instances.first() {
                                let url = item
                                    .uri
                                    .as_ref()
                                    .map(|u| {
                                        if u.starts_with("http") {
                                            u.clone()
                                        } else {
                                            format!("https://www.discogs.com{u}")
                                        }
                                    })
                                    .unwrap_or_else(|| {
                                        format!("https://www.discogs.com/release/{id}")
                                    });
                                match open::that(&url) {
                                    Ok(_) => {
                                        self.search_error = Some(format!(
                                            "Release already in collection (instance {}). Opened release page to move manually.",
                                            inst.instance_id
                                        ));
                                    }
                                    Err(err) => {
                                        self.search_error = Some(format!(
                                            "Release already in collection; failed to open browser: {err}"
                                        ));
                                    }
                                }
                                self.folder_picker_open = false;
                                self.collection_instance = None;
                                self.collection_info = None;
                                return;
                            }
                        }
                        Err(err) => {
                            self.search_error = Some(err.to_string());
                            return;
                        }
                    }
                }
            }
        }
        self.folder_picker_open = true;
    }

    fn add_selected_to_collection(&mut self) {
        if self.username.trim().is_empty() {
            self.search_error = Some("Collection: username required".to_string());
            return;
        }
        let folder_idx = match self.folder_selected {
            Some(idx) => idx,
            None => {
                self.search_error = Some("Select a folder first".to_string());
                return;
            }
        };
        let folder = match self.folders.get(folder_idx) {
            Some(f) => f,
            None => {
                self.search_error = Some("Select a valid folder".to_string());
                return;
            }
        };
        if let Some(idx) = self.selected_result {
            if let Some(item) = self.search_results.get(idx) {
                if let Some(id) = item.id {
                    if let Some(inst) = &self.collection_instance {
                        let url = item
                            .uri
                            .as_ref()
                            .map(|u| {
                                if u.starts_with("http") {
                                    u.clone()
                                } else {
                                    format!("https://www.discogs.com{u}")
                                }
                            })
                            .unwrap_or_else(|| format!("https://www.discogs.com/release/{id}"));
                        match open::that(&url) {
                            Ok(_) => {
                                self.search_error = Some(format!(
                                    "Release already in collection (instance {}). Opened release page in browser to move manually.",
                                    inst.instance_id
                                ));
                            }
                            Err(err) => {
                                self.search_error = Some(format!(
                                    "Release already in collection; failed to open browser: {err}"
                                ));
                            }
                        }
                        self.folder_picker_open = false;
                        self.collection_instance = None;
                        self.collection_info = None;
                    } else {
                        match self
                            .discogs
                            .add_to_collection(&self.username, folder.id, id)
                        {
                            Ok(_) => {
                                let msg = format!("Added release {id} to folder {}", folder.name);
                                self.search_error = Some(msg);
                                self.folder_picker_open = false;
                                self.collection_instance = None;
                                self.collection_info = None;
                            }
                            Err(err) => {
                                self.search_error = Some(err.to_string());
                            }
                        }
                    }
                } else {
                    self.search_error = Some("Selected item missing id".to_string());
                }
            }
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
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
