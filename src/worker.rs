use std::{
    collections::HashMap,
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use eframe::egui;

use crate::catalogue::CatalogueRelease;
use crate::discogs::{
    CollectionFolder, CollectionInstance, DiscogsClient, Release, SearchItem, SearchQuery,
};
use crate::label::{self, CodeKind, LabelContent, LabelOptions, LabelSize};
use crate::library::{Entry, Library, RecordRow, StoredRelease};
use crate::printer::{self, JobOptions, Printer};
use crate::shop_bridge::ShopPage;
use crate::shops::{Shop, ShopEntry, prefer_artist, search_plan};

/// Requests for the thread that talks to our own database. Each one is
/// answered by exactly one `LibraryResponse`.
pub enum LibraryRequest {
    /// (Re)connect with this URL and bring the schema up to date; an empty
    /// URL switches the library off.
    Configure {
        url: String,
    },
    /// Copies and cover state of these releases.
    Status(Vec<u32>),
    Sync(Box<SyncJob>),
    /// All records, for the collection tab.
    List,
    /// One record with everything stored about it. `for_scan`: it was asked for
    /// because its barcode was scanned.
    Details {
        id: u32,
        for_scan: bool,
    },
    /// The stored cover image of a record. Not counted as pending work.
    Image(u32),
    /// Our own notes for a record (blank clears them).
    SetNotes {
        id: u32,
        notes: String,
    },
    /// Stored thumbnails for list rows. Not counted as pending work.
    Thumbs(Vec<u32>),
    /// Store a downloaded cover for a release that is in the database.
    Cover {
        release_id: u32,
        image: Vec<u8>,
        source_url: String,
    },
}

/// Bring one release in line with the Discogs collection.
pub enum SyncJob {
    /// We own `copies` copies: store the release.
    Save {
        release_id: u32,
        release: Box<Release>,
        copies: u16,
    },
    /// We own none (any more): remove it.
    Remove { release_id: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    Saved { created: bool, copies: u16 },
    Removed { existed: bool },
}

pub enum LibraryResponse {
    /// No database configured.
    Disabled,
    Ready {
        /// Migrations applied while connecting.
        migrated: usize,
    },
    Failed(String),
    Status {
        ids: Vec<u32>,
        result: Result<HashMap<u32, Entry>, String>,
    },
    Synced {
        release_id: u32,
        result: Result<SyncOutcome, String>,
    },
    Cover {
        release_id: u32,
        result: Result<bool, String>,
    },
    List(Result<Vec<RecordRow>, String>),
    Details {
        id: u32,
        for_scan: bool,
        result: Result<Option<Box<StoredRelease>>, String>,
    },
    Image {
        id: u32,
        result: Result<Option<Vec<u8>>, String>,
    },
    /// Whether the record was found and its notes set.
    Notes {
        id: u32,
        result: Result<bool, String>,
    },
    Thumbs {
        ids: Vec<u32>,
        result: Result<Vec<(u32, Vec<u8>)>, String>,
    },
}

impl LibraryRequest {
    /// Whether the spinner should show while this is being worked on. Connecting
    /// has its own indicator, pictures arrive quietly in the background.
    fn counts_as_pending(&self) -> bool {
        !matches!(
            self,
            LibraryRequest::Configure { .. } | LibraryRequest::Image(_) | LibraryRequest::Thumbs(_)
        )
    }
}

impl LibraryResponse {
    /// The counterpart of `LibraryRequest::counts_as_pending`.
    fn counts_as_pending(&self) -> bool {
        !matches!(
            self,
            LibraryResponse::Disabled
                | LibraryResponse::Ready { .. }
                | LibraryResponse::Failed(_)
                | LibraryResponse::Image { .. }
                | LibraryResponse::Thumbs { .. }
        )
    }
}

/// State of the database thread: the URL, and the connection if there is one.
#[derive(Default)]
pub struct LibraryState {
    url: String,
    library: Option<Library>,
}

impl LibraryState {
    /// The connection, reconnecting if it was lost.
    fn connection(&mut self) -> Result<&mut Library, String> {
        if self.url.trim().is_empty() {
            return Err("no database configured".to_string());
        }
        if self.library.as_ref().is_none_or(Library::is_closed) {
            match Library::connect(&self.url) {
                Ok((library, _)) => self.library = Some(library),
                Err(err) => {
                    self.library = None;
                    return Err(format!("{err:#}"));
                }
            }
        }
        Ok(self.library.as_mut().expect("connected above"))
    }

    pub fn handle(&mut self, request: LibraryRequest) -> LibraryResponse {
        match request {
            LibraryRequest::Configure { url } => {
                self.url = url.trim().to_string();
                self.library = None;
                if self.url.is_empty() {
                    return LibraryResponse::Disabled;
                }
                match Library::connect(&self.url) {
                    Ok((library, migrated)) => {
                        self.library = Some(library);
                        LibraryResponse::Ready { migrated }
                    }
                    Err(err) => LibraryResponse::Failed(format!("{err:#}")),
                }
            }
            LibraryRequest::Status(ids) => {
                let result = self
                    .connection()
                    .and_then(|l| l.status(&ids).map_err(|e| format!("{e:#}")));
                LibraryResponse::Status { ids, result }
            }
            LibraryRequest::Sync(job) => {
                let release_id = match job.as_ref() {
                    SyncJob::Save { release_id, .. } | SyncJob::Remove { release_id } => {
                        *release_id
                    }
                };
                let result = self.connection().and_then(|library| match *job {
                    SyncJob::Save {
                        release_id,
                        release,
                        copies,
                    } => {
                        let record = CatalogueRelease::from_release(release_id, &release, copies)
                            .map_err(|e| format!("{e:#}"))?;
                        let saved = library
                            .save_release(&record)
                            .map_err(|e| format!("{e:#}"))?;
                        Ok(SyncOutcome::Saved {
                            created: saved.created,
                            copies,
                        })
                    }
                    SyncJob::Remove { release_id } => library
                        .delete_release(release_id)
                        .map(|existed| SyncOutcome::Removed { existed })
                        .map_err(|e| format!("{e:#}")),
                });
                LibraryResponse::Synced { release_id, result }
            }
            LibraryRequest::List => {
                let result = self
                    .connection()
                    .and_then(|l| l.list().map_err(|e| format!("{e:#}")));
                LibraryResponse::List(result)
            }
            LibraryRequest::Details { id, for_scan } => {
                let result = self.connection().and_then(|l| {
                    l.get_release(id)
                        .map(|r| r.map(Box::new))
                        .map_err(|e| format!("{e:#}"))
                });
                LibraryResponse::Details {
                    id,
                    for_scan,
                    result,
                }
            }
            LibraryRequest::Image(id) => {
                let result = self
                    .connection()
                    .and_then(|l| l.get_image(id).map_err(|e| format!("{e:#}")));
                LibraryResponse::Image { id, result }
            }
            LibraryRequest::SetNotes { id, notes } => {
                let result = self
                    .connection()
                    .and_then(|l| l.set_notes(id, &notes).map_err(|e| format!("{e:#}")));
                LibraryResponse::Notes { id, result }
            }
            LibraryRequest::Thumbs(ids) => {
                let result = self
                    .connection()
                    .and_then(|l| l.get_thumbs(&ids).map_err(|e| format!("{e:#}")));
                LibraryResponse::Thumbs { ids, result }
            }
            LibraryRequest::Cover {
                release_id,
                image,
                source_url,
            } => {
                let result = self.connection().and_then(|l| {
                    l.save_cover(release_id, &image, Some(&source_url))
                        .map_err(|e| format!("{e:#}"))
                });
                LibraryResponse::Cover { release_id, result }
            }
        }
    }
}

/// Everything needed to print one label (taken from the settings when the
/// key is pressed).
#[derive(Debug, Clone)]
pub struct PrintJob {
    pub content: LabelContent,
    pub host: String,
    pub port: u16,
    pub width_mm: f32,
    pub height_mm: f32,
    pub top_mm: f32,
    pub darkness: i8,
    /// 0 keeps the printer's own speed.
    pub speed_ips: u8,
    pub code: CodeKind,
}

impl PrintJob {
    /// Draws the label and sends it to the printer, if the printer is ready.
    pub fn run(&self) -> Result<()> {
        let size = LabelSize::from_mm(self.width_mm, self.height_mm);
        let options = LabelOptions::new(self.code);
        let img = label::render_with(&self.content, size, &options)?;
        let job = JobOptions {
            darkness: Some(self.darkness),
            speed_ips: (self.speed_ips > 0).then_some(self.speed_ips),
        };
        let zpl = printer::bitmap_zpl(
            &img,
            printer::centred_x(size.width),
            (self.top_mm * 8.0).round() as u32,
            &job,
        );
        Printer::new(&self.host, self.port).print(&zpl)
    }
}

/// Work for the background thread. Every request except `Configure` produces
/// exactly one `Response`, in request order.
pub enum Request {
    /// Swap the Discogs client and username used for later requests.
    Configure {
        client: Option<DiscogsClient>,
        username: String,
    },
    Scan(String),
    Search(SearchQuery),
    LoadRelease(u32),
    LoadFolders,
    CheckOwnership(u32),
    /// Download a cover image. Handled by a separate thread and not counted in
    /// `pending()`, so pictures never delay or flag normal requests.
    FetchImage(String),
    /// Find Discogs releases for what a shop knows about a scanned code
    /// (barcode, catalog number, artist/title).
    ShopLookup {
        shop: Shop,
        code: String,
        entry: ShopEntry,
    },
    AddToCollection {
        release_id: u32,
        folder_id: u32,
    },
    /// Draw a label and send it to the printer.
    PrintLabel(Box<PrintJob>),
    /// Ask a shop that needs no browser (deejay.de) for the page of a scanned code.
    FetchShopPage {
        shop: Shop,
        code: String,
    },
    /// Work on our own database (handled by a thread of its own).
    Library(LibraryRequest),
    /// Moves copies to another folder: `(current folder, instance id)` pairs.
    MoveCopies {
        release_id: u32,
        copies: Vec<(u32, u32)>,
        to_folder: u32,
    },
    RemoveFromCollection {
        release_id: u32,
        folder_id: u32,
        instance_id: u32,
    },
}

pub enum Response {
    /// Releases found for a scanned code (one for a release id, any number for a barcode).
    Scan {
        code: String,
        result: Result<Vec<SearchItem>, String>,
    },
    Search(Result<Vec<SearchItem>, String>),
    Release {
        id: u32,
        result: Result<Release, String>,
    },
    Folders(Result<Vec<CollectionFolder>, String>),
    Ownership {
        release_id: u32,
        result: Result<Vec<CollectionInstance>, String>,
    },
    Shop {
        shop: Shop,
        code: String,
        entry: ShopEntry,
        result: Result<Vec<SearchItem>, String>,
    },
    Image {
        url: String,
        result: Result<Vec<u8>, String>,
    },
    Added {
        release_id: u32,
        folder_id: u32,
        result: Result<(), String>,
    },
    Removed {
        release_id: u32,
        result: Result<(), String>,
    },
    Printed {
        release_id: u32,
        result: Result<(), String>,
    },
    Library(LibraryResponse),
    /// The page of a shop, as if a browser tab had brought it.
    ShopPage(ShopPage),
    Moved {
        release_id: u32,
        to_folder: u32,
        /// Number of copies moved.
        result: Result<usize, String>,
    },
}

/// Handle to the background threads that talk to Discogs, so the UI thread
/// never blocks on the network. One thread handles API requests in order, a
/// second one downloads images.
pub struct Worker {
    tx: Sender<Request>,
    image_tx: Sender<Request>,
    library_tx: Sender<LibraryRequest>,
    rx: Receiver<Response>,
    pending: usize,
}

impl Worker {
    pub fn spawn(repaint: egui::Context) -> Self {
        let (resp_tx, resp_rx) = channel::<Response>();
        let tx = Self::spawn_thread("discogs-worker", resp_tx.clone(), repaint.clone());
        let image_tx = Self::spawn_thread("discogs-images", resp_tx.clone(), repaint.clone());
        let library_tx = Self::spawn_library_thread(resp_tx, repaint);
        Self {
            tx,
            image_tx,
            library_tx,
            rx: resp_rx,
            pending: 0,
        }
    }

    fn spawn_library_thread(
        resp_tx: Sender<Response>,
        repaint: egui::Context,
    ) -> Sender<LibraryRequest> {
        let (req_tx, req_rx) = channel::<LibraryRequest>();
        thread::Builder::new()
            .name("library".to_string())
            .spawn(move || {
                let mut state = LibraryState::default();
                while let Ok(request) = req_rx.recv() {
                    let response = Response::Library(state.handle(request));
                    if resp_tx.send(response).is_err() {
                        break;
                    }
                    repaint.request_repaint();
                }
            })
            .expect("spawn library thread");
        req_tx
    }

    fn spawn_thread(
        name: &str,
        resp_tx: Sender<Response>,
        repaint: egui::Context,
    ) -> Sender<Request> {
        let (req_tx, req_rx) = channel::<Request>();
        thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let mut state = State {
                    client: None,
                    username: String::new(),
                    ..Default::default()
                };
                while let Ok(req) = req_rx.recv() {
                    if let Some(resp) = state.handle(req) {
                        if resp_tx.send(resp).is_err() {
                            break;
                        }
                        repaint.request_repaint();
                    }
                }
            })
            .expect("spawn discogs worker");
        req_tx
    }

    pub fn send(&mut self, req: Request) {
        match req {
            Request::Library(request) => {
                if request.counts_as_pending() {
                    self.pending += 1;
                }
                let _ = self.library_tx.send(request);
            }
            Request::FetchImage(_) => {
                let _ = self.image_tx.send(req);
            }
            Request::Configure { client, username } => {
                let _ = self.image_tx.send(Request::Configure {
                    client: client.clone(),
                    username: username.clone(),
                });
                let _ = self.tx.send(Request::Configure { client, username });
            }
            _ => {
                self.pending += 1;
                let _ = self.tx.send(req);
            }
        }
    }

    /// Collects finished responses without blocking.
    pub fn poll(&mut self) -> Vec<Response> {
        let out: Vec<Response> = self.rx.try_iter().collect();
        let answered = out
            .iter()
            .filter(|r| match r {
                Response::Image { .. } => false,
                Response::Library(response) => response.counts_as_pending(),
                _ => true,
            })
            .count();
        self.pending = self.pending.saturating_sub(answered);
        out
    }

    /// Number of requests sent but not yet answered (images excluded).
    pub fn pending(&self) -> usize {
        self.pending
    }
}

