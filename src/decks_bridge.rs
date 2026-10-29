//! Temporary localhost bridge between the app and a browser userscript.
//!
//! decks.de sits behind a bot check that only a real browser passes, so the
//! browser does the shop lookup: the userscript polls `/pending` for scanned
//! codes, runs the shop search on a decks.de tab and posts the result page
//! back to `/result`. The app then parses it (see `decks`).

use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Read,
    path::Path,
    sync::{Arc, Mutex, mpsc::Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eframe::egui;

use crate::decks::{DecksEntry, parse_results};

pub const DEFAULT_PORT: u16 = 47321;
const MAX_BODY: u64 = 5 * 1024 * 1024;
const CONNECTED_WITHIN: Duration = Duration::from_secs(5);

/// Result of one shop lookup, as reported by the browser.
#[derive(Debug)]
pub struct DecksPage {
    pub code: String,
    /// HTTP status the browser saw, if it reported one.
    pub status: Option<u16>,
    pub entries: Vec<DecksEntry>,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<VecDeque<String>>,
    last_poll: Mutex<Option<Instant>>,
}

#[derive(Clone)]
pub struct Bridge {
    shared: Arc<Shared>,
    token: String,
    tx: Sender<DecksPage>,
}

impl Bridge {
    /// Starts the HTTP server on 127.0.0.1 and returns a handle.
    pub fn spawn(
        port: u16,
        token: String,
        tx: Sender<DecksPage>,
        repaint: egui::Context,
    ) -> Result<Self> {
        let server = tiny_http::Server::http(("127.0.0.1", port))
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("listen on 127.0.0.1:{port}"))?;
        let bridge = Self {
            shared: Arc::new(Shared::default()),
            token,
            tx,
        };
        let handler = bridge.clone();
        thread::Builder::new()
            .name("decks-bridge".to_string())
            .spawn(move || {
                for mut request in server.incoming_requests() {
                    let method = request.method().to_string();
                    let url = request.url().to_string();
                    let mut body = String::new();
                    let _ = request.as_reader().take(MAX_BODY).read_to_string(&mut body);
                    let (status, text) = handler.handle(&method, &url, &body);
                    let _ = request
                        .respond(tiny_http::Response::from_string(text).with_status_code(status));
                    repaint.request_repaint();
                }
            })
            .context("spawn decks bridge thread")?;
        Ok(bridge)
    }

    /// Queues a scanned code for the browser to look up.
    pub fn enqueue(&self, code: String) {
        self.shared.pending.lock().unwrap().push_back(code);
    }

    /// Whether the userscript has polled recently.
    pub fn connected(&self) -> bool {
        self.shared
            .last_poll
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < CONNECTED_WITHIN)
    }

    /// Routes one request. Returns the status code and response body.
    fn handle(&self, method: &str, url: &str, body: &str) -> (u16, String) {
        let Ok(parsed) = url::Url::parse(&format!("http://localhost{url}")) else {
            return (400, "bad url".into());
        };
        let query: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        if query.get("token") != Some(&self.token) {
            return (403, "bad token".into());
        }
        match (method, parsed.path()) {
            ("GET", "/pending") => {
                *self.shared.last_poll.lock().unwrap() = Some(Instant::now());
                let codes: Vec<String> = self.shared.pending.lock().unwrap().drain(..).collect();
                (
                    200,
                    serde_json::to_string(&codes).unwrap_or_else(|_| "[]".into()),
                )
            }
            ("POST", "/result") => {
                let Some(code) = query.get("code") else {
                    return (400, "missing code".into());
                };
                let _ = self.tx.send(DecksPage {
                    code: code.clone(),
                    status: query.get("status").and_then(|s| s.parse().ok()),
                    entries: parse_results(body),
                });
                (200, "ok".into())
            }
            _ => (404, "not found".into()),
        }
    }
}

