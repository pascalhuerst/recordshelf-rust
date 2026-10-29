use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
};

use crate::barcode_reader::{self, BarcodeReader, ScanEvent};
use crate::config::Settings;
use crate::discogs::{
    CollectionFolder, CollectionInstance, DiscogsClient, Release, SearchItem, SearchQuery,
};
use crate::worker::{Request, Response, ScanResult, Worker};
use eframe::egui::{self, Color32, RichText};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Scan,
    Search,
}

/// One-line message for the status bar.
struct Notice {
    text: String,
    is_error: bool,
}

pub struct RecordShelfApp {
    settings: Settings,
    config_path: Option<PathBuf>,
    repaint: egui::Context,
    worker: Worker,
    scanner_stop: Arc<AtomicBool>,
    scanner_rx: Receiver<ScanEvent>,
    scanner_status: String,
    notice: Option<Notice>,
    tab: Tab,
    folders: Vec<CollectionFolder>,
    folder_picker_open: bool,
    folder_selected: Option<usize>,
    /// Known copies in the collection per release id (empty = checked, not owned).
    ownership: HashMap<u32, Vec<CollectionInstance>>,
    scan_result: Option<ScanResult>,
    search_focus: bool,
    search_q: String,
    search_artist: String,
    search_title: String,
    search_catno: String,
    search_barcode: String,
    search_results: Vec<SearchItem>,
    selected_result: Option<usize>,
    selected_release: Option<Release>,
}

impl RecordShelfApp {
    pub fn new(repaint: egui::Context, settings: Settings, config_path: Option<PathBuf>) -> Self {
        let (scanner_stop, scanner_rx, scanner_status) = start_scanner(&settings, &repaint);
        let mut app = Self {
            worker: Worker::spawn(repaint.clone()),
            settings,
            config_path,
            repaint,
            scanner_stop,
            scanner_rx,
            scanner_status,
            notice: None,
            tab: Tab::Scan,
            folders: Vec::new(),
            folder_picker_open: false,
            folder_selected: None,
            ownership: HashMap::new(),
            scan_result: None,
            search_focus: false,
            search_q: String::new(),
            search_artist: String::new(),
            search_title: String::new(),
            search_catno: String::new(),
            search_barcode: String::new(),
            search_results: Vec::new(),
            selected_result: None,
            selected_release: None,
        };
        app.configure_worker();
        app
    }

    /// Hands the current Discogs credentials to the worker and preloads folders.
    fn configure_worker(&mut self) {
        let client = if self.settings.has_token() {
            match DiscogsClient::new(
                &self.settings.user_agent,
                &self.settings.token,
                &self.settings.currency,
            ) {
                Ok(client) => Some(client),
                Err(err) => {
                    self.set_error(format!("{err:#}"));
                    None
                }
            }
        } else {
            self.set_error(
                "No Discogs token configured (set DISCOGS_TOKEN or token in the settings file)"
                    .to_string(),
            );
            None
        };
        let can_load_folders = client.is_some() && !self.settings.username.trim().is_empty();
        self.worker.send(Request::Configure {
            client,
            username: self.settings.username.clone(),
        });
        if can_load_folders {
            self.worker.send(Request::LoadFolders);
        }
    }

    fn set_info(&mut self, text: String) {
        self.notice = Some(Notice {
            text,
            is_error: false,
        });
    }

    fn set_error(&mut self, text: String) {
        self.notice = Some(Notice {
            text,
            is_error: true,
        });
    }

    fn selected_id(&self) -> Option<u32> {
        self.search_results
            .get(self.selected_result?)
            .and_then(|item| item.id)
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.scanner_rx.try_recv() {
            match ev {
                ScanEvent::Info(msg) => self.scanner_status = msg,
                ScanEvent::Code(code) => {
                    self.scanner_status = format!("Scanned {code}");
                    self.worker.send(Request::Scan(code));
                }
            }
        }
        for resp in self.worker.poll() {
            self.handle_response(resp);
        }
    }