#[derive(Default)]
struct State {
    client: Option<DiscogsClient>,
    username: String,
    /// For shops that are asked directly. A plain client: it must never carry
    /// the Discogs credentials.
    shop_http: Option<reqwest::blocking::Client>,
}

impl State {
    fn client(&self) -> Result<&DiscogsClient> {
        self.client
            .as_ref()
            .context("No Discogs token configured (open Settings)")
    }

    fn username(&self) -> Result<&str> {
        let name = self.username.trim();
        if name.is_empty() {
            anyhow::bail!("Collection: Discogs username required (open Settings)");
        }
        Ok(name)
    }

    fn handle(&mut self, req: Request) -> Option<Response> {
        Some(match req {
            Request::Configure { client, username } => {
                self.client = client;
                self.username = username;
                return None;
            }
            Request::Scan(code) => {
                let result = self.scan(&code).map_err(|e| format!("{e:#}"));
                Response::Scan { code, result }
            }
            Request::Search(query) => Response::Search(
                self.client()
                    .and_then(|c| c.search(&query))
                    .map(|r| r.results)
                    .map_err(|e| format!("{e:#}")),
            ),
            Request::LoadRelease(id) => Response::Release {
                id,
                result: self
                    .client()
                    .and_then(|c| c.release(id))
                    .map_err(|e| format!("{e:#}")),
            },
            Request::LoadFolders => Response::Folders(
                self.username()
                    .and_then(|u| self.client()?.collection_folders(u))
                    .map_err(|e| format!("{e:#}")),
            ),
            Request::CheckOwnership(release_id) => Response::Ownership {
                release_id,
                result: self
                    .username()
                    .and_then(|u| self.client()?.collection_instances(u, release_id))
                    .map_err(|e| format!("{e:#}")),
            },
            Request::FetchImage(url) => {
                let result = self
                    .client()
                    .and_then(|c| c.image(&url))
                    .map_err(|e| format!("{e:#}"));
                Response::Image { url, result }
            }
            Request::ShopLookup { shop, code, entry } => {
                let result = self.shop_lookup(&entry).map_err(|e| format!("{e:#}"));
                Response::Shop {
                    shop,
                    code,
                    entry,
                    result,
                }
            }
            Request::FetchShopPage { shop, code } => {
                Response::ShopPage(self.fetch_shop_page(shop, code))
            }
            // handled by the library thread, never gets here
            Request::Library(_) => return None,
            Request::PrintLabel(job) => Response::Printed {
                release_id: job.content.release_id,
                result: job.run().map_err(|e| format!("{e:#}")),
            },
            Request::MoveCopies {
                release_id,
                copies,
                to_folder,
            } => Response::Moved {
                release_id,
                to_folder,
                result: self
                    .move_copies(release_id, &copies, to_folder)
                    .map_err(|e| format!("{e:#}")),
            },
            Request::RemoveFromCollection {
                release_id,
                folder_id,
                instance_id,
            } => Response::Removed {
                release_id,
                result: self
                    .username()
                    .and_then(|u| {
                        self.client()?
                            .remove_from_collection(u, folder_id, release_id, instance_id)
                    })
                    .map_err(|e| format!("{e:#}")),
            },
            Request::AddToCollection {
                release_id,
                folder_id,
            } => Response::Added {
                release_id,
                folder_id,
                result: self
                    .username()
                    .and_then(|u| self.client()?.add_to_collection(u, folder_id, release_id))
                    .map_err(|e| format!("{e:#}")),
            },
        })
    }

