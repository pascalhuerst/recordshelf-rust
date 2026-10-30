//! hhv.de: the label on a record carries the shop's article number (e.g.
//! `525768`). Searching the shop for it redirects to the article page, which
//! embeds schema.org JSON-LD with artist, title, label, catalog number
//! (`mpn`) and the real barcode (`gtin`).

use scraper::{Html, Selector};
use serde_json::Value;

use crate::shops::ShopEntry;

/// Article numbers are 6-7 digit numbers.
pub fn is_hhv_code(code: &str) -> bool {
    let code = code.trim();
    (6..=7).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_digit())
}

/// The article number in what a sticker's barcode says: the number may be
/// padded with leading zeros (`0000525768`) and followed by a variant suffix
/// (`525768V1`). `None` if it is not one.
pub fn article_number(code: &str) -> Option<String> {
    let code = code.trim().to_ascii_lowercase();
    // a variant suffix: `v` and digits at the end
    let core = match code.rfind('v') {
        Some(at)
            if at > 0
                && code[at + 1..].bytes().all(|b| b.is_ascii_digit())
                && at + 1 < code.len() =>
        {
            &code[..at]
        }
        _ => code.as_str(),
    };
    if core.is_empty() || !core.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number = core.trim_start_matches('0');
    is_hhv_code(number).then(|| number.to_string())
}

/// The article page must be the one we asked for.
pub fn find_entry<'a>(code: &str, entries: &'a [ShopEntry]) -> Option<&'a ShopEntry> {
    entries.iter().find(|e| e.code == code.trim())
}

