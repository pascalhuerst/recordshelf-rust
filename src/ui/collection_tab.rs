//! The "Search Collection" tab: browse and search the records in our own
//! database, and look up a scanned sticker there (never on Discogs).

use super::*;
use crate::catalogue::join_separator;
use crate::collection::{ListState, NotesEditor};
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
            LibraryResponse::Notes { id, result } => match result {
                Ok(true) => {
                    self.set_info("Notes saved".to_string());
                    // show them, and make them searchable
                    if self.collection.selected_id == Some(id) {
                        self.collection.details = None;
                        self.collection.details_requested = None;
                    }
                    self.collection_changed();
                }
                Ok(false) => self.set_error(format!("Release {id} is not in the library any more")),
                Err(err) => self.set_error(format!("Notes not saved: {err}")),
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
        let content = LabelContent::from_stored(record);
        self.print_content(content, self.settings.tag_style());
    }

    /// D: ask before deleting the selected record from the library.
    fn ask_delete(&mut self) {
        let Some(row) = self.collection.selected_row() else {
            self.set_error("Select a record first".to_string());
            return;
        };
        let title = if row.artists.is_empty() {
            row.title.clone()
        } else {
            format!("{} - {}", row.artists, row.title)
        };
        self.collection.confirm_delete = Some((row.id, title));
    }

    fn confirm_delete(&mut self) {
        let Some((id, _)) = self.collection.confirm_delete.take() else {
            return;
        };
        self.library_busy.insert(id);
        self.worker
            .send(Request::Library(LibraryRequest::Sync(Box::new(
                SyncJob::Remove { release_id: id },
            ))));
    }

    /// E: edit our own notes for the selected record.
    fn start_edit(&mut self) {
        let Some(record) = self
            .collection
            .details
            .as_ref()
            .filter(|d| Some(d.id) == self.collection.selected_id)
        else {
            self.set_info("The record is still loading; press E again".to_string());
            return;
        };
        let title = match record.artist_display().as_str() {
            "" => record.title.clone(),
            artists => format!("{artists} - {}", record.title),
        };
        self.collection.editor = Some(NotesEditor {
            release_id: record.id,
            title,
            text: record.my_notes.clone().unwrap_or_default(),
            focus: true,
        });
    }

    fn save_notes(&mut self) {
        let Some(editor) = self.collection.editor.take() else {
            return;
        };
        self.worker.send(Request::Library(LibraryRequest::SetNotes {
            id: editor.release_id,
            notes: editor.text,
        }));
    }

    /// The delete confirmation and the notes editor.
    fn show_collection_dialogs(&mut self, ctx: &egui::Context) {
        let pressed = |key| ctx.input(|i| i.key_pressed(key));
        if let Some((_, title)) = self.collection.confirm_delete.clone() {
            let (mut yes, mut no) = (false, false);
            egui::Window::new("Delete from the library")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(format!("Delete \"{title}\" from the library?"));
                    ui.label(
                        RichText::new(
                            "Only our database is changed. The record stays in your Discogs collection, \
                             so auto sync may add it again.",
                        )
                        .small(),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        yes = ui.button("Delete  [Enter]").clicked();
                        no = ui.button("Cancel  [Esc]").clicked();
                    });
                });
            if yes || pressed(egui::Key::Enter) || pressed(egui::Key::Y) {
                self.confirm_delete();
            } else if no || pressed(egui::Key::Escape) || pressed(egui::Key::N) {
                self.collection.confirm_delete = None;
            }
            return;
        }
        if let Some(mut editor) = self.collection.editor.take() {
            let (mut save, mut cancel) = (false, false);
            egui::Window::new(format!("Notes: {}", editor.title))
                .collapsible(false)
                .default_width(440.0)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    let response = ui.add(
                        egui::TextEdit::multiline(&mut editor.text)
                            .desired_rows(6)
                            .desired_width(f32::INFINITY)
                            .hint_text("Your own notes about this record"),
                    );
                    if std::mem::take(&mut editor.focus) {
                        response.request_focus();
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        save = ui.button("Save  [Ctrl+Enter]").clicked();
                        cancel = ui.button("Cancel  [Esc]").clicked();
                    });
                });
            let ctrl_enter = ctx.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
            if save || ctrl_enter {
                self.collection.editor = Some(editor);
                self.save_notes();
            } else if cancel || pressed(egui::Key::Escape) {
                // discarded
            } else {
                self.collection.editor = Some(editor);
            }
        }
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

    /// Keys that work while the search field has the focus too: the arrows move
    /// through the list, Esc clears the search.
    fn handle_collection_navigation(&mut self, ctx: &egui::Context) {
        let pressed = |key| ctx.input(|i| i.key_pressed(key) && !i.modifiers.command);
        if pressed(egui::Key::Escape) && self.collection.clear() {
            // the search was cleared: carry on typing
            self.collection.focus_search = true;
        }
        if self
            .collection
            .step(pressed(egui::Key::ArrowUp), pressed(egui::Key::ArrowDown))
        {
            self.collection.details = None;
        }
    }

    /// Single letter commands; only while nothing is being typed.
    fn handle_collection_commands(&mut self, ctx: &egui::Context) {
        let pressed = |key| ctx.input(|i| i.key_pressed(key) && !i.modifiers.command);
        if pressed(egui::Key::Slash) {
            self.collection.focus_search = true;
        }
        if ctx.input(|i| {
            i.key_pressed(Command::Print.key()) && !i.modifiers.shift && !i.modifiers.command
        }) {
            self.print_stored();
        }
        if pressed(Command::Open.key()) {
            self.open_stored();
        }
        if pressed(egui::Key::D) {
            self.ask_delete();
        }
        if pressed(egui::Key::E) {
            self.start_edit();
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

        if !self.collection.dialog_open() {
            self.handle_collection_navigation(ui.ctx());
            if !search_active {
                self.handle_collection_commands(ui.ctx());
            }
        }
        self.show_collection_dialogs(ui.ctx());

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
        let (mut edit, mut delete) = (false, false);
        ui.horizontal(|ui| {
            if ui.button("Print label  [P]").clicked() {
                action = Some(Command::Print);
            }
            if ui.button("Open in browser  [O]").clicked() {
                action = Some(Command::Open);
            }
            if ui.button("Edit notes  [E]").clicked() {
                edit = true;
            }
            if ui.button("Delete  [D]").clicked() {
                delete = true;
            }
            if ui.button("Quit  [Q]").clicked() {
                action = Some(Command::Quit);
            }
            ui.label(RichText::new("Up/Down: browse   Esc: clear the search   /: search").small());
        });
        match action {
            Some(Command::Print) => self.print_stored(),
            Some(Command::Open) => self.open_stored(),
            Some(Command::Quit) => self.run_command(Command::Quit),
            _ => {}
        }
        if edit {
            self.start_edit();
        }
        if delete {
            self.ask_delete();
        }
        let selected = self.collection.selected_id;
        let mut sink = LinkSink::default();
        // the label picture of the record shown
        let label_view = {
            let content = self
                .collection
                .details
                .as_ref()
                .filter(|d| Some(d.id) == selected)
                .map(|d| LabelContent::from_stored(d));
            self.label_view(ui.ctx(), content)
        };
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
                // the label picture needs room beside the info; without it, it is left out
                let view = label_view.as_ref().filter(|v| {
                    ui.available_width() - COVER_SIZE - v.size.x - 24.0 >= MIN_INFO_WIDTH
                });
                let info_width = ui.available_width()
                    - COVER_SIZE
                    - view.map_or(0.0, |v| v.size.x + 24.0)
                    - 12.0;
                ui.horizontal_top(|ui| {
                    if self.collection.selected_row().is_some_and(|r| r.has_cover) {
                        draw_image(ui, &self.images, &cover_key(record.id), COVER_SIZE);
                    } else {
                        draw_placeholder(ui, COVER_SIZE);
                    }
                    ui.vertical(|ui| {
                        ui.set_max_width(info_width.max(MIN_INFO_WIDTH));
                        render_stored(ui, record, &mut sink);
                    });
                    if let Some(view) = view {
                        // at the right edge, beside the record info
                        super::label_preview::draw_label_view(ui, view);
                    }
                });
            });
        #[cfg(test)]
        {
            self.collection.test_links = std::mem::take(&mut sink.seen);
        }
        if let Some(value) = sink.clicked {
            self.collection.search_for(&value);
            self.collection.focus_search = false;
        }
    }
}

