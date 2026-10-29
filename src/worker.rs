use std::{
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eframe::egui;

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

    fn scan(&self, code: String) -> ScanResult {
        let start = Instant::now();
        let lookup = parse_release_id(&code).and_then(|id| {
            let release = self.client()?.release(id)?;
            // Ownership is best effort: a failed check must not hide the release.
            let owned = self
                .username()
                .and_then(|u| self.client()?.collection_instances(u, id))
                .ok();
            Ok((release, owned))
        });
        match lookup {
            Ok((release, owned)) => ScanResult {
                barcode: code,
                release: Some(release),
                owned,
                error: None,
                elapsed: start.elapsed(),
            },
            Err(err) => ScanResult {
                barcode: code,
                release: None,
                owned: None,
                error: Some(format!("{err:#}")),
                elapsed: start.elapsed(),
            },
        }
    }
}

fn parse_release_id(code: &str) -> Result<u32> {
    code.parse()
        .with_context(|| format!("invalid numeric barcode: {code}"))
}
