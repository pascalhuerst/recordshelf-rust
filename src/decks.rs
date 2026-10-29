//! Temporary helper for records bought at decks.de.
//!
//! Their labels carry a barcode with an internal code such as `CNF/O1`. The
//! shop's search page (reachable only from a real browser, see `decks_bridge`)
//! maps it to the label's catalog number, which Discogs can search for.

use scraper::{Html, Selector};

use crate::discogs::SearchQuery;

/// One release block from a decks.de search result page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecksEntry {
    /// Internal code, e.g. `cnf-o1`.
    pub code: String,
    pub artist: String,
    pub title: String,
    /// Label catalog number, e.g. `PCSS09`.
    pub catno: String,
    pub label: String,
}

impl DecksEntry {
    /// Title without the trailing "(180G / ...)" remarks the shop appends.
    pub fn short_title(&self) -> &str {
        match self.title.find('(') {
            Some(i) => self.title[..i].trim(),
            None => self.title.trim(),
        }
    }
}

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
pub fn parse_results(html: &str) -> Vec<DecksEntry> {
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
            Some(DecksEntry {
                code: el.value().attr("data-code")?.to_string(),
                artist: text(&artist),
                title: text(&title),
                catno: text(&catno),
                label: text(&label),
            })
        })
        .collect()
}

/// Picks the block that belongs to the scanned code.
pub fn find_entry<'a>(code: &str, entries: &'a [DecksEntry]) -> Option<&'a DecksEntry> {
    let wanted = normalize_code(code);
    entries.iter().find(|e| normalize_code(&e.code) == wanted)
}

/// Discogs queries to try, most specific first; the first one with results wins.
pub fn search_plan(entry: &DecksEntry) -> Vec<SearchQuery> {
    let base = SearchQuery {
        kind: Some("release".to_string()),
        per_page: Some(25),
        ..Default::default()
    };
    let mut plan = Vec::new();
    if !entry.catno.is_empty() {
        if !entry.label.is_empty() {
            plan.push(SearchQuery {
                catno: Some(entry.catno.clone()),
                label: Some(entry.label.clone()),
                ..base.clone()
            });
        }
        plan.push(SearchQuery {
            catno: Some(entry.catno.clone()),
            ..base.clone()
        });
    }
    if !entry.artist.is_empty() && !entry.short_title().is_empty() {
        plan.push(SearchQuery {
            artist: Some(entry.artist.clone()),
            release_title: Some(entry.short_title().to_string()),
            ..base
        });
    }
    plan
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

    #[test]
    fn short_title_drops_remarks() {
        let entries = parse_results(FIXTURE);
        assert_eq!(entries[1].short_title(), "WONDERSCAPE NUMINOUS");
    }

    #[test]
    fn search_plan_goes_from_specific_to_loose() {
        let entry = &parse_results(FIXTURE)[1];
        let plan = search_plan(entry);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].catno.as_deref(), Some("PCSS09"));
        assert_eq!(plan[0].label.as_deref(), Some("Primary colours"));
        assert_eq!(plan[1].catno.as_deref(), Some("PCSS09"));
        assert_eq!(plan[1].label, None);
        assert_eq!(plan[2].artist.as_deref(), Some("Altone"));
        assert_eq!(
            plan[2].release_title.as_deref(),
            Some("WONDERSCAPE NUMINOUS")
        );
        assert!(plan.iter().all(|q| q.kind.as_deref() == Some("release")));
    }

    #[test]
    fn search_plan_without_catno_uses_artist_and_title() {
        let entry = DecksEntry {
            code: "x-1".into(),
            artist: "A".into(),
            title: "T (remarks)".into(),
            catno: String::new(),
            label: "L".into(),
        };
        let plan = search_plan(&entry);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].release_title.as_deref(), Some("T"));
    }
}