    fn move_copies(&self, release_id: u32, copies: &[(u32, u32)], to_folder: u32) -> Result<usize> {
        let username = self.username()?;
        let client = self.client()?;
        for (done, &(folder_id, instance_id)) in copies.iter().enumerate() {
            client
                .move_instance(username, folder_id, release_id, instance_id, to_folder)
                .with_context(|| format!("moved {done} of {} copies", copies.len()))?;
        }
        Ok(copies.len())
    }

    /// Asks the shop for the page of a scanned code. Problems come back as a
    /// status of 0 (no reply) or the HTTP status, like from a browser tab.
    fn fetch_shop_page(&mut self, shop: Shop, code: String) -> ShopPage {
        self.fetch_shop_page_from(crate::deejay::BASE, shop, code)
    }

    fn fetch_shop_page_from(&mut self, base: &str, shop: Shop, code: String) -> ShopPage {
        let page = |status, entries| ShopPage {
            shop,
            code: code.clone(),
            status,
            entries,
        };
        let Shop::Deejay = shop else {
            return page(Some(0), Vec::new());
        };
        let Some(id) = crate::deejay::article_id(&code) else {
            return page(Some(0), Vec::new());
        };
        let client = self.shop_http.get_or_insert_with(|| {
            reqwest::blocking::Client::builder()
                .user_agent("RecordShelf (personal collection tool)")
                .timeout(Duration::from_secs(20))
                .build()
                .expect("build the shop client")
        });
        match crate::deejay::fetch_from(client, base, id) {
            Ok((status, html)) => page(Some(status), crate::shops::parse_page(shop, &html)),
            Err(_) => page(Some(0), Vec::new()),
        }
    }

