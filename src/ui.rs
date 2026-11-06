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
use crate::collection::Collection;
use crate::config::Settings;
use crate::controls::{
    self, AddDecision, Command, MoveDecision, PENDING_INSTANCE, RemoveDecision, SyncDecision,
};
use crate::discogs::{
    CollectionFolder, CollectionInstance, DiscogsClient, Release, SearchItem, SearchQuery,
    format_line,
};
use crate::label::{CodeKind, LabelContent};
use crate::library::Entry as LibraryEntry;
use crate::shop_bridge::{self, Bridge, ShopPage};
use crate::shops::{self, Route, Shop};
use crate::worker::{
    LibraryRequest, LibraryResponse, PrintJob, Request, Response, SyncJob, SyncOutcome, Worker,
};
use eframe::egui::{self, Color32, RichText};

mod collection_tab;
mod label_preview;
mod stacks_tab;

/// The two tabs of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    /// Scan or search Discogs and add records to the collection.
    Add,
    /// Browse and search the records in our own database.
    Collection,
    /// The stacks of labelled records, and their order.
    Stacks,
}

/// Opens a web page in the browser: the URL and, optionally, the browser command.
type UrlOpener = Box<dyn FnMut(&str, Option<&str>) -> std::io::Result<()>>;

/// The system's default browser, or the given command.
fn system_opener(url: &str, browser: Option<&str>) -> std::io::Result<()> {
    match browser {
        Some(browser) => open::with_detached(url, browser),
        None => open::that_detached(url),
    }
}

/// How long the app waits for already open shop tabs to report in before it
/// opens the missing ones.
const SHOP_TABS_DELAY: Duration = Duration::from_secs(3);

