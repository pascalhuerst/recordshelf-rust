//! Record shops whose labels we can scan (decks.de, hhv.de).
//!
//! Their barcodes are internal codes, not UPC/EAN. The shop's own page maps a
//! code to the release data (catalog number, label, sometimes the real
//! barcode), which Discogs can then be searched with. The pages can only be
//! fetched by a real browser (see `shop_bridge`); the parsing lives in
//! `decks` and `hhv`.

use crate::{decks, discogs::SearchQuery, hhv};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shop {
    Decks,
    Hhv,
}

impl Shop {
    pub const ALL: [Shop; 2] = [Shop::Decks, Shop::Hhv];

    /// Identifier used in the bridge API.
    pub fn key(self) -> &'static str {
        match self {
            Shop::Decks => "decks",
            Shop::Hhv => "hhv",
        }
    }

    /// Name for messages.
    pub fn name(self) -> &'static str {
        match self {
            Shop::Decks => "decks.de",
            Shop::Hhv => "hhv.de",
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

/// Where a scanned code goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Look it up in the shop through the browser.
    Shop(Shop),
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
}
