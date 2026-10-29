//! The "Search Collection" tab: browse and search the records in our own
//! database, and look up a scanned sticker there (never on Discogs).

use super::*;
use crate::collection::ListState;
use crate::library::StoredRelease;
use crate::worker::ScanCode;

const COLLECTION_PREFIX: &str = "db:";

fn thumb_key(id: u32) -> String {
    format!("{COLLECTION_PREFIX}thumb:{id}")
}

fn cover_key(id: u32) -> String {
    format!("{COLLECTION_PREFIX}cover:{id}")
}

/// "2017", "2017-03" or "2017-03-10", as far as the date is known.
pub(super) fn released_display(
    released_on: Option<&str>,
    precision: Option<&str>,
) -> Option<String> {
    let date = released_on?;
    let len = match precision? {
        "year" => 4,
        "month" => 7,
        _ => 10,
    };
    date.get(..len).map(str::to_string)
}

/// 225 -> "3:45", 3723 -> "1:02:03"
pub(super) fn duration_display(seconds: i32) -> String {
    let (h, m, s) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// What to tell the user about a scan in the collection tab, if the code can
/// not be one of our stickers (they hold a Discogs release id).
pub(super) fn scan_problem(code: &str) -> Option<String> {
    match ScanCode::classify(code) {
        Ok(ScanCode::ReleaseId(_)) => None,
        Ok(ScanCode::Barcode(_)) => Some(format!(
            "Cannot find {code}: barcodes are not stored here, scan the sticker on the sleeve"
        )),
        Err(_) => Some(format!(
            "Cannot find {code}: it is not a code of our stickers"
        )),
    }
}

impl RecordShelfApp {
    pub(super) fn on_tab_changed(&mut self) {
        if self.tab == Tab::Collection {
            self.collection.focus_search = true;
            self.load_collection_if_needed();
        }
    }

    /// Loads the list the first time, and again after the database changed.
    pub(super) fn load_collection_if_needed(&mut self) {
        if !self.library_ready() {
            return;
        }
        let c = &self.collection;
        if matches!(c.list, ListState::NotLoaded) || (c.stale && c.list != ListState::Loading) {
            self.request_collection_list();
        }
    }

    fn request_collection_list(&mut self) {
        self.collection.list = ListState::Loading;
        self.collection.stale = false;
        self.worker.send(Request::Library(LibraryRequest::List));
    }

    /// The database changed (a release was stored or removed).
    pub(super) fn collection_changed(&mut self) {
        if self.collection.list == ListState::Loading {
            self.collection.reload_pending = true;
        } else {
            self.collection.stale = true;
            if self.tab == Tab::Collection {
                self.load_collection_if_needed();
            }
        }
    }

    /// A scan while this tab is open: look the sticker up in our own database.
    pub(super) fn scan_in_collection(&mut self, code: String) {
        if !self.library_usable() {
            return;
        }
        if let Some(problem) = scan_problem(&code) {
            self.set_error(problem);
            return;
        }
        let Ok(ScanCode::ReleaseId(id)) = ScanCode::classify(&code) else {
            return;
        };
        self.set_info(format!("Looking up {id} in your collection..."));
        self.worker.send(Request::Library(LibraryRequest::Details {
            id,
            for_scan: true,
        }));
    }

    pub(super) fn handle_collection_response(&mut self, response: LibraryResponse) {
        match response {
            LibraryResponse::List(Ok(rows)) => {
                // pictures may have changed with the data
                self.images
                    .retain(|key, _| !key.starts_with(COLLECTION_PREFIX));
                self.collection.thumbs_requested.clear();
                self.collection.cover_requested.clear();
                self.collection.set_rows(rows);
                if std::mem::take(&mut self.collection.reload_pending) {
                    self.collection.stale = true;
                    self.load_collection_if_needed();
                }
            }
            LibraryResponse::List(Err(err)) => {
                self.collection.list = ListState::Failed(err.clone());
                self.set_error(format!("Database: {err}"));
            }
            LibraryResponse::Details {
                id,
                for_scan,
                result,
            } => match result {
                Ok(Some(record)) => {
                    if for_scan {
                        self.collection.show_only(id);
                        self.notice = None;
                    }
                    if self.collection.selected_id == Some(id) {
                        self.collection.details = Some(record);
                    }
                    if self.collection.details_requested == Some(id) {
                        self.collection.details_requested = None;
                    }
                }
                Ok(None) => {
                    if self.collection.details_requested == Some(id) {
                        self.collection.details_requested = None;
                    }
                    if for_scan {
                        self.set_error(format!(
                            "Not in your collection: release {id} was not found"
                        ));
                    }
                }
                Err(err) => self.set_error(format!("Database: {err}")),
            },
            LibraryResponse::Image { id, result } => {
                let state = match result {
                    Ok(Some(bytes)) => ImageState::Ready(bytes.into()),
                    _ => ImageState::Failed,
                };
                self.images.insert(cover_key(id), state);
            }
            LibraryResponse::Thumbs { ids, result } => match result {
                Ok(found) => {
                    let found_ids: HashSet<u32> = found.iter().map(|(id, _)| *id).collect();
                    for (id, bytes) in found {
                        self.images
                            .insert(thumb_key(id), ImageState::Ready(bytes.into()));
                    }
                    for id in ids.into_iter().filter(|id| !found_ids.contains(id)) {
                        self.images.insert(thumb_key(id), ImageState::Failed);
                    }
                }
                Err(err) => {
                    for id in ids {
                        self.images.insert(thumb_key(id), ImageState::Failed);
                    }
                    self.set_error(format!("Database: {err}"));
                }
            },
            _ => {}
        }
    }

    /// Asks for the details (and cover) of the selected record once.
    fn ensure_collection_details(&mut self) {
        let Some(id) = self.collection.selected_id else {
            return;
        };
        let have = self.collection.details.as_ref().is_some_and(|d| d.id == id);
        if !have && self.collection.details_requested != Some(id) {
            self.collection.details_requested = Some(id);
            self.worker.send(Request::Library(LibraryRequest::Details {
                id,
                for_scan: false,
            }));
        }
        let has_cover = self.collection.selected_row().is_some_and(|r| r.has_cover);
        if has_cover && self.collection.cover_requested.insert(id) {
            self.images.insert(cover_key(id), ImageState::Loading);
            self.worker
                .send(Request::Library(LibraryRequest::Image(id)));
        }
    }

    fn print_stored(&mut self) {
        let Some(record) = self
            .collection
            .details
            .as_ref()
            .filter(|d| Some(d.id) == self.collection.selected_id)
        else {
            self.set_info("The record is still loading; press P again".to_string());
            return;
        };
        if self.settings.printer_host.trim().is_empty() {
            self.set_error("No printer configured (printer_host in the settings file)".to_string());
            return;
        }
        let content = LabelContent::from_stored(record);
        let job = self.print_job(content);
        self.worker.send(Request::PrintLabel(Box::new(job)));
        self.set_info("Printing label...".to_string());
    }

    fn open_stored(&mut self) {
        let Some(id) = self.collection.selected_id else {
            self.set_error("Select a record first".to_string());
            return;
        };
        let uri = self
            .collection
            .details
            .as_ref()
            .filter(|d| d.id == id)
            .and_then(|d| d.discogs_uri.clone());
        let url = controls::release_url(uri.as_deref(), id);
        match open::that(&url) {
            Ok(()) => self.set_info(format!("Opened {url}")),
            Err(err) => self.set_error(format!("Could not open {url}: {err}")),
        }
    }

    fn handle_collection_keys(&mut self, ctx: &egui::Context) {
        let pressed = |key| ctx.input(|i| i.key_pressed(key) && !i.modifiers.command);
        if pressed(egui::Key::Escape) {
            self.collection.clear();
            self.collection.focus_search = true;
        }
        if self
            .collection
            .step(pressed(egui::Key::ArrowUp), pressed(egui::Key::ArrowDown))
        {
            self.collection.details = None;
        }
        if pressed(Command::Print.key()) {
            self.print_stored();
        }
        if pressed(Command::Open.key()) {
            self.open_stored();
        }
        if pressed(Command::Quit.key()) {
            self.run_command(Command::Quit);
        }
    }

    pub(super) fn ui_collection(&mut self, ui: &mut egui::Ui) {
        match self.library_link.clone() {
            LibraryLink::Off => {
                ui.label("No database configured. Run db/db.sh bootstrap, or set database_url in the settings.");
                return;
            }
            LibraryLink::Connecting => {
                ui.label("Connecting to the database...");
                return;
            }
            LibraryLink::Failed(msg) => {
                ui.colored_label(
                    Color32::RED,
                    format!("The database is not available: {msg}"),
                );
                return;
            }
            LibraryLink::Ready => {}
        }
        self.load_collection_if_needed();

        // search
        let focus = std::mem::take(&mut self.collection.focus_search);
        let mut search_active = false;
        ui.horizontal(|ui| {
            ui.label("Search:");
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.collection.search)
                    .desired_width(f32::INFINITY)
                    .hint_text("artist, title, label, catalog number, track, year..."),
            );
            if focus {
                response.request_focus();
            }
            search_active = response.has_focus();
            if response.changed() {
                self.collection.refilter();
                self.collection.details = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.collection.count_text()).small());
            if self.collection.only.is_some() {
                ui.colored_label(Color32::LIGHT_BLUE, "Scanned record only");
                if ui.button("Show all  [Esc]").clicked() {
                    self.collection.clear();
                }
            }
        });

        if !search_active {
            self.handle_collection_keys(ui.ctx());
        }

        match &self.collection.list {
            ListState::NotLoaded | ListState::Loading => {
                ui.label("Loading records...");
                return;
            }
            ListState::Failed(msg) => {
                ui.colored_label(Color32::RED, msg.clone());
                if ui.button("Try again").clicked() {
                    self.collection.list = ListState::NotLoaded;
                }
                return;
            }
            ListState::Loaded => {}
        }
        if self.collection.rows.is_empty() {
            ui.label("No records in the library yet. Records you add on the \"Add records\" tab are stored here.");
            return;
        }
        if self.collection.filtered.is_empty() {
            ui.label("No record matches.");
            return;
        }

        ui.separator();
        self.collection_list(ui);
        self.ensure_collection_details();
        ui.separator();
        self.collection_details(ui);
    }

    fn collection_list(&mut self, ui: &mut egui::Ui) {
        let total = self.collection.filtered.len();
        self.collection.row_height = THUMB_SIZE + ui.spacing().item_spacing.y;
        let mut area = egui::ScrollArea::vertical()
            .id_source("collection-list")
            .max_height(ui.available_height() * 0.55)
            .auto_shrink([false, true]);
        if let Some(offset) = self.collection.scroll_to.take() {
            area = area.vertical_scroll_offset(offset);
        }
        let mut clicked = None;
        let mut to_fetch: Vec<u32> = Vec::new();
        let output = area.show_rows(ui, THUMB_SIZE, total, |ui, range| {
            for position in range.clone() {
                let Some(row) = self.collection.row_at(position) else {
                    continue;
                };
                let selected = self.collection.selected_id == Some(row.id);
                let details = [
                    row.labels.clone(),
                    row.year.map(|y| y.to_string()).unwrap_or_default(),
                    row.format.clone(),
                    if row.copies > 1 {
                        format!("{}x", row.copies)
                    } else {
                        String::new()
                    },
                ]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" | ");
                let heading = if row.artists.is_empty() {
                    row.title.clone()
                } else {
                    format!("{} - {}", row.artists, row.title)
                };
                ui.horizontal(|ui| {
                    if row.has_cover {
                        if !draw_image(ui, &self.images, &thumb_key(row.id), THUMB_SIZE) {
                            to_fetch.push(row.id);
                        }
                    } else {
                        draw_placeholder(ui, THUMB_SIZE);
                    }
                    if ui
                        .selectable_label(selected, format!("{heading}\n{details}"))
                        .clicked()
                    {
                        clicked = Some(row.id);
                    }
                });
            }
            (range.start, range.len())
        });
        let (first, visible) = output.inner;
        self.collection.first_visible = first;
        self.collection.visible_rows = visible.max(1);
        if let Some(id) = clicked {
            self.collection.selected_id = Some(id);
            self.collection.details = None;
        }
        if !to_fetch.is_empty() {
            for id in &to_fetch {
                self.images.insert(thumb_key(*id), ImageState::Loading);
            }
            self.worker
                .send(Request::Library(LibraryRequest::Thumbs(to_fetch)));
        }
    }

    fn collection_details(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        ui.horizontal(|ui| {
            if ui.button("Print label  [P]").clicked() {
                action = Some(Command::Print);
            }
            if ui.button("Open in browser  [O]").clicked() {
                action = Some(Command::Open);
            }
            if ui.button("Quit  [Q]").clicked() {
                action = Some(Command::Quit);
            }
            ui.label(RichText::new("Up/Down: browse   Esc: clear the search").small());
        });
        match action {
            Some(Command::Print) => self.print_stored(),
            Some(Command::Open) => self.open_stored(),
            Some(Command::Quit) => self.run_command(Command::Quit),
            _ => {}
        }
        let selected = self.collection.selected_id;
        egui::ScrollArea::vertical()
            .id_source("collection-details")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let Some(record) = self
                    .collection
                    .details
                    .as_ref()
                    .filter(|d| Some(d.id) == selected)
                else {
                    if selected.is_some() {
                        ui.horizontal_top(|ui| {
                            draw_placeholder(ui, COVER_SIZE);
                            ui.label("Loading...");
                        });
                    }
                    return;
                };
                ui.horizontal_top(|ui| {
                    if self.collection.selected_row().is_some_and(|r| r.has_cover) {
                        draw_image(ui, &self.images, &cover_key(record.id), COVER_SIZE);
                    } else {
                        draw_placeholder(ui, COVER_SIZE);
                    }
                    ui.vertical(|ui| render_stored(ui, record));
                });
            });
    }
}