    fn handle_response(&mut self, resp: Response) {
        match resp {
            Response::Scan(res) => {
                if let (Some(id), Some(owned)) =
                    (res.release.as_ref().and_then(|r| r.id), &res.owned)
                {
                    self.ownership.insert(id, owned.clone());
                }
                self.scanner_status = format!("Last scan {} ({:.0?})", res.barcode, res.elapsed);
                self.scan_result = Some(res);
            }
            Response::Search(Ok(items)) => {
                self.selected_result = if items.is_empty() { None } else { Some(0) };
                self.search_focus = items.is_empty();
                self.search_results = items;
                self.selected_release = None;
                self.notice = None;
            }
            Response::Search(Err(err)) => self.set_error(err),
            Response::Release { id, result } => match result {
                Ok(release) => {
                    // Ignore answers for a result the user has already moved away from.
                    if self.selected_id() == Some(id) {
                        self.selected_release = Some(release);
                        self.notice = None;
                    }
                }
                Err(err) => self.set_error(err),
            },
            Response::Folders(Ok(folders)) => {
                self.folders = folders;
                self.folder_selected = match self.folder_selected {
                    _ if self.folders.is_empty() => None,
                    Some(i) => Some(i.min(self.folders.len() - 1)),
                    None => Some(0),
                };
            }
            Response::Folders(Err(err)) => self.set_error(err),
            Response::Ownership { release_id, result } => match result {
                Ok(instances) => {
                    self.ownership.insert(release_id, instances);
                }
                Err(err) => self.set_error(err),
            },
            Response::Added {
                release_id,
                folder_id,
                result,
            } => match result {
                Ok(()) => {
                    let folder = self
                        .folders
                        .iter()
                        .find(|f| f.id == folder_id)
                        .map_or_else(|| format!("#{folder_id}"), |f| f.name.clone());
                    self.set_info(format!("Added release {release_id} to folder {folder}"));
                    // Refresh what we know: new copy count and folder item counts.
                    self.worker.send(Request::CheckOwnership(release_id));
                    self.worker.send(Request::LoadFolders);
                }
                Err(err) => self.set_error(err),
            },
        }
    }

