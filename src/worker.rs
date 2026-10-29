use std::{
    sync::mpsc::{Receiver, Sender, channel},
    thread,
};

use anyhow::{Context, Result};
use eframe::egui;

use crate::discogs::{
    CollectionFolder, CollectionInstance, DiscogsClient, Release, SearchItem, SearchQuery,
};
use crate::label::{self, CodeKind, LabelContent, LabelOptions, LabelSize};
use crate::printer::{self, JobOptions, Printer};
use crate::shops::{Shop, ShopEntry, search_plan};

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
    rx: Receiver<Response>,
    pending: usize,
}

impl Worker {
    pub fn spawn(repaint: egui::Context) -> Self {
        let (resp_tx, resp_rx) = channel::<Response>();
        let tx = Self::spawn_thread("discogs-worker", resp_tx.clone(), repaint.clone());
        let image_tx = Self::spawn_thread("discogs-images", resp_tx, repaint);
        Self {
            tx,
            image_tx,
            rx: resp_rx,
            pending: 0,
        }
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
            .filter(|r| !matches!(r, Response::Image { .. }))
            .count();
        self.pending = self.pending.saturating_sub(answered);
        out
    }

    /// Number of requests sent but not yet answered (images excluded).
    pub fn pending(&self) -> usize {
        self.pending
    }
}

struct State {
    client: Option<DiscogsClient>,
    username: String,
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

    /// Runs the search plan and returns the first non-empty result list.
    fn shop_lookup(&self, entry: &ShopEntry) -> Result<Vec<SearchItem>> {
        let client = self.client()?;
        for query in search_plan(entry) {
            let found = client.search(&query)?.results;
            if !found.is_empty() {
                return Ok(found);
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
}
