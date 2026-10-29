use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    time::{Duration, Instant},
};

use crate::barcode_reader::{self, BarcodeReader, InputDevice, ScanEvent};
use crate::config::Settings;
use crate::controls::{self, AddDecision, Command, MoveDecision, PENDING_INSTANCE, RemoveDecision};
use crate::discogs::{
    CollectionFolder, CollectionInstance, DiscogsClient, Release, SearchItem, SearchQuery,
    format_line,
};
use crate::shop_bridge::{self, Bridge, ShopPage};
use crate::shops::{self, Route, Shop};
use crate::worker::{Request, Response, Worker};
use eframe::egui::{self, Color32, RichText};

/// Download state of a cover picture, keyed by URL.
enum ImageState {
    Loading,
    Ready(Arc<[u8]>),
    Failed,
}

/// Results beyond this many are only fetched once selected.
const PREFETCH_LIMIT: usize = 5;
/// How long the selection must rest before an unfetched release is requested,
/// so that holding an arrow key does not flood the API.
const SELECTION_DEBOUNCE: Duration = Duration::from_millis(250);

const THUMB_SIZE: f32 = 56.0;
const COVER_SIZE: f32 = 220.0;

/// What choosing a folder in the folder picker does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerPurpose {
    /// Set the folder that new releases are added to.
    SetTarget,
    /// Move these copies of a release to the chosen folder.
    Move {
        release_id: u32,
        copies: Vec<CollectionInstance>,
    },
}

/// Popup to choose which copies of a release to move (when there are several).
struct CopyPicker {
    release_id: u32,
    copies: Vec<CollectionInstance>,
    /// 0 = all copies, i = copy i-1.
    cursor: usize,
}

/// A destructive or unusual action waiting for Enter (yes) or Esc (no).
enum Confirm {
    AddDuplicate {
        release_id: u32,
        folder_id: u32,
        title: String,
        copies: usize,
    },
    Remove {
        release_id: u32,
        instance: CollectionInstance,
        title: String,
        folder_name: String,
    },
}

/// The shop bridge: localhost server plus the channel for its results.
struct BridgeState {
    bridge: Bridge,
    rx: Receiver<ShopPage>,
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
    /// Highlighted entry while the folder picker is open.
    picker_cursor: Option<usize>,
    /// Folder that releases are added to.
    folder_selected: Option<usize>,
    confirm: Option<Confirm>,
    picker_purpose: PickerPurpose,
    copy_picker: Option<CopyPicker>,
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
    /// Full release data by id; filled by prefetching and by browsing the results.
    releases: HashMap<u32, Release>,
    /// Releases already requested (so each is fetched once per session).
    requested: HashSet<u32>,
    /// Releases whose request failed; Enter retries these.
    failed: HashSet<u32>,
    last_selected: Option<u32>,
    selection_changed: Instant,
    settings_open: bool,
    images: HashMap<String, ImageState>,
    bridge: Option<BridgeState>,
    shop_info: Option<String>,
    settings_draft: Settings,
    devices: Vec<InputDevice>,
}

