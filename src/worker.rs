use std::{
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eframe::egui;

use crate::decks::{DecksEntry, search_plan};
use crate::discogs::{
    CollectionFolder, CollectionInstance, DiscogsClient, Release, SearchItem, SearchQuery,
};

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
    /// Find Discogs releases for a decks.de entry (by catalog number etc.).
    DecksLookup {
        code: String,
        entry: DecksEntry,
    },
    AddToCollection {
        release_id: u32,
        folder_id: u32,
    },
}

pub enum Response {
    Scan(ScanResult),
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
    Decks {
        code: String,
        entry: DecksEntry,
        result: Result<Vec<SearchItem>, String>,
    },
    Added {
        release_id: u32,
        folder_id: u32,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub barcode: String,
    pub release: Option<Release>,
    /// Copies already in the collection; `None` if unknown (no username, or the check failed).
    pub owned: Option<Vec<CollectionInstance>>,
    /// How many releases Discogs knows for a scanned barcode (1 for a release id).
    pub matches: usize,
    pub error: Option<String>,
    pub elapsed: Duration,
}

/// Handle to the background thread that talks to Discogs, so the UI thread
/// never blocks on the network.
pub struct Worker {
    tx: Sender<Request>,
    rx: Receiver<Response>,
    pending: usize,
}

impl Worker {
    pub fn spawn(repaint: egui::Context) -> Self {
        let (req_tx, req_rx) = channel::<Request>();
        let (resp_tx, resp_rx) = channel::<Response>();
        thread::Builder::new()
            .name("discogs-worker".to_string())
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
        Self {
            tx: req_tx,
            rx: resp_rx,
            pending: 0,
        }
    }

    pub fn send(&mut self, req: Request) {
        if !matches!(req, Request::Configure { .. }) {
            self.pending += 1;
        }
        let _ = self.tx.send(req);
    }

    /// Collects finished responses without blocking.
    pub fn poll(&mut self) -> Vec<Response> {
        let out: Vec<Response> = self.rx.try_iter().collect();
        self.pending = self.pending.saturating_sub(out.len());
        out
    }

    /// Number of requests sent but not yet answered.
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
            Request::Scan(code) => Response::Scan(self.scan(code)),
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
            Request::DecksLookup { code, entry } => {
                let result = self.decks_lookup(&entry).map_err(|e| format!("{e:#}"));
                Response::Decks {
                    code,
                    entry,
                    result,
                }
            }
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

    /// Runs the search plan and returns the first non-empty result list.
    fn decks_lookup(&self, entry: &DecksEntry) -> Result<Vec<SearchItem>> {
        let client = self.client()?;
        for query in search_plan(entry) {
            let found = client.search(&query)?.results;
            if !found.is_empty() {
                return Ok(found);
            }
        }
        Ok(Vec::new())
    }

    fn scan(&self, code: String) -> ScanResult {
        let start = Instant::now();
        let lookup = self.lookup_scan(&code);
        let elapsed = start.elapsed();
        match lookup {
            Ok((release, owned, matches)) => ScanResult {
                barcode: code,
                release: Some(release),
                owned,
                matches,
                error: None,
                elapsed,
            },
            Err(err) => ScanResult {
                barcode: code,
                release: None,
                owned: None,
                matches: 0,
                error: Some(format!("{err:#}")),
                elapsed,
            },
        }
    }

    fn lookup_scan(&self, code: &str) -> Result<(Release, Option<Vec<CollectionInstance>>, usize)> {
        let client = self.client()?;
        let (id, matches) = match ScanCode::classify(code)? {
            ScanCode::ReleaseId(id) => (id, 1),
            ScanCode::Barcode(barcode) => {
                let found = client.search(&SearchQuery {
                    barcode: Some(barcode.clone()),
                    kind: Some("release".to_string()),
                    per_page: Some(10),
                    ..Default::default()
                })?;
                let id = found
                    .results
                    .iter()
                    .find_map(|item| item.id)
                    .with_context(|| format!("no release found for barcode {barcode}"))?;
                (id, found.results.len())
            }
        };
        let release = client.release(id)?;
        // Ownership is best effort: a failed check must not hide the release.
        let owned = self
            .username()
            .and_then(|u| client.collection_instances(u, id))
            .ok();
        Ok((release, owned, matches))
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
}