    fn ui_scan(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let Some(result) = &self.scan_result else {
                    return;
                };
                ui.group(|ui| {
                    if let Some(err) = &result.error {
                        ui.colored_label(Color32::RED, format!("Error: {err}"));
                    } else if let Some(release) = &result.release {
                        if result.matches > 1 {
                            ui.colored_label(
                                Color32::LIGHT_BLUE,
                                format!(
                                    "{} releases share this barcode; showing the first. Use Search > Barcode to see all.",
                                    result.matches
                                ),
                            );
                        }
                        render_ownership(ui, result.owned.as_deref(), &self.folders);
                        render_release(ui, release);
                    } else {
                        ui.label("No data");
                    }
                });
            });
    }

    fn ui_search(&mut self, ui: &mut egui::Ui) {
        let mut trigger_search = false;
        let mut search_fields_active = false;
        let focus = std::mem::take(&mut self.search_focus);
        let fields = [
            ("Query:", &mut self.search_q),
            ("Artist:", &mut self.search_artist),
            ("Release:", &mut self.search_title),
            ("Catalog No:", &mut self.search_catno),
            ("Barcode:", &mut self.search_barcode),
        ];
        for (idx, (label, value)) in fields.into_iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(label);
                let resp = ui.text_edit_singleline(value);
                if idx == 0 && focus {
                    resp.request_focus();
                }
                search_fields_active |= resp.has_focus();
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    trigger_search = true;
                }
            });
        }
        if trigger_search {
            self.perform_search();
        }

        ui.separator();

        egui::ScrollArea::vertical()
            .id_source("search-results")
            .max_height(ui.available_height() * 0.5)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (idx, item) in self.search_results.iter().enumerate() {
                    let line = format!(
                        "{} | {} | {} | {} | {} | {}",
                        item.title_display(),
                        item.artist_display(),
                        item.label_display(),
                        item.year.map(|y| y.to_string()).unwrap_or_default(),
                        item.country.clone().unwrap_or_default(),
                        item.format_display()
                    );
                    let owned = item
                        .id
                        .and_then(|id| self.ownership.get(&id))
                        .is_some_and(|o| !o.is_empty());
                    let text = if owned {
                        RichText::new(format!("\u{2713} {line}")).color(Color32::LIGHT_GREEN)
                    } else {
                        RichText::new(line)
                    };
                    if ui
                        .selectable_label(self.selected_result == Some(idx), text)
                        .clicked()
                    {
                        self.selected_result = Some(idx);
                    }
                }
            });

        // Keyboard handling. Skipped while typing, and on the frame a search was
        // triggered by Enter so that the same keypress does not also pick a result.
        if !search_fields_active && !trigger_search {
            self.handle_search_keys(ui.ctx());
        }
        if self.folder_picker_open {
            self.show_folder_picker(ui.ctx());
        }

        ui.separator();

        egui::ScrollArea::vertical()
            .id_source("search-release")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(release) = &self.selected_release {
                    let owned = release.id.and_then(|id| self.ownership.get(&id));
                    render_ownership(ui, owned.map(Vec::as_slice), &self.folders);
                    render_release(ui, release);
                }
            });
    }

    fn handle_search_keys(&mut self, ctx: &egui::Context) {
        let pressed = |key| ctx.input(|i| i.key_pressed(key));
        let (up, down) = (pressed(egui::Key::ArrowUp), pressed(egui::Key::ArrowDown));
        let (enter, esc) = (pressed(egui::Key::Enter), pressed(egui::Key::Escape));

        if pressed(egui::Key::A) {
            if self.folder_picker_open {
                self.add_selected_to_collection();
            } else {
                self.open_folder_picker();
            }
            return;
        }

        if self.folder_picker_open {
            step(&mut self.folder_selected, self.folders.len(), up, down);
            if enter {
                self.add_selected_to_collection();
            }
            if esc {
                self.folder_picker_open = false;
            }
        } else {
            step(
                &mut self.selected_result,
                self.search_results.len(),
                up,
                down,
            );
            if enter {
                self.load_selected_release();
            }
            if esc {
                self.selected_result = None;
                self.selected_release = None;
                self.search_focus = true;
            }
        }
    }

    fn show_folder_picker(&mut self, ctx: &egui::Context) {
        let mut open = true;
        egui::Window::new("Add to folder")
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                let owned = self.selected_id().and_then(|id| self.ownership.get(&id));
                if let Some(owned) = owned.filter(|o| !o.is_empty()) {
                    ui.colored_label(
                        Color32::YELLOW,
                        format!(
                            "Already in your collection ({}x). You can still add another copy.",
                            owned.len()
                        ),
                    );
                }
                if self.folders.is_empty() {
                    ui.label("Loading folders...");
                }
                for (idx, folder) in self.folders.iter().enumerate() {
                    let label = format!("{} ({} items)", folder.name, folder.count);
                    if ui
                        .selectable_label(self.folder_selected == Some(idx), label)
                        .clicked()
                    {
                        self.folder_selected = Some(idx);
                    }
                }
                ui.add_space(4.0);
                ui.label(RichText::new("Up/Down: select  Enter or A: add  Esc: cancel").small());
            });
        self.folder_picker_open = open;
    }

    fn perform_search(&mut self) {
        let query = SearchQuery {
            query: non_empty(&self.search_q),
            artist: non_empty(&self.search_artist),
            release_title: non_empty(&self.search_title),
            catno: non_empty(&self.search_catno),
            barcode: non_empty(&self.search_barcode),
            kind: None,
            page: Some(1),
            per_page: Some(25),
        };
        self.worker.send(Request::Search(query));
        self.search_q.clear();
        self.search_artist.clear();
        self.search_title.clear();
        self.search_catno.clear();
        self.search_barcode.clear();
    }

    fn load_selected_release(&mut self) {
        match self.selected_id() {
            Some(id) => {
                self.worker.send(Request::LoadRelease(id));
                self.worker.send(Request::CheckOwnership(id));
            }
            None => self.set_error("Select a result first".to_string()),
        }
    }

    fn open_folder_picker(&mut self) {
        let Some(id) = self.selected_id() else {
            self.set_error("Select a result before choosing a folder".to_string());
            return;
        };
        if self.folders.is_empty() {
            self.worker.send(Request::LoadFolders);
        }
        // Always refresh: the collection may have changed on discogs.com.
        self.worker.send(Request::CheckOwnership(id));
        if self.folder_selected.is_none() && !self.folders.is_empty() {
            self.folder_selected = Some(0);
        }
        self.folder_picker_open = true;
    }

    fn add_selected_to_collection(&mut self) {
        let Some(id) = self.selected_id() else {
            self.set_error("Select a result first".to_string());
            return;
        };
        let Some(folder) = self.folder_selected.and_then(|i| self.folders.get(i)) else {
            self.set_error("Select a folder first".to_string());
            return;
        };
        self.worker.send(Request::AddToCollection {
            release_id: id,
            folder_id: folder.id,
        });
        self.folder_picker_open = false;
    }
}