/// Collects what was clicked while the details are drawn (and, in tests, where
/// the links are).
#[derive(Default)]
struct LinkSink {
    clicked: Option<String>,
    #[cfg(test)]
    seen: Vec<(String, egui::Rect)>,
}

impl LinkSink {
    /// A value that searches the collection for `search` when clicked.
    fn link(&mut self, ui: &mut egui::Ui, text: &str, search: &str) {
        let response = ui
            .link(text)
            .on_hover_text(format!("Search the collection for {search}"));
        #[cfg(test)]
        self.seen.push((search.to_string(), response.rect));
        if response.clicked() {
            self.clicked = Some(search.to_string());
        }
    }
}

/// One line of the details: a bold caption followed by whatever `content` draws.
fn detail_row(ui: &mut egui::Ui, caption: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new(caption).strong());
        content(ui);
    });
}

/// The details of a record from our database. Names, labels, catalog numbers,
/// the year, country, format parts, genres and styles are links that search the
/// collection for that value.
fn render_stored(ui: &mut egui::Ui, record: &StoredRelease, sink: &mut LinkSink) {
    ui.label(RichText::new(format!("Title: {}", record.title)).strong());
    if !record.artists.is_empty() {
        detail_row(ui, "Artist:", |ui| {
            for (i, (name, join)) in record.artists.iter().enumerate() {
                sink.link(ui, name, name);
                if i + 1 < record.artists.len() {
                    ui.label(join_separator(join.as_deref()).trim());
                }
            }
        });
    }
    if !record.labels.is_empty() {
        detail_row(ui, "Label:", |ui| {
            for (i, (name, catno)) in record.labels.iter().enumerate() {
                if i > 0 {
                    ui.label(";");
                }
                sink.link(ui, name, name);
                if !catno.is_empty() {
                    ui.label("(");
                    sink.link(ui, catno, catno);
                    ui.label(")");
                }
            }
        });
    }
    if let Some(date) = released_display(
        record.released_on.as_deref(),
        record.released_precision.as_deref(),
    ) {
        detail_row(ui, "Released:", |ui| {
            // the search knows the year, not the day
            sink.link(ui, &date, &date[..4.min(date.len())]);
        });
    }
    if !record.formats.is_empty() {
        detail_row(ui, "Format:", |ui| {
            for (i, format) in record.formats.iter().enumerate() {
                if i > 0 {
                    ui.label("|");
                }
                if format.qty > 1 {
                    ui.label(format!("{} x", format.qty));
                }
                for (j, part) in format.parts().iter().enumerate() {
                    if j > 0 {
                        ui.label(",");
                    }
                    sink.link(ui, part, part);
                }
            }
        });
    }
    ui.label(RichText::new(format!("Copies: {}", record.copies)).strong());
    if let Some(notes) = &record.my_notes {
        ui.label(RichText::new(format!("My notes: {notes}")).color(Color32::LIGHT_BLUE));
    }
    ui.add_space(6.0);
    if let Some(country) = &record.country {
        detail_row(ui, "Country:", |ui| sink.link(ui, country, country));
    }
    if !record.genres.is_empty() {
        detail_row(ui, "Genres:", |ui| {
            for (i, genre) in record.genres.iter().enumerate() {
                if i > 0 {
                    ui.label(",");
                }
                sink.link(ui, genre, genre);
            }
        });
    }
    if !record.styles.is_empty() {
        detail_row(ui, "Styles:", |ui| {
            for (i, style) in record.styles.iter().enumerate() {
                if i > 0 {
                    ui.label(",");
                }
                sink.link(ui, style, style);
            }
        });
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
            ui.label(format!(
                "  {} {}{duration}",
                track.position.as_deref().unwrap_or(""),
                track.title
            ));
        }
    }
    if !record.credits.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new("Credits:").strong());
        for (role, name, tracks) in &record.credits {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(format!("  {role}:"));
                sink.link(ui, name, name);
                if let Some(tracks) = tracks {
                    ui.label(format!("({tracks})"));
                }
            });
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
            my_notes: Some("Signed at the fair".into()),
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

    // --- delete (D) -------------------------------------------------------------

    #[test]
    fn d_asks_before_deleting_and_names_the_record() {
        let mut app = loaded_app(); // record 1 (Altone - Wonderscape) selected
        let before = app.worker.pending();
        app.ask_delete();
        assert_eq!(
            app.collection.confirm_delete,
            Some((1, "Altone - Wonderscape".to_string()))
        );
        assert_eq!(
            app.worker.pending(),
            before,
            "nothing happens before the answer"
        );
        assert!(app.collection.dialog_open());
    }

    #[test]
    fn declining_keeps_the_record() {
        let mut app = loaded_app();
        app.ask_delete();
        let before = app.worker.pending();
        app.collection.confirm_delete = None; // Esc / No
        assert_eq!(app.worker.pending(), before);
        assert!(app.library_busy.is_empty());
        assert!(!app.collection.dialog_open());
    }

    #[test]
    fn confirming_removes_it_from_the_library_only() {
        let mut app = loaded_app();
        app.ask_delete();
        let before = app.worker.pending();
        app.confirm_delete();
        assert_eq!(
            app.worker.pending(),
            before + 1,
            "one request to the database"
        );
        assert!(app.library_busy.contains(&1));
        assert!(app.collection.confirm_delete.is_none());
        // the Discogs side is not touched: no collection request was sent, and the
        // answer says so
        app.handle_library_response(LibraryResponse::Synced {
            release_id: 1,
            result: Ok(SyncOutcome::Removed { existed: true }),
        });
        let notice = app.notice.as_ref().unwrap();
        assert!(!notice.is_error);
        assert!(
            notice.text.contains("Removed from the library")
                && notice.text.contains("Altone - Wonderscape")
        );
        assert!(notice.text.contains("still in your Discogs collection"));
        assert_eq!(
            app.library[&1].copies, 0,
            "the Add tab now knows it is not in the library"
        );
        assert!(
            app.collection.stale || app.collection.list == ListState::Loading,
            "the list is reloaded"
        );
    }

    #[test]
    fn d_needs_a_selected_record() {
        let mut app = app();
        app.collection.set_rows(Vec::new());
        app.ask_delete();
        assert!(app.collection.confirm_delete.is_none());
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));
    }

    #[test]
    fn a_deleted_record_is_added_again_by_auto_sync_when_it_is_still_owned() {
        // the documented consequence of "library only"
        let mut app = loaded_app();
        app.settings.library_auto_sync = true;
        app.releases.insert(1, Release::default());
        app.ownership.insert(
            1,
            vec![CollectionInstance {
                instance_id: 5,
                folder_id: 1,
            }],
        );
        app.handle_library_response(LibraryResponse::Synced {
            release_id: 1,
            result: Ok(SyncOutcome::Removed { existed: true }),
        });
        assert!(
            app.library_busy.contains(&1),
            "a new sync is already on its way"
        );
    }

    // --- notes (E) -----------------------------------------------------------------

    #[test]
    fn e_opens_the_editor_with_the_current_notes() {
        let mut app = loaded_app();
        app.collection.details = Some(stored(1));
        app.start_edit();
        let editor = app.collection.editor.as_ref().unwrap();
        assert_eq!(editor.release_id, 1);
        assert_eq!(editor.text, "Signed at the fair");
        assert_eq!(editor.title, "Altone & Björk - Title 1");
        assert!(editor.focus, "the text field gets the focus");
        assert!(app.collection.dialog_open());
    }

    #[test]
    fn e_waits_for_the_record_to_load() {
        let mut app = loaded_app();
        app.start_edit();
        assert!(app.collection.editor.is_none());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("still loading"))
        );
        // details of another record do not count
        app.collection.details = Some(stored(3));
        app.start_edit();
        assert!(app.collection.editor.is_none());
    }

    #[test]
    fn saving_sends_the_text_and_closes_the_editor() {
        let mut app = loaded_app();
        app.collection.details = Some(stored(1));
        app.start_edit();
        app.collection.editor.as_mut().unwrap().text = "new text".into();
        let before = app.worker.pending();
        app.save_notes();
        assert_eq!(app.worker.pending(), before + 1);
        assert!(app.collection.editor.is_none());
    }

    #[test]
    fn saved_notes_are_reloaded_and_the_list_is_refreshed() {
        let mut app = loaded_app();
        app.collection.details = Some(stored(1));
        app.handle_library_response(LibraryResponse::Notes {
            id: 1,
            result: Ok(true),
        });
        assert!(
            app.collection.details.is_none(),
            "the details are fetched again with the new notes"
        );
        assert!(app.collection.details_requested.is_none());
        assert!(app.notice.as_ref().is_some_and(|n| n.text == "Notes saved"));
        assert!(
            app.collection.stale || app.collection.list == ListState::Loading,
            "the search must know the new notes"
        );
        // errors are reported
        app.handle_library_response(LibraryResponse::Notes {
            id: 1,
            result: Err("lost".into()),
        });
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.is_error && n.text.contains("lost"))
        );
        app.handle_library_response(LibraryResponse::Notes {
            id: 1,
            result: Ok(false),
        });
        assert!(app.notice.as_ref().is_some_and(|n| n.is_error));
    }

    #[test]
    fn the_keys_of_the_list_are_off_while_a_dialog_is_open() {
        let mut app = loaded_app();
        assert!(!app.collection.dialog_open());
        app.collection.editor = Some(NotesEditor {
            release_id: 1,
            title: "t".into(),
            text: String::new(),
            focus: false,
        });
        assert!(app.collection.dialog_open());
        app.collection.editor = None;
        app.collection.confirm_delete = Some((1, "t".into()));
        assert!(app.collection.dialog_open());
    }

    #[test]
    fn the_dialogs_and_the_notes_can_be_drawn() {
        let mut app = loaded_app();
        app.collection.details = Some(stored(1));
        render(&mut app); // details with notes
        app.collection.confirm_delete = Some((1, "Altone - Wonderscape".into()));
        render(&mut app);
        app.collection.confirm_delete = None;
        app.start_edit();
        render(&mut app);
        render(&mut app);
        assert!(
            app.collection.editor.is_some(),
            "the editor stays open while nobody answers"
        );
        assert!(
            !app.collection.editor.as_ref().unwrap().focus,
            "the focus was given once"
        );
    }

    // --- keyboard ---------------------------------------------------------------

    fn key_event(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// One frame of the tab with these key presses.
    fn frame(app: &mut RecordShelfApp, keys: &[egui::Key]) {
        let raw = egui::RawInput {
            events: keys.iter().map(|k| key_event(*k)).collect(),
            ..Default::default()
        };
        let ctx = app.repaint.clone();
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.ui_collection(ui));
        });
    }

    fn type_text(app: &mut RecordShelfApp, text: &str) {
        let raw = egui::RawInput {
            events: vec![egui::Event::Text(text.to_string())],
            ..Default::default()
        };
        let ctx = app.repaint.clone();
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.ui_collection(ui));
        });
    }

    #[test]
    fn the_arrows_work_right_after_the_tab_opens_while_the_search_has_the_focus() {
        let mut app = loaded_app();
        assert!(
            app.collection.focus_search,
            "the search field is focused when the tab opens"
        );
        frame(&mut app, &[]);
        frame(&mut app, &[]);
        assert_eq!(app.collection.selected_id, Some(1));
        frame(&mut app, &[egui::Key::ArrowDown]);
        assert_eq!(
            app.collection.selected_id,
            Some(2),
            "Down moved the selection without Esc first"
        );
        frame(&mut app, &[egui::Key::ArrowDown]);
        assert_eq!(app.collection.selected_id, Some(3));
        frame(&mut app, &[egui::Key::ArrowUp]);
        assert_eq!(app.collection.selected_id, Some(2));
    }

    #[test]
    fn typing_searches_and_the_arrows_still_move() {
        let mut app = loaded_app();
        frame(&mut app, &[]);
        frame(&mut app, &[]);
        type_text(&mut app, "garnier");
        assert_eq!(app.collection.search, "garnier");
        assert_eq!(app.collection.filtered.len(), 1);
        type_text(&mut app, " ");
        frame(&mut app, &[egui::Key::ArrowDown]);
        assert_eq!(
            app.collection.selected_id,
            Some(3),
            "the only match stays selected at the end of the list"
        );
    }

    #[test]
    fn letters_are_text_while_typing_and_commands_otherwise() {
        let mut app = loaded_app();
        frame(&mut app, &[]);
        frame(&mut app, &[]);
        // the search field has the focus: D is a letter of the search
        type_text(&mut app, "d");
        assert_eq!(app.collection.search, "d");
        assert!(app.collection.confirm_delete.is_none());
        // Esc clears the search and keeps typing possible
        frame(&mut app, &[egui::Key::Escape]);
        assert_eq!(app.collection.search, "");
        // a second Esc leaves the field: now D is a command
        frame(&mut app, &[]);
        frame(&mut app, &[egui::Key::Escape]);
        frame(&mut app, &[]);
        assert!(!app.collection.focus_search);
        frame(&mut app, &[egui::Key::D]);
        assert!(
            app.collection.confirm_delete.is_some(),
            "D asks to delete when no text is being typed"
        );
    }

    #[test]
    fn slash_goes_back_to_the_search() {
        let mut app = loaded_app();
        frame(&mut app, &[]);
        frame(&mut app, &[egui::Key::Escape]);
        frame(&mut app, &[]);
        assert!(!app.collection.focus_search);
        frame(&mut app, &[egui::Key::Slash]);
        assert!(app.collection.focus_search, "/ asks for the search field");
        frame(&mut app, &[]);
        type_text(&mut app, "b");
        assert_eq!(app.collection.search, "b");
    }

    #[test]
    fn the_keys_do_nothing_while_a_dialog_is_open() {
        let mut app = loaded_app();
        app.collection.confirm_delete = Some((1, "x".into()));
        frame(&mut app, &[egui::Key::ArrowDown]);
        assert_eq!(
            app.collection.selected_id,
            Some(1),
            "the list stays where it is behind the dialog"
        );
    }

    // --- clickable values ------------------------------------------------------------

    /// A frame in which the mouse clicks at `pos`.
    fn click_at(app: &mut RecordShelfApp, pos: egui::Pos2) {
        let raw = egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let ctx = app.repaint.clone();
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.ui_collection(ui));
        });
    }

    fn app_showing_details() -> RecordShelfApp {
        let mut app = loaded_app();
        app.collection.details = Some(stored(1));
        frame(&mut app, &[]);
        frame(&mut app, &[]); // links get their final places
        app
    }

    fn link_pos(app: &RecordShelfApp, search: &str) -> egui::Pos2 {
        app.collection
            .test_links
            .iter()
            .find(|(s, _)| s == search)
            .unwrap_or_else(|| {
                panic!(
                    "no link for {search:?} in {:?}",
                    app.collection
                        .test_links
                        .iter()
                        .map(|l| &l.0)
                        .collect::<Vec<_>>()
                )
            })
            .1
            .center()
    }

    #[test]
    fn the_values_of_a_record_are_links() {
        let app = app_showing_details();
        let searches: Vec<&str> = app
            .collection
            .test_links
            .iter()
            .map(|(s, _)| s.as_str())
            .collect();
        for expected in [
            "Altone",
            "Björk",
            "Label",
            "CAT 1",
            "2017",
            "12\"",
            "33 RPM",
            "EP",
            "DE",
            "Electronic",
            "Techno",
            "Pete",
        ] {
            assert!(
                searches.contains(&expected),
                "{expected:?} should be a link, links: {searches:?}"
            );
        }
    }

    #[test]
    fn clicking_an_artist_searches_for_them() {
        let mut app = app_showing_details();
        let pos = link_pos(&app, "Altone");
        click_at(&mut app, pos);
        assert_eq!(app.collection.search, "Altone");
        assert_eq!(
            app.collection.filtered.len(),
            1,
            "only the record with Altone: {:?}",
            app.collection.search
        );
        assert_eq!(app.collection.selected_id, Some(1));
    }

    #[test]
    fn clicking_a_value_with_spaces_searches_for_the_phrase() {
        let mut app = app_showing_details();
        app.collection.details = Some({
            let mut record = stored(1);
            record.styles = vec!["Dub Techno".into()];
            record
        });
        frame(&mut app, &[]);
        frame(&mut app, &[]);
        let pos = link_pos(&app, "Dub Techno");
        click_at(&mut app, pos);
        assert_eq!(app.collection.search, "\"Dub Techno\"");
    }

    #[test]
    fn clicking_the_year_searches_the_year_not_the_date() {
        let mut app = app_showing_details();
        let pos = link_pos(&app, "2017");
        click_at(&mut app, pos);
        assert_eq!(app.collection.search, "2017");
    }

    #[test]
    fn clicking_a_format_value_with_a_quote_in_it_works() {
        let mut app = app_showing_details();
        let pos = link_pos(&app, "12\"");
        click_at(&mut app, pos);
        assert_eq!(app.collection.search, "\"12\\\"\"");
        assert_eq!(
            crate::collection::search_terms(&app.collection.search),
            vec!["12\"".to_string()]
        );
    }

    #[test]
    fn a_click_leaves_the_scan_view_and_does_not_steal_the_focus() {
        let mut app = app_showing_details();
        app.collection.show_only(1);
        frame(&mut app, &[]);
        frame(&mut app, &[]);
        let pos = link_pos(&app, "Altone");
        click_at(&mut app, pos);
        assert_eq!(app.collection.only, None);
        assert!(
            !app.collection.focus_search,
            "the keyboard shortcuts keep working"
        );
    }

    #[test]
    fn nothing_is_searched_without_a_click() {
        let app = app_showing_details();
        assert_eq!(app.collection.search, "");
        assert_eq!(app.collection.filtered.len(), 3);
    }

    // --- details look -----------------------------------------------------------------

    /// All text drawn in a frame with its font size.
    fn drawn_text(app: &mut RecordShelfApp) -> Vec<(String, f32)> {
        let ctx = app.repaint.clone();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.ui_collection(ui));
        });
        let mut out = Vec::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut out);
        }
        out
    }

    fn collect_text(shape: &egui::epaint::Shape, out: &mut Vec<(String, f32)>) {
        match shape {
            egui::epaint::Shape::Text(text) => {
                let size = text
                    .galley
                    .job
                    .sections
                    .first()
                    .map_or(0.0, |s| s.format.font_id.size);
                out.push((text.galley.text().to_string(), size));
            }
            egui::epaint::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_text(s, out)),
            _ => {}
        }
    }

    #[test]
    fn the_details_have_no_hint_line() {
        let mut app = app_showing_details();
        let texts = drawn_text(&mut app);
        assert!(
            texts.iter().any(|(t, _)| t.contains("Tracklist")),
            "the details are drawn: {texts:?}"
        );
        assert!(
            !texts.iter().any(|(t, _)| t.contains("Click a value")),
            "{texts:?}"
        );
    }

    #[test]
    fn track_names_have_the_size_of_the_other_details() {
        let mut app = app_showing_details();
        let texts = drawn_text(&mut app);
        let size_of = |needle: &str| {
            texts
                .iter()
                .find(|(t, _)| t.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not drawn in {texts:?}"))
                .1
        };
        let body = size_of("Copies:");
        assert_eq!(
            size_of("A1 One [3:45]"),
            body,
            "a track with a position and a duration"
        );
        assert_eq!(size_of("Two"), body, "a track without them");
        assert_eq!(size_of("Producer:"), body, "the credits as well");
    }
}