/// How far the connection to our own database is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LibraryLink {
    /// No database configured.
    Off,
    Connecting,
    Ready,
    Failed(String),
}

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
    /// The reader threads send their scans through this.
    scanner_tx: std::sync::mpsc::Sender<ScanEvent>,
    scanner_status: String,
    /// The label picture beside the record info.
    label_panel: Option<label_preview::LabelPanel>,
    /// Opens the shop pages (replaced in tests).
    url_opener: UrlOpener,
    /// When the shop pages are to be opened, if still to be done.
    shop_tabs_due: Option<Instant>,
    /// What the sticker said for the article number looked up last: (number, scanned text).
    last_shop_scan: Option<(String, String)>,
    /// The devices a scanner thread is listening to (several receivers can work at once).
    scanner_active: HashSet<PathBuf>,
    scanner_next_try: Instant,
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
    tab: Tab,
    collection: Collection,
    stacks: stacks_tab::StacksState,
    library_link: LibraryLink,
    /// What our database has, per release (copies 0 = not in it).
    library: HashMap<u32, LibraryEntry>,
    library_status_requested: HashSet<u32>,
    library_status_queue: Vec<u32>,
    /// Releases with a sync request on its way.
    library_busy: HashSet<u32>,
    /// Releases whose sync was asked for with S: it runs as soon as the data is there.
    pending_forced: HashSet<u32>,
    /// Cover downloads that are waited for before they are stored: url -> release.
    cover_wanted: HashMap<String, u32>,
    bridge: Option<BridgeState>,
    shop_info: Option<String>,
    settings_draft: Settings,
    /// The name typed for a new printer profile in the settings, and what is wrong with it.
    profile_input: String,
    profile_error: Option<String>,
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
        let (scanner_stop, scanner_rx, scanner_tx, scanner_status, scanner_active) =
            start_scanner(&settings, &repaint);
        let mut app = Self {
            worker: Worker::spawn(repaint.clone()),
            settings,
            config_path,
            repaint,
            scanner_stop,
            scanner_rx,
            scanner_tx,
            scanner_status,
            label_panel: None,
            url_opener: Box::new(system_opener),
            shop_tabs_due: None,
            last_shop_scan: None,
            scanner_active,
            scanner_next_try: Instant::now() + SCANNER_RETRY,
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
            tab: Tab::Add,
            collection: Collection::default(),
            stacks: stacks_tab::StacksState::default(),
            library_link: LibraryLink::Off,
            library: HashMap::new(),
            library_status_requested: HashSet::new(),
            library_status_queue: Vec::new(),
            library_busy: HashSet::new(),
            pending_forced: HashSet::new(),
            cover_wanted: HashMap::new(),
            bridge: None,
            shop_info: None,
            settings_draft: Settings::default(),
            profile_input: String::new(),
            profile_error: None,
            devices: Vec::new(),
        };
        app.configure_worker();
        app.configure_library();
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

    // --- our own database ------------------------------------------------

    /// (Re)connects to the database from the settings; forgets what was known.
    fn configure_library(&mut self) {
        let url = self.settings.database_url.trim().to_string();
        self.library.clear();
        self.library_status_requested.clear();
        self.library_status_queue.clear();
        self.library_busy.clear();
        self.pending_forced.clear();
        self.cover_wanted.clear();
        self.collection = Collection::default();
        self.images.retain(|key, _| !key.starts_with("db:"));
        self.library_link = if url.is_empty() {
            LibraryLink::Off
        } else {
            LibraryLink::Connecting
        };
        self.worker
            .send(Request::Library(LibraryRequest::Configure { url }));
    }

    fn library_ready(&self) -> bool {
        self.library_link == LibraryLink::Ready
    }

    /// Asks the database about a release we know nothing about yet (batched, see
    /// `flush_library_status`).
    fn queue_library_status(&mut self, id: u32) {
        if self.library_ready()
            && !self.library.contains_key(&id)
            && self.library_status_requested.insert(id)
        {
            self.library_status_queue.push(id);
        }
    }

    fn flush_library_status(&mut self) {
        if !self.library_status_queue.is_empty() {
            let ids = std::mem::take(&mut self.library_status_queue);
            self.worker
                .send(Request::Library(LibraryRequest::Status(ids)));
        }
    }

    /// Called when new data about a release arrived (release, collection
    /// status, database status): syncs it if that is wanted.
    fn after_data(&mut self, id: u32) {
        let forced = self.pending_forced.contains(&id);
        if forced || self.settings.library_auto_sync {
            self.maybe_sync(id, forced);
        }
    }

    /// Brings our database in line with the Discogs collection for one release
    /// (see `controls::decide_sync`). Waits quietly for data that is still missing.
    fn maybe_sync(&mut self, id: u32, force: bool) {
        if !self.library_ready() || self.library_busy.contains(&id) {
            return;
        }
        let Some(owned) = self.ownership.get(&id).map(Vec::len) else {
            return;
        };
        let Some(in_library) = self.library.get(&id).map(|e| e.copies) else {
            self.queue_library_status(id);
            return;
        };
        match controls::decide_sync(owned, in_library, force) {
            SyncDecision::Nothing => {
                if force && owned == 0 {
                    self.set_info("Not in your collection, so not in the library".to_string());
                }
            }
            SyncDecision::Remove => {
                self.library_busy.insert(id);
                self.worker
                    .send(Request::Library(LibraryRequest::Sync(Box::new(
                        SyncJob::Remove { release_id: id },
                    ))));
            }
            SyncDecision::Save(copies) => {
                let Some(release) = self.releases.get(&id).cloned().map(Box::new) else {
                    self.request_release(id);
                    return;
                };
                self.library_busy.insert(id);
                self.worker
                    .send(Request::Library(LibraryRequest::Sync(Box::new(
                        SyncJob::Save {
                            release_id: id,
                            release,
                            copies,
                        },
                    ))));
            }
        }
        self.pending_forced.remove(&id);
    }

    /// S: sync the selected release now, whatever the mode.
    fn sync_selected(&mut self) {
        if !self.library_usable() {
            return;
        }
        let Some(id) = self.selected_id() else {
            self.set_error("Select a release first".to_string());
            return;
        };
        self.pending_forced.insert(id);
        // what is still missing: the release data (which brings the collection
        // status along) or just the collection status
        if !self.releases.contains_key(&id) {
            self.request_release(id);
        } else if !self.ownership.contains_key(&id) {
            self.worker.send(Request::CheckOwnership(id));
        }
        self.queue_library_status(id);
        self.set_info("Syncing to the library...".to_string());
        self.maybe_sync(id, true);
        self.flush_library_status();
    }

    /// Explains why the library cannot be used right now, if it cannot.
    fn library_usable(&mut self) -> bool {
        match &self.library_link {
            LibraryLink::Ready => true,
            LibraryLink::Off => {
                self.set_error("No database configured (database_url in the settings)".to_string());
                false
            }
            LibraryLink::Connecting => {
                self.set_info("Still connecting to the database...".to_string());
                false
            }
            LibraryLink::Failed(msg) => {
                let msg = format!("The database is not available: {msg}");
                self.set_error(msg);
                false
            }
        }
    }

    /// U: switch between syncing by hand and syncing automatically.
    fn toggle_auto_sync(&mut self) {
        if self.library_link == LibraryLink::Off {
            self.set_error("No database configured (database_url in the settings)".to_string());
            return;
        }
        let auto = !self.settings.library_auto_sync;
        self.settings.library_auto_sync = auto;
        if let Some(path) = &self.config_path
            && let Err(err) = Settings::update_on_disk(path, |s| s.library_auto_sync = auto)
        {
            self.set_error(format!("{err:#}"));
            return;
        }
        self.set_info(if auto {
            "Auto sync on: the library follows your collection".to_string()
        } else {
            "Auto sync off: press S to sync a release".to_string()
        });
        if auto {
            let ids: Vec<u32> = self.search_results.iter().filter_map(|i| i.id).collect();
            for id in ids {
                self.after_data(id);
            }
        }
    }

    /// Stores the cover of a release that is in our database: from the picture
    /// we already have, or after it was downloaded.
    fn ensure_cover(&mut self, id: u32) {
        let Some(url) = self
            .releases
            .get(&id)
            .and_then(|r| r.cover_url())
            .map(str::to_string)
        else {
            return;
        };
        if let Some(ImageState::Ready(bytes)) = self.images.get(&url) {
            let image = bytes.to_vec();
            self.worker.send(Request::Library(LibraryRequest::Cover {
                release_id: id,
                image,
                source_url: url,
            }));
            return;
        }
        self.cover_wanted.insert(url.clone(), id);
        if matches!(self.images.get(&url), Some(ImageState::Failed)) {
            self.images.remove(&url); // try the download again
        }
        self.ensure_image(&url);
    }

    fn handle_library_response(&mut self, response: LibraryResponse) {
        match response {
            LibraryResponse::Disabled => self.library_link = LibraryLink::Off,
            LibraryResponse::Failed(msg) => {
                self.set_error(format!("Database: {msg}"));
                self.library_link = LibraryLink::Failed(msg);
            }
            LibraryResponse::Ready { migrated } => {
                self.library_link = LibraryLink::Ready;
                if self.tab == Tab::Collection {
                    self.load_collection_if_needed();
                }
                if migrated > 0 {
                    self.set_info(format!(
                        "Database brought up to date ({migrated} migration(s))"
                    ));
                }
                self.load_stacks();
                // what we already know about: look it up in the database
                let ids: Vec<u32> = self.ownership.keys().copied().collect();
                for id in ids {
                    self.queue_library_status(id);
                }
            }
            LibraryResponse::Status { ids, result } => match result {
                Ok(found) => {
                    for id in &ids {
                        let entry = found.get(id).copied().unwrap_or(LibraryEntry {
                            copies: 0,
                            has_cover: false,
                        });
                        self.library.insert(*id, entry);
                    }
                    for id in ids {
                        self.after_data(id);
                    }
                }
                Err(err) => {
                    for id in &ids {
                        self.library_status_requested.remove(id);
                    }
                    self.set_error(format!("Database: {err}"));
                }
            },
            LibraryResponse::Synced { release_id, result } => {
                self.library_busy.remove(&release_id);
                if result.is_ok() {
                    self.collection_changed();
                }
                match result {
                    Ok(SyncOutcome::Saved { created, copies }) => {
                        let has_cover = self.library.get(&release_id).is_some_and(|e| e.has_cover);
                        self.library.insert(
                            release_id,
                            LibraryEntry {
                                copies: i16::try_from(copies).unwrap_or(i16::MAX),
                                has_cover,
                            },
                        );
                        let title = self.title_of(release_id);
                        self.set_info(if created {
                            format!("Added to the library: {title}")
                        } else {
                            format!("Library updated ({copies}x): {title}")
                        });
                        if !has_cover {
                            self.ensure_cover(release_id);
                        }
                    }
                    Ok(SyncOutcome::Removed { existed }) => {
                        self.library.insert(
                            release_id,
                            LibraryEntry {
                                copies: 0,
                                has_cover: false,
                            },
                        );
                        if existed {
                            let title = self.title_of(release_id);
                            let note = if self.tab == Tab::Collection {
                                " (still in your Discogs collection)"
                            } else {
                                ""
                            };
                            self.set_info(format!("Removed from the library: {title}{note}"));
                        }
                    }
                    Err(err) => self.set_error(format!("Library sync failed: {err}")),
                }
                // the collection may have changed while this was running
                self.after_data(release_id);
            }
            LibraryResponse::Stacks(result) => self.handle_stacks_response(result),
            response @ (LibraryResponse::List(_)
            | LibraryResponse::Details { .. }
            | LibraryResponse::Image { .. }
            | LibraryResponse::Notes { .. }
            | LibraryResponse::Thumbs { .. }) => self.handle_collection_response(response),
            LibraryResponse::Cover { release_id, result } => match result {
                Ok(true) => {
                    if let Some(entry) = self.library.get_mut(&release_id) {
                        entry.has_cover = true;
                    }
                }
                Ok(false) => {}
                Err(err) => self.set_error(format!("Cover not stored: {err}")),
            },
        }
    }

    fn title_of(&self, id: u32) -> String {
        self.search_results
            .iter()
            .find(|i| i.id == Some(id))
            .map(SearchItem::title_display)
            .filter(|t| !t.is_empty())
            .or_else(|| self.releases.get(&id).and_then(|r| r.title.clone()))
            .or_else(|| {
                self.collection
                    .rows
                    .iter()
                    .find(|r| r.id == id)
                    .map(|r| format!("{} - {}", r.artists, r.title))
            })
            .unwrap_or_else(|| format!("release {id}"))
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
                if self.settings.shop_open_tabs {
                    // tabs that are open already report in within a second: give them time
                    self.shop_tabs_due = Some(Instant::now() + SHOP_TABS_DELAY);
                }
            }
            Err(err) => self.set_error(format!("shop bridge: {err:#}")),
        }
    }

    /// Opens the shop pages that have no connected tab yet in the browser, so the
    /// userscript can run there. Pages that are open already are left alone.
    fn open_shop_tabs(&mut self) {
        let Some(state) = &self.bridge else {
            return;
        };
        let missing: Vec<Shop> = Shop::BROWSER
            .into_iter()
            .filter(|shop| !state.bridge.connected(*shop))
            .collect();
        if missing.is_empty() {
            self.set_info("The shop tabs are already connected".to_string());
            return;
        }
        let browser = Some(self.settings.shop_browser.trim())
            .filter(|b| !b.is_empty())
            .map(str::to_string);
        let (mut opened, mut failed) = (Vec::new(), Vec::new());
        for shop in missing {
            let Some(url) = shop.tab_url() else {
                continue;
            };
            match (self.url_opener)(url, browser.as_deref()) {
                Ok(()) => opened.push(shop.name()),
                Err(err) => failed.push(format!("{url}: {err}")),
            }
        }
        if !failed.is_empty() {
            self.set_error(format!(
                "Could not open in the browser: {}",
                failed.join("; ")
            ));
        } else {
            self.set_info(format!(
                "Opened {} in the browser; pass the site's check there, the userscript does the rest",
                opened.join(" and ")
            ));
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
            let advice = if shop.uses_browser() {
                format!("reload the {name} tab")
            } else {
                // the shop may have started to check requests, or the network is down
                format!(
                    "{name} did not answer as expected; try again, or open the page in your browser"
                )
            };
            self.set_error(format!(
                "{name} lookup for {code} failed ({status}); {advice}"
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
                let sticker = self
                    .last_shop_scan
                    .as_ref()
                    .filter(|(number, _)| *number == code)
                    .map(|(_, raw)| format!(" (the sticker said {raw})"))
                    .unwrap_or_default();
                self.set_error(format!(
                    "{name} has no exact match for {code}{sticker} ({} results){hint}",
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

    /// Looks for a scanner that was not there at the start or that was unplugged.
    fn retry_scanner(&mut self, ctx: &egui::Context) {
        let configured = !self.settings.device_name.trim().is_empty()
            || !self.settings.device_path.trim().is_empty();
        if !configured {
            return;
        }
        let now = Instant::now();
        if now < self.scanner_next_try {
            ctx.request_repaint_after(self.scanner_next_try - now);
            return;
        }
        self.scanner_next_try = now + SCANNER_RETRY;
        // receivers that were plugged in meanwhile start; the ones working go on
        let devices = barcode_reader::list_devices();
        let started = spawn_readers(
            &self.settings,
            &devices,
            &self.repaint,
            &self.scanner_stop,
            &self.scanner_tx,
            &mut self.scanner_active,
        );
        if started > 0 {
            self.scanner_status = "Waiting for scans...".to_string();
        } else if self.scanner_active.is_empty() {
            // keep the explanation of what is missing, unchanged between tries
            self.scanner_status = barcode_reader::explain_missing(
                &devices,
                &self.settings.device_name,
                &self.settings.device_path,
            );
        }
        ctx.request_repaint_after(SCANNER_RETRY);
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
        let mut draft = self.settings_draft.clone().normalized();
        // Zoom (Ctrl +/-) and the target folder (F) change live and are saved
        // as they change; the dialog does not edit them.
        draft.zoom_factor = self.settings.zoom_factor;
        draft.folder_id = self.settings.folder_id;
        if let Some(path) = &self.config_path
            && let Err(err) = draft.save(path)
        {
            self.set_error(format!("{err:#}"));
            return;
        }
        let old = std::mem::replace(&mut self.settings, draft);
        let new = &self.settings;

        let scanner_changed =
            (&old.device_name, &old.device_path) != (&new.device_name, &new.device_path);
        let discogs_changed = (&old.token, &old.username, &old.currency, &old.user_agent)
            != (&new.token, &new.username, &new.currency, &new.user_agent);

        let library_changed = old.database_url != new.database_url;

        if scanner_changed {
            self.scanner_stop.store(true, Ordering::SeqCst);
            let (stop, rx, tx, status, active) = start_scanner(&self.settings, &self.repaint);
            self.scanner_stop = stop;
            self.scanner_rx = rx;
            self.scanner_tx = tx;
            self.scanner_status = status;
            self.scanner_active = active;
            self.scanner_next_try = Instant::now() + SCANNER_RETRY;
        }
        if discogs_changed {
            // other credentials may mean another account: forget what we fetched
            self.folders.clear();
            self.folder_selected = None;
            self.ownership.clear();
            self.images.clear();
            self.releases.clear();
            self.requested.clear();
            self.failed.clear();
            self.notice = None;
            self.configure_worker();
        }
        if library_changed {
            self.configure_library();
        }
        if self.config_path.is_none() {
            self.set_info("Settings applied (no config path, not saved)".to_string());
        } else {
            self.set_info("Settings saved".to_string());
        }
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let (mut save, mut cancel) = (false, false);
        let mut change_folder = false;
        let devices = self.devices.clone();
        let mut refresh_devices = false;
        let folder_name = self.current_folder().map(|f| f.name.clone());
        let config_path = self.config_path.clone();
        let max_height = (ctx.screen_rect().height() * 0.8).max(200.0);
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(max_height)
                    .show(ui, |ui| {
                        let draft = &mut self.settings_draft;

                        ui.heading("Discogs");
                        egui::Grid::new("settings-discogs")
                            .num_columns(2)
                            .spacing([8.0, 6.0])
                            .show(ui, |ui| {
                                ui.label("Token:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.token)
                                        .password(true)
                                        .desired_width(f32::INFINITY),
                                );
                                ui.end_row();
                                ui.label("Username:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.username)
                                        .desired_width(f32::INFINITY),
                                );
                                ui.end_row();
                                ui.label("Currency:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.currency)
                                        .desired_width(80.0),
                                );
                                ui.end_row();
                                ui.label("User agent:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.user_agent)
                                        .desired_width(f32::INFINITY),
                                );
                                ui.end_row();
                                ui.label("Add to folder:");
                                ui.horizontal(|ui| {
                                    ui.label(folder_name.as_deref().unwrap_or("(not loaded yet)"));
                                    if ui.button("Change...").clicked() {
                                        change_folder = true;
                                    }
                                });
                                ui.end_row();
                            });

                        ui.add_space(8.0);
                        ui.heading("Barcode scanner");
                        egui::Grid::new("settings-scanner")
                            .num_columns(2)
                            .spacing([8.0, 6.0])
                            .show(ui, |ui| {
                                ui.label("Name:");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut draft.device_name)
                                            .desired_width(240.0),
                                    );
                                    egui::ComboBox::from_id_source("scanner-device")
                                        .selected_text("Pick...")
                                        .show_ui(ui, |ui| {
                                            // a device that is picked is added to the names;
                                            // picking it again takes it out
                                            let mut other = false;
                                            for dev in devices.iter().filter(|d| d.keyboard_like) {
                                                device_choice(ui, draft, dev);
                                            }
                                            for dev in devices.iter().filter(|d| !d.keyboard_like) {
                                                if !other {
                                                    other = true;
                                                    ui.separator();
                                                    ui.label(RichText::new("Other input devices").small());
                                                }
                                                device_choice(ui, draft, dev);
                                            }
                                            if devices.is_empty() {
                                                ui.label("No input devices found (permissions? the input group)");
                                            }
                                        });
                                    if ui.button("Refresh").clicked() {
                                        refresh_devices = true;
                                    }
                                });
                                ui.end_row();
                                ui.label("Fallback path:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.device_path)
                                        .desired_width(f32::INFINITY),
                                );
                                ui.end_row();
                            });
                        ui.label(
                            RichText::new(
                                "Scanners are found by name (substring match), so they keep working \
                                 when /dev/input numbering changes. Several names, separated by ;, \
                                 can be used at the same time, and all devices of a name are used. \
                                 The path is only a fallback.",
                            )
                            .small(),
                        );

                        ui.add_space(8.0);
                        ui.heading("Display");
                        ui.horizontal(|ui| {
                            ui.label("Zoom:");
                            // applied at once, like Ctrl +/-; saved as it changes
                            let mut zoom = ctx.zoom_factor();
                            if ui
                                .add(
                                    egui::DragValue::new(&mut zoom)
                                        .clamp_range(0.5..=4.0)
                                        .speed(0.01)
                                        .fixed_decimals(2)
                                        .suffix("x"),
                                )
                                .changed()
                            {
                                ctx.set_zoom_factor(zoom);
                            }
                            ui.label(RichText::new("(also Ctrl +/-, Ctrl 0)").small());
                        });

                        ui.add_space(8.0);
                        ui.heading("Label printer");
                        egui::Grid::new("settings-printer")
                            .num_columns(2)
                            .spacing([8.0, 6.0])
                            .show(ui, |ui| {
                                ui.label("Profile:");
                                ui.vertical(|ui| {
                                    profile_row(ui, draft, &mut self.profile_input, &mut self.profile_error);
                                });
                                ui.end_row();
                                ui.label("Host:");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut draft.printer_host)
                                            .desired_width(200.0)
                                            .hint_text("e.g. 192.168.4.55"),
                                    );
                                    ui.label("Port:");
                                    ui.add(
                                        egui::DragValue::new(&mut draft.printer_port)
                                            .clamp_range(1..=65535),
                                    );
                                });
                                ui.end_row();
                                ui.label("Print area:");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.printer_width_mm)
                                            .clamp_range(20.0..=120.0)
                                            .speed(0.5)
                                            .fixed_decimals(0)
                                            .suffix(" mm wide"),
                                    );
                                    ui.label(RichText::new("(ZD411: 56, ZD421: 104)").small());
                                });
                                ui.end_row();
                                ui.label("Label size:");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.label_width_mm)
                                            .clamp_range(20.0..=draft.printer_width_mm)
                                            .speed(0.1)
                                            .fixed_decimals(1)
                                            .suffix(" mm"),
                                    );
                                    ui.label("x");
                                    ui.add(
                                        egui::DragValue::new(&mut draft.label_height_mm)
                                            .clamp_range(20.0..=150.0)
                                            .speed(0.1)
                                            .fixed_decimals(1)
                                            .suffix(" mm"),
                                    );
                                    ui.label(RichText::new("(52 x 39 real, 51 x 38 test labels)").small());
                                });
                                ui.end_row();
                                ui.label("Top offset:");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.label_top_mm)
                                            .clamp_range(0.0..=10.0)
                                            .speed(0.05)
                                            .fixed_decimals(2)
                                            .suffix(" mm"),
                                    );
                                    ui.label(RichText::new("(label edge below the print origin)").small());
                                });
                                ui.end_row();
                                ui.label("Code:");
                                egui::ComboBox::from_id_source("label-code")
                                    .selected_text(if draft.label_code == "qr" {
                                        "QR code"
                                    } else {
                                        "Barcode (Code 128)"
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut draft.label_code,
                                            "barcode".to_string(),
                                            "Barcode (Code 128)",
                                        );
                                        ui.selectable_value(
                                            &mut draft.label_code,
                                            "qr".to_string(),
                                            "QR code",
                                        );
                                    });
                                ui.end_row();
                                ui.label("Preview scale:");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.preview_ppi)
                                            .clamp_range(40.0..=400.0)
                                            .speed(0.5)
                                            .fixed_decimals(0)
                                            .suffix(" points/inch"),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "(96 is usual; change it until the preview is as big as a real label)",
                                        )
                                        .small(),
                                    );
                                });
                                ui.end_row();
                                ui.label("Style tags:");
                                egui::ComboBox::from_id_source("label-tags")
                                    .selected_text(match draft.label_tags.as_str() {
                                        "outline" => "Outlined",
                                        "filled" => "Filled",
                                        _ => "Off",
                                    })
                                    .show_ui(ui, |ui| {
                                        for (value, name) in [
                                            ("off", "Off"),
                                            ("outline", "Outlined"),
                                            ("filled", "Filled (white on black)"),
                                        ] {
                                            ui.selectable_value(
                                                &mut draft.label_tags,
                                                value.to_string(),
                                                name,
                                            );
                                        }
                                    });
                                ui.end_row();
                                ui.label("Darkness:");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.printer_darkness)
                                            .clamp_range(-30..=30)
                                            .prefix("+"),
                                    );
                                    ui.label(
                                        RichText::new("(added to the printer's own setting, per label)").small(),
                                    );
                                });
                                ui.end_row();
                                ui.label("Speed:");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.printer_speed_ips)
                                            .clamp_range(0..=6)
                                            .suffix(" ips"),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "(0 = the printer's own; slower prints darker. The printer keeps the last speed.)",
                                        )
                                        .small(),
                                    );
                                });
                                ui.end_row();
                            });
                        ui.label(
                            RichText::new("Printing is switched off while the host is empty.").small(),
                        );

                        ui.add_space(8.0);
                        ui.heading("Shop bridge");
                        ui.checkbox(
                            &mut draft.shop_open_tabs,
                            "Open the decks.de and hhv.de pages in the browser when started with --shop-bridge",
                        );
                        ui.horizontal(|ui| {
                            ui.label("Browser:");
                            ui.add(
                                egui::TextEdit::singleline(&mut draft.shop_browser)
                                    .hint_text("empty: the default browser, or e.g. chromium")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                        ui.label(
                            RichText::new(
                                "Use the browser that has the userscript manager. deejay.de needs no tab.",
                            )
                            .small(),
                        );

                        ui.add_space(8.0);
                        ui.heading("Database");
                        ui.horizontal(|ui| {
                            ui.label("URL:");
                            ui.add(
                                egui::TextEdit::singleline(&mut draft.database_url)
                                    .password(true)
                                    .hint_text("postgres://recordshelf:...@host/recordshelf")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                        ui.checkbox(
                            &mut draft.library_auto_sync,
                            "Keep the library in line with my Discogs collection automatically",
                        );
                        ui.label(
                            RichText::new(
                                "The URL is written by db/db.sh bootstrap. Without auto sync, press S to sync \
                                 the selected release (U switches).",
                            )
                            .small(),
                        );

                        ui.add_space(6.0);
                        if let Some(path) = &config_path {
                            ui.label(RichText::new(format!("Saved to {}", path.display())).small());
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    save = ui.button("Save").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if refresh_devices {
            self.devices = barcode_reader::list_devices();
        }
        if change_folder {
            self.open_folder_picker();
        }
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
        let raw = code;
        let code = shops::normalize_scan(&raw, self.bridge.is_some());
        self.last_shop_scan = (code != raw).then(|| (code.clone(), raw.clone()));
        if code != raw {
            // say which number is looked up: the sticker says more than the article number
            self.set_info(format!("Scanned {raw}: article number {code}"));
        }
        let route = shops::route_scan(&code, self.bridge.is_some(), |shop| {
            self.bridge
                .as_ref()
                .is_some_and(|b| b.bridge.connected(shop))
        });
        match route {
            Route::Shop(shop) => self.lookup_in_shop(shop, code),
            Route::Fetch(shop) => {
                self.shop_info = Some(format!("{} {code}: looking it up...", shop.name()));
                self.worker.send(Request::FetchShopPage { shop, code });
            }
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
                ScanEvent::Stopped(path) => {
                    self.scanner_active.remove(&path);
                    self.scanner_next_try = Instant::now() + SCANNER_RETRY;
                }
                ScanEvent::Code(code) => {
                    self.scanner_status = format!("Scanned {code}");
                    match self.tab {
                        Tab::Add => self.route_scan(code),
                        Tab::Collection => self.scan_in_collection(code),
                        Tab::Stacks => self.scan_in_stacks(code),
                    }
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
        self.flush_library_status();
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
                    self.queue_library_status(id);
                    self.after_data(id);
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
                let wanted = self.cover_wanted.remove(&url);
                let state = match result {
                    Ok(bytes) => {
                        if let Some(release_id) = wanted {
                            self.worker.send(Request::Library(LibraryRequest::Cover {
                                release_id,
                                image: bytes.clone(),
                                source_url: url.clone(),
                            }));
                        }
                        ImageState::Ready(bytes.into())
                    }
                    Err(_) => {
                        if let Some(release_id) = wanted {
                            let title = self.title_of(release_id);
                            self.set_error(format!("Could not download the cover of {title}"));
                        }
                        ImageState::Failed
                    }
                };
                self.images.insert(url, state);
            }
            Response::Library(response) => self.handle_library_response(response),
            Response::ShopPage(page) => self.handle_shop_page(page),
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
                    self.queue_library_status(release_id);
                    self.after_data(release_id);
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
            Response::Printed { release_id, result } => match result {
                Ok(()) => {
                    self.set_info(format!(
                        "Label for release {release_id} sent to the printer"
                    ));
                    self.stack_after_print(release_id);
                }
                Err(err) => self.set_error(format!("Printing failed: {err}")),
            },
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
        let label_view = {
            let content = self.shown_release_label();
            self.label_view(ui.ctx(), content)
        };
        let mut label_action = label_preview::LabelAction::default();
        egui::ScrollArea::vertical()
            .id_source("search-release")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(release) = self.selected_release() {
                    let owned = release.id.and_then(|id| self.ownership.get(&id));
                    let split = label_view.as_ref().and_then(|v| {
                        label_preview::split_widths(
                            ui.available_width(),
                            COVER_SIZE,
                            v.size.x,
                            ui.ctx().screen_rect().center().x - ui.max_rect().left(),
                        )
                    });
                    // cover and info; `width` is what the info may use
                    let draw_info = |ui: &mut egui::Ui, width: f32| {
                        ui.horizontal_top(|ui| {
                            match &cover {
                                Some(url) => {
                                    draw_image(ui, &self.images, url, COVER_SIZE);
                                }
                                None => draw_placeholder(ui, COVER_SIZE),
                            }
                            ui.vertical(|ui| {
                                ui.set_max_width((width - COVER_SIZE - 12.0).max(120.0));
                                render_ownership(ui, owned.map(Vec::as_slice), &self.folders);
                                if self.library_ready()
                                    && let Some(id) = release.id
                                    && let Some((text, color)) = library_line(
                                        owned.map(Vec::len),
                                        self.library.get(&id).copied(),
                                    )
                                {
                                    ui.colored_label(color, text);
                                    ui.add_space(4.0);
                                }
                                render_release(ui, release);
                            });
                        });
                    };
                    match (split, &label_view) {
                        (Some((left, right)), Some(view)) => {
                            label_action = label_preview::split_pane(
                                ui,
                                left,
                                right,
                                |ui| draw_info(ui, left),
                                |ui| label_preview::draw_label_panel(ui, view),
                            );
                        }
                        _ => draw_info(ui, ui.available_width()),
                    }
                } else if self.selected_id().is_some() {
                    ui.horizontal_top(|ui| {
                        draw_placeholder(ui, COVER_SIZE);
                        ui.label("Loading release...");
                    });
                }
            });
        self.apply_label_action(label_action);
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

        let command_pressed = |c: &Command| {
            ctx.input(|i| i.key_pressed(c.key()) && !i.modifiers.shift && !i.modifiers.command)
        };
        if let Some(cmd) = Command::ALL.into_iter().find(command_pressed) {
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
            self.start_new_search();
        }
    }

    /// Esc: leave the current result and go to the search fields. The note
    /// about the shop lookup that produced the results goes away with it.
    fn start_new_search(&mut self) {
        self.selected_result = None;
        self.search_focus = true;
        self.shop_info = None;
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
            Command::AutoSync => {
                self.toggle_auto_sync();
                return;
            }
            Command::Sync => {
                self.sync_selected();
                return;
            }
            Command::AutoStack => {
                self.toggle_auto_stack();
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
            Command::Print => self.print_label(id),
            Command::Stack => self.put_on_stack(id),
            Command::ChooseFolder
            | Command::Quit
            | Command::Sync
            | Command::AutoSync
            | Command::AutoStack => {}
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

    /// A print job for this label content, with the printer settings.
    fn print_job(&self, content: LabelContent) -> PrintJob {
        let settings = &self.settings;
        PrintJob {
            content,
            host: settings.printer_host.clone(),
            port: settings.printer_port,
            width_mm: settings.label_width_mm,
            height_mm: settings.label_height_mm,
            top_mm: settings.label_top_mm,
            print_width_mm: settings.printer_width_mm,
            darkness: settings.printer_darkness,
            speed_ips: settings.printer_speed_ips,
            code: if settings.label_code == "qr" {
                CodeKind::Qr
            } else {
                CodeKind::Barcode
            },
            tag_style: settings.tag_style(),
            parts: settings.label_parts(),
        }
    }

    /// Labels (and their preview) are for releases in the collection. Tells the
    /// user why not if this one is not, or finds out first if that is not known yet.
    fn owned_or_explain(&mut self, id: u32, key: &str) -> bool {
        match self.ownership.get(&id) {
            None => {
                self.worker.send(Request::CheckOwnership(id));
                self.set_info(format!("Checking your collection first; press {key} again"));
                false
            }
            Some(copies) if copies.is_empty() => {
                self.set_error(
                    "Labels are only printed for releases in your collection".to_string(),
                );
                false
            }
            Some(_) => true,
        }
    }

    /// Prints the sticker for a release we own.
    fn print_label(&mut self, id: u32) {
        if !self.owned_or_explain(id, "P") {
            return;
        }
        if self.settings.printer_host.trim().is_empty() {
            self.set_error("No printer configured (printer_host in the settings file)".to_string());
            return;
        }
        let Some(release) = self.releases.get(&id) else {
            self.request_release(id);
            self.set_info("The release is still loading; press P again".to_string());
            return;
        };
        let content = LabelContent::from_release(id, release);
        self.print_content(content, self.settings.tag_style());
    }

    /// Sends a label to the printer (with the given tag style).
    fn print_content(&mut self, content: LabelContent, tag_style: Option<crate::label::TagStyle>) {
        if self.settings.printer_host.trim().is_empty() {
            self.set_error("No printer configured (printer_host in the settings file)".to_string());
            return;
        }
        let mut job = self.print_job(content);
        job.tag_style = tag_style;
        self.worker.send(Request::PrintLabel(Box::new(job)));
        self.set_info("Printing label...".to_string());
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
        let owned = self
            .selected_id()
            .and_then(|id| self.ownership.get(&id))
            .is_some_and(|copies| !copies.is_empty());
        let library_off = self.library_link == LibraryLink::Off;
        let auto_sync = self.settings.library_auto_sync;
        let auto_stack = self.settings.stack_auto_add;
        let mut clicked = None;
        ui.horizontal(|ui| {
            for cmd in Command::ALL {
                // the label button only appears for records we own
                if cmd.needs_ownership() && !owned {
                    continue;
                }
                // the library buttons only appear when a database is configured
                if matches!(
                    cmd,
                    Command::Sync | Command::AutoSync | Command::Stack | Command::AutoStack
                ) && library_off
                {
                    continue;
                }
                let label = match cmd {
                    Command::Add => format!("Add to {folder}"),
                    Command::Remove => "Remove".to_string(),
                    Command::Move => "Move".to_string(),
                    Command::Open => "Open in browser".to_string(),
                    Command::Print => "Print label".to_string(),
                    Command::Sync => "Sync to library".to_string(),
                    Command::AutoSync => {
                        format!("Auto sync: {}", if auto_sync { "on" } else { "off" })
                    }
                    Command::Stack => "Put on stack".to_string(),
                    Command::AutoStack => {
                        format!("Auto stack: {}", if auto_stack { "on" } else { "off" })
                    }
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
        // a manual search replaces whatever the shop lookup showed
        self.shop_info = None;
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
        self.retry_scanner(ctx);
        if let Some(due) = self.shop_tabs_due {
            let now = Instant::now();
            if now >= due {
                self.shop_tabs_due = None;
                self.open_shop_tabs();
            } else {
                ctx.request_repaint_after(due - now);
            }
        }
        self.persist_zoom(ctx);
        if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if !self.settings_open {
            let (f1, f2, f3) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::F1),
                    i.key_pressed(egui::Key::F2),
                    i.key_pressed(egui::Key::F3),
                )
            });
            let wanted = if f1 {
                Some(Tab::Add)
            } else if f2 {
                Some(Tab::Collection)
            } else if f3 {
                Some(Tab::Stacks)
            } else {
                None
            };
            if let Some(tab) = wanted.filter(|t| *t != self.tab) {
                self.tab = tab;
                self.on_tab_changed();
            }
        }
        if self.bridge.is_some() {
            // Keeps the "browser connected" indicators fresh.
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }

        egui::TopBottomPanel::top("top-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RecordShelf");
                ui.separator();
                let before = self.tab;
                ui.selectable_value(&mut self.tab, Tab::Add, "Add records  [F1]");
                ui.selectable_value(&mut self.tab, Tab::Collection, "Search Collection  [F2]");
                ui.selectable_value(&mut self.tab, Tab::Stacks, "Stacks  [F3]");
                if self.tab != before {
                    self.on_tab_changed();
                }
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
                    for shop in Shop::BROWSER {
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
                if let Some((text, color)) =
                    library_indicator(&self.library_link, self.settings.library_auto_sync)
                {
                    ui.separator();
                    ui.colored_label(color, text);
                }
                if let Some((text, color)) = self.stack_indicator() {
                    ui.separator();
                    ui.colored_label(color, text);
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
                        ui.label("Working...");
                        ui.spinner();
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Add => self.ui_main(ui),
            Tab::Collection => self.ui_collection(ui),
            Tab::Stacks => self.ui_stacks(ui),
        });

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

/// The printer profiles in the settings: choose one, add the settings as shown
/// as a new one, delete the others.
fn profile_row(
    ui: &mut egui::Ui,
    draft: &mut Settings,
    input: &mut String,
    error: &mut Option<String>,
) {
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_source("printer-profile")
            .selected_text(draft.profile_name().to_string())
            .show_ui(ui, |ui| {
                for name in draft.profile_names() {
                    if ui
                        .selectable_label(name == draft.profile_name(), &name)
                        .clicked()
                    {
                        draft.switch_profile(&name);
                        *error = None;
                    }
                }
            });
        ui.add(
            egui::TextEdit::singleline(input)
                .desired_width(140.0)
                .hint_text("name of a new profile"),
        );
        if ui
            .add_enabled(!input.trim().is_empty(), egui::Button::new("Add profile"))
            .on_hover_text("Keeps these printer settings as they are for the old profile and starts the new one with them")
            .clicked()
        {
            match draft.add_profile(input) {
                Ok(()) => {
                    input.clear();
                    *error = None;
                }
                Err(err) => *error = Some(err),
            }
        }
    });
    let others: Vec<String> = draft.profile_names().into_iter().skip(1).collect();
    if !others.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for name in others {
                if ui.small_button(format!("Delete {name}")).clicked() {
                    draft.delete_profile(&name);
                }
            }
        });
    }
    if let Some(err) = error {
        ui.colored_label(Color32::RED, err.as_str());
    }
}

/// One entry of the scanner list in the settings: a check mark when the name is used.
fn device_choice(ui: &mut egui::Ui, draft: &mut Settings, dev: &barcode_reader::InputDevice) {
    let used = barcode_reader::split_names(&draft.device_name)
        .iter()
        .any(|n| n.eq_ignore_ascii_case(&dev.name));
    let label = format!(
        "{}{} ({})",
        if used { "✔ " } else { "" },
        dev.name,
        dev.path.display()
    );
    if ui.selectable_label(used, label).clicked() {
        draft.device_name = barcode_reader::toggle_name(&draft.device_name, &dev.name);
    }
}

/// Starts the scanner thread for the configured device. Returns its stop flag,
/// the event receiver and an initial status line.
#[allow(clippy::type_complexity)]
fn start_scanner(
    settings: &Settings,
    repaint: &egui::Context,
) -> (
    Arc<AtomicBool>,
    Receiver<ScanEvent>,
    std::sync::mpsc::Sender<ScanEvent>,
    String,
    HashSet<PathBuf>,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let devices = barcode_reader::list_devices();
    let mut active = HashSet::new();
    let started = spawn_readers(settings, &devices, repaint, &stop, &tx, &mut active);
    let status = if started > 0 {
        "Waiting for scans...".to_string()
    } else {
        barcode_reader::explain_missing(&devices, &settings.device_name, &settings.device_path)
    };
    (stop, rx, tx, status, active)
}

/// Starts a reader thread for every configured device that has none yet (any
/// number of receivers can be used at once). Returns how many were started.
fn spawn_readers(
    settings: &Settings,
    devices: &[barcode_reader::InputDevice],
    repaint: &egui::Context,
    stop: &Arc<AtomicBool>,
    tx: &std::sync::mpsc::Sender<ScanEvent>,
    active: &mut HashSet<PathBuf>,
) -> usize {
    let mut started = 0;
    for path in barcode_reader::pick_devices(devices, &settings.device_name, &settings.device_path)
    {
        if active.contains(&path) {
            continue;
        }
        match BarcodeReader::spawn(
            path.clone(),
            stop.clone(),
            tx.clone(),
            Some(repaint.clone()),
        ) {
            Ok(()) => {
                active.insert(path);
                started += 1;
            }
            Err(err) => {
                let _ = tx.send(ScanEvent::Info(format!("Scanner not started: {err}")));
            }
        }
    }
    started
}

/// How often a scanner that is not there is looked for again (plugging it in
/// is enough, no restart needed).
const SCANNER_RETRY: Duration = Duration::from_secs(3);

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

/// Status bar indicator for the database link: text and colour.
fn library_indicator(link: &LibraryLink, auto_sync: bool) -> Option<(String, Color32)> {
    match link {
        LibraryLink::Off => None,
        LibraryLink::Connecting => Some(("Library: connecting...".into(), Color32::YELLOW)),
        LibraryLink::Failed(_) => Some(("Library: not available".into(), Color32::RED)),
        LibraryLink::Ready if auto_sync => {
            Some(("Library: auto sync [U]".into(), Color32::LIGHT_GREEN))
        }
        LibraryLink::Ready => Some(("Library: manual sync [S], auto [U]".into(), Color32::YELLOW)),
    }
}

/// How a release stands towards our database, compared with the Discogs
/// collection: green when they agree, yellow when a sync is due.
fn library_line(owned: Option<usize>, entry: Option<LibraryEntry>) -> Option<(String, Color32)> {
    let (owned, in_library) = (owned?, usize::try_from(entry?.copies).unwrap_or(0));
    match (owned, in_library) {
        (0, 0) => None,
        (o, l) if o == l => Some((format!("In the library ({l}x)"), Color32::LIGHT_GREEN)),
        (_, 0) => Some((
            "Not in the library yet (press S to sync)".into(),
            Color32::YELLOW,
        )),
        (0, l) => Some((
            format!("In the library ({l}x), but no longer in your collection (press S)"),
            Color32::YELLOW,
        )),
        (o, l) => Some((
            format!("Library has {l}x, your collection {o}x (press S to sync)"),
            Color32::YELLOW,
        )),
    }
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

    pub(super) fn app_with_results(n: u32) -> RecordShelfApp {
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

    pub(super) fn app_ready_to_add() -> RecordShelfApp {
        let mut app = app_with_results(2);
        app.folders = vec![folder(1, "Uncategorized"), folder(5, "Techno")];
        app.folder_selected = Some(1);
        app.selected_result = Some(0);
        app
    }

    pub(super) fn copy(instance_id: u32, folder_id: u32) -> CollectionInstance {
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
    fn a_sticker_with_a_variant_looks_up_the_article_and_names_both_in_an_error() {
        let mut app = app_with_bridge();
        connect(&app, Shop::Hhv);
        app.route_scan("525768V1".into());
        assert!(
            app.shop_info
                .as_deref()
                .is_some_and(|i| i.contains("525768") && i.contains("waiting")),
            "asked the shop for the article number"
        );
        app.handle_shop_page(ShopPage {
            shop: Shop::Hhv,
            code: "525768".into(),
            status: Some(200),
            entries: vec![],
        });
        let text = &app.notice.as_ref().unwrap().text;
        assert!(
            text.contains("no exact match for 525768 (the sticker said 525768V1)"),
            "{text}"
        );
        // a plain number says nothing extra
        app.route_scan("525768".into());
        app.handle_shop_page(ShopPage {
            shop: Shop::Hhv,
            code: "525768".into(),
            status: Some(200),
            entries: vec![],
        });
        assert!(!app.notice.as_ref().unwrap().text.contains("sticker said"));
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

    fn app_ready_to_print(printer_host: &str) -> RecordShelfApp {
        let mut app = app_ready_to_add();
        app.settings.printer_host = printer_host.to_string();
        app.releases.insert(1, Release::default());
        app.ownership.insert(1, vec![copy(10, 1)]);
        app
    }

    #[test]
    fn p_sends_a_print_job_for_an_owned_release() {
        let mut app = app_ready_to_print("127.0.0.1");
        let before = app.worker.pending();
        app.run_command(Command::Print);
        assert_eq!(app.worker.pending(), before + 1);
        assert!(app.notice.as_ref().is_some_and(|n| !n.is_error));
    }

    #[test]
    fn p_needs_an_owned_release_a_printer_and_loaded_data() {
        // not owned
        let mut app = app_ready_to_print("127.0.0.1");
        app.ownership.insert(1, Vec::new());
        let before = app.worker.pending();
        app.run_command(Command::Print);
        assert_eq!(app.worker.pending(), before);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("collection"))
        );

        // ownership unknown: check first
        let mut app = app_ready_to_print("127.0.0.1");
        app.ownership.clear();
        let before = app.worker.pending();
        app.run_command(Command::Print);
        assert_eq!(app.worker.pending(), before + 1, "only the ownership check");

        // no printer
        let mut app = app_ready_to_print("  ");
        let before = app.worker.pending();
        app.run_command(Command::Print);
        assert_eq!(app.worker.pending(), before);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("printer"))
        );

        // release data not loaded yet
        let mut app = app_ready_to_print("127.0.0.1");
        app.releases.clear();
        app.requested.clear();
        let before = app.worker.pending();
        app.run_command(Command::Print);
        assert!(app.requested.contains(&1), "the release is requested");
        assert!(app.worker.pending() > before);
    }

    #[test]
    fn print_results_are_reported() {
        let mut app = app_ready_to_print("127.0.0.1");
        app.handle_response(Response::Printed {
            release_id: 1,
            result: Err("not printing: the printer is out of labels".into()),
        });
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("out of labels"))
        );
        app.handle_response(Response::Printed {
            release_id: 1,
            result: Ok(()),
        });
        assert!(app.notice.as_ref().is_some_and(|n| !n.is_error));
    }

    fn settings_dialog_app(dir: &std::path::Path) -> RecordShelfApp {
        let mut app = app_with_results(0);
        app.config_path = Some(dir.join("config.toml"));
        app.settings = Settings {
            token: "tok".into(),
            ..Settings::default()
        };
        app.open_settings();
        app
    }

    #[test]
    fn saving_the_dialog_stores_every_edited_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        {
            let d = &mut app.settings_draft;
            d.printer_host = "  192.168.4.55 ".into();
            d.printer_port = 9101;
            d.label_width_mm = 52.0;
            d.label_height_mm = 39.0;
            d.label_top_mm = 2.0;
            d.label_code = "qr".into();
            d.printer_darkness = 12;
            d.printer_speed_ips = 4;
        }
        app.save_settings();
        let saved = Settings::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(saved.printer_host, "192.168.4.55");
        assert_eq!(saved.printer_port, 9101);
        assert_eq!((saved.label_width_mm, saved.label_height_mm), (52.0, 39.0));
        assert_eq!(saved.label_top_mm, 2.0);
        assert_eq!(saved.label_code, "qr");
        assert_eq!((saved.printer_darkness, saved.printer_speed_ips), (12, 4));
        assert_eq!(app.settings, saved, "the running app uses what was saved");
    }

    #[test]
    fn out_of_range_values_are_corrected_before_saving() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings_draft.printer_darkness = 99;
        app.settings_draft.label_width_mm = 200.0;
        app.save_settings();
        assert_eq!(app.settings.printer_darkness, 30);
        assert_eq!(app.settings.label_width_mm, 56.0);
    }

    #[test]
    fn zoom_and_folder_are_not_overwritten_by_the_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings.zoom_factor = 1.5;
        app.settings.folder_id = 7;
        app.settings_draft.zoom_factor = 1.0; // stale copy from when the dialog opened
        app.settings_draft.folder_id = 0;
        app.save_settings();
        assert_eq!((app.settings.zoom_factor, app.settings.folder_id), (1.5, 7));
    }

    #[test]
    fn printer_changes_keep_what_was_fetched_but_account_changes_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.releases.insert(1, Release::default());
        app.ownership.insert(1, Vec::new());

        app.settings_draft.printer_host = "192.168.4.55".into();
        app.save_settings();
        assert!(app.releases.contains_key(&1) && app.ownership.contains_key(&1));

        app.open_settings();
        app.settings_draft.username = "someone-else".into();
        app.save_settings();
        assert!(app.releases.is_empty() && app.ownership.is_empty());
    }

    #[test]
    fn a_new_printer_setting_is_used_by_the_next_label() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings_draft.printer_host = "127.0.0.1".into();
        app.save_settings();
        app.search_results = vec![SearchItem {
            id: Some(1),
            ..Default::default()
        }];
        app.selected_result = Some(0);
        app.releases.insert(1, Release::default());
        app.ownership.insert(1, vec![copy(10, 1)]);
        let before = app.worker.pending();
        app.run_command(Command::Print);
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn the_database_url_is_saved_from_the_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings_draft.database_url =
            " postgres://recordshelf:pw@lightdesk.local/recordshelf ".into();
        app.save_settings();
        let saved = Settings::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(
            saved.database_url,
            "postgres://recordshelf:pw@lightdesk.local/recordshelf"
        );
    }

    #[test]
    fn saving_from_the_dialog_keeps_settings_it_does_not_show() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = \"tok\"\nfuture_setting = 42\n").unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings = Settings::load(&path).unwrap();
        app.open_settings();
        app.settings_draft.username = "me".into();
        app.save_settings();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("future_setting = 42")
        );
    }

    #[test]
    fn escape_clears_the_selection_and_the_shop_note() {
        let mut app = app_with_results(2);
        app.selected_result = Some(1);
        app.shop_info = Some(
            "decks.de CNF/O1: Altone - WONDERSCAPE NUMINOUS | PCSS09 | Primary colours".into(),
        );
        app.start_new_search();
        assert_eq!(app.selected_result, None);
        assert!(app.search_focus);
        assert_eq!(app.shop_info, None);
        assert_eq!(app.search_results.len(), 2, "the list itself stays");
    }

    #[test]
    fn a_manual_search_clears_the_shop_note() {
        let mut app = app_with_results(0);
        app.shop_info = Some("hhv.de 525768: ...".into());
        app.search_q = "garnier".into();
        app.perform_search();
        assert_eq!(app.shop_info, None);
    }

    #[test]
    fn a_new_scan_replaces_the_shop_note() {
        let mut app = app_with_results(0);
        app.shop_info = Some("old note".into());
        app.handle_shop_page(ShopPage {
            shop: Shop::Hhv,
            code: "525768".into(),
            status: Some(200),
            entries: vec![hhv_entry()],
        });
        assert!(
            app.shop_info
                .as_ref()
                .is_some_and(|i| i.contains("525768") && !i.contains("old"))
        );
    }

    // --- library -------------------------------------------------------------

    fn app_with_library(auto: bool) -> RecordShelfApp {
        let mut app = app_ready_to_add();
        app.library_link = LibraryLink::Ready;
        app.settings.library_auto_sync = auto;
        app.releases.insert(1, Release::default());
        app
    }

    fn entry(copies: i16, has_cover: bool) -> LibraryEntry {
        LibraryEntry { copies, has_cover }
    }

    #[test]
    fn auto_sync_stores_an_owned_release_that_is_missing() {
        let mut app = app_with_library(true);
        app.library.insert(1, entry(0, false));
        app.ownership.insert(1, vec![copy(10, 1)]);
        let before = app.worker.pending();
        app.after_data(1);
        assert_eq!(app.worker.pending(), before + 1);
        assert!(app.library_busy.contains(&1));
    }

    #[test]
    fn auto_sync_does_nothing_when_they_agree() {
        let mut app = app_with_library(true);
        app.library.insert(1, entry(2, true));
        app.ownership.insert(1, vec![copy(10, 1), copy(11, 1)]);
        let before = app.worker.pending();
        app.after_data(1);
        assert_eq!(app.worker.pending(), before);
        assert!(app.library_busy.is_empty());
    }

    #[test]
    fn auto_sync_updates_the_copy_count() {
        let mut app = app_with_library(true);
        app.library.insert(1, entry(1, true));
        app.ownership.insert(1, vec![copy(10, 1), copy(11, 1)]);
        let before = app.worker.pending();
        app.after_data(1);
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn auto_sync_removes_a_release_that_is_no_longer_owned() {
        let mut app = app_with_library(true);
        app.library.insert(1, entry(1, true));
        app.ownership.insert(1, Vec::new());
        let before = app.worker.pending();
        app.after_data(1);
        assert_eq!(app.worker.pending(), before + 1);
        // nothing to remove for a release that is not in the library
        let mut app = app_with_library(true);
        app.library.insert(1, entry(0, false));
        app.ownership.insert(1, Vec::new());
        let before = app.worker.pending();
        app.after_data(1);
        assert_eq!(app.worker.pending(), before);
    }

    #[test]
    fn nothing_is_synced_without_the_data_it_needs() {
        // collection status unknown
        let mut app = app_with_library(true);
        app.library.insert(1, entry(0, false));
        let before = app.worker.pending();
        app.after_data(1);
        assert_eq!(app.worker.pending(), before);

        // database status unknown: it is asked for, once, in one batch
        let mut app = app_with_library(true);
        app.ownership.insert(1, vec![copy(10, 1)]);
        app.after_data(1);
        app.after_data(1);
        assert_eq!(app.library_status_queue, vec![1]);
        let before = app.worker.pending();
        app.flush_library_status();
        assert_eq!(app.worker.pending(), before + 1);
        app.flush_library_status();
        assert_eq!(
            app.worker.pending(),
            before + 1,
            "an empty queue sends nothing"
        );

        // release data missing: it is requested instead
        let mut app = app_with_library(true);
        app.releases.clear();
        app.library.insert(1, entry(0, false));
        app.ownership.insert(1, vec![copy(10, 1)]);
        app.after_data(1);
        assert!(app.requested.contains(&1));
        assert!(
            app.library_busy.is_empty(),
            "no sync without the release data"
        );
    }

    #[test]
    fn manual_mode_waits_for_s() {
        let mut app = app_with_library(false);
        app.library.insert(1, entry(0, false));
        app.ownership.insert(1, vec![copy(10, 1)]);
        let before = app.worker.pending();
        app.after_data(1);
        assert_eq!(
            app.worker.pending(),
            before,
            "manual mode does not sync by itself"
        );
        app.run_command(Command::Sync);
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn s_refreshes_a_release_even_when_the_counts_agree() {
        let mut app = app_with_library(false);
        app.library.insert(1, entry(1, true));
        app.ownership.insert(1, vec![copy(10, 1)]);
        let before = app.worker.pending();
        app.run_command(Command::Sync);
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn s_waits_for_missing_data_and_then_syncs() {
        let mut app = app_with_library(false);
        app.releases.clear();
        // nothing known yet: the release and the collection status are requested
        app.run_command(Command::Sync);
        assert!(app.requested.contains(&1) && app.pending_forced.contains(&1));
        assert!(app.library_busy.is_empty());
        // the data arrives one piece at a time
        app.handle_response(Response::Ownership {
            release_id: 1,
            result: Ok(vec![copy(10, 1)]),
        });
        assert!(
            app.library_busy.is_empty(),
            "still waiting for the database status and the release"
        );
        app.handle_library_response(LibraryResponse::Status {
            ids: vec![1],
            result: Ok(std::collections::HashMap::new()),
        });
        assert!(app.library_busy.is_empty(), "still waiting for the release");
        app.handle_response(Response::Release {
            id: 1,
            result: Ok(Release::default()),
        });
        assert!(
            app.library_busy.contains(&1),
            "now it syncs, although auto sync is off"
        );
        assert!(!app.pending_forced.contains(&1));
    }

    #[test]
    fn s_for_a_release_we_do_not_own_says_so() {
        let mut app = app_with_library(false);
        app.library.insert(1, entry(0, false));
        app.ownership.insert(1, Vec::new());
        app.run_command(Command::Sync);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("Not in your collection"))
        );
        assert!(app.library_busy.is_empty());
    }

    #[test]
    fn a_release_is_not_synced_twice_at_once() {
        let mut app = app_with_library(true);
        app.library.insert(1, entry(0, false));
        app.ownership.insert(1, vec![copy(10, 1)]);
        app.after_data(1);
        let before = app.worker.pending();
        app.after_data(1);
        assert_eq!(
            app.worker.pending(),
            before,
            "the first request is still on its way"
        );
    }

    #[test]
    fn the_library_commands_explain_why_they_cannot_run() {
        let mut app = app_with_library(true);
        app.library_link = LibraryLink::Off;
        app.run_command(Command::Sync);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("database_url"))
        );
        app.run_command(Command::AutoSync);
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));
        assert!(app.settings.library_auto_sync, "the mode is unchanged");

        app.library_link = LibraryLink::Failed("connection refused".into());
        app.run_command(Command::Sync);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("connection refused"))
        );
        app.library_link = LibraryLink::Connecting;
        app.run_command(Command::Sync);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| !n.is_error && n.text.contains("connecting"))
        );
    }

    #[test]
    fn u_switches_the_mode_and_remembers_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_library(true);
        app.config_path = Some(dir.path().join("config.toml"));
        app.run_command(Command::AutoSync);
        assert!(!app.settings.library_auto_sync);
        assert!(
            !Settings::load(&dir.path().join("config.toml"))
                .unwrap()
                .library_auto_sync
        );
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("Auto sync off"))
        );
        app.run_command(Command::AutoSync);
        assert!(app.settings.library_auto_sync);
        assert!(
            Settings::load(&dir.path().join("config.toml"))
                .unwrap()
                .library_auto_sync
        );
    }

    #[test]
    fn turning_auto_sync_on_catches_up_with_the_results() {
        let mut app = app_with_library(false);
        app.library.insert(1, entry(0, false));
        app.ownership.insert(1, vec![copy(10, 1)]);
        let before = app.worker.pending();
        app.run_command(Command::AutoSync);
        assert_eq!(
            app.worker.pending(),
            before + 1,
            "the owned release is synced at once"
        );
    }

    #[test]
    fn database_answers_update_what_is_known() {
        let mut app = app_with_library(false);
        let mut found = std::collections::HashMap::new();
        found.insert(1, entry(2, true));
        app.handle_library_response(LibraryResponse::Status {
            ids: vec![1, 2],
            result: Ok(found),
        });
        assert_eq!(app.library[&1], entry(2, true));
        assert_eq!(
            app.library[&2],
            entry(0, false),
            "not found means not in the library"
        );
    }

    #[test]
    fn a_finished_sync_updates_the_entry_and_fetches_the_cover() {
        let mut app = app_with_library(true);
        app.library.insert(1, entry(0, false));
        app.ownership.insert(1, vec![copy(10, 1)]);
        app.releases.insert(
            1,
            serde_json::from_str(r#"{"id": 1, "images": [{"type": "primary", "uri": "https://i.discogs.com/c.jpg"}]}"#).unwrap(),
        );
        app.library_busy.insert(1);
        app.handle_library_response(LibraryResponse::Synced {
            release_id: 1,
            result: Ok(SyncOutcome::Saved {
                created: true,
                copies: 1,
            }),
        });
        assert_eq!(app.library[&1], entry(1, false));
        assert!(app.library_busy.is_empty());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.starts_with("Added to the library"))
        );
        assert_eq!(
            app.cover_wanted.get("https://i.discogs.com/c.jpg"),
            Some(&1)
        );
        assert!(
            app.images.contains_key("https://i.discogs.com/c.jpg"),
            "the download was started"
        );
    }

    #[test]
    fn a_downloaded_cover_is_stored_in_the_library() {
        let mut app = app_with_library(true);
        app.cover_wanted
            .insert("https://i.discogs.com/c.jpg".into(), 1);
        let before = app.worker.pending();
        app.handle_response(Response::Image {
            url: "https://i.discogs.com/c.jpg".into(),
            result: Ok(vec![1, 2, 3]),
        });
        assert_eq!(app.worker.pending(), before + 1, "a store request was sent");
        assert!(app.cover_wanted.is_empty());
        // an image that was only displayed is not stored
        let before = app.worker.pending();
        app.handle_response(Response::Image {
            url: "https://i.discogs.com/other.jpg".into(),
            result: Ok(vec![1]),
        });
        assert_eq!(app.worker.pending(), before);
    }

    #[test]
    fn a_cover_that_is_already_loaded_is_stored_without_downloading_again() {
        let mut app = app_with_library(true);
        app.releases.insert(
            1,
            serde_json::from_str(r#"{"id": 1, "images": [{"type": "primary", "uri": "https://i.discogs.com/c.jpg"}]}"#).unwrap(),
        );
        app.images.insert(
            "https://i.discogs.com/c.jpg".into(),
            ImageState::Ready(vec![1, 2].into()),
        );
        let before = app.worker.pending();
        app.ensure_cover(1);
        assert_eq!(app.worker.pending(), before + 1);
        assert!(app.cover_wanted.is_empty());
    }

    #[test]
    fn a_failed_cover_download_is_reported() {
        let mut app = app_with_library(true);
        app.cover_wanted
            .insert("https://i.discogs.com/c.jpg".into(), 1);
        app.handle_response(Response::Image {
            url: "https://i.discogs.com/c.jpg".into(),
            result: Err("403".into()),
        });
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("cover"))
        );
    }

    #[test]
    fn connection_results_set_the_link() {
        let mut app = app_with_library(true);
        app.library_link = LibraryLink::Connecting;
        app.handle_library_response(LibraryResponse::Ready { migrated: 1 });
        assert_eq!(app.library_link, LibraryLink::Ready);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("1 migration"))
        );
        app.handle_library_response(LibraryResponse::Failed("connection refused".into()));
        assert_eq!(
            app.library_link,
            LibraryLink::Failed("connection refused".into())
        );
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));
        app.handle_library_response(LibraryResponse::Disabled);
        assert_eq!(app.library_link, LibraryLink::Off);
    }

    #[test]
    fn a_connection_made_later_looks_up_what_is_already_known() {
        let mut app = app_with_library(true);
        app.library_link = LibraryLink::Connecting;
        app.ownership.insert(1, vec![copy(10, 1)]);
        app.handle_library_response(LibraryResponse::Ready { migrated: 0 });
        assert_eq!(app.library_status_queue, vec![1]);
    }

    #[test]
    fn a_failed_sync_is_reported_and_can_be_retried() {
        let mut app = app_with_library(true);
        app.library_busy.insert(1);
        app.handle_library_response(LibraryResponse::Synced {
            release_id: 1,
            result: Err("connection lost".into()),
        });
        assert!(app.library_busy.is_empty());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("connection lost"))
        );
    }

    #[test]
    fn changing_the_database_url_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.library.insert(1, entry(1, true));
        app.settings_draft.database_url = "postgres://x@127.0.0.1:1/y".into();
        app.save_settings();
        assert_eq!(app.library_link, LibraryLink::Connecting);
        assert!(
            app.library.is_empty(),
            "what was known about the old database is forgotten"
        );
        app.open_settings();
        app.settings_draft.database_url = String::new();
        app.save_settings();
        assert_eq!(app.library_link, LibraryLink::Off);
    }

    #[test]
    fn auto_sync_can_be_chosen_in_the_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings_draft.library_auto_sync = false;
        app.save_settings();
        assert!(
            !Settings::load(&dir.path().join("config.toml"))
                .unwrap()
                .library_auto_sync
        );
    }

    #[test]
    fn the_library_line_compares_collection_and_database() {
        let text = |owned, e| library_line(owned, e).map(|(t, _)| t);
        assert_eq!(
            text(Some(1), Some(entry(1, true))).as_deref(),
            Some("In the library (1x)")
        );
        assert_eq!(
            text(Some(2), Some(entry(2, false))).as_deref(),
            Some("In the library (2x)")
        );
        assert!(
            text(Some(1), Some(entry(0, false)))
                .unwrap()
                .contains("Not in the library yet")
        );
        assert!(
            text(Some(2), Some(entry(1, true)))
                .unwrap()
                .contains("Library has 1x, your collection 2x")
        );
        assert!(
            text(Some(0), Some(entry(1, true)))
                .unwrap()
                .contains("no longer in your collection")
        );
        assert_eq!(
            text(Some(0), Some(entry(0, false))),
            None,
            "nothing to say about a release we do not own"
        );
        assert_eq!(
            text(None, Some(entry(1, true))),
            None,
            "collection status unknown"
        );
        assert_eq!(text(Some(1), None), None, "database status unknown");
        assert_eq!(
            library_line(Some(1), Some(entry(1, true))).unwrap().1,
            Color32::LIGHT_GREEN
        );
        assert_eq!(
            library_line(Some(1), Some(entry(0, false))).unwrap().1,
            Color32::YELLOW
        );
    }

    #[test]
    fn the_status_bar_shows_the_mode() {
        assert_eq!(library_indicator(&LibraryLink::Off, true), None);
        assert!(
            library_indicator(&LibraryLink::Ready, true)
                .unwrap()
                .0
                .contains("auto sync")
        );
        assert!(
            library_indicator(&LibraryLink::Ready, false)
                .unwrap()
                .0
                .contains("manual sync")
        );
        assert_eq!(
            library_indicator(&LibraryLink::Failed("x".into()), true)
                .unwrap()
                .1,
            Color32::RED
        );
        assert!(
            library_indicator(&LibraryLink::Connecting, true)
                .unwrap()
                .0
                .contains("connecting")
        );
    }

    // --- deejay.de ---------------------------------------------------------------------

    fn deejay_entry() -> shops::ShopEntry {
        shops::ShopEntry {
            code: "960305".into(),
            artist: "Groove Minority".into(),
            title: "EP #2".into(),
            catno: "GME-002".into(),
            label: "> France".into(),
            barcode: None,
        }
    }

    #[test]
    fn a_deejay_qr_code_is_asked_of_the_shop_directly() {
        // no bridge, no browser: the app asks the shop itself
        let mut app = app_with_results(0);
        assert!(app.bridge.is_none());
        let before = app.worker.pending();
        app.route_scan("https://deejay.de/__960305".into());
        assert_eq!(app.worker.pending(), before + 1);
        assert!(
            app.shop_info
                .as_ref()
                .is_some_and(|i| i.contains("deejay.de") && i.contains("looking it up"))
        );
        assert!(
            app.notice
                .as_ref()
                .is_none_or(|n| !n.text.contains("bridge")),
            "no complaint about a missing bridge"
        );
    }

    #[test]
    fn a_deejay_article_starts_the_discogs_lookup() {
        let mut app = app_with_results(0);
        let before = app.worker.pending();
        app.handle_shop_page(ShopPage {
            shop: Shop::Deejay,
            code: "https://deejay.de/__960305".into(),
            status: Some(200),
            entries: vec![deejay_entry()],
        });
        assert_eq!(app.worker.pending(), before + 1);
        assert!(
            app.shop_info
                .as_ref()
                .is_some_and(|i| i.contains("GME-002") && i.contains("Groove Minority"))
        );
    }

    #[test]
    fn a_failed_deejay_lookup_does_not_talk_about_browser_tabs() {
        let mut app = app_with_results(0);
        app.handle_shop_page(ShopPage {
            shop: Shop::Deejay,
            code: "https://deejay.de/__960305".into(),
            status: Some(403),
            entries: Vec::new(),
        });
        let notice = app.notice.as_ref().unwrap();
        assert!(
            notice.is_error && notice.text.contains("403") && !notice.text.contains("tab"),
            "{}",
            notice.text
        );
        // a browser shop still says so
        app.handle_shop_page(ShopPage {
            shop: Shop::Hhv,
            code: "525768".into(),
            status: Some(403),
            entries: Vec::new(),
        });
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("reload the hhv.de tab")
        );
    }

    #[test]
    fn a_deejay_page_for_another_article_is_not_used() {
        let mut app = app_with_results(0);
        let before = app.worker.pending();
        app.handle_shop_page(ShopPage {
            shop: Shop::Deejay,
            code: "https://deejay.de/__111111".into(),
            status: Some(200),
            entries: vec![deejay_entry()],
        });
        assert_eq!(app.worker.pending(), before);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("no exact match"))
        );
    }

    #[test]
    fn the_status_bar_lists_only_the_browser_shops() {
        // deejay.de needs no tab, so it has no connection indicator
        assert!(!Shop::BROWSER.contains(&Shop::Deejay));
    }

    // --- scanner ------------------------------------------------------------------------

    #[test]
    fn a_scanner_that_is_not_plugged_in_is_explained_and_looked_for_again() {
        let mut app = app_with_results(0);
        app.settings.device_name = "No Such Scanner 12345".into();
        app.settings.device_path = String::new();
        app.scanner_next_try = Instant::now() - Duration::from_secs(1);
        let ctx = app.repaint.clone();
        app.retry_scanner(&ctx);
        assert!(app.scanner_active.is_empty());
        assert!(
            app.scanner_status
                .contains("\"No Such Scanner 12345\" not found"),
            "{}",
            app.scanner_status
        );
        assert!(
            app.scanner_next_try > Instant::now(),
            "the next try is later"
        );
        // not again before the time is up
        app.scanner_status = "marker".into();
        app.retry_scanner(&ctx);
        assert_eq!(app.scanner_status, "marker");
    }

    #[test]
    fn nothing_is_retried_without_a_configured_scanner() {
        let mut app = app_with_results(0);
        app.settings.device_name = String::new();
        app.settings.device_path = String::new();
        app.scanner_next_try = Instant::now() - Duration::from_secs(1);
        app.scanner_status = "marker".into();
        let ctx = app.repaint.clone();
        app.retry_scanner(&ctx);
        assert_eq!(app.scanner_status, "marker");
    }

    #[test]
    fn a_stopped_scanner_thread_makes_the_app_look_again() {
        let mut app = app_with_results(0);
        app.scanner_active
            .insert(PathBuf::from("/dev/input/event9"));
        // a second channel stands in for the scanner thread
        let (tx, rx) = std::sync::mpsc::channel();
        app.scanner_rx = rx;
        tx.send(ScanEvent::Info("Scanner lost".into())).unwrap();
        tx.send(ScanEvent::Stopped(PathBuf::from("/dev/input/event9")))
            .unwrap();
        app.drain_events();
        assert!(app.scanner_active.is_empty());
        assert_eq!(app.scanner_status, "Scanner lost");
        assert!(app.scanner_next_try > Instant::now());
    }

    #[test]
    fn saving_the_scanner_settings_starts_the_search_for_the_new_device() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings_draft.device_name = "Another Missing Scanner".into();
        app.save_settings();
        assert!(
            app.scanner_status
                .contains("\"Another Missing Scanner\" not found"),
            "{}",
            app.scanner_status
        );
        assert!(app.scanner_active.is_empty());
    }

    // --- opening the shop pages ---------------------------------------------------------

    type OpenLog = std::rc::Rc<std::cell::RefCell<Vec<(String, Option<String>)>>>;

    fn app_with_recording_browser() -> (RecordShelfApp, OpenLog) {
        let mut app = app_with_results(0);
        let (tx, rx) = std::sync::mpsc::channel();
        let bridge = Bridge::spawn(0, "t".into(), tx, egui::Context::default()).unwrap();
        app.bridge = Some(BridgeState { bridge, rx });
        let log: OpenLog = Default::default();
        let sink = log.clone();
        app.url_opener = Box::new(move |url, browser| {
            sink.borrow_mut()
                .push((url.to_string(), browser.map(str::to_string)));
            Ok(())
        });
        (app, log)
    }

    #[test]
    fn both_browser_shop_pages_are_opened_when_nothing_is_connected() {
        let (mut app, log) = app_with_recording_browser();
        app.open_shop_tabs();
        let urls: Vec<String> = log.borrow().iter().map(|(u, _)| u.clone()).collect();
        assert_eq!(urls, ["https://www.decks.de/decks/", "https://www.hhv.de/"]);
        assert!(
            log.borrow().iter().all(|(_, b)| b.is_none()),
            "the default browser"
        );
        let notice = app.notice.as_ref().unwrap();
        assert!(
            !notice.is_error && notice.text.contains("decks.de and hhv.de"),
            "{}",
            notice.text
        );
    }

    #[test]
    fn deejay_needs_no_page() {
        let (mut app, log) = app_with_recording_browser();
        app.open_shop_tabs();
        assert!(!log.borrow().iter().any(|(u, _)| u.contains("deejay")));
    }

    #[test]
    fn a_tab_that_is_already_connected_is_not_opened_again() {
        let (mut app, log) = app_with_recording_browser();
        // the hhv.de tab polls the bridge
        app.bridge
            .as_ref()
            .unwrap()
            .bridge
            .handle("GET", "/pending?token=t&shop=hhv", "");
        app.open_shop_tabs();
        let urls: Vec<String> = log.borrow().iter().map(|(u, _)| u.clone()).collect();
        assert_eq!(urls, ["https://www.decks.de/decks/"]);
        // all connected: nothing at all
        log.borrow_mut().clear();
        app.bridge
            .as_ref()
            .unwrap()
            .bridge
            .handle("GET", "/pending?token=t&shop=decks", "");
        app.open_shop_tabs();
        assert!(log.borrow().is_empty());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("already connected"))
        );
    }

    #[test]
    fn the_configured_browser_is_used() {
        let (mut app, log) = app_with_recording_browser();
        app.settings.shop_browser = "  chromium ".into();
        app.open_shop_tabs();
        assert!(
            log.borrow()
                .iter()
                .all(|(_, b)| b.as_deref() == Some("chromium")),
            "{:?}",
            log.borrow()
        );
    }

    #[test]
    fn a_browser_that_cannot_be_started_is_reported() {
        let (mut app, _log) = app_with_recording_browser();
        app.url_opener = Box::new(|_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such browser",
            ))
        });
        app.open_shop_tabs();
        let notice = app.notice.as_ref().unwrap();
        assert!(
            notice.is_error
                && notice.text.contains("no such browser")
                && notice.text.contains("decks.de"),
            "{}",
            notice.text
        );
    }

    #[test]
    fn nothing_is_opened_without_the_bridge() {
        let mut app = app_with_results(0);
        let log: OpenLog = Default::default();
        let sink = log.clone();
        app.url_opener = Box::new(move |u, b| {
            sink.borrow_mut()
                .push((u.to_string(), b.map(str::to_string)));
            Ok(())
        });
        app.open_shop_tabs();
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn starting_the_bridge_schedules_the_pages_unless_switched_off() {
        let dir = tempfile::tempdir().unwrap();
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut app = app_with_results(0);
        app.config_path = Some(dir.path().join("config.toml"));
        app.settings.shop_open_tabs = true;
        app.start_bridge(port);
        let due = app.shop_tabs_due.expect("scheduled");
        assert!(
            due > Instant::now(),
            "a moment is left for open tabs to report in"
        );

        let port2 = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut app = app_with_results(0);
        app.config_path = Some(dir.path().join("config.toml"));
        app.settings.shop_open_tabs = false;
        app.start_bridge(port2);
        assert!(app.shop_tabs_due.is_none());
    }

    #[test]
    fn the_shop_browser_is_saved_from_the_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings_draft.shop_browser = "  firefox ".into();
        app.settings_draft.shop_open_tabs = false;
        app.save_settings();
        let saved = Settings::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(saved.shop_browser, "firefox");
        assert!(!saved.shop_open_tabs);
    }

    // --- style tags on the label -----------------------------------------------------------

    #[test]
    fn the_hidden_label_parts_go_into_the_print_job() {
        let mut app = app_with_results(0);
        let content = crate::label::sample_content();
        assert_eq!(
            app.print_job(content.clone()).parts,
            crate::label::LabelParts::default()
        );
        app.settings.label_hidden = vec!["tracks".into()];
        let parts = app.print_job(content).parts;
        assert!(!parts.tracks && parts.title);
    }

    #[test]
    fn the_tag_style_of_the_settings_goes_into_the_print_job() {
        let mut app = app_with_results(0);
        let content = crate::label::sample_content_with_tags();
        assert_eq!(
            app.print_job(content.clone()).tag_style,
            None,
            "off by default"
        );
        app.settings.label_tags = "outline".into();
        assert_eq!(
            app.print_job(content.clone()).tag_style,
            Some(crate::label::TagStyle::Outline)
        );
        app.settings.label_tags = "filled".into();
        assert_eq!(
            app.print_job(content).tag_style,
            Some(crate::label::TagStyle::Filled)
        );
    }

    #[test]
    fn the_tag_style_can_be_chosen_in_the_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = settings_dialog_app(dir.path());
        app.settings_draft.label_tags = "filled".into();
        app.save_settings();
        assert_eq!(
            app.settings.tag_style(),
            Some(crate::label::TagStyle::Filled)
        );
        assert_eq!(
            Settings::load(&dir.path().join("config.toml"))
                .unwrap()
                .label_tags,
            "filled"
        );
    }
}
