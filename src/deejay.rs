//! deejay.de: the QR code on the label holds the shop's URL of the article,
//! e.g. `https://deejay.de/__960305`. Unlike decks.de and hhv.de the shop serves
//! its pages to a plain HTTP request (no bot check), so the app fetches the
//! page itself; no browser is involved.
//!
//! The article page is server-rendered; it has artist, title, label, catalog
//! number and the tracks, but no barcode.

use anyhow::{Context, Result};
use scraper::{Html, Selector};

use crate::shops::ShopEntry;

pub const BASE: &str = "https://www.deejay.de";

/// The article id in a scanned code: the digits at the end of the path of a
/// deejay.de URL (`https://deejay.de/__960305`, a product page URL, ...).
/// Tolerant about the separators, which scanners tend to garble.
pub fn article_id(code: &str) -> Option<u32> {
    let lower = code.trim().to_lowercase();
    let (_, path) = lower.split_once("deejay.de/")?;
    let path = path.split(['?', '#']).next()?;
    let digits: String = path
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    (digits.len() >= 4).then(|| digits.parse().ok()).flatten()
}

/// The article must be the one we asked for.
pub fn find_entry<'a>(code: &str, entries: &'a [ShopEntry]) -> Option<&'a ShopEntry> {
    let id = article_id(code)?;
    entries.iter().find(|e| e.code == id.to_string())
}

fn text(element: scraper::ElementRef) -> String {
    element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reads the article from an article page. Anything else (a challenge page, a
/// search result, an error page) yields nothing.
pub fn parse_results(html: &str) -> Vec<ShopEntry> {
    let doc = Html::parse_document(html);
    let select = |css: &str| Selector::parse(css).expect("valid selector");
    let first = |css: &str| doc.select(&select(css)).next().map(text);

    let Some(title) = first("h1.pd-title").filter(|t| !t.is_empty()) else {
        return Vec::new();
    };
    let Some(id) = doc
        .select(&select("[data-article-id]"))
        .next()
        .and_then(|e| e.value().attr("data-article-id"))
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    else {
        return Vec::new();
    };
    let artist = doc
        .select(&select("h2.artist a"))
        .map(text)
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    vec![ShopEntry {
        code: id.to_string(),
        artist,
        title,
        catno: first(".musiclabel strong").unwrap_or_default(),
        label: first(".musiclabel a").unwrap_or_default(),
        barcode: None,
    }]
}

/// Fetches the article page and returns the HTTP status and the page.
/// `https://www.deejay.de/__ID` redirects to the article page.
pub fn fetch_from(
    client: &reqwest::blocking::Client,
    base: &str,
    id: u32,
) -> Result<(u16, String)> {
    let response = client
        .get(format!("{base}/__{id}"))
        .send()
        .context("request the deejay.de article page")?;
    let status = response.status().as_u16();
    let body = response.text().context("read the deejay.de article page")?;
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const FIXTURE: &str = include_str!("../tests/fixtures/deejay_product_960305.html");

    #[test]
    fn finds_the_article_id_in_what_the_qr_code_gives() {
        for code in [
            "https://deejay.de/__960305",
            "https://www.deejay.de/__960305",
            "HTTPS://DEEJAY.DE/__960305",
            "http://deejay.de/__960305",
            "https://deejay.de/__960305?utm=x",
            "https://www.deejay.de/Groove_Minority_EP_%232_GME-002_Vinyl__960305",
            "  https://deejay.de/__960305\n",
            // garbled separators
            "HTTPS//DEEJAY.DE/-960305",
            "deejay.de/960305",
        ] {
            assert_eq!(article_id(code), Some(960305), "{code}");
        }
    }

    #[test]
    fn other_codes_are_not_deejay_articles() {
        for code in [
            "",
            "960305",
            "__960305",
            "https://www.hhv.de/records/artikel/x-525768",
            "https://deejay.de/",
            "https://deejay.de/Groove+Minority",
            "https://deejay.de/__12",
            "CNF/O1",
            "https://example.com/deejay.de/__960305x",
        ] {
            assert_eq!(article_id(code), None, "{code}");
        }
    }

    #[test]
    fn parses_the_article_page() {
        let entries = parse_results(FIXTURE);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.code, "960305");
        assert_eq!(e.artist, "Groove Minority");
        assert_eq!(e.title, "EP #2");
        assert_eq!(e.catno, "GME-002");
        assert_eq!(
            e.label, "> France",
            "the label name keeps its odd character"
        );
        assert_eq!(e.barcode, None);
    }

    #[test]
    fn several_artists_are_joined() {
        let html = FIXTURE.replace(
            r#"<a href="/Groove+Minority">Groove Minority</a>"#,
            r#"<a href="/a">One</a></span><span><a href="/b">Two</a>"#,
        );
        assert_eq!(parse_results(&html)[0].artist, "One, Two");
    }

    #[test]
    fn finds_only_the_requested_article() {
        let entries = parse_results(FIXTURE);
        assert!(find_entry("https://deejay.de/__960305", &entries).is_some());
        assert!(find_entry("https://deejay.de/__960306", &entries).is_none());
        assert!(find_entry("not a url", &entries).is_none());
    }

    #[test]
    fn pages_without_an_article_parse_to_nothing() {
        assert!(parse_results("").is_empty());
        assert!(parse_results("<html><body>Just a moment...</body></html>").is_empty());
        // a title without an article id, and an id without a title
        assert!(parse_results(r#"<h1 class="pd-title">Only</h1>"#).is_empty());
        assert!(parse_results(r#"<div data-article-id="1234" hidden></div>"#).is_empty());
        assert!(
            parse_results(r#"<h1 class="pd-title">T</h1><div data-article-id="abc"></div>"#)
                .is_empty()
        );
    }

    #[test]
    fn missing_details_are_empty_not_fatal() {
        let html = r#"<h1 class="pd-title">Only A Title</h1><div data-article-id="777777"></div>"#;
        let e = &parse_results(html)[0];
        assert_eq!(
            (e.artist.as_str(), e.catno.as_str(), e.label.as_str()),
            ("", "", "")
        );
        assert_eq!(e.title, "Only A Title");
    }

    /// Serves one canned HTTP response; returns the base URL and the request.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
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

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .user_agent("test-agent")
            .build()
            .unwrap()
    }

    #[test]
    fn fetches_the_page_by_article_id() {
        let (base, server) = serve_once("200 OK", FIXTURE);
        let (status, body) = fetch_from(&client(), &base, 960305).unwrap();
        assert_eq!(status, 200);
        assert_eq!(parse_results(&body).len(), 1);
        let request = server.join().unwrap().to_lowercase();
        assert!(request.starts_with("get /__960305 "), "{request}");
        assert!(request.contains("user-agent: test-agent"), "{request}");
        assert!(
            !request.contains("authorization"),
            "no credentials go to the shop: {request}"
        );
    }

    #[test]
    fn a_blocked_request_is_reported_by_its_status() {
        let (base, _server) = serve_once("403 Forbidden", "<html>Just a moment...</html>");
        let (status, body) = fetch_from(&client(), &base, 960305).unwrap();
        assert_eq!(status, 403);
        assert!(parse_results(&body).is_empty());
    }

    #[test]
    fn an_unreachable_shop_is_a_clear_error() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = format!(
            "{:#}",
            fetch_from(&client(), &format!("http://127.0.0.1:{port}"), 1).unwrap_err()
        );
        assert!(err.contains("deejay.de article page"), "{err}");
    }
}