/// The details of a record from our database.
fn render_stored(ui: &mut egui::Ui, record: &StoredRelease) {
    ui.label(RichText::new(format!("Title: {}", record.title)).strong());
    let artists = record.artist_display();
    if !artists.is_empty() {
        ui.label(RichText::new(format!("Artist: {artists}")).strong());
    }
    if !record.labels.is_empty() {
        let labels = record
            .labels
            .iter()
            .map(|(name, catno)| {
                if catno.is_empty() {
                    name.clone()
                } else {
                    format!("{name} ({catno})")
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        ui.label(RichText::new(format!("Label: {labels}")).strong());
    }
    if let Some(date) = released_display(
        record.released_on.as_deref(),
        record.released_precision.as_deref(),
    ) {
        ui.label(RichText::new(format!("Released: {date}")).strong());
    }
    let format = record.format_display();
    if !format.is_empty() {
        ui.label(RichText::new(format!("Format: {format}")).strong());
    }
    ui.label(RichText::new(format!("Copies: {}", record.copies)).strong());
    ui.add_space(6.0);
    if let Some(country) = &record.country {
        ui.label(RichText::new(format!("Country: {country}")).small());
    }
    if !record.genres.is_empty() {
        ui.label(RichText::new(format!("Genres: {}", record.genres.join(", "))).small());
    }
    if !record.styles.is_empty() {
        ui.label(RichText::new(format!("Styles: {}", record.styles.join(", "))).small());
    }
    if let Some(notes) = &record.notes {
        ui.label(RichText::new(format!("Notes: {notes}")).small());
    }
    if !record.tracks.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new("Tracklist:").strong());
        for track in &record.tracks {
            let duration = track
                .duration_seconds
                .map(|d| format!(" [{}]", duration_display(d)))
                .unwrap_or_default();
            ui.label(
                RichText::new(format!(
                    "  {} {}{duration}",
                    track.position.as_deref().unwrap_or(""),
                    track.title
                ))
                .small(),
            );
        }
    }
    if !record.credits.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new("Credits:").strong());
        for (role, name, tracks) in &record.credits {
            let on = tracks
                .as_deref()
                .map(|t| format!(" ({t})"))
                .unwrap_or_default();
            ui.label(RichText::new(format!("  {role}: {name}{on}")).small());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{RecordRow, StoredFormat, StoredTrack};
    use crate::ui::tests::app_with_results;

    fn row(id: u32, artists: &str, title: &str, has_cover: bool) -> RecordRow {
        RecordRow {
            id,
            title: title.into(),
            artists: artists.into(),
            labels: "Label (CAT 1)".into(),
            year: Some(2017),
            format: "12\", 33 RPM".into(),
            copies: if id == 2 { 2 } else { 1 },
            has_cover,
            haystack: crate::catalogue::fold(&format!("{title} {artists}")),
        }
    }

    fn stored(id: u32) -> Box<StoredRelease> {
        Box::new(StoredRelease {
            id,
            master_id: None,
            title: format!("Title {id}"),
            released_on: Some("2017-03-01".into()),
            released_precision: Some("month".into()),
            year: Some(2017),
            country: Some("DE".into()),
            genres: vec!["Electronic".into()],
            styles: vec!["Techno".into()],
            notes: Some("Pressed in DE".into()),
            discogs_uri: Some("https://www.discogs.com/release/1".into()),
            copies: 2,
            artists: vec![("Altone".into(), Some("&".into())), ("Björk".into(), None)],
            labels: vec![("Label".into(), "CAT 1".into())],
            formats: vec![StoredFormat {
                qty: 1,
                size: Some(12),
                rpm: Some(33),
                channels: "stereo".into(),
                weight_g: None,
                text: None,
                descriptions: vec!["EP".into()],
            }],
            tracks: vec![
                StoredTrack {
                    position: Some("A1".into()),
                    title: "One".into(),
                    duration_seconds: Some(225),
                },
                StoredTrack {
                    position: None,
                    title: "Two".into(),
                    duration_seconds: None,
                },
            ],
            credits: vec![("Producer".into(), "Pete".into(), Some("A1".into()))],
        })
    }

    fn app() -> RecordShelfApp {
        let mut app = app_with_results(0);
        app.library_link = LibraryLink::Ready;
        app.tab = Tab::Collection;
        app
    }

    fn loaded_app() -> RecordShelfApp {
        let mut app = app();
        app.collection.set_rows(vec![
            row(1, "Altone", "Wonderscape", true),
            row(2, "Björk", "Vulnicura", false),
            row(3, "Laurent Garnier", "Speicher 95", true),
        ]);
        app
    }

    fn render(app: &mut RecordShelfApp) {
        let ctx = app.repaint.clone();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.ui_collection(ui));
        });
    }

    #[test]
    fn small_helpers() {
        assert_eq!(
            released_display(Some("2017-03-10"), Some("day")).as_deref(),
            Some("2017-03-10")
        );
        assert_eq!(
            released_display(Some("2017-03-01"), Some("month")).as_deref(),
            Some("2017-03")
        );
        assert_eq!(
            released_display(Some("2017-01-01"), Some("year")).as_deref(),
            Some("2017")
        );
        assert_eq!(released_display(None, None), None);
        assert_eq!(released_display(Some("2017-03-01"), None), None);
        assert_eq!(duration_display(225), "3:45");
        assert_eq!(duration_display(5), "0:05");
        assert_eq!(duration_display(3723), "1:02:03");
        assert_eq!(duration_display(0), "0:00");
    }

    #[test]
    fn only_codes_of_our_stickers_are_looked_up() {
        assert_eq!(scan_problem("32474433"), None);
        assert_eq!(
            scan_problem("525768"),
            None,
            "a 6 digit number is a release id here, not an hhv article"
        );
        assert!(
            scan_problem("5012345678900")
                .unwrap()
                .contains("barcodes are not stored")
        );
        assert!(
            scan_problem("CNF/O1")
                .unwrap()
                .contains("not a code of our stickers")
        );
        assert!(scan_problem("").unwrap().contains("Cannot find"));
    }

    #[test]
    fn opening_the_tab_loads_the_list_once() {
        let mut app = app();
        let before = app.worker.pending();
        app.on_tab_changed();
        assert_eq!(app.collection.list, ListState::Loading);
        assert_eq!(app.worker.pending(), before + 1);
        assert!(app.collection.focus_search);
        app.on_tab_changed();
        app.load_collection_if_needed();
        assert_eq!(app.worker.pending(), before + 1, "already loading");
    }

    #[test]
    fn nothing_is_loaded_without_a_database() {
        let mut app = app();
        app.library_link = LibraryLink::Off;
        let before = app.worker.pending();
        app.on_tab_changed();
        assert_eq!(app.worker.pending(), before);
        assert_eq!(app.collection.list, ListState::NotLoaded);
    }

    #[test]
    fn the_list_arrives_and_pictures_are_refreshed() {
        let mut app = app();
        app.images
            .insert("db:thumb:1".into(), ImageState::Ready(vec![1].into()));
        app.images.insert(
            "https://i.discogs.com/x.jpg".into(),
            ImageState::Ready(vec![2].into()),
        );
        app.collection.thumbs_requested.insert(1);
        app.handle_library_response(LibraryResponse::List(Ok(vec![row(1, "A", "T", true)])));
        assert_eq!(app.collection.list, ListState::Loaded);
        assert_eq!(app.collection.rows.len(), 1);
        assert!(
            !app.images.contains_key("db:thumb:1"),
            "database pictures are fetched again"
        );
        assert!(
            app.images.contains_key("https://i.discogs.com/x.jpg"),
            "Discogs pictures are not touched"
        );
        assert!(app.collection.thumbs_requested.is_empty());
    }

    #[test]
    fn a_failed_list_is_reported() {
        let mut app = app();
        app.handle_library_response(LibraryResponse::List(Err("connection lost".into())));
        assert_eq!(
            app.collection.list,
            ListState::Failed("connection lost".into())
        );
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));
    }

    #[test]
    fn a_sync_makes_the_list_stale() {
        let synced = || LibraryResponse::Synced {
            release_id: 1,
            result: Ok(SyncOutcome::Saved {
                created: true,
                copies: 1,
            }),
        };
        // on the other tab: marked, loaded when the tab opens
        let mut app = loaded_app();
        app.tab = Tab::Add;
        let before = app.worker.pending();
        app.handle_library_response(synced());
        assert!(app.collection.stale);
        assert_eq!(
            app.worker.pending(),
            before,
            "nothing is loaded for a tab nobody looks at"
        );
        app.tab = Tab::Collection;
        app.on_tab_changed();
        assert_eq!(app.collection.list, ListState::Loading);

        // while the tab is open: loaded at once
        let mut app = loaded_app();
        let before = app.worker.pending();
        app.handle_library_response(synced());
        assert_eq!(app.worker.pending(), before + 1);

        // while a load is running: once more afterwards
        let mut app = app_with_library_tab_loading();
        app.handle_library_response(synced());
        assert!(app.collection.reload_pending);
        let before = app.worker.pending();
        app.handle_library_response(LibraryResponse::List(Ok(vec![row(1, "A", "T", false)])));
        assert!(!app.collection.reload_pending);
        assert_eq!(app.worker.pending(), before + 1, "the list is loaded again");
        // a failed sync changes nothing
        let mut app = loaded_app();
        let before = app.worker.pending();
        app.handle_library_response(LibraryResponse::Synced {
            release_id: 1,
            result: Err("x".into()),
        });
        assert_eq!(app.worker.pending(), before);
    }

    fn app_with_library_tab_loading() -> RecordShelfApp {
        let mut app = app();
        app.on_tab_changed();
        app
    }

    #[test]
    fn a_scan_asks_the_database_only() {
        let mut app = loaded_app();
        let before = app.worker.pending();
        app.scan_in_collection("32474433".into());
        assert_eq!(app.worker.pending(), before + 1);
        assert!(
            app.collection.only.is_none(),
            "the list changes when the answer arrives"
        );
    }

    #[test]
    fn scans_that_cannot_be_ours_are_refused_without_asking_anyone() {
        for (code, expected) in [
            ("5012345678900", "barcodes are not stored"),
            ("CNF/O1", "not a code of our stickers"),
            ("garbage", "not a code of our stickers"),
        ] {
            let mut app = loaded_app();
            let before = app.worker.pending();
            app.scan_in_collection(code.into());
            assert_eq!(app.worker.pending(), before, "{code}");
            assert!(
                app.notice
                    .as_ref()
                    .is_some_and(|n| n.is_error && n.text.contains(expected)),
                "{code}"
            );
        }
    }

    #[test]
    fn a_scan_without_a_database_says_so() {
        let mut app = loaded_app();
        app.library_link = LibraryLink::Off;
        let before = app.worker.pending();
        app.scan_in_collection("32474433".into());
        assert_eq!(app.worker.pending(), before);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("database"))
        );
    }

    #[test]
    fn a_found_sticker_shows_only_that_record() {
        let mut app = loaded_app();
        app.set_info("Looking up 3 in your collection...".to_string());
        app.handle_library_response(LibraryResponse::Details {
            id: 3,
            for_scan: true,
            result: Ok(Some(stored(3))),
        });
        assert_eq!(app.collection.only, Some(3));
        assert_eq!(app.collection.filtered.len(), 1);
        assert_eq!(app.collection.selected_id, Some(3));
        assert_eq!(app.collection.details.as_ref().map(|d| d.id), Some(3));
        assert!(app.notice.is_none(), "the lookup message is gone");
        // Esc brings the whole list back
        app.collection.clear();
        assert_eq!(app.collection.filtered.len(), 3);
    }

    #[test]
    fn an_unknown_sticker_gives_a_message_and_leaves_the_list_alone() {
        let mut app = loaded_app();
        app.collection.search = "bjork".into();
        app.collection.refilter();
        app.handle_library_response(LibraryResponse::Details {
            id: 999,
            for_scan: true,
            result: Ok(None),
        });
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error
            && n.text.contains("Not in your collection")
            && n.text.contains("999")));
        assert_eq!(app.collection.only, None);
        assert_eq!(app.collection.search, "bjork");
        assert_eq!(app.collection.filtered.len(), 1);
    }

    #[test]
    fn a_sticker_scanned_before_the_list_is_loaded_still_works() {
        let mut app = app();
        app.handle_library_response(LibraryResponse::Details {
            id: 2,
            for_scan: true,
            result: Ok(Some(stored(2))),
        });
        assert_eq!(app.collection.only, Some(2));
        app.handle_library_response(LibraryResponse::List(Ok(vec![
            row(1, "A", "One", false),
            row(2, "B", "Two", false),
        ])));
        assert_eq!(
            app.collection.filtered.len(),
            1,
            "the scan view survives the list arriving"
        );
        assert_eq!(app.collection.selected_id, Some(2));
    }

    #[test]
    fn details_for_a_record_that_is_no_longer_selected_are_dropped() {
        let mut app = loaded_app();
        app.collection.selected_id = Some(1);
        app.handle_library_response(LibraryResponse::Details {
            id: 3,
            for_scan: false,
            result: Ok(Some(stored(3))),
        });
        assert!(app.collection.details.is_none());
        app.handle_library_response(LibraryResponse::Details {
            id: 1,
            for_scan: false,
            result: Ok(Some(stored(1))),
        });
        assert_eq!(app.collection.details.as_ref().map(|d| d.id), Some(1));
    }

    #[test]
    fn details_and_cover_of_the_selection_are_requested_once() {
        let mut app = loaded_app(); // record 1 is selected and has a cover
        let before = app.worker.pending();
        app.ensure_collection_details();
        assert_eq!(
            app.worker.pending(),
            before + 1,
            "the details; the cover is not counted as work"
        );
        assert_eq!(app.collection.details_requested, Some(1));
        assert!(matches!(
            app.images.get("db:cover:1"),
            Some(ImageState::Loading)
        ));
        app.ensure_collection_details();
        assert_eq!(app.worker.pending(), before + 1, "asked only once");
        // a record without a cover has nothing to fetch
        app.collection.selected_id = Some(2);
        app.ensure_collection_details();
        assert!(!app.images.contains_key("db:cover:2"));
    }

    #[test]
    fn pictures_from_the_database_are_stored_by_key() {
        let mut app = loaded_app();
        app.handle_library_response(LibraryResponse::Thumbs {
            ids: vec![1, 3, 4],
            result: Ok(vec![(1, vec![1, 2]), (3, vec![3])]),
        });
        assert!(matches!(
            app.images.get("db:thumb:1"),
            Some(ImageState::Ready(_))
        ));
        assert!(matches!(
            app.images.get("db:thumb:3"),
            Some(ImageState::Ready(_))
        ));
        assert!(
            matches!(app.images.get("db:thumb:4"), Some(ImageState::Failed)),
            "not found: do not ask again"
        );
        app.handle_library_response(LibraryResponse::Thumbs {
            ids: vec![5],
            result: Err("lost".into()),
        });
        assert!(matches!(
            app.images.get("db:thumb:5"),
            Some(ImageState::Failed)
        ));
        app.handle_library_response(LibraryResponse::Image {
            id: 1,
            result: Ok(Some(vec![9, 9])),
        });
        assert!(matches!(
            app.images.get("db:cover:1"),
            Some(ImageState::Ready(_))
        ));
        app.handle_library_response(LibraryResponse::Image {
            id: 2,
            result: Ok(None),
        });
        assert!(matches!(
            app.images.get("db:cover:2"),
            Some(ImageState::Failed)
        ));
    }

    #[test]
    fn a_stored_record_can_be_printed() {
        let mut app = loaded_app();
        app.settings.printer_host = "127.0.0.1".into();
        // details not there yet
        let before = app.worker.pending();
        app.print_stored();
        assert_eq!(app.worker.pending(), before);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("still loading"))
        );
        // loaded
        app.collection.selected_id = Some(1);
        app.collection.details = Some(stored(1));
        app.print_stored();
        assert_eq!(app.worker.pending(), before + 1);
        // details of another record do not count
        app.collection.selected_id = Some(2);
        let before = app.worker.pending();
        app.print_stored();
        assert_eq!(app.worker.pending(), before);
        // no printer
        app.collection.selected_id = Some(1);
        app.settings.printer_host = String::new();
        app.print_stored();
        assert_eq!(app.worker.pending(), before);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("printer"))
        );
    }

    #[test]
    fn the_label_of_a_stored_record_has_what_the_sleeve_shows() {
        let content = LabelContent::from_stored(&stored(1));
        assert_eq!(content.artist, "Altone & Björk");
        assert_eq!(
            (content.label.as_str(), content.catno.as_str()),
            ("Label", "CAT 1")
        );
        assert_eq!(content.size_inch, Some(12));
        assert_eq!(content.tracks.len(), 2);
        assert_eq!(content.tracks[1].position, "", "a track without a position");
    }

    #[test]
    fn every_state_of_the_tab_can_be_drawn() {
        // no database / connecting / failed
        for link in [
            LibraryLink::Off,
            LibraryLink::Connecting,
            LibraryLink::Failed("refused".into()),
        ] {
            let mut app = app();
            app.library_link = link;
            render(&mut app);
        }
        // loading, failed list, empty library, nothing matching
        let mut app = app();
        render(&mut app);
        app.collection.list = ListState::Failed("x".into());
        render(&mut app);
        app.collection.set_rows(Vec::new());
        render(&mut app);
        let mut app = loaded_app();
        app.collection.search = "zzzz".into();
        app.collection.refilter();
        render(&mut app);
    }

    #[test]
    fn the_list_and_the_details_can_be_drawn() {
        let mut app = loaded_app();
        render(&mut app);
        // the visible rows with a cover ask for their thumbnails
        assert!(matches!(
            app.images.get("db:thumb:1"),
            Some(ImageState::Loading)
        ));
        assert!(matches!(
            app.images.get("db:thumb:3"),
            Some(ImageState::Loading)
        ));
        assert!(
            !app.images.contains_key("db:thumb:2"),
            "no cover, nothing to fetch"
        );
        assert!(app.collection.visible_rows >= 1);
        // with pictures, details and a scan view
        let png = {
            let img = image::RgbImage::from_fn(40, 40, |x, y| image::Rgb([x as u8, y as u8, 9]));
            let mut bytes = std::io::Cursor::new(Vec::new());
            img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
            bytes.into_inner()
        };
        app.images
            .insert("db:thumb:1".into(), ImageState::Ready(png.clone().into()));
        app.images
            .insert("db:cover:1".into(), ImageState::Ready(png.into()));
        app.collection.details = Some(stored(1));
        render(&mut app);
        render(&mut app);
        app.collection.show_only(1);
        render(&mut app);
    }

    #[test]
    fn a_long_list_is_drawn_lazily() {
        let mut app = app();
        app.collection.set_rows(
            (0..5000)
                .map(|i| row(i, "Artist", &format!("Title {i}"), true))
                .collect(),
        );
        render(&mut app);
        let asked = app
            .images
            .keys()
            .filter(|k| k.starts_with("db:thumb:"))
            .count();
        assert!(
            asked > 0 && asked < 100,
            "only the visible rows fetch their thumbnail, not {asked}"
        );
    }

    #[test]
    fn the_add_tab_is_unaffected_by_scans_elsewhere() {
        // scans in the add tab still go through the shop / Discogs routing
        let mut app = app_with_results(0);
        assert_eq!(app.tab, Tab::Add);
        let before = app.worker.pending();
        app.route_scan("5012345678900".into());
        assert_eq!(app.worker.pending(), before + 1);
    }
}
