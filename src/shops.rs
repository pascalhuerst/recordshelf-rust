//! Record shops whose labels we can scan (decks.de, hhv.de, deejay.de).
//!
//! Their barcodes are internal codes, not UPC/EAN. The shop's own page maps a
//! code to the release data (catalog number, label, sometimes the real
//! barcode), which Discogs can then be searched with. decks.de and hhv.de
//! pages can only be fetched by a real browser (see `shop_bridge`); deejay.de
//! serves its pages to the app directly. The parsing lives in `decks`, `hhv`
//! and `deejay`.

use crate::{
    catalogue::fold,
    decks, deejay,
    discogs::{SearchItem, SearchQuery},
    hhv,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shop {
    Decks,
    Hhv,
    Deejay,
}

impl Shop {
    pub const ALL: [Shop; 3] = [Shop::Decks, Shop::Hhv, Shop::Deejay];
    /// The shops that need a browser tab (and the userscript).
    pub const BROWSER: [Shop; 2] = [Shop::Decks, Shop::Hhv];

    /// Whether the page has to be fetched by a browser tab. Otherwise the app
    /// asks the shop itself.
    pub fn uses_browser(self) -> bool {
        Self::BROWSER.contains(&self)
    }

    /// Identifier used in the bridge API.
    pub fn key(self) -> &'static str {
        match self {
            Shop::Decks => "decks",
            Shop::Hhv => "hhv",
            Shop::Deejay => "deejay",
        }
    }

    /// Name for messages.
    pub fn name(self) -> &'static str {
        match self {
            Shop::Decks => "decks.de",
            Shop::Hhv => "hhv.de",
            Shop::Deejay => "deejay.de",
        }
    }

    pub fn from_key(key: &str) -> Option<Shop> {
        Self::ALL.into_iter().find(|s| s.key() == key)
    }
}

/// What a shop knows about one release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShopEntry {
    /// The shop's internal code.
    pub code: String,
    pub artist: String,
    pub title: String,
    /// Label catalog number, e.g. `PCSS09`.
    pub catno: String,
    pub label: String,
    /// The release's real UPC/EAN, if the shop lists it.
    pub barcode: Option<String>,
}

impl ShopEntry {
    /// Title without the trailing "(180G / ...)" remarks some shops append.
    pub fn short_title(&self) -> &str {
        match self.title.find('(') {
            Some(i) => self.title[..i].trim(),
            None => self.title.trim(),
        }
    }
}

/// Picks the entry that belongs to the scanned code (shops may return loosely
/// matching extras).
pub fn find_entry<'a>(shop: Shop, code: &str, entries: &'a [ShopEntry]) -> Option<&'a ShopEntry> {
    match shop {
        Shop::Decks => decks::find_entry(code, entries),
        Shop::Hhv => hhv::find_entry(code, entries),
        Shop::Deejay => deejay::find_entry(code, entries),
    }
}

/// Reads a shop page into what the shop knows about the release.
pub fn parse_page(shop: Shop, html: &str) -> Vec<ShopEntry> {
    match shop {
        Shop::Decks => decks::parse_results(html),
        Shop::Hhv => hhv::parse_results(html),
        Shop::Deejay => deejay::parse_results(html),
    }
}

/// Barcode spellings worth trying: Discogs lists most records with a 12 digit
/// UPC or 13 digit EAN, while shops often give a zero padded GTIN-14.
pub fn barcode_variants(gtin: &str) -> Vec<String> {
    let digits: String = gtin.chars().filter(char::is_ascii_digit).collect();
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for len in [12, 13] {
        if trimmed.len() <= len {
            out.push(format!("{trimmed:0>len$}"));
        }
    }
    out.dedup();
    out
}

/// Discogs queries to try, most specific first; the first one with results wins.
pub fn search_plan(entry: &ShopEntry) -> Vec<SearchQuery> {
    let base = SearchQuery {
        kind: Some("release".to_string()),
        per_page: Some(25),
        ..Default::default()
    };
    let mut plan = Vec::new();
    for barcode in entry.barcode.iter().flat_map(|b| barcode_variants(b)) {
        plan.push(SearchQuery {
            barcode: Some(barcode),
            ..base.clone()
        });
    }
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

/// Keeps the Discogs results that name the shop's artist, when there are any.
/// A catalog number alone is not unique on Discogs (labels reuse them, and
/// "GME-002" matches "GME002" of other labels), so the first hit may be an
/// unrelated record. Without any match everything is kept: the shop may spell
/// the artist differently ("VA", "Various").
pub fn prefer_artist(items: Vec<SearchItem>, entry: &ShopEntry) -> Vec<SearchItem> {
    let artists: Vec<String> = entry
        .artist
        .split(',')
        .map(fold)
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();
    if artists.is_empty() {
        return items;
    }
    let (matching, others): (Vec<_>, Vec<_>) = items.into_iter().partition(|item| {
        let title = fold(&item.title_display());
        artists.iter().any(|a| title.contains(a.as_str()))
    });
    if matching.is_empty() {
        others
    } else {
        matching
    }
}

/// Where a scanned code goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Look it up in the shop through the browser.
    Shop(Shop),
    /// The app asks the shop itself (no browser needed).
    Fetch(Shop),
    /// Belongs to a shop whose browser tab is not available.
    ShopUnavailable(Shop),
    /// Plain Discogs release id or UPC/EAN.
    Discogs,
}