    /// Runs the search plan and returns the first non-empty result list.
    fn shop_lookup(&self, entry: &ShopEntry) -> Result<Vec<SearchItem>> {
        let client = self.client()?;
        for query in search_plan(entry) {
            let found = client.search(&query)?.results;
            if !found.is_empty() {
                return Ok(prefer_artist(found, entry));
            }
        }
        Ok(Vec::new())
    }

    fn scan(&self, code: &str) -> Result<Vec<SearchItem>> {
        let client = self.client()?;
        match ScanCode::classify(code)? {
            ScanCode::ReleaseId(id) => {
                let release = client.release(id)?;
                Ok(vec![SearchItem::from_release(id, &release)])
            }
            ScanCode::Barcode(barcode) => Ok(client
                .search(&SearchQuery {
                    barcode: Some(barcode),
                    kind: Some("release".to_string()),
                    per_page: Some(25),
                    ..Default::default()
                })?
                .results),
        }
    }
}

/// What a scanned code refers to.
#[derive(Debug, PartialEq, Eq)]
pub enum ScanCode {
    /// A Discogs release id (short numbers, e.g. from a printed label).
    ReleaseId(u32),
    /// A UPC/EAN printed on the record sleeve (12-14 digits).
    Barcode(String),
}

impl ScanCode {
    pub fn classify(code: &str) -> Result<Self> {
        let code = code.trim();
        if !code.is_empty()
            && code.bytes().all(|b| b.is_ascii_digit())
            && (12..=14).contains(&code.len())
        {
            return Ok(Self::Barcode(code.to_string()));
        }
        code.parse()
            .map(Self::ReleaseId)
            .with_context(|| format!("not a release id or UPC/EAN barcode: {code}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn short_numbers_are_release_ids() {
        assert_eq!(
            ScanCode::classify("249504").unwrap(),
            ScanCode::ReleaseId(249504)
        );
        assert_eq!(
            ScanCode::classify("35000000").unwrap(),
            ScanCode::ReleaseId(35_000_000)
        );
    }

    #[test]
    fn twelve_to_fourteen_digits_are_barcodes() {
        for code in ["012345678905", "5012345678900", "15012345678907"] {
            assert_eq!(
                ScanCode::classify(code).unwrap(),
                ScanCode::Barcode(code.to_string())
            );
        }
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(
            ScanCode::classify(" 5012345678900\n").unwrap(),
            ScanCode::Barcode("5012345678900".to_string())
        );
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(ScanCode::classify("").is_err());
        assert!(ScanCode::classify("12-34").is_err());
        assert!(ScanCode::classify("abc").is_err());
        // too long for a barcode and too big for a release id
        assert!(ScanCode::classify("123456789012345").is_err());
    }

    #[test]
    fn requests_without_client_report_missing_token() {
        let mut state = State {
            client: None,
            username: "me".into(),
            ..Default::default()
        };
        match state.handle(Request::LoadRelease(1)) {
            Some(Response::Release {
                id: 1,
                result: Err(msg),
            }) => {
                assert!(msg.contains("token"), "{msg}")
            }
            _ => panic!("expected an error response"),
        }
    }

    #[test]
    fn collection_requests_need_a_username() {
        let mut state = State {
            client: None,
            username: "  ".into(),
            ..Default::default()
        };
        match state.handle(Request::LoadFolders) {
            Some(Response::Folders(Err(msg))) => assert!(msg.contains("username"), "{msg}"),
            _ => panic!("expected an error response"),
        }
    }

    #[test]
    fn configure_has_no_response() {
        let mut state = State {
            client: None,
            username: String::new(),
            ..Default::default()
        };
        let resp = state.handle(Request::Configure {
            client: None,
            username: "new".into(),
        });
        assert!(resp.is_none());
        assert_eq!(state.username, "new");
    }

    #[test]
    fn image_requests_do_not_count_as_pending() {
        let mut worker = Worker::spawn(egui::Context::default());
        worker.send(Request::FetchImage("https://i.discogs.com/x.jpg".into()));
        assert_eq!(worker.pending(), 0);
        worker.send(Request::LoadFolders);
        assert_eq!(worker.pending(), 1);

        // No client is configured, so both answer immediately with an error.
        let mut responses = Vec::new();
        for _ in 0..200 {
            responses.extend(worker.poll());
            if responses.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(responses.len(), 2);
        assert!(
            responses
                .iter()
                .any(|r| matches!(r, Response::Image { result: Err(_), .. }))
        );
        assert_eq!(worker.pending(), 0);
    }

    fn job(host: &str, port: u16) -> PrintJob {
        PrintJob {
            content: label::sample_content(),
            host: host.to_string(),
            port,
            width_mm: 51.0,
            height_mm: 38.0,
            top_mm: 1.5,
            darkness: 9,
            speed_ips: 3,
            code: CodeKind::Barcode,
        }
    }

    #[test]
    fn a_print_job_sends_the_label_with_the_configured_settings() {
        let (printer, server) = printer::testing::fake_printer(printer::testing::STATUS_OK);
        job(&printer.host, printer.port).run().unwrap();
        let received = server.join().unwrap();
        assert_eq!(received[0], "~HS");
        let zpl = &received[1];
        assert!(zpl.starts_with("^XA"), "{}", &zpl[..40.min(zpl.len())]);
        // centred (51 mm in 56 mm) and 1.5 mm from the top
        assert!(
            zpl.contains("^MD9\n^PR3\n^FO20,12^GFA,"),
            "{}",
            &zpl[..80.min(zpl.len())]
        );
        assert!(zpl.contains(&format!("^GFA,{0},{0},51,", 51 * 304)));
    }

    #[test]
    fn a_zero_speed_leaves_the_printer_speed_alone() {
        let (printer, server) = printer::testing::fake_printer(printer::testing::STATUS_OK);
        PrintJob {
            speed_ips: 0,
            ..job(&printer.host, printer.port)
        }
        .run()
        .unwrap();
        let zpl = server.join().unwrap().remove(1);
        assert!(zpl.contains("^MD9\n^FO"));
        assert!(!zpl.contains("^PR"));
    }

    #[test]
    fn a_print_request_answers_with_the_outcome() {
        let mut state = State {
            client: None,
            username: String::new(),
            ..Default::default()
        };
        // nothing listens on this port
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        match state.handle(Request::PrintLabel(Box::new(job("127.0.0.1", port)))) {
            Some(Response::Printed {
                release_id: 32474433,
                result: Err(msg),
            }) => assert!(msg.contains("connect to printer"), "{msg}"),
            _ => panic!("expected a failed print"),
        }
    }

    // --- the database thread, against a real PostgreSQL ------------------------

    fn release_json(id: u32, title: &str) -> Release {
        serde_json::from_str(&format!(
            r#"{{"id": {id}, "title": "{title}", "released": "2017-03",
                "artists": [{{"id": 1, "name": "Artist"}}],
                "labels": [{{"id": 5, "name": "Label", "catno": "CAT 1"}}],
                "formats": [{{"name": "Vinyl", "descriptions": ["12\""]}}],
                "tracklist": [{{"position": "A1", "title": "One", "duration": "3:00"}}]}}"#
        ))
        .unwrap()
    }

    fn save_job(id: u32, copies: u16) -> LibraryRequest {
        LibraryRequest::Sync(Box::new(SyncJob::Save {
            release_id: id,
            release: Box::new(release_json(id, "A Title")),
            copies,
        }))
    }

    fn png() -> Vec<u8> {
        let img = image::RgbImage::from_fn(30, 30, |x, y| image::Rgb([x as u8, y as u8, 7]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn the_library_thread_walks_through_a_releases_life() {
        let Some(server) = crate::library::testing::server() else {
            return;
        };
        let mut state = LibraryState::default();
        let url = server.new_database("w_life");
        assert!(matches!(
            state.handle(LibraryRequest::Configure { url }),
            LibraryResponse::Ready { migrated: 2 }
        ));

        // not there yet
        match state.handle(LibraryRequest::Status(vec![7])) {
            LibraryResponse::Status { ids, result } => {
                assert_eq!(ids, vec![7]);
                assert!(result.unwrap().is_empty());
            }
            _ => panic!("expected a status"),
        }
        // stored
        match state.handle(save_job(7, 2)) {
            LibraryResponse::Synced {
                release_id: 7,
                result,
            } => {
                assert_eq!(
                    result.unwrap(),
                    SyncOutcome::Saved {
                        created: true,
                        copies: 2
                    }
                );
            }
            _ => panic!("expected a sync result"),
        }
        // a second copy on Discogs: updated, not created
        match state.handle(save_job(7, 3)) {
            LibraryResponse::Synced { result, .. } => {
                assert_eq!(
                    result.unwrap(),
                    SyncOutcome::Saved {
                        created: false,
                        copies: 3
                    }
                );
            }
            _ => panic!(),
        }
        // the cover
        match state.handle(LibraryRequest::Cover {
            release_id: 7,
            image: png(),
            source_url: "https://i.discogs.com/c.png".into(),
        }) {
            LibraryResponse::Cover {
                release_id: 7,
                result,
            } => assert!(result.unwrap()),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Status(vec![7, 8])) {
            LibraryResponse::Status { result, .. } => {
                let found = result.unwrap();
                assert_eq!(found.len(), 1);
                assert_eq!(
                    found[&7],
                    Entry {
                        copies: 3,
                        has_cover: true
                    }
                );
            }
            _ => panic!(),
        }
        // gone from the collection: removed, with its cover
        match state.handle(LibraryRequest::Sync(Box::new(SyncJob::Remove {
            release_id: 7,
        }))) {
            LibraryResponse::Synced { result, .. } => {
                assert_eq!(result.unwrap(), SyncOutcome::Removed { existed: true });
            }
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Sync(Box::new(SyncJob::Remove {
            release_id: 7,
        }))) {
            LibraryResponse::Synced { result, .. } => {
                assert_eq!(result.unwrap(), SyncOutcome::Removed { existed: false });
            }
            _ => panic!(),
        }
        // a cover for a release that is not (any more) there is not stored
        match state.handle(LibraryRequest::Cover {
            release_id: 7,
            image: png(),
            source_url: String::new(),
        }) {
            LibraryResponse::Cover { result, .. } => assert!(!result.unwrap()),
            _ => panic!(),
        }
    }

    #[test]
    fn requests_without_a_database_fail_with_a_message() {
        let mut state = LibraryState::default();
        assert!(matches!(
            state.handle(LibraryRequest::Configure { url: "  ".into() }),
            LibraryResponse::Disabled
        ));
        match state.handle(LibraryRequest::Status(vec![1])) {
            LibraryResponse::Status { result, .. } => {
                assert!(result.unwrap_err().contains("no database configured"))
            }
            _ => panic!(),
        }
        match state.handle(save_job(1, 1)) {
            LibraryResponse::Synced { result, .. } => assert!(result.is_err()),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Cover {
            release_id: 1,
            image: png(),
            source_url: String::new(),
        }) {
            LibraryResponse::Cover { result, .. } => assert!(result.is_err()),
            _ => panic!(),
        }
        // an unreachable database is a failed connection, not a crash
        match state.handle(LibraryRequest::Configure {
            url: "postgres://x@127.0.0.1:1/y".into(),
        }) {
            LibraryResponse::Failed(msg) => {
                assert!(msg.contains("connect to the database"), "{msg}")
            }
            _ => panic!(),
        }
    }

    #[test]
    fn a_release_without_a_title_is_reported_not_stored() {
        let Some(server) = crate::library::testing::server() else {
            return;
        };
        let mut state = LibraryState::default();
        state.handle(LibraryRequest::Configure {
            url: server.new_database("w_notitle"),
        });
        let job = LibraryRequest::Sync(Box::new(SyncJob::Save {
            release_id: 3,
            release: Box::new(Release::default()),
            copies: 1,
        }));
        match state.handle(job) {
            LibraryResponse::Synced { result, .. } => {
                assert!(result.unwrap_err().contains("no title"))
            }
            _ => panic!(),
        }
    }

    #[test]
    fn the_connection_is_rebuilt_after_it_was_lost() {
        let Some(server) = crate::library::testing::server() else {
            return;
        };
        let mut state = LibraryState::default();
        let url = server.new_database("w_reconnect");
        state.handle(LibraryRequest::Configure { url: url.clone() });
        state.handle(save_job(1, 1));
        // the server drops the connection (e.g. a restart)
        let mut admin = crate::library::testing::raw(&url);
        admin
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = 'w_reconnect' AND pid <> pg_backend_pid()",
                &[],
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        // the first request after that may fail on the dead connection; the next one works again
        let _ = state.handle(LibraryRequest::Status(vec![1]));
        match state.handle(LibraryRequest::Status(vec![1])) {
            LibraryResponse::Status { result, .. } => assert_eq!(result.unwrap()[&1].copies, 1),
            _ => panic!(),
        }
    }

    #[test]
    fn library_requests_are_counted_but_connecting_is_not() {
        let Some(server) = crate::library::testing::server() else {
            return;
        };
        let mut worker = Worker::spawn(egui::Context::default());
        worker.send(Request::Library(LibraryRequest::Configure {
            url: server.new_database("w_pending"),
        }));
        assert_eq!(worker.pending(), 0, "connecting is not counted");
        worker.send(Request::Library(LibraryRequest::Status(vec![1])));
        assert_eq!(worker.pending(), 1);
        let mut responses = Vec::new();
        for _ in 0..300 {
            responses.extend(worker.poll());
            if responses.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(responses.len(), 2, "the connection result and the status");
        assert_eq!(worker.pending(), 0);
        assert!(matches!(
            responses[0],
            Response::Library(LibraryResponse::Ready { .. })
        ));
    }

    #[test]
    fn the_library_thread_serves_the_collection_tab() {
        let Some(server) = crate::library::testing::server() else {
            return;
        };
        let mut state = LibraryState::default();
        state.handle(LibraryRequest::Configure {
            url: server.new_database("w_browse"),
        });
        state.handle(save_job(7, 2));
        state.handle(save_job(8, 1));
        state.handle(LibraryRequest::Cover {
            release_id: 7,
            image: png(),
            source_url: String::new(),
        });

        match state.handle(LibraryRequest::List) {
            LibraryResponse::List(rows) => {
                let rows = rows.unwrap();
                assert_eq!(rows.len(), 2);
                assert!(
                    rows.iter()
                        .any(|r| r.id == 7 && r.copies == 2 && r.has_cover)
                );
                assert!(rows.iter().any(|r| r.id == 8 && !r.has_cover));
            }
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Details {
            id: 7,
            for_scan: true,
        }) {
            LibraryResponse::Details {
                id: 7,
                for_scan: true,
                result,
            } => {
                let record = result.unwrap().unwrap();
                assert_eq!(record.title, "A Title");
                assert_eq!(record.tracks.len(), 1);
            }
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Details {
            id: 404,
            for_scan: false,
        }) {
            LibraryResponse::Details { result, .. } => assert!(result.unwrap().is_none()),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Image(7)) {
            LibraryResponse::Image { id: 7, result } => assert!(result.unwrap().is_some()),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Image(8)) {
            LibraryResponse::Image { result, .. } => assert!(result.unwrap().is_none()),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Thumbs(vec![7, 8])) {
            LibraryResponse::Thumbs { ids, result } => {
                assert_eq!(ids, vec![7, 8]);
                let found = result.unwrap();
                assert_eq!(found.len(), 1);
                assert_eq!(found[0].0, 7);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn browsing_requests_without_a_database_fail_with_a_message() {
        let mut state = LibraryState::default();
        match state.handle(LibraryRequest::List) {
            LibraryResponse::List(r) => assert!(r.unwrap_err().contains("no database configured")),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Details {
            id: 1,
            for_scan: false,
        }) {
            LibraryResponse::Details { result, .. } => assert!(result.is_err()),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Image(1)) {
            LibraryResponse::Image { result, .. } => assert!(result.is_err()),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Thumbs(vec![1])) {
            LibraryResponse::Thumbs { result, .. } => assert!(result.is_err()),
            _ => panic!(),
        }
    }

    #[test]
    fn pictures_do_not_show_the_spinner_but_the_list_does() {
        let mut worker = Worker::spawn(egui::Context::default());
        worker.send(Request::Library(LibraryRequest::Thumbs(vec![1])));
        worker.send(Request::Library(LibraryRequest::Image(1)));
        assert_eq!(worker.pending(), 0);
        worker.send(Request::Library(LibraryRequest::List));
        worker.send(Request::Library(LibraryRequest::Details {
            id: 1,
            for_scan: false,
        }));
        assert_eq!(worker.pending(), 2);
        let mut responses = Vec::new();
        for _ in 0..300 {
            responses.extend(worker.poll());
            if responses.len() >= 4 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(responses.len(), 4);
        assert_eq!(
            worker.pending(),
            0,
            "only the counted requests were subtracted"
        );
    }

    #[test]
    fn the_library_thread_stores_notes() {
        let Some(server) = crate::library::testing::server() else {
            return;
        };
        let mut state = LibraryState::default();
        state.handle(LibraryRequest::Configure {
            url: server.new_database("w_notes"),
        });
        state.handle(save_job(7, 1));
        match state.handle(LibraryRequest::SetNotes {
            id: 7,
            notes: "  signed ".into(),
        }) {
            LibraryResponse::Notes { id: 7, result } => assert!(result.unwrap()),
            _ => panic!(),
        }
        match state.handle(LibraryRequest::Details {
            id: 7,
            for_scan: false,
        }) {
            LibraryResponse::Details { result, .. } => {
                assert_eq!(result.unwrap().unwrap().my_notes.as_deref(), Some("signed"));
            }
            _ => panic!(),
        }
        match state.handle(LibraryRequest::SetNotes {
            id: 404,
            notes: "x".into(),
        }) {
            LibraryResponse::Notes { result, .. } => assert!(!result.unwrap()),
            _ => panic!(),
        }
        let mut offline = LibraryState::default();
        match offline.handle(LibraryRequest::SetNotes {
            id: 1,
            notes: "x".into(),
        }) {
            LibraryResponse::Notes { result, .. } => assert!(result.is_err()),
            _ => panic!(),
        }
    }

    // --- shops asked directly -------------------------------------------------------

    fn serve_page(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (base, handle)
    }

    const DEEJAY_PAGE: &str = include_str!("../tests/fixtures/deejay_product_960305.html");

    #[test]
    fn the_worker_fetches_a_deejay_page_and_parses_it() {
        let (base, server) = serve_page("200 OK", DEEJAY_PAGE);
        let mut state = State::default();
        let page =
            state.fetch_shop_page_from(&base, Shop::Deejay, "https://deejay.de/__960305".into());
        assert_eq!(page.shop, Shop::Deejay);
        assert_eq!(page.status, Some(200));
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].catno, "GME-002");
        let request = server.join().unwrap().to_lowercase();
        assert!(request.starts_with("get /__960305 "), "{request}");
        assert!(request.contains("user-agent: recordshelf"), "{request}");
        assert!(
            !request.contains("authorization"),
            "the Discogs credentials stay home: {request}"
        );
    }

    #[test]
    fn a_blocked_deejay_request_comes_back_with_its_status() {
        let (base, _server) = serve_page("403 Forbidden", "<html>Just a moment...</html>");
        let page = State::default().fetch_shop_page_from(
            &base,
            Shop::Deejay,
            "https://deejay.de/__960305".into(),
        );
        assert_eq!(page.status, Some(403));
        assert!(page.entries.is_empty());
    }

    #[test]
    fn an_unreachable_deejay_is_reported_as_no_reply() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let page = State::default().fetch_shop_page_from(
            &format!("http://127.0.0.1:{port}"),
            Shop::Deejay,
            "https://deejay.de/__960305".into(),
        );
        assert_eq!(page.status, Some(0));
        assert!(page.entries.is_empty());
    }

    #[test]
    fn only_deejay_is_fetched_directly_and_only_for_its_urls() {
        let mut state = State::default();
        let page = state.fetch_shop_page_from("http://127.0.0.1:1", Shop::Hhv, "525768".into());
        assert_eq!(page.status, Some(0), "hhv needs a browser");
        let page = state.fetch_shop_page_from("http://127.0.0.1:1", Shop::Deejay, "960305".into());
        assert_eq!(page.status, Some(0), "no article URL, nothing to ask");
    }

    #[test]
    fn a_fetch_request_answers_with_a_shop_page() {
        let (base, _server) = serve_page("200 OK", DEEJAY_PAGE);
        let mut state = State::default();
        let page =
            state.fetch_shop_page_from(&base, Shop::Deejay, "https://deejay.de/__960305".into());
        assert_eq!(
            page.code, "https://deejay.de/__960305",
            "the scanned code comes back as it was"
        );
        // through the request/response path
        match state.handle(Request::FetchShopPage {
            shop: Shop::Hhv,
            code: "525768".into(),
        }) {
            Some(Response::ShopPage(p)) => assert_eq!(p.shop, Shop::Hhv),
            _ => panic!("expected a shop page"),
        }
    }
}