impl eframe::App for RecordShelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

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

        egui::TopBottomPanel::bottom("status-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.scanner_status);
                if let Some(notice) = &self.notice {
                    ui.separator();
                    let color = if notice.is_error {
                        Color32::RED
                    } else {
                        Color32::LIGHT_GREEN
                    };
                    ui.colored_label(color, &notice.text);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.worker.pending() > 0 {
                        ui.label("Contacting Discogs...");
                        ui.spinner();
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Scan => self.ui_scan(ui),
            Tab::Search => self.ui_search(ui),
        });
    }
}

impl Drop for RecordShelfApp {
    fn drop(&mut self) {
        self.scanner_stop.store(true, Ordering::SeqCst);
    }
}

/// Moves a list selection up or down, staying within `0..len`.
fn step(selected: &mut Option<usize>, len: usize, up: bool, down: bool) {
    if len == 0 {
        return;
    }
    if up {
        *selected = selected.map(|s| s.saturating_sub(1));
    }
    if down {
        *selected = Some(selected.map_or(0, |s| (s + 1).min(len - 1)));
    }
}

/// Starts the scanner thread for the configured device. Returns its stop flag,
/// the event receiver and an initial status line.
fn start_scanner(
    settings: &Settings,
    repaint: &egui::Context,
) -> (Arc<AtomicBool>, Receiver<ScanEvent>, String) {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let devices = barcode_reader::list_devices();
    let status =
        match barcode_reader::pick_device(&devices, &settings.device_name, &settings.device_path) {
            Some(path) => {
                match BarcodeReader::spawn(path, stop.clone(), tx, Some(repaint.clone())) {
                    Ok(()) => "Waiting for scans...".to_string(),
                    Err(err) => format!("Scanner not started: {err}"),
                }
            }
            None => "No scanner configured (set device_name in the settings file)".to_string(),
        };
    (stop, rx, status)
}

fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Shows whether (and where) a release is already in the collection.
fn render_ownership(
    ui: &mut egui::Ui,
    owned: Option<&[CollectionInstance]>,
    folders: &[CollectionFolder],
) {
    match owned {
        None => {}
        Some([]) => {
            ui.colored_label(Color32::GRAY, "Not in your collection");
        }
        Some(instances) => {
            let names: Vec<String> = instances
                .iter()
                .map(|inst| {
                    folders
                        .iter()
                        .find(|f| f.id == inst.folder_id)
                        .map_or_else(|| format!("#{}", inst.folder_id), |f| f.name.clone())
                })
                .collect();
            ui.colored_label(
                Color32::YELLOW,
                format!(
                    "\u{26A0} Already in your collection ({}x): {}",
                    instances.len(),
                    names.join(", ")
                ),
            );
        }
    }
    ui.add_space(4.0);
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
