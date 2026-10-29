//! State of the "Search Collection" tab: the list of all records in our own
//! database, the search, and the selection. Pure logic; drawing and database
//! requests are in `ui`.

use std::collections::HashSet;

use crate::catalogue::fold;
use crate::library::{RecordRow, StoredRelease};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListState {
    NotLoaded,
    Loading,
    Loaded,
    Failed(String),
}

pub struct Collection {
    pub rows: Vec<RecordRow>,
    pub list: ListState,
    /// The database changed since the list was loaded.
    pub stale: bool,
    pub search: String,
    pub focus_search: bool,
    /// After scanning a sticker only that record is shown.
    pub only: Option<u32>,
    /// Indices into `rows` of what the search lets through, in list order.
    pub filtered: Vec<usize>,
    pub selected_id: Option<u32>,
    pub details: Option<Box<StoredRelease>>,
    pub details_requested: Option<u32>,
    /// First row on screen and number of rows on screen, from the last frame.
    pub first_visible: usize,
    pub visible_rows: usize,
    pub row_height: f32,
    /// Scroll offset to apply in the next frame (set when the selection moves).
    pub scroll_to: Option<f32>,
    pub thumbs_requested: HashSet<u32>,
    pub cover_requested: HashSet<u32>,
    /// The database changed while the list was loading: load it again afterwards.
    pub reload_pending: bool,
}

impl Default for Collection {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            list: ListState::NotLoaded,
            stale: false,
            search: String::new(),
            focus_search: true,
            only: None,
            filtered: Vec::new(),
            selected_id: None,
            details: None,
            details_requested: None,
            first_visible: 0,
            visible_rows: 8,
            row_height: 64.0,
            scroll_to: None,
            thumbs_requested: HashSet::new(),
            cover_requested: HashSet::new(),
            reload_pending: false,
        }
    }
}

/// Whether every word of the search occurs in the record's searchable text
/// (`haystack` is already folded).
pub fn matches(haystack: &str, terms: &[String]) -> bool {
    terms.iter().all(|t| haystack.contains(t.as_str()))
}

