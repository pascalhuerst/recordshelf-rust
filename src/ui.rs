use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
};

use crate::barcode_reader::{self, BarcodeReader, InputDevice, ScanEvent};
use crate::config::Settings;
use crate::decks;
use crate::decks_bridge::{self, Bridge, DecksPage};
use crate::discogs::{
    CollectionFolder, CollectionInstance, DiscogsClient, Release, SearchItem, SearchQuery,
};
use crate::worker::{Request, Response, Worker};
use eframe::egui::{self, Color32, RichText};

/// Download state of a cover picture, keyed by URL.
enum ImageState {
    Loading,
    Ready(Arc<[u8]>),
    Failed,
}

const THUMB_SIZE: f32 = 56.0;
const COVER_SIZE: f32 = 220.0;

/// The decks.de bridge: localhost server plus the channel for its results.
struct DecksState {
    bridge: Bridge,
    rx: Receiver<DecksPage>,
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
    folders: Vec<CollectionFolder>,
    folder_picker_open: bool,
    folder_selected: Option<usize>,
    /// Known copies in the collection per release id (empty = checked, not owned).
    ownership: HashMap<u32, Vec<CollectionInstance>>,
    search_focus: bool,
    search_q: String,
    search_artist: String,
    search_title: String,
    search_catno: String,
    search_barcode: String,
    search_results: Vec<SearchItem>,
    selected_result: Option<usize>,
    selected_release: Option<Release>,
    settings_open: bool,
    images: HashMap<String, ImageState>,
    decks: Option<DecksState>,
    decks_info: Option<String>,
    settings_draft: Settings,
    devices: Vec<InputDevice>,
}

impl RecordShelfApp {
    pub fn new(
        repaint: egui::Context,
        settings: Settings,
        config_path: Option<PathBuf>,
        decks_port: Option<u16>,
    ) -> Self {
        egui_extras::install_image_loaders(&repaint);
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
            folders: Vec::new(),
            folder_picker_open: false,
            folder_selected: None,
            ownership: HashMap::new(),
            search_focus: false,
            search_q: String::new(),
            search_artist: String::new(),
            search_title: String::new(),
            search_catno: String::new(),
            search_barcode: String::new(),
            search_results: Vec::new(),
            selected_result: None,
            selected_release: None,
            settings_open: false,
            images: HashMap::new(),
            decks: None,
            decks_info: None,
            settings_draft: Settings::default(),
            devices: Vec::new(),
        };
        app.configure_worker();
        if let Some(port) = decks_port {
            app.start_decks_bridge(port);
        }
        if !app.settings.has_token() {
            app.open_settings();
        }
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

    /// Starts the localhost bridge and writes the userscript next to the settings file.
    fn start_decks_bridge(&mut self, port: u16) {
        let Some(dir) = self.config_path.as_ref().and_then(|p| p.parent()) else {
            self.set_error("decks bridge needs a config directory".to_string());
            return;
        };
        let script_path = dir.join("decks.user.js");
        let result = decks_bridge::load_or_create_token(dir).and_then(|token| {
            std::fs::write(&script_path, decks_bridge::userscript(port, &token))?;
            let (tx, rx) = std::sync::mpsc::channel();
            let bridge = Bridge::spawn(port, token, tx, self.repaint.clone())?;
            Ok(DecksState { bridge, rx })
        });
        match result {
            Ok(state) => {
                self.decks = Some(state);
                self.set_info(format!(
                    "decks bridge on 127.0.0.1:{port}; install userscript {}",
                    script_path.display()
                ));
            }
            Err(err) => self.set_error(format!("decks bridge: {err:#}")),
        }
    }

    fn handle_decks_page(&mut self, page: DecksPage) {
        let code = page.code;
        if page.status != Some(200) {
            let status = page
                .status
                .map_or("no reply".to_string(), |s| s.to_string());
            self.decks_info = None;
            self.set_error(format!(
                "decks.de lookup for {code} failed ({status}); reload the decks.de tab"
            ));
            return;
        }
        match decks::find_entry(&code, &page.entries) {
            Some(entry) => {
                self.decks_info = Some(format!(
                    "decks {code}: {} - {} | {} | {}",
                    entry.artist, entry.title, entry.catno, entry.label
                ));
                self.worker.send(Request::DecksLookup {
                    code,
                    entry: entry.clone(),
                });
            }
            None => {
                self.decks_info = None;
                self.set_error(format!(
                    "decks.de has no exact match for {code} ({} results)",
                    page.entries.len()
                ));
            }
        }
    }

