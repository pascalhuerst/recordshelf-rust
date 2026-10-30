//! Localhost bridge between the app and a browser userscript.
//!
//! The shops (decks.de, hhv.de) sit behind bot checks that only a real browser
//! passes, so the browser does the shop lookup: the userscript polls
//! `/pending?shop=...` for scanned codes, runs the shop's search on an open tab
//! of that shop and posts the result page back to `/result`. The app then
//! parses it (see `decks` and `hhv`).

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

use crate::shops::{Shop, ShopEntry, parse_page};

pub const DEFAULT_PORT: u16 = 47321;
const MAX_BODY: u64 = 5 * 1024 * 1024;
const CONNECTED_WITHIN: Duration = Duration::from_secs(5);

/// Result of one shop lookup, as reported by the browser.
#[derive(Debug)]
pub struct ShopPage {
    pub shop: Shop,
    pub code: String,
    /// HTTP status the browser saw, if it reported one.
    pub status: Option<u16>,
    pub entries: Vec<ShopEntry>,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<VecDeque<(Shop, String)>>,
    last_poll: Mutex<HashMap<Shop, Instant>>,
}

#[derive(Clone)]
pub struct Bridge {
    shared: Arc<Shared>,
    token: String,
    tx: Sender<ShopPage>,
}

impl Bridge {
    /// Starts the HTTP server on 127.0.0.1 and returns a handle.
    pub fn spawn(
        port: u16,
        token: String,
        tx: Sender<ShopPage>,
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

    /// Queues a scanned code for the browser to look up in `shop`.
    pub fn enqueue(&self, shop: Shop, code: String) {
        self.shared.pending.lock().unwrap().push_back((shop, code));
    }

    /// Whether a tab of `shop` has polled recently.
    pub fn connected(&self, shop: Shop) -> bool {
        self.shared
            .last_poll
            .lock()
            .unwrap()
            .get(&shop)
            .is_some_and(|t| t.elapsed() < CONNECTED_WITHIN)
    }

    /// Routes one request. Returns the status code and response body.
    pub(crate) fn handle(&self, method: &str, url: &str, body: &str) -> (u16, String) {
        let Ok(parsed) = url::Url::parse(&format!("http://localhost{url}")) else {
            return (400, "bad url".into());
        };
        let query: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        if query.get("token") != Some(&self.token) {
            return (403, "bad token".into());
        }
        // Scripts from before hhv.de support do not send `shop`; they are decks.de only.
        let shop = match query.get("shop") {
            None => Shop::Decks,
            Some(key) => match Shop::from_key(key).filter(|s| s.uses_browser()) {
                Some(shop) => shop,
                None => return (400, "unknown shop".into()),
            },
        };
        match (method, parsed.path()) {
            ("GET", "/pending") => {
                self.shared
                    .last_poll
                    .lock()
                    .unwrap()
                    .insert(shop, Instant::now());
                let mut pending = self.shared.pending.lock().unwrap();
                let (mine, others): (VecDeque<_>, VecDeque<_>) =
                    pending.drain(..).partition(|(s, _)| *s == shop);
                *pending = others;
                let codes: Vec<String> = mine.into_iter().map(|(_, code)| code).collect();
                (
                    200,
                    serde_json::to_string(&codes).unwrap_or_else(|_| "[]".into()),
                )
            }
            ("POST", "/result") => {
                let Some(code) = query.get("code") else {
                    return (400, "missing code".into());
                };
                let entries = parse_page(shop, body);
                let _ = self.tx.send(ShopPage {
                    shop,
                    code: code.clone(),
                    status: query.get("status").and_then(|s| s.parse().ok()),
                    entries,
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

/// Version of the userscript; bump when it changes so that it is clear it must be reinstalled.
pub const USERSCRIPT_VERSION: u32 = 2;

/// Userscript (Tampermonkey/Violentmonkey) for the shop tabs.
pub fn userscript(port: u16, token: &str) -> String {
    format!(
        r#"// ==UserScript==
// @name         RecordShelf shop bridge
// @namespace    recordshelf
// @version      {version}
// @description  Looks up scanned decks.de / hhv.de codes for the RecordShelf app
// @match        https://www.decks.de/*
// @match        https://www.hhv.de/*
// @grant        GM_xmlhttpRequest
// @connect      127.0.0.1
// @run-at       document-idle
// ==/UserScript==
(function () {{
  'use strict';
  if (window.top !== window) return; // only the top-level tab, not iframes
  const BASE = 'http://127.0.0.1:{port}';
  const TOKEN = '{token}';

  // Each shop: its name for the app, how to build the search request, and the
  // page's character set.
  const SHOPS = {{
    'www.decks.de': {{
      key: 'decks',
      charset: 'iso-8859-1',
      url: (code) => '/decks/workfloor/search_db.php?' + new URLSearchParams({{
        such: code.replace('/', ' '), wosuch: 'all', wassuch: 'atl', where: '',
      }}),
    }},
    'www.hhv.de': {{
      key: 'hhv',
      charset: 'utf-8',
      // redirects to the article page when the number is an article number
      url: (code) => '/katalog/filter/suche-S11?' + new URLSearchParams({{ af: 'true', term: code }}),
    }},
  }};
  const shop = SHOPS[location.hostname];
  if (!shop) return;

  const app = (method, path, params, data) =>
    new Promise((resolve, reject) =>
      GM_xmlhttpRequest({{
        method,
        url: BASE + path + '?' + new URLSearchParams({{ token: TOKEN, shop: shop.key, ...params }}),
        data,
        headers: {{ 'Content-Type': 'text/html; charset=utf-8' }},
        onload: resolve,
        onerror: reject,
        ontimeout: reject,
      }}));

  async function lookup(code) {{
    let status = 0, html = '';
    try {{
      const resp = await fetch(shop.url(code), {{ credentials: 'same-origin' }});
      status = resp.status;
      html = new TextDecoder(shop.charset).decode(await resp.arrayBuffer());
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
"#,
        version = USERSCRIPT_VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};

    const DECKS_PAGE: &str = include_str!("../tests/fixtures/decks_search_cnf_o1.html");
    const HHV_PAGE: &str = include_str!("../tests/fixtures/hhv_product_525768.html");

    fn bridge() -> (Bridge, Receiver<ShopPage>) {
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
        assert!(
            !b.connected(Shop::Decks),
            "unauthenticated polls must not count"
        );
    }

    #[test]
    fn pending_codes_are_handed_out_once() {
        let (b, _rx) = bridge();
        assert!(!b.connected(Shop::Decks));
        b.enqueue(Shop::Decks, "CNF/O1".into());
        b.enqueue(Shop::Decks, "CN9/FV".into());
        let (status, body) = b.handle("GET", "/pending?token=secret&shop=decks", "");
        assert_eq!(status, 200);
        assert_eq!(body, r#"["CNF/O1","CN9/FV"]"#);
        assert_eq!(
            b.handle("GET", "/pending?token=secret&shop=decks", "").1,
            "[]"
        );
        assert!(b.connected(Shop::Decks));
    }

    #[test]
    fn each_shop_only_gets_its_own_codes() {
        let (b, _rx) = bridge();
        b.enqueue(Shop::Decks, "CNF/O1".into());
        b.enqueue(Shop::Hhv, "525768".into());
        assert_eq!(
            b.handle("GET", "/pending?token=secret&shop=hhv", "").1,
            r#"["525768"]"#
        );
        assert!(b.connected(Shop::Hhv));
        assert!(!b.connected(Shop::Decks), "the decks tab has not polled");
        assert_eq!(
            b.handle("GET", "/pending?token=secret&shop=decks", "").1,
            r#"["CNF/O1"]"#
        );
        assert!(b.connected(Shop::Decks));
    }

    #[test]
    fn scripts_without_a_shop_are_treated_as_decks() {
        let (b, rx) = bridge();
        b.enqueue(Shop::Decks, "CNF/O1".into());
        assert_eq!(
            b.handle("GET", "/pending?token=secret", "").1,
            r#"["CNF/O1"]"#
        );
        assert!(b.connected(Shop::Decks));
        b.handle(
            "POST",
            "/result?token=secret&code=CNF%2FO1&status=200",
            DECKS_PAGE,
        );
        assert_eq!(rx.try_recv().unwrap().shop, Shop::Decks);
    }

    #[test]
    fn unknown_shops_are_rejected() {
        let (b, _rx) = bridge();
        assert_eq!(
            b.handle("GET", "/pending?token=secret&shop=nope", "").0,
            400
        );
    }

    #[test]
    fn decks_result_is_parsed_and_forwarded() {
        let (b, rx) = bridge();
        let (status, _) = b.handle(
            "POST",
            "/result?token=secret&shop=decks&code=CNF%2FO1&status=200",
            DECKS_PAGE,
        );
        assert_eq!(status, 200);
        let page = rx.try_recv().unwrap();
        assert_eq!(page.shop, Shop::Decks);
        assert_eq!(page.code, "CNF/O1");
        assert_eq!(page.status, Some(200));
        assert_eq!(page.entries.len(), 2);
    }

    #[test]
    fn hhv_result_is_parsed_with_the_hhv_parser() {
        let (b, rx) = bridge();
        b.handle(
            "POST",
            "/result?token=secret&shop=hhv&code=525768&status=200",
            HHV_PAGE,
        );
        let page = rx.try_recv().unwrap();
        assert_eq!(page.shop, Shop::Hhv);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].catno, "Kompakt Ex 095");
    }

    #[test]
    fn blocked_page_yields_status_and_no_entries() {
        let (b, rx) = bridge();
        b.handle(
            "POST",
            "/result?token=secret&shop=hhv&code=525768&status=403",
            "<html>bot check</html>",
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
    fn userscript_embeds_port_token_and_both_shops() {
        let script = userscript(4711, "abc123");
        assert!(script.contains("http://127.0.0.1:4711"));
        assert!(script.contains("'abc123'"));
        assert!(script.contains("@grant        GM_xmlhttpRequest"));
        assert!(script.contains("// @match        https://www.decks.de/*"));
        assert!(script.contains("// @match        https://www.hhv.de/*"));
        assert!(script.contains(&format!("@version      {USERSCRIPT_VERSION}")));
        assert!(script.contains("/katalog/filter/suche-S11"));
    }

    #[test]
    fn serves_over_a_real_socket() {
        let (tx, _rx) = channel();
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let b = Bridge::spawn(port, "tok".into(), tx, egui::Context::default()).unwrap();
        b.enqueue(Shop::Hhv, "525768".into());
        let body = reqwest::blocking::get(format!(
            "http://127.0.0.1:{port}/pending?token=tok&shop=hhv"
        ))
        .unwrap()
        .text()
        .unwrap();
        assert_eq!(body, r#"["525768"]"#);
    }

    #[test]
    fn shops_that_need_no_browser_are_not_served() {
        let (b, _rx) = bridge();
        assert_eq!(
            b.handle("GET", "/pending?token=secret&shop=deejay", "").0,
            400
        );
        assert_eq!(
            b.handle("POST", "/result?token=secret&shop=deejay&code=1", "x")
                .0,
            400
        );
    }
}