/// Decides how to treat a scanned code.
///
/// decks.de codes (`ABC/12`) are unambiguous. hhv.de article numbers are plain
/// 6-7 digit numbers, which look like Discogs release ids, so they are only
/// taken for hhv when the bridge is running (`bridge_on`); then a number of
/// that length is never looked up as a Discogs id.
pub fn route_scan(code: &str, bridge_on: bool, connected: impl Fn(Shop) -> bool) -> Route {
    // the QR code of deejay.de holds the article URL: unmistakable, and no browser needed
    if deejay::article_id(code).is_some() {
        return Route::Fetch(Shop::Deejay);
    }
    let shop = if decks::is_decks_code(code) {
        Shop::Decks
    } else if bridge_on && hhv::is_hhv_code(code) {
        Shop::Hhv
    } else {
        return Route::Discogs;
    };
    if bridge_on && connected(shop) {
        Route::Shop(shop)
    } else {
        Route::ShopUnavailable(shop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> ShopEntry {
        ShopEntry {
            code: "cnf-o1".into(),
            artist: "Altone".into(),
            title: "WONDERSCAPE NUMINOUS (180G / GREEN MARBLED VINYL)".into(),
            catno: "PCSS09".into(),
            label: "Primary colours".into(),
            barcode: None,
        }
    }

    #[test]
    fn shop_keys_roundtrip() {
        for shop in Shop::ALL {
            assert_eq!(Shop::from_key(shop.key()), Some(shop));
        }
        assert_eq!(Shop::from_key("other"), None);
        assert_eq!(Shop::Hhv.name(), "hhv.de");
    }

    #[test]
    fn short_title_drops_remarks() {
        assert_eq!(entry().short_title(), "WONDERSCAPE NUMINOUS");
    }

    #[test]
    fn search_plan_goes_from_specific_to_loose() {
        let plan = search_plan(&entry());
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
    fn a_barcode_is_tried_first() {
        let mut e = entry();
        e.barcode = Some("00880319822216".into());
        let plan = search_plan(&e);
        assert_eq!(plan.len(), 5);
        assert_eq!(plan[0].barcode.as_deref(), Some("880319822216"));
        assert_eq!(plan[1].barcode.as_deref(), Some("0880319822216"));
        assert!(plan[2].barcode.is_none() && plan[2].catno.is_some());
    }

    #[test]
    fn barcode_variants_handle_padding() {
        assert_eq!(
            barcode_variants("00880319822216"),
            ["880319822216", "0880319822216"]
        );
        assert_eq!(barcode_variants("5012345678900"), ["5012345678900"]);
        assert_eq!(barcode_variants("0 5012345678900"), ["5012345678900"]);
        assert!(barcode_variants("").is_empty());
        assert!(barcode_variants("0000").is_empty());
        assert!(barcode_variants("n/a").is_empty());
    }

    #[test]
    fn search_plan_without_catno_uses_artist_and_title() {
        let e = ShopEntry {
            catno: String::new(),
            title: "T (remarks)".into(),
            ..entry()
        };
        let plan = search_plan(&e);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].release_title.as_deref(), Some("T"));
    }

    #[test]
    fn empty_entry_gives_empty_plan() {
        let e = ShopEntry {
            code: "x".into(),
            artist: String::new(),
            title: String::new(),
            catno: String::new(),
            label: String::new(),
            barcode: None,
        };
        assert!(search_plan(&e).is_empty());
    }

    #[test]
    fn decks_codes_route_to_decks() {
        assert_eq!(
            route_scan("CNF/O1", true, |_| true),
            Route::Shop(Shop::Decks)
        );
        assert_eq!(
            route_scan("CNF/O1", true, |_| false),
            Route::ShopUnavailable(Shop::Decks)
        );
        assert_eq!(
            route_scan("CNF/O1", false, |_| true),
            Route::ShopUnavailable(Shop::Decks)
        );
    }

    #[test]
    fn hhv_numbers_route_to_hhv_only_with_the_bridge() {
        assert_eq!(
            route_scan("525768", true, |s| s == Shop::Hhv),
            Route::Shop(Shop::Hhv)
        );
        assert_eq!(
            route_scan("525768", true, |_| false),
            Route::ShopUnavailable(Shop::Hhv)
        );
        assert_eq!(route_scan("525768", false, |_| true), Route::Discogs);
    }

    #[test]
    fn other_codes_go_to_discogs() {
        assert_eq!(route_scan("24950", true, |_| true), Route::Discogs);
        assert_eq!(route_scan("35000000", true, |_| true), Route::Discogs);
        assert_eq!(route_scan("5012345678900", true, |_| true), Route::Discogs);
        assert_eq!(route_scan("abc", true, |_| true), Route::Discogs);
    }

    #[test]
    fn deejay_urls_are_fetched_by_the_app() {
        for bridge_on in [true, false] {
            assert_eq!(
                route_scan("https://deejay.de/__960305", bridge_on, |_| false),
                Route::Fetch(Shop::Deejay),
                "with or without the bridge"
            );
        }
        assert_eq!(
            route_scan(
                "HTTPS://WWW.DEEJAY.DE/GROOVE_MINORITY_EP__960305",
                true,
                |_| false
            ),
            Route::Fetch(Shop::Deejay)
        );
        // the number alone is still an hhv article (bridge on) or a Discogs id
        assert_eq!(
            route_scan("960305", true, |s| s == Shop::Hhv),
            Route::Shop(Shop::Hhv)
        );
        assert_eq!(route_scan("960305", false, |_| true), Route::Discogs);
    }

    #[test]
    fn only_decks_and_hhv_use_a_browser() {
        assert!(Shop::Decks.uses_browser() && Shop::Hhv.uses_browser());
        assert!(!Shop::Deejay.uses_browser());
        assert_eq!(Shop::BROWSER.len(), 2);
        assert_eq!(Shop::from_key("deejay"), Some(Shop::Deejay));
        assert_eq!(Shop::Deejay.name(), "deejay.de");
    }

    #[test]
    fn pages_are_parsed_by_their_shop() {
        let decks = include_str!("../tests/fixtures/decks_search_cnf_o1.html");
        let hhv = include_str!("../tests/fixtures/hhv_product_525768.html");
        let deejay = include_str!("../tests/fixtures/deejay_product_960305.html");
        assert_eq!(parse_page(Shop::Decks, decks).len(), 2);
        assert_eq!(parse_page(Shop::Hhv, hhv)[0].catno, "Kompakt Ex 095");
        assert_eq!(parse_page(Shop::Deejay, deejay)[0].catno, "GME-002");
        assert!(
            parse_page(Shop::Deejay, decks).is_empty(),
            "a page of another shop is not an article"
        );
        let entries = parse_page(Shop::Deejay, deejay);
        assert!(find_entry(Shop::Deejay, "https://deejay.de/__960305", &entries).is_some());
        assert!(find_entry(Shop::Deejay, "https://deejay.de/__1", &entries).is_none());
    }

    fn item(id: u32, title: &str) -> SearchItem {
        SearchItem {
            id: Some(id),
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn results_that_name_the_artist_come_first_and_alone() {
        let e = ShopEntry {
            artist: "Groove Minority".into(),
            ..entry()
        };
        let items = vec![
            item(1, "Graphics - There's A Way Back"),
            item(2, "Hank Crawford / Hubert Laws - Family"),
            item(3, "Groove Minority - EP 02"),
        ];
        let kept = prefer_artist(items, &e);
        assert_eq!(kept.iter().map(|i| i.id).collect::<Vec<_>>(), [Some(3)]);
    }

    #[test]
    fn matching_ignores_case_and_accents_and_handles_several_artists() {
        let e = ShopEntry {
            artist: "BJÖRK, Altone".into(),
            ..entry()
        };
        let kept = prefer_artist(
            vec![
                item(1, "Other - X"),
                item(2, "bjork - Vulnicura"),
                item(3, "Altone - Y"),
            ],
            &e,
        );
        assert_eq!(
            kept.iter().map(|i| i.id).collect::<Vec<_>>(),
            [Some(2), Some(3)]
        );
    }

    #[test]
    fn nothing_is_dropped_when_no_result_names_the_artist() {
        let e = ShopEntry {
            artist: "Various".into(),
            ..entry()
        };
        let items = vec![item(1, "A - One"), item(2, "B - Two")];
        assert_eq!(prefer_artist(items.clone(), &e).len(), 2);
        let no_artist = ShopEntry {
            artist: String::new(),
            ..entry()
        };
        assert_eq!(
            prefer_artist(items, &no_artist).len(),
            2,
            "without an artist there is nothing to compare"
        );
        assert!(prefer_artist(Vec::new(), &e).is_empty());
    }
}
