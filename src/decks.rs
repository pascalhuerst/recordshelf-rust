//! Temporary helper for records bought at decks.de.
//!
//! Their labels carry a barcode with an internal code such as `CNF/O1`. The
//! shop's search page (reachable only from a real browser, see `decks_bridge`)
//! maps it to the label's catalog number, which Discogs can search for.

use scraper::{Html, Selector};

use crate::shops::ShopEntry;

/// Whether a scanned string looks like a decks code (`ABC/12`).
pub fn is_decks_code(code: &str) -> bool {
    let Some((left, right)) = code.trim().split_once('/') else {
        return false;
    };
    let part_ok =
        |p: &str| (1..=6).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric());
    part_ok(left) && part_ok(right) && left.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Form used by the shop: lower case, `/` replaced by `-` (`CNF/O1` -> `cnf-o1`).
pub fn normalize_code(code: &str) -> String {
    code.trim().to_lowercase().replace(['/', ' '], "-")
}

fn collapse(text: impl Iterator<Item = impl AsRef<str>>) -> String {
    text.map(|t| t.as_ref().to_string())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extracts every release block from a search result page.
pub fn parse_results(html: &str) -> Vec<ShopEntry> {
    let doc = Html::parse_document(html);
    let block = Selector::parse("div.oneLine[data-code]").expect("valid selector");
    let field = |css: &str| Selector::parse(css).expect("valid selector");
    let (artist, title, catno, label) = (
        field(".LArtist"),
        field(".LTitel"),
        field(".LLabelcat"),
        field(".LLabel"),
    );
    doc.select(&block)
        .filter_map(|el| {
            let text = |sel: &Selector| {
                el.select(sel)
                    .next()
                    .map(|e| collapse(e.text()))
                    .unwrap_or_default()
            };
            Some(ShopEntry {
                code: el.value().attr("data-code")?.to_string(),
                artist: text(&artist),
                title: text(&title),
                catno: text(&catno),
                label: text(&label),
                barcode: None,
            })
        })
        .collect()
}

/// Picks the block that belongs to the scanned code.
pub fn find_entry<'a>(code: &str, entries: &'a [ShopEntry]) -> Option<&'a ShopEntry> {
    let wanted = normalize_code(code);
    entries.iter().find(|e| normalize_code(&e.code) == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/decks_search_cnf_o1.html");

    #[test]
    fn recognises_decks_codes() {
        for code in [
            "CNF/O1",
            "CN9/FV",
            "CQO/WM",
            "CAJ/VI",
            "CNU/CH",
            " cnf/o1\n",
        ] {
            assert!(is_decks_code(code), "{code}");
        }
        for code in [
            "",
            "5012345678900",
            "249504",
            "12/34",
            "CNF",
            "A/B/C",
            "CNF/",
            "/O1",
            "CN F/O1",
        ] {
            assert!(!is_decks_code(code), "{code}");
        }
    }

    #[test]
    fn normalises_like_the_shop() {
        assert_eq!(normalize_code("CNF/O1"), "cnf-o1");
        assert_eq!(normalize_code(" CN9/FV "), "cn9-fv");
    }

    #[test]
    fn parses_all_blocks() {
        let entries = parse_results(FIXTURE);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].code, "cnf-35");
        assert_eq!(entries[0].catno, "6553272");
        assert_eq!(entries[1].code, "cnf-o1");
        assert_eq!(entries[1].artist, "Altone");
        assert_eq!(entries[1].catno, "PCSS09");
        assert_eq!(entries[1].label, "Primary colours");
        assert!(entries[1].title.starts_with("WONDERSCAPE NUMINOUS (180G"));
    }

    #[test]
    fn picks_the_exact_match_not_the_other_hit() {
        let entries = parse_results(FIXTURE);
        let hit = find_entry("CNF/O1", &entries).unwrap();
        assert_eq!(hit.catno, "PCSS09");
        assert!(find_entry("XXX/99", &entries).is_none());
    }

    #[test]
    fn page_without_results_parses_to_nothing() {
        assert!(parse_results("<html><body>Just a moment...</body></html>").is_empty());
        assert!(parse_results("").is_empty());
    }
}