    /// Shows releases found for a scan in the results list and loads the first.
    fn show_candidates(&mut self, items: Vec<SearchItem>, none_found: String) {
        if items.is_empty() {
            self.set_error(none_found);
            return;
        }
        self.notice = None;
        self.folder_picker_open = false;
        self.search_results = items;
        self.selected_result = Some(0);
        self.selected_release = None;
        self.load_selected_release();
    }

    /// Starts downloading a picture unless it is already loading or cached.
    fn ensure_image(&mut self, url: &str) {
        if !self.images.contains_key(url) {
            self.images.insert(url.to_string(), ImageState::Loading);
            self.worker.send(Request::FetchImage(url.to_string()));
        }
    }

    fn open_settings(&mut self) {
        self.settings_draft = self.settings.clone();
        self.devices = barcode_reader::list_devices();
        self.settings_open = true;
    }

    /// Persists the edited settings and applies them without a restart.
    fn save_settings(&mut self) {
        let draft = self.settings_draft.clone();
        if let Some(path) = &self.config_path
            && let Err(err) = draft.save(path)
        {
            self.set_error(format!("{err:#}"));
            return;
        }
        self.settings = draft;
        self.scanner_stop.store(true, Ordering::SeqCst);
        let (stop, rx, status) = start_scanner(&self.settings, &self.repaint);
        self.scanner_stop = stop;
        self.scanner_rx = rx;
        self.scanner_status = status;
        self.folders.clear();
        self.folder_selected = None;
        self.ownership.clear();
        self.images.clear();
        self.notice = None;
        self.configure_worker();
        if self.config_path.is_none() {
            self.set_info("Settings applied (no config path, not saved)".to_string());
        }
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let (mut save, mut cancel) = (false, false);
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                let draft = &mut self.settings_draft;
                egui::Grid::new("settings-grid")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("Discogs token:");
                        ui.add(egui::TextEdit::singleline(&mut draft.token).password(true));
                        ui.end_row();
                        ui.label("Discogs username:");
                        ui.text_edit_singleline(&mut draft.username);
                        ui.end_row();
                        ui.label("Currency:");
                        ui.text_edit_singleline(&mut draft.currency);
                        ui.end_row();
                        ui.label("User agent:");
                        ui.text_edit_singleline(&mut draft.user_agent);
                        ui.end_row();
                        ui.label("Scanner name:");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut draft.device_name);
                            egui::ComboBox::from_id_source("scanner-device")
                                .selected_text("Pick...")
                                .show_ui(ui, |ui| {
                                    for dev in &self.devices {
                                        let label =
                                            format!("{} ({})", dev.name, dev.path.display());
                                        if ui.selectable_label(false, label).clicked() {
                                            draft.device_name = dev.name.clone();
                                        }
                                    }
                                });
                            if ui.button("Refresh").clicked() {
                                self.devices = barcode_reader::list_devices();
                            }
                        });
                        ui.end_row();
                        ui.label("Fallback device path:");
                        ui.text_edit_singleline(&mut draft.device_path);
                        ui.end_row();
                    });
                ui.label(
                    RichText::new(
                        "The scanner is found by name (substring match), so it keeps working \
                         when /dev/input numbering changes. The path is only a fallback.",
                    )
                    .small(),
                );
                if let Some(path) = &self.config_path {
                    ui.label(RichText::new(format!("Saved to {}", path.display())).small());
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    save = ui.button("Save").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if save {
            self.save_settings();
        }
        self.settings_open = open && !save && !cancel;
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
                    if decks::is_decks_code(&code) {
                        match &self.decks {
                            Some(state) => {
                                self.decks_info =
                                    Some(format!("decks {code}: waiting for the browser..."));
                                state.bridge.enqueue(code);
                            }
                            None => self.set_error(format!(
                                "{code} looks like a decks.de code; start with --decks-bridge"
                            )),
                        }
                    } else {
                        self.worker.send(Request::Scan(code));
                    }
                }
            }
        }
        let pages: Vec<DecksPage> = self
            .decks
            .as_ref()
            .map(|d| d.rx.try_iter().collect())
            .unwrap_or_default();
        for page in pages {
            self.handle_decks_page(page);
        }
        for resp in self.worker.poll() {
            self.handle_response(resp);
        }
    }

    fn handle_response(&mut self, resp: Response) {
        match resp {
            Response::Scan { code, result } => match result {
                Ok(items) => {
                    let msg = format!("No release found for {code}");
                    self.show_candidates(items, msg);
                }
                Err(err) => self.set_error(err),
            },
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
            Response::Decks {
                code,
                entry,
                result,
            } => match result {
                Ok(items) => self.show_candidates(
                    items,
                    format!(
                        "No Discogs release found for {code} ({} / {})",
                        entry.catno, entry.label
                    ),
                ),
                Err(err) => self.set_error(err),
            },
            Response::Image { url, result } => {
                let state = match result {
                    Ok(bytes) => ImageState::Ready(bytes.into()),
                    Err(_) => ImageState::Failed,
                };
                self.images.insert(url, state);
            }
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

    fn ui_main(&mut self, ui: &mut egui::Ui) {
        let mut trigger_search = false;
        let mut search_fields_active = false;
        if let Some(info) = &self.decks_info {
            ui.colored_label(Color32::LIGHT_BLUE, info);
        }
        let focus = std::mem::take(&mut self.search_focus);
        let fields = [
            ("Query:", &mut self.search_q),
            ("Artist:", &mut self.search_artist),
            ("Release:", &mut self.search_title),
            ("Catalog No:", &mut self.search_catno),
            ("Barcode:", &mut self.search_barcode),
        ];
        // Two-column grid: labels share one column, so all inputs start at the
        // same x and stretch to the right edge of the window.
        egui::Grid::new("search-fields")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                for (idx, (label, value)) in fields.into_iter().enumerate() {
                    ui.label(label);
                    let resp =
                        ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
                    if idx == 0 && focus {
                        resp.request_focus();
                    }
                    search_fields_active |= resp.has_focus();
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        trigger_search = true;
                    }
                    ui.end_row();
                }
            });
        if trigger_search {
            self.perform_search();
        }

        ui.separator();

        let mut to_fetch: Vec<String> = Vec::new();
        egui::ScrollArea::vertical()
            .id_source("search-results")
            .max_height(ui.available_height() * 0.5)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (idx, item) in self.search_results.iter().enumerate() {
                    let details = [
                        item.label_display(),
                        item.year.map(|y| y.to_string()).unwrap_or_default(),
                        item.country.clone().unwrap_or_default(),
                        item.format_display(),
                    ]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(" | ");
                    let line = format!("{}\n{details}", item.title_display());
                    let owned = item
                        .id
                        .and_then(|id| self.ownership.get(&id))
                        .is_some_and(|o| !o.is_empty());
                    let text = if owned {
                        RichText::new(format!("\u{2713} {line}")).color(Color32::LIGHT_GREEN)
                    } else {
                        RichText::new(line)
                    };
                    let row_top = ui.cursor().min;
                    let visible = ui.is_rect_visible(egui::Rect::from_min_size(
                        row_top,
                        egui::vec2(1.0, THUMB_SIZE),
                    ));
                    ui.horizontal(|ui| {
                        match item.thumb.as_deref().filter(|u| !u.is_empty()) {
                            Some(url) => {
                                // Only fetch pictures of rows that are actually on screen.
                                if !draw_image(ui, &self.images, url, THUMB_SIZE) && visible {
                                    to_fetch.push(url.to_string());
                                }
                            }
                            None => {
                                draw_placeholder(ui, THUMB_SIZE);
                            }
                        }
                        if ui
                            .selectable_label(self.selected_result == Some(idx), text)
                            .clicked()
                        {
                            self.selected_result = Some(idx);
                        }
                    });
                }
            });
        for url in to_fetch {
            self.ensure_image(&url);
        }

        // Keyboard handling. Skipped while typing, and on the frame a search was
        // triggered by Enter so that the same keypress does not also pick a result.
        if !search_fields_active && !trigger_search && !self.settings_open {
            self.handle_search_keys(ui.ctx());
        }
        if self.folder_picker_open {
            self.show_folder_picker(ui.ctx());
        }

        ui.separator();

        let cover = self
            .selected_release
            .as_ref()
            .and_then(|r| r.cover_url())
            .map(str::to_string);
        if let Some(url) = &cover {
            self.ensure_image(url);
        }
        egui::ScrollArea::vertical()
            .id_source("search-release")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(release) = &self.selected_release {
                    let owned = release.id.and_then(|id| self.ownership.get(&id));
                    ui.horizontal_top(|ui| {
                        if let Some(url) = &cover {
                            draw_image(ui, &self.images, url, COVER_SIZE);
                        }
                        ui.vertical(|ui| {
                            render_ownership(ui, owned.map(Vec::as_slice), &self.folders);
                            render_release(ui, release);
                        });
                    });
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
            label: None,
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
        if self.decks.is_some() {
            // Keeps the "browser connected" indicator fresh.
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }

        egui::TopBottomPanel::top("top-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RecordShelf");
                ui.separator();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::widgets::global_dark_light_mode_switch(ui);
                    if ui.button("Settings").clicked() {
                        self.open_settings();
                    }
                });
            });
        });

        egui::TopBottomPanel::bottom("status-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.scanner_status);
                if let Some(state) = &self.decks {
                    ui.separator();
                    if state.bridge.connected() {
                        ui.colored_label(Color32::LIGHT_GREEN, "decks: browser connected");
                    } else {
                        ui.colored_label(Color32::YELLOW, "decks: browser not connected");
                    }
                }
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

        egui::CentralPanel::default().show(ctx, |ui| self.ui_main(ui));

        if self.settings_open {
            self.show_settings(ctx);
        }
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

/// Draws a downloaded picture, or a placeholder box, always occupying exactly
/// `size` x `size` so that rows and layouts do not shift.
/// Returns whether the URL is already known, i.e. needs no fetch.
fn draw_image(
    ui: &mut egui::Ui,
    images: &HashMap<String, ImageState>,
    url: &str,
    size: f32,
) -> bool {
    match images.get(url) {
        Some(ImageState::Ready(bytes)) => {
            let rect = placeholder_rect(ui, size);
            ui.put(
                rect,
                egui::Image::from_bytes(format!("bytes://{url}"), bytes.clone())
                    .fit_to_exact_size(rect.size())
                    .rounding(3.0),
            );
            true
        }
        Some(_) => {
            draw_placeholder(ui, size);
            true
        }
        None => {
            draw_placeholder(ui, size);
            false
        }
    }
}

fn placeholder_rect(ui: &mut egui::Ui, size: f32) -> egui::Rect {
    ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover())
        .0
}