impl RecordShelfApp {
    pub fn new(
        repaint: egui::Context,
        settings: Settings,
        config_path: Option<PathBuf>,
        bridge_port: Option<u16>,
    ) -> Self {
        egui_extras::install_image_loaders(&repaint);
        repaint.set_zoom_factor(settings.zoom());
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
            picker_cursor: None,
            folder_selected: None,
            confirm: None,
            picker_purpose: PickerPurpose::SetTarget,
            copy_picker: None,
            ownership: HashMap::new(),
            search_focus: false,
            search_q: String::new(),
            search_artist: String::new(),
            search_title: String::new(),
            search_catno: String::new(),
            search_barcode: String::new(),
            search_results: Vec::new(),
            selected_result: None,
            releases: HashMap::new(),
            requested: HashSet::new(),
            failed: HashSet::new(),
            last_selected: None,
            selection_changed: Instant::now(),
            settings_open: false,
            images: HashMap::new(),
            bridge: None,
            shop_info: None,
            settings_draft: Settings::default(),
            devices: Vec::new(),
        };
        app.configure_worker();
        if let Some(port) = bridge_port {
            app.start_bridge(port);
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
    fn start_bridge(&mut self, port: u16) {
        let Some(dir) = self.config_path.as_ref().and_then(|p| p.parent()) else {
            self.set_error("shop bridge needs a config directory".to_string());
            return;
        };
        let script_path = dir.join("recordshelf.user.js");
        let result = shop_bridge::load_or_create_token(dir).and_then(|token| {
            std::fs::write(&script_path, shop_bridge::userscript(port, &token))?;
            let (tx, rx) = std::sync::mpsc::channel();
            let bridge = Bridge::spawn(port, token, tx, self.repaint.clone())?;
            Ok(BridgeState { bridge, rx })
        });
        match result {
            Ok(state) => {
                self.bridge = Some(state);
                self.set_info(format!(
                    "shop bridge on 127.0.0.1:{port}; install userscript {}",
                    script_path.display()
                ));
            }
            Err(err) => self.set_error(format!("shop bridge: {err:#}")),
        }
    }

    /// Sends a scanned code to the browser tab of its shop.
    fn lookup_in_shop(&mut self, shop: Shop, code: String) {
        if let Some(state) = &self.bridge {
            self.shop_info = Some(format!(
                "{} {code}: waiting for the browser...",
                shop.name()
            ));
            state.bridge.enqueue(shop, code);
        }
    }

    fn handle_shop_page(&mut self, page: ShopPage) {
        let ShopPage {
            shop,
            code,
            status,
            entries,
        } = page;
        let name = shop.name();
        if status != Some(200) {
            let status = status.map_or("no reply".to_string(), |s| s.to_string());
            self.shop_info = None;
            self.set_error(format!(
                "{name} lookup for {code} failed ({status}); reload the {name} tab"
            ));
            return;
        }
        match shops::find_entry(shop, &code, &entries) {
            Some(entry) => {
                self.shop_info = Some(format!(
                    "{name} {code}: {} - {} | {} | {}",
                    entry.artist, entry.title, entry.catno, entry.label
                ));
                self.worker.send(Request::ShopLookup {
                    shop,
                    code,
                    entry: entry.clone(),
                });
            }
            None => {
                self.shop_info = None;
                let hint = if entries.is_empty() {
                    format!("; if you are sure it exists, reload the {name} tab")
                } else {
                    String::new()
                };
                self.set_error(format!(
                    "{name} has no exact match for {code} ({} results){hint}",
                    entries.len()
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
        self.close_folder_picker();
        self.copy_picker = None;
        self.confirm = None;
        self.search_results = items;
        self.selected_result = Some(0);
        self.prefetch_results();
    }

    /// Fetches release data (and collection status) for the first few results
    /// so that browsing them shows details immediately.
    fn prefetch_results(&mut self) {
        let ids: Vec<u32> = self
            .search_results
            .iter()
            .take(PREFETCH_LIMIT)
            .filter_map(|item| item.id)
            .collect();
        for id in ids {
            self.request_release(id);
        }
    }

    /// Requests a release and its collection status unless already done.
    fn request_release(&mut self, id: u32) {
        if self.requested.insert(id) {
            self.worker.send(Request::LoadRelease(id));
            if !self.ownership.contains_key(&id) {
                self.worker.send(Request::CheckOwnership(id));
            }
        }
    }

    fn selected_release(&self) -> Option<&Release> {
        self.releases.get(&self.selected_id()?)
    }

    /// Remembers zoom changes (Ctrl +/-) in the settings file.
    fn persist_zoom(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        if (zoom - self.settings.zoom_factor).abs() < 1e-3 {
            return;
        }
        self.settings.zoom_factor = zoom;
        if let Some(path) = &self.config_path
            && let Err(err) = Settings::save_zoom(path, zoom)
        {
            self.set_error(format!("{err:#}"));
        }
    }

    /// Loads the selected release once the selection has settled.
    fn sync_selection(&mut self, ctx: &egui::Context) {
        let id = self.selected_id();
        if id != self.last_selected {
            self.last_selected = id;
            self.selection_changed = Instant::now();
        }
        if let Some(id) = id.filter(|id| !self.requested.contains(id)) {
            let waited = self.selection_changed.elapsed();
            if waited >= SELECTION_DEBOUNCE {
                self.request_release(id);
            } else {
                ctx.request_repaint_after(SELECTION_DEBOUNCE - waited);
            }
        }
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
        let mut draft = self.settings_draft.clone();
        // Zoom changes live (Ctrl +/-); do not let a stale copy in the draft undo it.
        draft.zoom_factor = self.settings.zoom_factor;
        draft.folder_id = self.settings.folder_id;
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
        self.releases.clear();
        self.requested.clear();
        self.failed.clear();
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

    /// Sends a scanned code to the shop browser tab or to Discogs.
    fn route_scan(&mut self, code: String) {
        let route = shops::route_scan(&code, self.bridge.is_some(), |shop| {
            self.bridge
                .as_ref()
                .is_some_and(|b| b.bridge.connected(shop))
        });
        match route {
            Route::Shop(shop) => self.lookup_in_shop(shop, code),
            Route::ShopUnavailable(shop) if self.bridge.is_none() => {
                self.set_error(format!(
                    "{code} looks like a {} code; start with --shop-bridge",
                    shop.name()
                ));
            }
            Route::ShopUnavailable(shop) => {
                let name = shop.name();
                self.set_error(format!(
                    "{code} looks like a {name} code, but no {name} tab is connected (open {name} with the userscript installed)"
                ));
            }
            Route::Discogs => self.worker.send(Request::Scan(code)),
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.scanner_rx.try_recv() {
            match ev {
                ScanEvent::Info(msg) => self.scanner_status = msg,
                ScanEvent::Code(code) => {
                    self.scanner_status = format!("Scanned {code}");
                    self.route_scan(code);
                }
            }
        }
        let pages: Vec<ShopPage> = self
            .bridge
            .as_ref()
            .map(|b| b.rx.try_iter().collect())
            .unwrap_or_default();
        for page in pages {
            self.handle_shop_page(page);
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
                self.prefetch_results();
                self.notice = None;
            }
            Response::Search(Err(err)) => self.set_error(err),
            Response::Release { id, result } => match result {
                Ok(release) => {
                    self.failed.remove(&id);
                    self.releases.insert(id, release);
                }
                Err(err) => {
                    self.failed.insert(id);
                    self.set_error(format!("{err} (press Enter to retry)"));
                }
            },
            Response::Shop {
                shop,
                code,
                entry,
                result,
            } => match result {
                Ok(items) => self.show_candidates(
                    items,
                    format!(
                        "No Discogs release found for {} {code} ({} / {})",
                        shop.name(),
                        entry.catno,
                        entry.label
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
            Response::Folders(Ok(mut folders)) => {
                // Folder 0 ("All") is a view; releases cannot be added to it.
                folders.retain(|f| f.id != 0);
                self.folders = folders;
                let wanted = self
                    .folders
                    .iter()
                    .position(|f| f.id == self.settings.folder_id);
                self.folder_selected = match (wanted, self.folder_selected) {
                    _ if self.folders.is_empty() => None,
                    (Some(i), _) => Some(i),
                    (None, Some(i)) => Some(i.min(self.folders.len() - 1)),
                    (None, None) => Some(0),
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
            } => {
                match result {
                    Ok(()) => {
                        let folder = self
                            .folders
                            .iter()
                            .find(|f| f.id == folder_id)
                            .map_or_else(|| format!("#{folder_id}"), |f| f.name.clone());
                        self.set_info(format!("Added release {release_id} to {folder}"));
                    }
                    Err(err) => self.set_error(err),
                }
                self.resync_collection(release_id);
            }
            Response::Moved {
                release_id,
                to_folder,
                result,
            } => {
                match result {
                    Ok(count) => {
                        let folder = self
                            .folders
                            .iter()
                            .find(|f| f.id == to_folder)
                            .map_or_else(|| format!("#{to_folder}"), |f| f.name.clone());
                        let copies = if count == 1 { "copy" } else { "copies" };
                        self.set_info(format!("Moved {count} {copies} to {folder}"));
                    }
                    Err(err) => self.set_error(err),
                }
                self.resync_collection(release_id);
            }
            Response::Removed { release_id, result } => {
                match result {
                    Ok(()) => {
                        self.set_info(format!("Removed release {release_id} from your collection"))
                    }
                    Err(err) => self.set_error(err),
                }
                self.resync_collection(release_id);
            }
        }
    }

    /// Reloads what we know about a release's copies and the folder counts,
    /// replacing any optimistic guess made when the command was issued.
    fn resync_collection(&mut self, release_id: u32) {
        self.worker.send(Request::CheckOwnership(release_id));
        self.worker.send(Request::LoadFolders);
    }

    fn ui_main(&mut self, ui: &mut egui::Ui) {
        let mut trigger_search = false;
        let mut search_fields_active = false;
        if let Some(info) = &self.shop_info {
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
                        // Prefer the full release data once loaded: it always has the
                        // free-text part (colour etc.) that tells pressings apart.
                        item.id
                            .and_then(|id| self.releases.get(&id))
                            .and_then(|r| r.formats.as_deref())
                            .map(format_line)
                            .filter(|l| !l.is_empty())
                            .unwrap_or_else(|| item.format_display()),
                    ]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(" | ");
                    let line = format!("{}\n{details}", item.title_display());
                    // Yellow third line for pressings that are already in the collection.
                    let owned_line = item
                        .id
                        .and_then(|id| self.ownership.get(&id))
                        .and_then(|o| ownership_text(Some(o), &self.folders))
                        .filter(|(_, owned)| *owned)
                        .map(|(text, _)| text);
                    let text = row_job(egui::TextStyle::Body.resolve(ui.style()), line, owned_line);
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
        self.show_copy_picker(ui.ctx());
        self.show_confirm(ui.ctx());

        ui.separator();
        if let Some(cmd) = self.command_bar(ui) {
            self.run_command(cmd);
        }
        ui.separator();

        let cover = self
            .selected_release()
            .and_then(|r| r.cover_url())
            .map(str::to_string);
        if let Some(url) = &cover {
            self.ensure_image(url);
        }
        egui::ScrollArea::vertical()
            .id_source("search-release")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(release) = self.selected_release() {
                    let owned = release.id.and_then(|id| self.ownership.get(&id));
                    ui.horizontal_top(|ui| {
                        match &cover {
                            Some(url) => {
                                draw_image(ui, &self.images, url, COVER_SIZE);
                            }
                            None => draw_placeholder(ui, COVER_SIZE),
                        }
                        ui.vertical(|ui| {
                            render_ownership(ui, owned.map(Vec::as_slice), &self.folders);
                            render_release(ui, release);
                        });
                    });
                } else if self.selected_id().is_some() {
                    ui.horizontal_top(|ui| {
                        draw_placeholder(ui, COVER_SIZE);
                        ui.label("Loading release...");
                    });
                }
            });
    }

    fn handle_search_keys(&mut self, ctx: &egui::Context) {
        let pressed = |key| ctx.input(|i| i.key_pressed(key) && !i.modifiers.command);
        let (up, down) = (pressed(egui::Key::ArrowUp), pressed(egui::Key::ArrowDown));
        let (enter, esc) = (pressed(egui::Key::Enter), pressed(egui::Key::Escape));

        if self.confirm.is_some() {
            if enter || pressed(egui::Key::Y) {
                self.accept_confirm();
            } else if esc || pressed(egui::Key::N) {
                self.confirm = None;
            }
            return;
        }

        if let Some(picker) = &mut self.copy_picker {
            let mut cursor = Some(picker.cursor);
            step(&mut cursor, picker.copies.len() + 1, up, down);
            picker.cursor = cursor.unwrap_or(0);
            if enter {
                self.choose_copies();
            } else if esc {
                self.copy_picker = None;
            }
            return;
        }

        if self.folder_picker_open {
            step(&mut self.picker_cursor, self.folders.len(), up, down);
            if enter && let Some(idx) = self.picker_cursor {
                self.choose_folder(idx);
            }
            if esc {
                self.close_folder_picker();
            }
            return;
        }

        if let Some(cmd) = Command::ALL.into_iter().find(|c| pressed(c.key())) {
            self.run_command(cmd);
            return;
        }
        step(
            &mut self.selected_result,
            self.search_results.len(),
            up,
            down,
        );
        if enter {
            self.retry_selected_release();
        }
        if esc {
            self.selected_result = None;
            self.search_focus = true;
        }
    }

    fn show_folder_picker(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut chosen = None;
        let title = match self.picker_purpose {
            PickerPurpose::SetTarget => "Choose folder",
            PickerPurpose::Move { .. } => "Move to folder",
        };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                if self.folders.is_empty() {
                    ui.label("Loading folders...");
                }
                for (idx, folder) in self.folders.iter().enumerate() {
                    let label = format!("{} ({} items)", folder.name, folder.count);
                    if ui
                        .selectable_label(self.picker_cursor == Some(idx), label)
                        .clicked()
                    {
                        chosen = Some(idx);
                    }
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Up/Down: select   Enter: use this folder   Esc: cancel").small(),
                );
            });
        if let Some(idx) = chosen {
            self.choose_folder(idx);
        } else if !open {
            self.close_folder_picker();
        }
    }

    fn close_folder_picker(&mut self) {
        self.folder_picker_open = false;
        self.picker_purpose = PickerPurpose::SetTarget;
    }

    fn show_copy_picker(&mut self, ctx: &egui::Context) {
        let Some(picker) = &self.copy_picker else {
            return;
        };
        let mut open = true;
        let mut chosen = None;
        egui::Window::new("Move which copies?")
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                for idx in 0..=picker.copies.len() {
                    let label = if idx == 0 {
                        format!("All {} copies", picker.copies.len())
                    } else {
                        let copy = &picker.copies[idx - 1];
                        format!("Copy {idx}: in {}", self.folder_name(copy.folder_id))
                    };
                    if ui.selectable_label(picker.cursor == idx, label).clicked() {
                        chosen = Some(idx);
                    }
                }
                ui.add_space(4.0);
                ui.label(RichText::new("Up/Down: select   Enter: continue   Esc: cancel").small());
            });
        if let Some(idx) = chosen
            && let Some(picker) = &mut self.copy_picker
        {
            picker.cursor = idx;
            self.choose_copies();
        } else if !open {
            self.copy_picker = None;
        }
    }

    fn folder_name(&self, id: u32) -> String {
        self.folders
            .iter()
            .find(|f| f.id == id)
            .map_or_else(|| format!("#{id}"), |f| f.name.clone())
    }

    /// Second step of a move: the copies are chosen, now pick the folder.
    fn choose_copies(&mut self) {
        let Some(picker) = self.copy_picker.take() else {
            return;
        };
        let copies = match picker.cursor {
            0 => picker.copies,
            i => picker.copies.get(i - 1).cloned().into_iter().collect(),
        };
        self.start_move_to_folder(picker.release_id, copies);
    }

    fn start_move_to_folder(&mut self, release_id: u32, copies: Vec<CollectionInstance>) {
        self.picker_purpose = PickerPurpose::Move { release_id, copies };
        self.open_folder_picker_for_current();
    }

    fn move_release(&mut self, id: u32) {
        match controls::decide_move(self.ownership.get(&id).map(Vec::as_slice)) {
            MoveDecision::Pick(copies) if copies.len() == 1 => {
                self.start_move_to_folder(id, copies);
            }
            MoveDecision::Pick(copies) => {
                self.copy_picker = Some(CopyPicker {
                    release_id: id,
                    copies,
                    cursor: 0,
                });
            }
            MoveDecision::NotOwned => self.set_error("Not in your collection".to_string()),
            MoveDecision::NeedsCheck => {
                self.worker.send(Request::CheckOwnership(id));
                self.set_info("Checking your collection first; press M again".to_string());
            }
        }
    }

    fn perform_move(&mut self, release_id: u32, copies: Vec<CollectionInstance>, to_folder: u32) {
        let todo = controls::plan_move(&copies, to_folder);
        if todo.is_empty() {
            let name = self.folder_name(to_folder);
            self.set_info(format!("Already in {name}; nothing to move"));
            return;
        }
        // Optimistic: show the new folders now; the resync after the request corrects it.
        if let Some(owned) = self.ownership.get_mut(&release_id) {
            for copy in owned
                .iter_mut()
                .filter(|c| todo.iter().any(|t| t.instance_id == c.instance_id))
            {
                copy.folder_id = to_folder;
            }
        }
        self.worker.send(Request::MoveCopies {
            release_id,
            copies: todo.iter().map(|c| (c.folder_id, c.instance_id)).collect(),
            to_folder,
        });
        self.set_info("Moving...".to_string());
    }

    fn show_confirm(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else {
            return;
        };
        let text = match confirm {
            Confirm::AddDuplicate { title, copies, .. } => {
                format!("You already own {copies}x \"{title}\".\nAdd another copy?")
            }
            Confirm::Remove {
                title, folder_name, ..
            } => format!("Remove one copy of \"{title}\"\nfrom folder {folder_name}?"),
        };
        let (mut yes, mut no) = (false, false);
        egui::Window::new("Confirm")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(text);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    yes = ui.button("Yes  [Enter]").clicked();
                    no = ui.button("No  [Esc]").clicked();
                });
            });
        if yes {
            self.accept_confirm();
        } else if no {
            self.confirm = None;
        }
    }