/// Reads the shared secret from `dir/decks-token`, creating it on first use.
pub fn load_or_create_token(dir: &Path) -> Result<String> {
    let path = dir.join("decks-token");
    if let Ok(existing) = fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("read /dev/urandom")?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(&path, &token).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

/// Userscript (Tampermonkey/Violentmonkey) for the decks.de tab.
pub fn userscript(port: u16, token: &str) -> String {
    format!(
        r#"// ==UserScript==
// @name         RecordShelf decks.de bridge
// @namespace    recordshelf
// @version      1
// @description  Looks up scanned decks.de codes for the RecordShelf app
// @match        https://www.decks.de/*
// @grant        GM_xmlhttpRequest
// @connect      127.0.0.1
// @run-at       document-idle
// ==/UserScript==
(function () {{
  'use strict';
  if (window.top !== window) return; // only the top-level tab, not iframes
  const BASE = 'http://127.0.0.1:{port}';
  const TOKEN = '{token}';

  const app = (method, path, params, data) =>
    new Promise((resolve, reject) =>
      GM_xmlhttpRequest({{
        method,
        url: BASE + path + '?' + new URLSearchParams({{ token: TOKEN, ...params }}),
        data,
        headers: {{ 'Content-Type': 'text/html; charset=utf-8' }},
        onload: resolve,
        onerror: reject,
        ontimeout: reject,
      }}));

  async function lookup(code) {{
    const term = code.replace('/', ' ');
    const url = '/decks/workfloor/search_db.php?' +
      new URLSearchParams({{ such: term, wosuch: 'all', wassuch: 'atl', where: '' }});
    let status = 0, html = '';
    try {{
      const resp = await fetch(url, {{ credentials: 'same-origin' }});
      status = resp.status;
      html = new TextDecoder('iso-8859-1').decode(await resp.arrayBuffer());
    }} catch (e) {{ /* reported as status 0 */ }}
    await app('POST', '/result', {{ code, status }}, html);
  }}

  async function tick() {{
    try {{
      const r = await app('GET', '/pending', {{}});
      for (const code of JSON.parse(r.responseText)) await lookup(code);
    }} catch (e) {{ /* app not running; keep trying */ }}
    setTimeout(tick, 1000);
  }}
  tick();
}})();
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};

    fn bridge() -> (Bridge, Receiver<DecksPage>) {
        let (tx, rx) = channel();
        let bridge = Bridge {
            shared: Arc::new(Shared::default()),
            token: "secret".into(),
            tx,
        };
        (bridge, rx)
    }

    #[test]
    fn rejects_missing_or_wrong_token() {
        let (b, _rx) = bridge();
        assert_eq!(b.handle("GET", "/pending", "").0, 403);
        assert_eq!(b.handle("GET", "/pending?token=nope", "").0, 403);
        assert!(!b.connected(), "unauthenticated polls must not count");
    }

    #[test]
    fn pending_codes_are_handed_out_once() {
        let (b, _rx) = bridge();
        assert!(!b.connected());
        b.enqueue("CNF/O1".into());
        b.enqueue("CN9/FV".into());
        let (status, body) = b.handle("GET", "/pending?token=secret", "");
        assert_eq!(status, 200);
        assert_eq!(body, r#"["CNF/O1","CN9/FV"]"#);
        assert_eq!(b.handle("GET", "/pending?token=secret", "").1, "[]");
        assert!(b.connected());
    }

    #[test]
    fn result_is_parsed_and_forwarded() {
        let (b, rx) = bridge();
        let html = include_str!("../tests/fixtures/decks_search_cnf_o1.html");
        let (status, _) = b.handle(
            "POST",
            "/result?token=secret&code=CNF%2FO1&status=200",
            html,
        );
        assert_eq!(status, 200);
        let page = rx.try_recv().unwrap();
        assert_eq!(page.code, "CNF/O1");
        assert_eq!(page.status, Some(200));
        assert_eq!(page.entries.len(), 2);
    }

    #[test]
    fn blocked_page_yields_status_and_no_entries() {
        let (b, rx) = bridge();
        b.handle(
            "POST",
            "/result?token=secret&code=A%2FB&status=403",
            "Just a moment...",
        );
        let page = rx.try_recv().unwrap();
        assert_eq!(page.status, Some(403));
        assert!(page.entries.is_empty());
    }

    #[test]
    fn result_needs_a_code_and_unknown_routes_404() {
        let (b, rx) = bridge();
        assert_eq!(b.handle("POST", "/result?token=secret", "x").0, 400);
        assert!(rx.try_recv().is_err());
        assert_eq!(b.handle("GET", "/other?token=secret", "").0, 404);
        assert_eq!(b.handle("GET", "/result?token=secret&code=x", "").0, 404);
    }

    #[test]
    fn token_is_created_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_token(dir.path()).unwrap();
        assert_eq!(first.len(), 32);
        assert_eq!(load_or_create_token(dir.path()).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("decks-token"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn userscript_embeds_port_and_token() {
        let script = userscript(4711, "abc123");
        assert!(script.contains("http://127.0.0.1:4711"));
        assert!(script.contains("'abc123'"));
        assert!(script.contains("@grant        GM_xmlhttpRequest"));
    }

    #[test]
    fn serves_over_a_real_socket() {
        let (tx, _rx) = channel();
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let b = Bridge::spawn(port, "tok".into(), tx, egui::Context::default()).unwrap();
        b.enqueue("CNF/O1".into());
        let body = reqwest::blocking::get(format!("http://127.0.0.1:{port}/pending?token=tok"))
            .unwrap()
            .text()
            .unwrap();
        assert_eq!(body, r#"["CNF/O1"]"#);
    }
}