/// Empty box shown while a picture is loading or when there is none.
fn draw_placeholder(ui: &mut egui::Ui, size: f32) {
    let rect = placeholder_rect(ui, size);
    ui.painter()
        .rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    ui.painter().rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
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
    if let Some(artists) = &release.artists
        && !artists.is_empty()
    {
        let names = artists
            .iter()
            .filter_map(|a| a.name.as_deref())
            .collect::<Vec<_>>()
            .join(", ");
        if !names.is_empty() {
            ui.label(RichText::new(format!("Artist: {names}")).strong());
        }
    }
    if let Some(labels) = &release.labels
        && !labels.is_empty()
    {
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
    if let Some(year) = release.year
        && year > 0
    {
        ui.label(RichText::new(format!("Year: {year}")).strong());
    }

    ui.add_space(8.0);

    // Secondary info
    if let Some(country) = &release.country
        && !country.is_empty()
    {
        ui.label(RichText::new(format!("Country: {country}")).small());
    }
    if let Some(formats) = &release.formats
        && !formats.is_empty()
    {
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
    if let Some(genres) = &release.genres
        && !genres.is_empty()
    {
        ui.label(RichText::new(format!("Genres: {}", genres.join(", "))).small());
    }
    if let Some(styles) = &release.styles
        && !styles.is_empty()
    {
        ui.label(RichText::new(format!("Styles: {}", styles.join(", "))).small());
    }

    ui.add_space(8.0);

    // Tracklist
    if let Some(tracks) = &release.tracklist
        && !tracks.is_empty()
    {
        ui.label(RichText::new("Tracklist:").strong());
        for track in tracks {
            let mut title = track.title.clone().unwrap_or_default();
            if let Some(duration) = &track.duration
                && !duration.is_empty()
            {
                title.push_str(&format!(" [{duration}]"));
            }
            let pos = track.position.as_deref().unwrap_or("");
            ui.label(RichText::new(format!("  {pos} {title}")).small());
        }
    }

    ui.add_space(8.0);

    if let Some(uri) = &release.uri
        && !uri.is_empty()
    {
        ui.hyperlink(uri);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_moves_within_bounds() {
        let mut sel = Some(1);
        step(&mut sel, 3, false, true);
        assert_eq!(sel, Some(2));
        step(&mut sel, 3, false, true);
        assert_eq!(sel, Some(2), "stays on the last entry");
        step(&mut sel, 3, true, false);
        step(&mut sel, 3, true, false);
        step(&mut sel, 3, true, false);
        assert_eq!(sel, Some(0), "stays on the first entry");
    }

    #[test]
    fn step_down_without_selection_selects_first() {
        let mut sel = None;
        step(&mut sel, 3, false, true);
        assert_eq!(sel, Some(0));
    }

    #[test]
    fn step_up_without_selection_stays_empty() {
        let mut sel = None;
        step(&mut sel, 3, true, false);
        assert_eq!(sel, None);
    }

    #[test]
    fn step_on_empty_list_does_nothing() {
        let mut sel = None;
        step(&mut sel, 0, false, true);
        assert_eq!(sel, None);
    }

    #[test]
    fn non_empty_trims_and_drops_blanks() {
        assert_eq!(non_empty("  abc "), Some("abc".to_string()));
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty(""), None);
    }
}