/// Reads the article from an article page. Anything else (search result list,
/// bot check page, ...) yields nothing.
pub fn parse_results(html: &str) -> Vec<ShopEntry> {
    let doc = Html::parse_document(html);
    let selector =
        Selector::parse(r#"script[type="application/ld+json"]"#).expect("valid selector");
    let nodes: Vec<Value> = doc
        .select(&selector)
        .filter_map(|s| serde_json::from_str::<Value>(&s.text().collect::<String>()).ok())
        .flat_map(flatten)
        .collect();

    let of_type = |t: &str| {
        nodes
            .iter()
            .find(|n| n.get("@type").and_then(Value::as_str) == Some(t))
    };
    let Some(product) = of_type("Product") else {
        return Vec::new();
    };
    let album = of_type("MusicAlbum");

    let text = |v: Option<&Value>| match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let code = text(product.get("sku"));
    if code.is_empty() {
        return Vec::new();
    }

    // "Artist - Title" is the fallback when the album block is missing.
    let name = text(product.get("name"));
    let (name_artist, name_title) = name
        .split_once(" - ")
        .map_or((String::new(), name.clone()), |(a, t)| {
            (a.to_string(), t.to_string())
        });
    let pick = |preferred: String, fallback: String| {
        if preferred.is_empty() {
            fallback
        } else {
            preferred
        }
    };

    let artist = pick(
        text(
            album
                .and_then(|a| a.get("byArtist"))
                .and_then(|a| a.get("name")),
        ),
        name_artist,
    );
    let title = pick(text(album.and_then(|a| a.get("name"))), name_title);
    let label = pick(
        text(product.get("brand").and_then(|b| b.get("name"))),
        text(
            album
                .and_then(|a| a.get("albumRelease"))
                .and_then(|r| r.get("recordLabel"))
                .and_then(|l| l.get("name")),
        ),
    );
    let gtin = pick(
        text(product.get("gtin")),
        text(
            product
                .get("offers")
                .and_then(|o| o.as_array()?.first())
                .and_then(|o| o.get("gtin")),
        ),
    );

    vec![ShopEntry {
        code,
        artist,
        title,
        catno: text(product.get("mpn")),
        label,
        barcode: (!gtin.is_empty()).then_some(gtin),
    }]
}

/// JSON-LD may be a single object, an array, or an object with `@graph`.
fn flatten(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.into_iter().flat_map(flatten).collect(),
        Value::Object(mut map) => match map.remove("@graph") {
            Some(graph) => flatten(graph),
            None => vec![Value::Object(map)],
        },
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/hhv_product_525768.html");

    #[test]
    fn recognises_article_numbers() {
        for code in ["525768", " 525768\n", "1234567"] {
            assert!(is_hhv_code(code), "{code}");
        }
    }

    #[test]
    fn reads_the_article_number_from_padded_and_variant_codes() {
        for (code, number) in [
            ("525768", "525768"),
            ("0000525768", "525768"),
            ("00000000525768", "525768"),
            ("525768V1", "525768"),
            ("525768v12", "525768"),
            (" 0525768V1\n", "525768"),
            ("000525768v1", "525768"),
        ] {
            assert_eq!(article_number(code).as_deref(), Some(number), "{code}");
        }
        for code in [
            "",
            "v1",
            "000000v1",
            "000000",
            "12345",
            "12345678",
            "525768v",
            "525768x1",
            "52576a",
            "5012345678900",
            "00880319822216",
            "CNF/O1",
            "525768v1v1",
        ] {
            assert_eq!(article_number(code), None, "{code}");
        }
    }

    #[test]
    fn recognises_article_numbers_2() {
        for code in [
            "",
            "12345",
            "12345678",
            "52576a",
            "525-768",
            "CNF/O1",
            "5012345678900",
        ] {
            assert!(!is_hhv_code(code), "{code}");
        }
    }

    #[test]
    fn parses_the_article_page() {
        let entries = parse_results(FIXTURE);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.code, "525768");
        assert_eq!(e.artist, "Laurent Garnier");
        assert_eq!(e.title, "Speicher 95 Tribute EP");
        assert_eq!(e.catno, "Kompakt Ex 095");
        assert_eq!(e.label, "Kompakt Extra");
        assert_eq!(e.barcode.as_deref(), Some("00880319822216"));
    }

    #[test]
    fn finds_the_requested_article_only() {
        let entries = parse_results(FIXTURE);
        assert!(find_entry("525768", &entries).is_some());
        assert!(find_entry(" 525768 ", &entries).is_some());
        assert!(find_entry("525769", &entries).is_none());
    }

    #[test]
    fn pages_without_an_article_parse_to_nothing() {
        assert!(parse_results("").is_empty());
        assert!(parse_results("<html><body>Keine Treffer</body></html>").is_empty());
        // the bot check page served before the browser has its cookie
        assert!(parse_results(r#"<html><head><script type="text/javascript">function go(){}</script></head><body onload="go()"></body></html>"#).is_empty());
    }

    fn page(json: &str) -> String {
        format!(r#"<html><head><script type="application/ld+json">{json}</script></head></html>"#)
    }

    #[test]
    fn works_without_the_album_block() {
        let html = page(
            r#"{"@type":"Product","name":"Some Artist - Some Title","sku":"600001","mpn":"CAT 1","brand":{"name":"Some Label"},"offers":[{"gtin":"5012345678900"}]}"#,
        );
        let e = &parse_results(&html)[0];
        assert_eq!(e.artist, "Some Artist");
        assert_eq!(e.title, "Some Title");
        assert_eq!(e.label, "Some Label");
        assert_eq!(
            e.barcode.as_deref(),
            Some("5012345678900"),
            "gtin from the offer"
        );
    }

    #[test]
    fn numeric_sku_and_missing_fields_are_tolerated() {
        let html = page(r#"[{"@type":"Product","name":"Only A Title","sku":600002}]"#);
        let e = &parse_results(&html)[0];
        assert_eq!(e.code, "600002");
        assert_eq!(e.title, "Only A Title");
        assert_eq!(e.artist, "");
        assert_eq!(e.catno, "");
        assert_eq!(e.barcode, None);
    }

    #[test]
    fn broken_json_ld_is_skipped() {
        let html = format!(
            r#"<script type="application/ld+json">{{not json</script>{}"#,
            FIXTURE
        );
        assert_eq!(parse_results(&html).len(), 1);
    }
}