    fn accept_confirm(&mut self) {
        match self.confirm.take() {
            Some(Confirm::AddDuplicate {
                release_id,
                folder_id,
                ..
            }) => self.send_add(release_id, folder_id),
            Some(Confirm::Remove {
                release_id,
                instance,
                ..
            }) => {
                // Optimistic: forget the copy now; the resync after the request corrects it.
                if let Some(copies) = self.ownership.get_mut(&release_id) {
                    copies.retain(|c| c.instance_id != instance.instance_id);
                }
                self.worker.send(Request::RemoveFromCollection {
                    release_id,
                    folder_id: instance.folder_id,
                    instance_id: instance.instance_id,
                });
                self.set_info("Removing...".to_string());
            }
            None => {}
        }
    }

    fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::ChooseFolder => {
                self.open_folder_picker();
                return;
            }
            Command::Quit => {
                self.repaint.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            _ => {}
        }
        let Some(id) = self.selected_id() else {
            self.set_error("Select a release first".to_string());
            return;
        };
        match cmd {
            Command::Add => self.add_release(id),
            Command::Remove => self.remove_release(id),
            Command::Move => self.move_release(id),
            Command::Open => self.open_release(id),
            Command::ChooseFolder | Command::Quit => {}
        }
    }

    fn current_folder(&self) -> Option<&CollectionFolder> {
        self.folders.get(self.folder_selected?)
    }

    fn selected_title(&self) -> String {
        self.search_results
            .get(self.selected_result.unwrap_or(usize::MAX))
            .map(SearchItem::title_display)
            .unwrap_or_default()
    }

    fn add_release(&mut self, id: u32) {
        let Some(folder_id) = self.current_folder().map(|f| f.id) else {
            if self.folders.is_empty() {
                self.worker.send(Request::LoadFolders);
                self.set_error("Folders are not loaded yet; try again in a moment".to_string());
            } else {
                self.open_folder_picker();
                self.set_info("Choose the folder to add to first".to_string());
            }
            return;
        };
        match controls::decide_add(self.ownership.get(&id).map(Vec::as_slice)) {
            AddDecision::Add => self.send_add(id, folder_id),
            AddDecision::ConfirmDuplicate(copies) => {
                self.confirm = Some(Confirm::AddDuplicate {
                    release_id: id,
                    folder_id,
                    title: self.selected_title(),
                    copies,
                });
            }
            AddDecision::NeedsCheck => {
                self.worker.send(Request::CheckOwnership(id));
                self.set_info("Checking your collection first; press A again".to_string());
            }
        }
    }

    fn send_add(&mut self, id: u32, folder_id: u32) {
        // Optimistic: count the copy right away so a quick second press asks
        // before adding a duplicate; the resync after the request corrects it.
        self.ownership
            .entry(id)
            .or_default()
            .push(CollectionInstance {
                instance_id: PENDING_INSTANCE,
                folder_id,
            });
        self.worker.send(Request::AddToCollection {
            release_id: id,
            folder_id,
        });
        self.set_info("Adding...".to_string());
    }

    fn remove_release(&mut self, id: u32) {
        match controls::decide_remove(self.ownership.get(&id).map(Vec::as_slice)) {
            RemoveDecision::Confirm(instance) => {
                let folder_name = self
                    .folders
                    .iter()
                    .find(|f| f.id == instance.folder_id)
                    .map_or_else(|| format!("#{}", instance.folder_id), |f| f.name.clone());
                self.confirm = Some(Confirm::Remove {
                    release_id: id,
                    instance,
                    title: self.selected_title(),
                    folder_name,
                });
            }
            RemoveDecision::NotOwned => {
                self.set_error("Not in your collection".to_string());
            }
            RemoveDecision::NeedsCheck => {
                self.worker.send(Request::CheckOwnership(id));
                self.set_info("Checking your collection first; press R again".to_string());
            }
        }
    }

    fn open_release(&mut self, id: u32) {
        let uri = self
            .search_results
            .get(self.selected_result.unwrap_or(usize::MAX))
            .and_then(|item| item.uri.as_deref());
        let url = controls::release_url(uri, id);
        match open::that(&url) {
            Ok(()) => self.set_info(format!("Opened {url}")),
            Err(err) => self.set_error(format!("Could not open {url}: {err}")),
        }
    }

    /// The folder picker for `F`: sets where new releases go.
    fn open_folder_picker(&mut self) {
        self.picker_purpose = PickerPurpose::SetTarget;
        self.open_folder_picker_for_current();
    }

    fn open_folder_picker_for_current(&mut self) {
        if self.folders.is_empty() {
            self.worker.send(Request::LoadFolders);
        }
        self.picker_cursor = self.folder_selected.or(Some(0));
        self.folder_picker_open = true;
    }

    fn choose_folder(&mut self, idx: usize) {
        let Some(folder) = self.folders.get(idx) else {
            return;
        };
        let (id, name) = (folder.id, folder.name.clone());
        if let PickerPurpose::Move { release_id, copies } = self.picker_purpose.clone() {
            self.close_folder_picker();
            self.perform_move(release_id, copies, id);
            return;
        }
        self.folder_selected = Some(idx);
        self.close_folder_picker();
        self.settings.folder_id = id;
        if let Some(path) = &self.config_path
            && let Err(err) = Settings::update_on_disk(path, |s| s.folder_id = id)
        {
            self.set_error(format!("{err:#}"));
            return;
        }
        self.set_info(format!("New releases go to {name}"));
    }

    /// Row of buttons that mirror the keyboard commands and show their keys.
    fn command_bar(&mut self, ui: &mut egui::Ui) -> Option<Command> {
        let has_selection = self.selected_id().is_some();
        let folder = self
            .current_folder()
            .map_or("?".to_string(), |f| f.name.clone());
        let mut clicked = None;
        ui.horizontal(|ui| {
            for cmd in Command::ALL {
                let label = match cmd {
                    Command::Add => format!("Add to {folder}"),
                    Command::Remove => "Remove".to_string(),
                    Command::Move => "Move".to_string(),
                    Command::Open => "Open in browser".to_string(),
                    Command::ChooseFolder => "Change folder".to_string(),
                    Command::Quit => "Quit".to_string(),
                };
                let text = format!("{label}  [{}]", cmd.key_label());
                let enabled = has_selection || !cmd.needs_selection();
                if ui.add_enabled(enabled, egui::Button::new(text)).clicked() {
                    clicked = Some(cmd);
                }
            }
            ui.label(RichText::new("Up/Down: browse   Esc: new search").small());
        });
        clicked
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

    /// Enter on a result whose release failed to load tries again. Releases
    /// that are loaded or still loading are left alone, nothing is refetched.
    fn retry_selected_release(&mut self) {
        if let Some(id) = self.selected_id()
            && self.failed.remove(&id)
        {
            self.requested.remove(&id);
            self.request_release(id);
        }
    }
}