pub fn search_terms(search: &str) -> Vec<String> {
    fold(search)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

impl Collection {
    /// Takes a freshly loaded list. The selection stays if that record is still there.
    pub fn set_rows(&mut self, rows: Vec<RecordRow>) {
        self.rows = rows;
        self.list = ListState::Loaded;
        self.stale = false;
        self.refilter();
    }

    /// Applies the search (and the scan filter) to the list.
    pub fn refilter(&mut self) {
        let terms = search_terms(&self.search);
        self.filtered = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| self.only.is_none_or(|id| r.id == id) && matches(&r.haystack, &terms))
            .map(|(i, _)| i)
            .collect();
        // keep the selection if it is still listed, else take the first record
        let still_there = self
            .selected_id
            .is_some_and(|id| self.filtered.iter().any(|i| self.rows[*i].id == id));
        if !still_there {
            self.selected_id = self.filtered.first().map(|i| self.rows[*i].id);
            self.scroll_to = Some(0.0);
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected_id?;
        self.filtered.iter().position(|i| self.rows[*i].id == id)
    }

    pub fn selected_row(&self) -> Option<&RecordRow> {
        self.selected_index()
            .map(|pos| &self.rows[self.filtered[pos]])
    }

    pub fn row_at(&self, position: usize) -> Option<&RecordRow> {
        self.filtered.get(position).map(|i| &self.rows[*i])
    }

    /// Moves the selection one row (clamped) and makes sure it is on screen.
    /// Returns true if the selection changed.
    pub fn step(&mut self, up: bool, down: bool) -> bool {
        if self.filtered.is_empty() {
            return false;
        }
        let current = self.selected_index();
        let next = match (current, up, down) {
            (None, _, true) => 0,
            (Some(i), true, false) => i.saturating_sub(1),
            (Some(i), false, true) => (i + 1).min(self.filtered.len() - 1),
            _ => return false,
        };
        if Some(next) == current {
            return false;
        }
        self.selected_id = Some(self.rows[self.filtered[next]].id);
        self.scroll_into_view(next);
        true
    }

    /// Scrolls so that the row at `position` is fully visible.
    pub fn scroll_into_view(&mut self, position: usize) {
        let visible = self.visible_rows.max(1);
        if position < self.first_visible {
            self.scroll_to = Some(position as f32 * self.row_height);
        } else if position + 1 > self.first_visible + visible {
            // the row becomes the last one on screen
            let first = (position + 1).saturating_sub(visible);
            self.scroll_to = Some(first as f32 * self.row_height);
        }
    }

    /// Esc: first leaves the scan view, then clears the search.
    /// Returns true if something was cleared.
    pub fn clear(&mut self) -> bool {
        if self.only.is_some() {
            self.only = None;
            self.refilter();
            true
        } else if !self.search.is_empty() {
            self.search.clear();
            self.refilter();
            true
        } else {
            false
        }
    }

    /// Shows only this record (a scanned sticker).
    pub fn show_only(&mut self, id: u32) {
        self.only = Some(id);
        self.search.clear();
        self.selected_id = Some(id);
        self.refilter();
        self.scroll_to = Some(0.0);
    }

    pub fn count_text(&self) -> String {
        if self.filtered.len() == self.rows.len() {
            format!("{} records", self.rows.len())
        } else {
            format!("{} of {} records", self.filtered.len(), self.rows.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u32, artists: &str, title: &str, extra: &str) -> RecordRow {
        RecordRow {
            id,
            title: title.into(),
            artists: artists.into(),
            labels: String::new(),
            year: None,
            format: String::new(),
            copies: 1,
            has_cover: false,
            haystack: fold(&format!("{title} {artists} {extra}")),
        }
    }

    fn sample() -> Collection {
        let mut c = Collection::default();
        c.set_rows(vec![
            row(
                1,
                "Altone",
                "Wonderscape Numinous",
                "dub techno pcss09 self replication",
            ),
            row(2, "Björk", "Vulnicura", "strings"),
            row(
                3,
                "Laurent Garnier",
                "Speicher 95 Tribute EP",
                "kompakt extra",
            ),
            row(4, "Altone", "Another One", "dub techno"),
        ]);
        c
    }

    fn ids(c: &Collection) -> Vec<u32> {
        c.filtered.iter().map(|i| c.rows[*i].id).collect()
    }

    #[test]
    fn everything_is_listed_without_a_search() {
        let c = sample();
        assert_eq!(ids(&c), [1, 2, 3, 4]);
        assert_eq!(c.count_text(), "4 records");
        assert_eq!(c.selected_id, Some(1), "the first record is selected");
    }

    #[test]
    fn every_word_must_match() {
        let mut c = sample();
        c.search = "altone dub".into();
        c.refilter();
        assert_eq!(ids(&c), [1, 4]);
        c.search = "altone numinous".into();
        c.refilter();
        assert_eq!(ids(&c), [1]);
        c.search = "altone garnier".into();
        c.refilter();
        assert!(ids(&c).is_empty());
        assert_eq!(c.count_text(), "0 of 4 records");
    }

    #[test]
    fn search_ignores_case_and_accents_and_matches_parts_of_words() {
        let mut c = sample();
        for (search, expected) in [
            ("BJORK", vec![2]),
            ("björk", vec![2]),
            ("bjö", vec![2]),
            ("pcss", vec![1]),
            ("TRIB", vec![3]),
            ("  dub   techno  ", vec![1, 4]),
        ] {
            c.search = search.into();
            c.refilter();
            assert_eq!(ids(&c), expected, "{search:?}");
        }
        c.search = "   ".into();
        c.refilter();
        assert_eq!(ids(&c).len(), 4, "blank means everything");
    }

    #[test]
    fn the_selection_survives_a_search_that_still_shows_it() {
        let mut c = sample();
        c.selected_id = Some(4);
        c.search = "dub".into();
        c.refilter();
        assert_eq!(c.selected_id, Some(4));
        c.search = "garnier".into();
        c.refilter();
        assert_eq!(
            c.selected_id,
            Some(3),
            "otherwise the first match is selected"
        );
        c.search = "nothing like it".into();
        c.refilter();
        assert_eq!(c.selected_id, None);
        assert_eq!(c.selected_index(), None);
    }

    #[test]
    fn arrows_move_one_row_and_stop_at_the_ends() {
        let mut c = sample();
        assert!(!c.step(true, false), "already at the top");
        assert!(c.step(false, true));
        assert_eq!(c.selected_id, Some(2));
        assert!(c.step(false, true));
        assert!(c.step(false, true));
        assert_eq!(c.selected_id, Some(4));
        assert!(!c.step(false, true), "already at the bottom");
        assert!(c.step(true, false));
        assert_eq!(c.selected_id, Some(3));
        assert!(!c.step(true, true), "both keys cancel out");
        assert!(!c.step(false, false));
    }

    #[test]
    fn down_without_a_selection_picks_the_first_row() {
        let mut c = sample();
        c.selected_id = None;
        assert!(c.step(false, true));
        assert_eq!(c.selected_id, Some(1));
        let mut empty = Collection::default();
        assert!(!empty.step(false, true));
    }

    #[test]
    fn the_list_scrolls_to_keep_the_selection_visible() {
        let mut c = Collection::default();
        c.set_rows(
            (0..100)
                .map(|i| row(i, "A", &format!("Title {i}"), ""))
                .collect(),
        );
        c.row_height = 10.0;
        c.visible_rows = 5;
        c.first_visible = 0;
        c.scroll_to = None;
        // moving within the screen does not scroll
        for _ in 0..4 {
            c.step(false, true);
        }
        assert_eq!(c.scroll_to, None);
        // one more row: the screen moves so that the selection is the last row
        c.step(false, true);
        assert_eq!(c.selected_index(), Some(5));
        assert_eq!(c.scroll_to, Some(10.0));
        // going back above the first visible row scrolls up to it
        c.first_visible = 3;
        c.scroll_to = None;
        c.selected_id = Some(3);
        c.step(true, false);
        assert_eq!(c.selected_index(), Some(2));
        assert_eq!(c.scroll_to, Some(20.0));
    }

    #[test]
    fn a_scanned_record_is_the_only_one_shown_until_esc() {
        let mut c = sample();
        c.search = "altone".into();
        c.refilter();
        c.show_only(3);
        assert_eq!(ids(&c), [3]);
        assert_eq!(c.search, "", "the search is cleared");
        assert_eq!(c.selected_id, Some(3));
        assert!(c.clear(), "Esc leaves the scan view");
        assert_eq!(ids(&c), [1, 2, 3, 4]);
        assert_eq!(c.selected_id, Some(3), "the scanned record stays selected");
        assert!(!c.clear(), "nothing left to clear");
    }

    #[test]
    fn esc_clears_the_search() {
        let mut c = sample();
        c.search = "garnier".into();
        c.refilter();
        assert!(c.clear());
        assert_eq!(c.search, "");
        assert_eq!(ids(&c).len(), 4);
    }

    #[test]
    fn a_scan_filter_for_an_unknown_id_shows_nothing() {
        let mut c = sample();
        c.show_only(999);
        assert!(ids(&c).is_empty());
        assert_eq!(c.selected_id, None);
        c.clear();
        assert_eq!(ids(&c).len(), 4);
    }

    #[test]
    fn a_reloaded_list_keeps_the_selection() {
        let mut c = sample();
        c.selected_id = Some(3);
        c.set_rows(vec![
            row(3, "Laurent Garnier", "Speicher 95 Tribute EP", ""),
            row(5, "New", "Record", ""),
        ]);
        assert_eq!(c.selected_id, Some(3));
        assert_eq!(c.list, ListState::Loaded);
        assert!(!c.stale);
        c.set_rows(vec![row(5, "New", "Record", "")]);
        assert_eq!(
            c.selected_id,
            Some(5),
            "the record is gone: the first one is selected"
        );
        c.set_rows(Vec::new());
        assert_eq!(c.selected_id, None);
        assert_eq!(c.count_text(), "0 records");
    }

    #[test]
    fn the_selected_row_can_be_read() {
        let mut c = sample();
        c.selected_id = Some(2);
        assert_eq!(
            c.selected_row().map(|r| r.title.as_str()),
            Some("Vulnicura")
        );
        assert_eq!(c.row_at(0).map(|r| r.id), Some(1));
        assert!(c.row_at(10).is_none());
    }
}