impl eframe::App for RecordShelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.sync_selection(ctx);
        self.persist_zoom(ctx);
        if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if self.bridge.is_some() {
            // Keeps the "browser connected" indicators fresh.
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
                if let Some(state) = &self.bridge {
                    for shop in Shop::ALL {
                        ui.separator();
                        if state.bridge.connected(shop) {
                            ui.colored_label(
                                Color32::LIGHT_GREEN,
                                format!("{}: connected", shop.name()),
                            );
                        } else {
                            ui.colored_label(
                                Color32::YELLOW,
                                format!("{}: not connected", shop.name()),
                            );
                        }
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

/// Layout of a result row: the release text in the normal text colour and,
/// for releases already in the collection, a yellow line below it.
fn row_job(font: egui::FontId, text: String, owned_line: Option<String>) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let plain = egui::TextFormat {
        font_id: font.clone(),
        // follow the widget's own colour (changes when the row is selected)
        color: Color32::PLACEHOLDER,
        ..Default::default()
    };
    job.append(&text, 0.0, plain);
    if let Some(line) = owned_line {
        let yellow = egui::TextFormat {
            font_id: font,
            color: Color32::YELLOW,
            ..Default::default()
        };
        job.append(&format!("\n{line}"), 0.0, yellow);
    }
    job
}

/// Text for the collection status of a release: "Nx in your collection: folders"
/// or "Not in your collection". `None` while the status is unknown.
fn ownership_text(
    owned: Option<&[CollectionInstance]>,
    folders: &[CollectionFolder],
) -> Option<(String, bool)> {
    match owned? {
        [] => Some(("Not in your collection".to_string(), false)),
        instances => {
            let names: Vec<String> = instances
                .iter()
                .map(|inst| {
                    folders
                        .iter()
                        .find(|f| f.id == inst.folder_id)
                        .map_or_else(|| format!("#{}", inst.folder_id), |f| f.name.clone())
                })
                .collect();
            Some((
                format!(
                    "{}x in your collection: {}",
                    instances.len(),
                    names.join(", ")
                ),
                true,
            ))
        }
    }
}

/// Shows whether (and where) a release is already in the collection:
/// yellow when owned, blue when not.
fn render_ownership(
    ui: &mut egui::Ui,
    owned: Option<&[CollectionInstance]>,
    folders: &[CollectionFolder],
) {
    if let Some((text, is_owned)) = ownership_text(owned, folders) {
        let color = if is_owned {
            Color32::YELLOW
        } else {
            Color32::LIGHT_BLUE
        };
        ui.colored_label(color, text);
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
        let formats_str = format_line(formats);
        if !formats_str.is_empty() {
            ui.label(RichText::new(format!("Format: {formats_str}")).strong());
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

    fn app_with_results(n: u32) -> RecordShelfApp {
        let mut app =
            RecordShelfApp::new(egui::Context::default(), Settings::default(), None, None);
        app.search_results = (1..=n)
            .map(|id| SearchItem {
                id: Some(id),
                ..Default::default()
            })
            .collect();
        app
    }

    #[test]
    fn prefetch_covers_only_the_first_results() {
        let mut app = app_with_results(8);
        app.prefetch_results();
        assert_eq!(app.requested, (1..=PREFETCH_LIMIT as u32).collect());
        // one release + one ownership request each
        assert_eq!(app.worker.pending(), PREFETCH_LIMIT * 2);
        app.prefetch_results();
        assert_eq!(
            app.worker.pending(),
            PREFETCH_LIMIT * 2,
            "nothing is fetched twice"
        );
    }

    #[test]
    fn known_ownership_is_not_requested_again() {
        let mut app = app_with_results(1);
        app.ownership.insert(1, Vec::new());
        app.request_release(1);
        assert_eq!(app.worker.pending(), 1);
    }

    #[test]
    fn selecting_beyond_the_prefetched_results_waits_for_the_debounce() {
        let ctx = egui::Context::default();
        let mut app = app_with_results(8);
        app.prefetch_results();
        app.selected_result = Some(6);
        app.sync_selection(&ctx);
        assert!(!app.requested.contains(&7), "still settling");
        app.selection_changed = Instant::now() - SELECTION_DEBOUNCE * 2;
        app.sync_selection(&ctx);
        assert!(app.requested.contains(&7));
    }

    #[test]
    fn prefetched_selection_needs_no_extra_request() {
        let ctx = egui::Context::default();
        let mut app = app_with_results(3);
        app.prefetch_results();
        let before = app.worker.pending();
        app.selected_result = Some(2);
        app.selection_changed = Instant::now() - SELECTION_DEBOUNCE * 2;
        app.sync_selection(&ctx);
        assert_eq!(app.worker.pending(), before);
    }

    #[test]
    fn enter_does_not_refetch_a_loaded_release() {
        let mut app = app_with_results(2);
        app.selected_result = Some(0);
        app.prefetch_results();
        app.releases.insert(1, Release::default());
        let before = app.worker.pending();
        app.retry_selected_release();
        assert_eq!(app.worker.pending(), before);
    }

    #[test]
    fn enter_does_not_duplicate_a_request_in_flight() {
        let mut app = app_with_results(2);
        app.selected_result = Some(0);
        app.prefetch_results();
        let before = app.worker.pending();
        app.retry_selected_release();
        assert_eq!(app.worker.pending(), before);
    }

    #[test]
    fn enter_retries_a_failed_release() {
        let mut app = app_with_results(2);
        app.selected_result = Some(0);
        app.prefetch_results();
        app.ownership.insert(1, Vec::new());
        app.handle_response(Response::Release {
            id: 1,
            result: Err("boom".into()),
        });
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));
        let before = app.worker.pending();
        app.retry_selected_release();
        assert_eq!(app.worker.pending(), before + 1);
        // a second Enter while the retry is running does nothing
        app.retry_selected_release();
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn selected_release_follows_the_selection() {
        let mut app = app_with_results(2);
        app.releases.insert(2, Release::default());
        app.selected_result = Some(0);
        assert!(app.selected_release().is_none());
        app.selected_result = Some(1);
        assert!(app.selected_release().is_some());
    }

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

    fn folder(id: u32, name: &str) -> CollectionFolder {
        CollectionFolder {
            id,
            name: name.into(),
            count: 0,
        }
    }

    fn app_ready_to_add() -> RecordShelfApp {
        let mut app = app_with_results(2);
        app.folders = vec![folder(1, "Uncategorized"), folder(5, "Techno")];
        app.folder_selected = Some(1);
        app.selected_result = Some(0);
        app
    }

    fn copy(instance_id: u32, folder_id: u32) -> CollectionInstance {
        CollectionInstance {
            instance_id,
            folder_id,
        }
    }

    #[test]
    fn add_sends_immediately_when_not_owned() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, Vec::new());
        let before = app.worker.pending();
        app.run_command(Command::Add);
        assert_eq!(app.worker.pending(), before + 1);
        assert!(app.confirm.is_none());
        assert_eq!(app.ownership[&1], vec![copy(PENDING_INSTANCE, 5)]);
    }

    #[test]
    fn a_second_quick_add_asks_first() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, Vec::new());
        app.run_command(Command::Add);
        let before = app.worker.pending();
        app.run_command(Command::Add);
        assert_eq!(app.worker.pending(), before, "nothing sent yet");
        assert!(matches!(
            app.confirm,
            Some(Confirm::AddDuplicate {
                copies: 1,
                folder_id: 5,
                ..
            })
        ));
        app.accept_confirm();
        assert_eq!(app.worker.pending(), before + 1);
        assert_eq!(app.ownership[&1].len(), 2);
    }

    #[test]
    fn add_with_unknown_collection_status_checks_first() {
        let mut app = app_ready_to_add();
        let before = app.worker.pending();
        app.run_command(Command::Add);
        assert_eq!(app.worker.pending(), before + 1, "only the ownership check");
        assert!(!app.ownership.contains_key(&1));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn add_without_folders_does_not_send() {
        let mut app = app_with_results(1);
        app.selected_result = Some(0);
        app.ownership.insert(1, Vec::new());
        let before = app.worker.pending();
        app.run_command(Command::Add);
        assert_eq!(
            app.worker.pending(),
            before + 1,
            "folder list is requested instead"
        );
        assert!(app.ownership[&1].is_empty());
    }

    #[test]
    fn remove_asks_then_removes_the_last_copy() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, vec![copy(10, 1), copy(11, 5)]);
        let before = app.worker.pending();
        app.run_command(Command::Remove);
        assert_eq!(app.worker.pending(), before);
        match &app.confirm {
            Some(Confirm::Remove {
                instance,
                folder_name,
                ..
            }) => {
                assert_eq!(instance, &copy(11, 5));
                assert_eq!(folder_name, "Techno");
            }
            _ => panic!("expected a remove confirmation"),
        }
        app.accept_confirm();
        assert_eq!(app.worker.pending(), before + 1);
        assert_eq!(app.ownership[&1], vec![copy(10, 1)]);
    }

    #[test]
    fn declining_the_confirmation_changes_nothing() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, vec![copy(10, 1)]);
        app.run_command(Command::Remove);
        let before = app.worker.pending();
        app.confirm = None;
        assert_eq!(app.worker.pending(), before);
        assert_eq!(app.ownership[&1], vec![copy(10, 1)]);
    }

    #[test]
    fn remove_of_an_unowned_release_is_an_error() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, Vec::new());
        let before = app.worker.pending();
        app.run_command(Command::Remove);
        assert_eq!(app.worker.pending(), before);
        assert!(app.confirm.is_none());
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));
    }

    #[test]
    fn commands_need_a_selection() {
        let mut app = app_ready_to_add();
        app.selected_result = None;
        let before = app.worker.pending();
        app.run_command(Command::Add);
        app.run_command(Command::Remove);
        app.run_command(Command::Open);
        assert_eq!(app.worker.pending(), before);
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));
    }

    #[test]
    fn folder_list_hides_all_and_restores_the_saved_choice() {
        let mut app = app_with_results(0);
        app.settings.folder_id = 5;
        app.handle_response(Response::Folders(Ok(vec![
            folder(0, "All"),
            folder(1, "Uncategorized"),
            folder(5, "Techno"),
        ])));
        assert_eq!(app.folders.len(), 2);
        assert!(app.folders.iter().all(|f| f.id != 0));
        assert_eq!(app.current_folder().map(|f| f.id), Some(5));
    }

    #[test]
    fn folder_defaults_to_the_first_one() {
        let mut app = app_with_results(0);
        app.handle_response(Response::Folders(Ok(vec![
            folder(0, "All"),
            folder(1, "Uncategorized"),
        ])));
        assert_eq!(app.current_folder().map(|f| f.id), Some(1));
    }

    #[test]
    fn choosing_a_folder_is_remembered_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut app = app_ready_to_add();
        app.config_path = Some(path.clone());
        app.open_folder_picker();
        assert!(app.folder_picker_open);
        app.choose_folder(0);
        assert!(!app.folder_picker_open);
        assert_eq!(app.current_folder().map(|f| f.id), Some(1));
        assert_eq!(Settings::load(&path).unwrap().folder_id, 1);
    }

    #[test]
    fn finished_add_resyncs_the_collection() {
        let mut app = app_ready_to_add();
        let before = app.worker.pending();
        app.handle_response(Response::Added {
            release_id: 1,
            folder_id: 5,
            result: Ok(()),
        });
        assert_eq!(
            app.worker.pending(),
            before + 2,
            "ownership check and folder reload"
        );
    }

    #[test]
    fn ownership_text_counts_copies() {
        let folders = vec![folder(1, "Uncategorized"), folder(5, "Techno")];
        assert_eq!(ownership_text(None, &folders), None);
        assert_eq!(
            ownership_text(Some(&[]), &folders),
            Some(("Not in your collection".to_string(), false))
        );
        assert_eq!(
            ownership_text(Some(&[copy(10, 5)]), &folders),
            Some(("1x in your collection: Techno".to_string(), true))
        );
        assert_eq!(
            ownership_text(Some(&[copy(10, 5), copy(11, 1), copy(12, 9)]), &folders),
            Some((
                "3x in your collection: Techno, Uncategorized, #9".to_string(),
                true
            ))
        );
    }

    #[test]
    fn moving_a_single_copy_goes_straight_to_the_folder_picker() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, vec![copy(10, 1)]);
        let before = app.worker.pending();
        app.run_command(Command::Move);
        assert!(app.folder_picker_open);
        assert!(app.copy_picker.is_none());
        assert_eq!(
            app.worker.pending(),
            before,
            "nothing sent until a folder is chosen"
        );

        app.choose_folder(1); // Techno, id 5
        assert!(!app.folder_picker_open);
        assert_eq!(app.picker_purpose, PickerPurpose::SetTarget);
        assert_eq!(app.worker.pending(), before + 1);
        assert_eq!(app.ownership[&1], vec![copy(10, 5)]);
        assert_eq!(
            app.settings.folder_id, 0,
            "moving must not change the target folder"
        );
    }

    #[test]
    fn moving_into_the_current_folder_does_nothing() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, vec![copy(10, 5)]);
        app.run_command(Command::Move);
        let before = app.worker.pending();
        app.choose_folder(1);
        assert_eq!(app.worker.pending(), before);
        assert_eq!(app.ownership[&1], vec![copy(10, 5)]);
    }

    #[test]
    fn several_copies_ask_which_to_move() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, vec![copy(10, 1), copy(11, 1)]);
        app.run_command(Command::Move);
        assert!(!app.folder_picker_open);
        assert_eq!(app.copy_picker.as_ref().map(|p| p.cursor), Some(0));

        // "All copies" moves both
        app.choose_copies();
        assert!(app.folder_picker_open);
        let before = app.worker.pending();
        app.choose_folder(1);
        assert_eq!(app.worker.pending(), before + 1);
        assert_eq!(app.ownership[&1], vec![copy(10, 5), copy(11, 5)]);
    }

    #[test]
    fn a_single_copy_can_be_picked_from_several() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, vec![copy(10, 1), copy(11, 1)]);
        app.run_command(Command::Move);
        app.copy_picker.as_mut().unwrap().cursor = 2; // second copy
        app.choose_copies();
        app.choose_folder(1);
        assert_eq!(app.ownership[&1], vec![copy(10, 1), copy(11, 5)]);
    }

    #[test]
    fn cancelling_the_move_picker_resets_the_purpose() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, vec![copy(10, 1)]);
        app.run_command(Command::Move);
        app.close_folder_picker();
        assert_eq!(app.picker_purpose, PickerPurpose::SetTarget);
        // F afterwards sets the target folder again, not a move
        app.run_command(Command::ChooseFolder);
        let before = app.worker.pending();
        app.choose_folder(1);
        assert_eq!(app.worker.pending(), before);
        assert_eq!(app.settings.folder_id, 5);
    }

    #[test]
    fn move_needs_an_owned_release() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, Vec::new());
        app.run_command(Command::Move);
        assert!(app.copy_picker.is_none() && !app.folder_picker_open);
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));

        let mut app = app_ready_to_add();
        let before = app.worker.pending();
        app.run_command(Command::Move);
        assert_eq!(
            app.worker.pending(),
            before + 1,
            "unknown status: check first"
        );
        assert!(!app.folder_picker_open);

        let mut app = app_ready_to_add();
        app.ownership.insert(1, vec![copy(PENDING_INSTANCE, 1)]);
        app.run_command(Command::Move);
        assert!(
            !app.folder_picker_open,
            "a copy still being added cannot be moved"
        );
    }

    #[test]
    fn finished_move_resyncs_the_collection() {
        let mut app = app_ready_to_add();
        let before = app.worker.pending();
        app.handle_response(Response::Moved {
            release_id: 1,
            to_folder: 5,
            result: Ok(2),
        });
        assert_eq!(app.worker.pending(), before + 2);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| !n.is_error && n.text.contains("2 copies"))
        );
    }

    fn app_with_bridge() -> RecordShelfApp {
        let mut app = app_with_results(0);
        let (tx, rx) = std::sync::mpsc::channel();
        let bridge = Bridge::spawn(0, "t".into(), tx, egui::Context::default()).unwrap();
        app.bridge = Some(BridgeState { bridge, rx });
        app
    }

    fn connect(app: &RecordShelfApp, shop: Shop) {
        let path = format!("/pending?token=t&shop={}", shop.key());
        app.bridge.as_ref().unwrap().bridge.handle("GET", &path, "");
    }

    fn hhv_entry() -> shops::ShopEntry {
        shops::ShopEntry {
            code: "525768".into(),
            artist: "Laurent Garnier".into(),
            title: "Speicher 95 Tribute EP".into(),
            catno: "Kompakt Ex 095".into(),
            label: "Kompakt Extra".into(),
            barcode: Some("00880319822216".into()),
        }
    }

    #[test]
    fn six_digit_numbers_stay_discogs_ids_without_the_bridge() {
        let mut app = app_with_results(0);
        let before = app.worker.pending();
        app.route_scan("525768".into());
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn decks_codes_without_the_bridge_explain_what_to_do() {
        let mut app = app_with_results(0);
        let before = app.worker.pending();
        app.route_scan("CNF/O1".into());
        assert_eq!(app.worker.pending(), before);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("--shop-bridge"))
        );
    }

    #[test]
    fn hhv_numbers_need_a_connected_hhv_tab() {
        let mut app = app_with_bridge();
        let before = app.worker.pending();
        app.route_scan("525768".into());
        assert_eq!(
            app.worker.pending(),
            before,
            "must not be looked up as a Discogs id"
        );
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("hhv.de"))
        );

        connect(&app, Shop::Hhv);
        app.route_scan("525768".into());
        assert_eq!(
            app.worker.pending(),
            before,
            "goes to the browser, not to Discogs"
        );
        assert!(
            app.shop_info
                .as_ref()
                .is_some_and(|i| i.contains("waiting"))
        );
        let queued =
            app.bridge
                .as_ref()
                .unwrap()
                .bridge
                .handle("GET", "/pending?token=t&shop=hhv", "");
        assert_eq!(queued.1, r#"["525768"]"#);
    }

    #[test]
    fn other_numbers_still_go_to_discogs_with_the_bridge_on() {
        let mut app = app_with_bridge();
        let before = app.worker.pending();
        app.route_scan("5012345678900".into());
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn a_shop_page_with_the_article_starts_the_discogs_lookup() {
        let mut app = app_with_results(0);
        let before = app.worker.pending();
        app.handle_shop_page(ShopPage {
            shop: Shop::Hhv,
            code: "525768".into(),
            status: Some(200),
            entries: vec![hhv_entry()],
        });
        assert_eq!(app.worker.pending(), before + 1);
        assert!(
            app.shop_info
                .as_ref()
                .is_some_and(|i| i.contains("Kompakt Ex 095"))
        );
    }

    #[test]
    fn failed_or_unmatched_shop_pages_are_reported() {
        let mut app = app_with_results(0);
        let before = app.worker.pending();
        app.handle_shop_page(ShopPage {
            shop: Shop::Hhv,
            code: "525768".into(),
            status: Some(403),
            entries: Vec::new(),
        });
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("403"))
        );
        app.handle_shop_page(ShopPage {
            shop: Shop::Hhv,
            code: "999999".into(),
            status: Some(200),
            entries: vec![hhv_entry()],
        });
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("no exact match"))
        );
        assert_eq!(app.worker.pending(), before);
    }

    #[test]
    fn owned_rows_get_a_yellow_line() {
        let font = egui::FontId::default();
        let plain = row_job(font.clone(), "Title\ndetails".into(), None);
        assert_eq!(plain.sections.len(), 1);
        assert_eq!(plain.sections[0].format.color, Color32::PLACEHOLDER);

        let owned = row_job(
            font,
            "Title\ndetails".into(),
            Some("1x in your collection: Techno".into()),
        );
        assert_eq!(owned.sections.len(), 2);
        assert_eq!(owned.sections[1].format.color, Color32::YELLOW);
        assert!(owned.text.ends_with("\n1x in your collection: Techno"));
        assert_eq!(
            owned.sections[0].format.color,
            Color32::PLACEHOLDER,
            "release text keeps its colour"
        );
    }
}
