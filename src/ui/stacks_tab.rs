//! The Stacks tab: records are labelled one after another and put back into a
//! box in that order; a stack remembers the order. Here the stacks are made,
//! looked at and corrected.

use super::*;
use crate::library::{StackEntry, StackInfo};
use crate::worker::{ScanCode, StackOp, StackState};

/// Opens a file in the viewer.
pub(super) type FileOpener = Box<dyn FnMut(&std::path::Path) -> std::io::Result<()>>;

/// What the threads that talk to CUPS report back.
pub(super) enum ListEvent {
    /// The printers and CUPS' default one.
    Printers(Result<(Vec<String>, Option<String>), String>),
    /// What `lp` said about the list that was sent.
    Printed(Result<String, String>),
}

type PrintersQuery =
    std::sync::Arc<dyn Fn() -> Result<(Vec<String>, Option<String>), String> + Send + Sync>;
type PrintFile =
    std::sync::Arc<dyn Fn(&std::path::Path, &str, &str) -> Result<String, String> + Send + Sync>;

/// How CUPS is asked (replaced in tests, which must not print).
#[derive(Clone)]
pub(super) struct ListBackend {
    pub printers: PrintersQuery,
    pub print: PrintFile,
}

impl ListBackend {
    pub fn cups() -> Self {
        Self {
            printers: std::sync::Arc::new(|| {
                crate::stack_list::cups_printers().map_err(|e| format!("{e:#}"))
            }),
            print: std::sync::Arc::new(|path, printer, title| {
                crate::stack_list::print_file(path, printer, title).map_err(|e| format!("{e:#}"))
            }),
        }
    }
}

/// A window asking for a stack name: a new stack, or the new name of one.
pub(super) struct NameDialog {
    /// The stack to rename (`None`: start a new one).
    pub rename: Option<i32>,
    pub text: String,
    focus: bool,
}

#[derive(Default)]
pub(super) struct StacksState {
    pub stacks: Vec<StackInfo>,
    /// The stack whose records are listed.
    pub shown: Option<i32>,
    pub entries: Vec<StackEntry>,
    /// The selected record of the shown stack.
    pub selected: Option<u32>,
    /// The stacks were read from the database at least once.
    pub loaded: bool,
    pub dialog: Option<NameDialog>,
    /// Deleting this stack waits for a confirmation.
    pub confirm_delete: Option<i32>,
    /// Where a record is moved to with the "Move to" field.
    move_to: u32,
    scroll_to_selected: bool,
}

impl StacksState {
    /// The stack that newly labelled records go on.
    pub fn active(&self) -> Option<&StackInfo> {
        self.stacks.iter().find(|s| s.active)
    }

    pub fn shown_info(&self) -> Option<&StackInfo> {
        self.stacks.iter().find(|s| Some(s.id) == self.shown)
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.entries.iter().position(|e| e.release_id == id)
    }

    /// Takes over what the database says; the selection stays if the record is still there.
    pub fn apply(&mut self, state: StackState) {
        self.stacks = state.stacks;
        let (shown, entries) = match state.shown {
            Some((id, entries)) => (Some(id), entries),
            None => (None, Vec::new()),
        };
        if shown != self.shown {
            self.selected = None;
        }
        self.shown = shown;
        self.entries = entries;
        if self.selected_index().is_none() {
            self.selected = None;
        }
        self.loaded = true;
    }
}

impl RecordShelfApp {
    /// Asks CUPS which printers there are (in the background).
    pub(super) fn ask_list_printers(&mut self) {
        self.list_printers_asked = true;
        let (tx, ask, ctx) = (
            self.list_events_tx.clone(),
            self.list_backend.printers.clone(),
            self.repaint.clone(),
        );
        std::thread::spawn(move || {
            let _ = tx.send(ListEvent::Printers(ask()));
            ctx.request_repaint();
        });
    }

    pub(super) fn drain_list_events(&mut self) {
        while let Ok(event) = self.list_events_rx.try_recv() {
            match event {
                ListEvent::Printers(Ok((printers, default))) => {
                    self.list_printers = printers;
                    self.list_default = default;
                }
                // not worth an error: printing says it when it is needed
                ListEvent::Printers(Err(_)) => {}
                ListEvent::Printed(Ok(said)) => self.set_info(if said.is_empty() {
                    "The list was sent to the printer".to_string()
                } else {
                    format!("The list was sent to the printer ({said})")
                }),
                ListEvent::Printed(Err(err)) => {
                    self.set_error(format!("Printing the list failed: {err}"))
                }
            }
        }
    }

    /// Writes the PDF of the shown stack; returns its path and the stack's name.
    fn write_list_pdf(&mut self) -> Option<(std::path::PathBuf, String)> {
        let Some(info) = self.stacks.shown_info() else {
            self.set_error("There is no stack to print".to_string());
            return None;
        };
        let name = info.name.clone();
        let path = crate::stack_list::pdf_path(&name);
        match crate::stack_list::write_pdf(&path, &name, &self.stacks.entries) {
            Ok(()) => Some((path, name)),
            Err(err) => {
                self.set_error(format!("Could not make the list: {err:#}"));
                None
            }
        }
    }

    /// Shows the list of the stack as PDF in the viewer.
    pub(super) fn preview_stack_list(&mut self) {
        let Some((path, _)) = self.write_list_pdf() else {
            return;
        };
        match (self.file_opener)(&path) {
            Ok(()) => self.set_info(format!("The list is in {}", path.display())),
            Err(err) => self.set_error(format!("Could not open {}: {err}", path.display())),
        }
    }

    /// Prints the list of the stack on the paper printer.
    pub(super) fn print_stack_list(&mut self) {
        let Some((path, name)) = self.write_list_pdf() else {
            return;
        };
        let printer = match crate::stack_list::choose_printer(
            &self.settings.list_printer,
            &self.list_printers,
            self.list_default.as_deref(),
        ) {
            Ok(printer) => printer,
            Err(err) => {
                if !self.list_printers_asked {
                    self.ask_list_printers();
                }
                self.set_error(format!("{err}"));
                return;
            }
        };
        self.set_info(format!("Sending the list of stack {name} to {printer}..."));
        let (tx, print, ctx) = (
            self.list_events_tx.clone(),
            self.list_backend.print.clone(),
            self.repaint.clone(),
        );
        let title = format!("Stack {name}");
        std::thread::spawn(move || {
            let _ = tx.send(ListEvent::Printed(print(&path, &printer, &title)));
            ctx.request_repaint();
        });
    }

    fn choose_list_printer(&mut self, printer: String) {
        self.settings.list_printer = printer.clone();
        if let Some(path) = &self.config_path
            && let Err(err) = Settings::update_on_disk(path, |s| s.list_printer = printer)
        {
            self.set_error(format!("{err:#}"));
        }
    }

    /// Reads the stacks (when the database is connected).
    pub(super) fn load_stacks(&mut self) {
        self.stack_op(StackOp::Load);
    }

    pub(super) fn stack_op(&mut self, op: StackOp) {
        if !self.library_ready() {
            return;
        }
        let show = self.stacks.shown;
        self.worker
            .send(Request::Library(LibraryRequest::Stacks { op, show }));
    }

    pub(super) fn handle_stacks_response(&mut self, result: Result<Box<StackState>, String>) {
        match result {
            Ok(state) => {
                let note = state.note.clone();
                self.stacks.apply(*state);
                if let Some(note) = note {
                    self.set_info(note);
                }
            }
            Err(err) => self.set_error(format!("Stacks: {err}")),
        }
    }

    /// A label was printed: it goes on the active stack, if stacking is automatic.
    pub(super) fn stack_after_print(&mut self, release_id: u32) {
        if !self.settings.stack_auto_add || !self.library_ready() {
            return;
        }
        if let Some(stack) = self.stacks.active().map(|s| s.id) {
            self.stack_op(StackOp::Add {
                stack,
                release: release_id,
            });
        }
    }

    /// Puts a record on the active stack by hand (key K).
    pub(super) fn put_on_stack(&mut self, release_id: u32) {
        if !self.library_usable() {
            return;
        }
        let Some(stack) = self.stacks.active().map(|s| s.id) else {
            self.set_error("No active stack: start one on the Stacks tab (F3, then N)".to_string());
            return;
        };
        self.stack_op(StackOp::Add {
            stack,
            release: release_id,
        });
    }

    pub(super) fn toggle_auto_stack(&mut self) {
        if self.library_link == LibraryLink::Off {
            self.set_error("No database configured (database_url in the settings)".to_string());
            return;
        }
        let auto = !self.settings.stack_auto_add;
        self.settings.stack_auto_add = auto;
        if let Some(path) = &self.config_path
            && let Err(err) = Settings::update_on_disk(path, |s| s.stack_auto_add = auto)
        {
            self.set_error(format!("{err:#}"));
            return;
        }
        self.set_info(if auto {
            "Auto stack on: every printed label goes on the active stack".to_string()
        } else {
            "Auto stack off: press K to put a record on the stack".to_string()
        });
    }

    /// The status bar line about stacking.
    pub(super) fn stack_indicator(&self) -> Option<(String, Color32)> {
        if !self.library_ready() || !self.stacks.loaded || self.stacks.stacks.is_empty() {
            return None;
        }
        Some(match self.stacks.active() {
            Some(stack) if self.settings.stack_auto_add => (
                format!("Stack: {} ({}) auto [J]", stack.name, stack.count),
                Color32::LIGHT_GREEN,
            ),
            Some(stack) => (
                format!(
                    "Stack: {} ({}) manual [K], auto [J]",
                    stack.name, stack.count
                ),
                Color32::YELLOW,
            ),
            None => ("No active stack [F3]".to_string(), Color32::GRAY),
        })
    }

    /// A scanned sticker on the Stacks tab selects the record in the shown stack.
    pub(super) fn scan_in_stacks(&mut self, code: String) {
        let Ok(ScanCode::ReleaseId(id)) = ScanCode::classify(&code) else {
            self.set_error(format!("{code} is not the number of one of our labels"));
            return;
        };
        if self.stacks.entries.iter().any(|e| e.release_id == id) {
            self.stacks.selected = Some(id);
            self.stacks.scroll_to_selected = true;
            self.notice = None;
        } else {
            self.set_info(format!("Release {id} is not on this stack"));
        }
    }

    fn selected_stack_entry(&self) -> Option<&StackEntry> {
        self.stacks.entries.get(self.stacks.selected_index()?)
    }

    fn move_selected_entry(&mut self, to: u32) {
        if let (Some(stack), Some(entry)) = (self.stacks.shown, self.selected_stack_entry()) {
            let release = entry.release_id;
            self.stack_op(StackOp::MoveTo {
                stack,
                release,
                position: to,
            });
        }
    }

    fn remove_selected_entry(&mut self) {
        let Some(stack) = self.stacks.shown else {
            return;
        };
        let Some(index) = self.stacks.selected_index() else {
            self.set_error("Select a record first".to_string());
            return;
        };
        let release = self.stacks.entries[index].release_id;
        // the record after it takes its place in the selection
        let next = self
            .stacks
            .entries
            .get(index + 1)
            .or_else(|| {
                index
                    .checked_sub(1)
                    .and_then(|i| self.stacks.entries.get(i))
            })
            .map(|e| e.release_id);
        self.stack_op(StackOp::Remove { stack, release });
        self.stacks.selected = next;
    }

    fn show_selected_entry_in_collection(&mut self) {
        let Some(id) = self.stacks.selected else {
            return;
        };
        self.tab = Tab::Collection;
        self.on_tab_changed();
        self.scan_in_collection(id.to_string());
    }

    pub(super) fn ui_stacks(&mut self, ui: &mut egui::Ui) {
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
        if !self.stacks.loaded {
            self.load_stacks();
        }
        if !self.list_printers_asked {
            self.ask_list_printers();
        }
        let ctx = ui.ctx().clone();
        let modal = self.stacks.dialog.is_some() || self.stacks.confirm_delete.is_some();
        if !modal {
            self.handle_stack_keys(&ctx);
        }

        egui::SidePanel::left("stack-list")
            .resizable(true)
            .default_width(300.0)
            .show_inside(ui, |ui| self.stack_list(ui));
        egui::CentralPanel::default().show_inside(ui, |ui| self.stack_records(ui));

        self.show_stack_dialogs(&ctx);
    }

    fn handle_stack_keys(&mut self, ctx: &egui::Context) {
        let pressed = |key| ctx.input(|i| i.key_pressed(key) && !i.modifiers.command);
        let (alt, plain) = ctx.input(|i| (i.modifiers.alt, !i.modifiers.shift && !i.modifiers.alt));
        let (up, down) = (pressed(egui::Key::ArrowUp), pressed(egui::Key::ArrowDown));
        if alt {
            if let Some(index) = self.stacks.selected_index() {
                if up && index > 0 {
                    self.move_selected_entry(index as u32);
                    self.stacks.scroll_to_selected = true;
                }
                if down && index + 1 < self.stacks.entries.len() {
                    self.move_selected_entry(index as u32 + 2);
                    self.stacks.scroll_to_selected = true;
                }
            }
            return;
        }
        if up || down {
            let mut index = self.stacks.selected_index();
            step(&mut index, self.stacks.entries.len(), up, down);
            self.stacks.selected = index
                .and_then(|i| self.stacks.entries.get(i))
                .map(|e| e.release_id);
            self.stacks.scroll_to_selected = true;
        }
        if !plain {
            return;
        }
        if pressed(egui::Key::N) {
            self.open_name_dialog(None);
        }
        if pressed(egui::Key::A)
            && let Some(id) = self.stacks.shown
        {
            self.stack_op(StackOp::Activate(Some(id)));
        }
        if pressed(egui::Key::R) || pressed(egui::Key::Delete) {
            self.remove_selected_entry();
        }
        if pressed(egui::Key::Enter) {
            self.show_selected_entry_in_collection();
        }
        if pressed(egui::Key::P) {
            self.print_stack_list();
        }
        if pressed(egui::Key::V) {
            self.preview_stack_list();
        }
        if pressed(Command::AutoStack.key()) {
            self.toggle_auto_stack();
        }
        if pressed(Command::Quit.key()) {
            self.run_command(Command::Quit);
        }
    }

    fn open_name_dialog(&mut self, rename: Option<i32>) {
        let text = rename
            .and_then(|id| self.stacks.stacks.iter().find(|s| s.id == id))
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.stacks.dialog = Some(NameDialog {
            rename,
            text,
            focus: true,
        });
    }

    fn stack_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("Stacks");
            if ui.button("New stack  [N]").clicked() {
                self.open_name_dialog(None);
            }
        });
        ui.add_space(4.0);
        let mut show = None;
        egui::ScrollArea::vertical()
            .id_source("stack-list-scroll")
            .max_height((ui.available_height() - 150.0).max(80.0))
            .show(ui, |ui| {
                for stack in &self.stacks.stacks {
                    let text = format!(
                        "{}{}  ({})",
                        if stack.active { "● " } else { "" },
                        stack.name,
                        stack.count
                    );
                    if ui
                        .selectable_label(Some(stack.id) == self.stacks.shown, text)
                        .clicked()
                    {
                        show = Some(stack.id);
                    }
                }
            });
        if let Some(id) = show {
            // list that one: ask for it, the answer brings its records
            self.stacks.shown = Some(id);
            self.stack_op(StackOp::Load);
        }
        ui.separator();
        let shown = self.stacks.shown_info().cloned();
        ui.add_enabled_ui(shown.is_some(), |ui| {
            ui.horizontal_wrapped(|ui| {
                let is_active = shown.as_ref().is_some_and(|s| s.active);
                if ui
                    .add_enabled(!is_active, egui::Button::new("Make active  [A]"))
                    .clicked()
                    && let Some(s) = &shown
                {
                    self.stack_op(StackOp::Activate(Some(s.id)));
                }
                if ui
                    .add_enabled(
                        self.stacks.active().is_some(),
                        egui::Button::new("Stop stacking"),
                    )
                    .clicked()
                {
                    self.stack_op(StackOp::Activate(None));
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button("Rename").clicked() {
                    self.open_name_dialog(self.stacks.shown);
                }
                if ui.button("Delete").clicked() {
                    self.stacks.confirm_delete = self.stacks.shown;
                }
            });
        });
        ui.add_space(6.0);
        ui.separator();
        ui.label(RichText::new("List on paper").strong());
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(shown.is_some(), egui::Button::new("Print list  [P]"))
                .on_hover_text("Prints the shown stack on the paper printer (A4)")
                .clicked()
            {
                self.print_stack_list();
            }
            if ui
                .add_enabled(shown.is_some(), egui::Button::new("View PDF  [V]"))
                .clicked()
            {
                self.preview_stack_list();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Printer:");
            let chosen = self.settings.list_printer.clone();
            let default_name = self
                .list_default
                .clone()
                .map(|d| format!("CUPS default ({d})"))
                .unwrap_or_else(|| "CUPS default".to_string());
            let mut pick = None;
            egui::ComboBox::from_id_source("list-printer")
                .selected_text(if chosen.is_empty() {
                    default_name.clone()
                } else {
                    chosen.clone()
                })
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(chosen.is_empty(), &default_name)
                        .clicked()
                    {
                        pick = Some(String::new());
                    }
                    for printer in &self.list_printers {
                        if ui.selectable_label(*printer == chosen, printer).clicked() {
                            pick = Some(printer.clone());
                        }
                    }
                });
            if ui.small_button("Refresh").clicked() {
                self.ask_list_printers();
            }
            if let Some(printer) = pick {
                self.choose_list_printer(printer);
            }
        });
        ui.add_space(6.0);
        let mut auto = self.settings.stack_auto_add;
        if ui
            .checkbox(&mut auto, "Stack printed labels automatically  [J]")
            .changed()
        {
            self.toggle_auto_stack();
        }
        ui.label(
            RichText::new("● is the stack new labels go on. Records that are not in the library cannot be stacked.")
                .small()
                .weak(),
        );
    }

    fn stack_records(&mut self, ui: &mut egui::Ui) {
        let Some(info) = self.stacks.shown_info().cloned() else {
            ui.add_space(8.0);
            ui.label("No stack yet. Press N to start one: the labels you print from then on go on it, in the order you print them.");
            return;
        };
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading(&info.name);
            ui.label(format!("{} records", info.count));
            if info.active {
                ui.colored_label(Color32::LIGHT_GREEN, "active");
            }
        });
        ui.horizontal_wrapped(|ui| {
            let index = self.stacks.selected_index();
            let len = self.stacks.entries.len();
            if ui
                .add_enabled(
                    index.is_some_and(|i| i > 0),
                    egui::Button::new("Up  [Alt+Up]"),
                )
                .clicked()
                && let Some(i) = index
            {
                self.move_selected_entry(i as u32);
            }
            if ui
                .add_enabled(
                    index.is_some_and(|i| i + 1 < len),
                    egui::Button::new("Down  [Alt+Down]"),
                )
                .clicked()
                && let Some(i) = index
            {
                self.move_selected_entry(i as u32 + 2);
            }
            ui.label("to number");
            ui.add(
                egui::DragValue::new(&mut self.stacks.move_to).clamp_range(1..=len.max(1) as u32),
            );
            if ui
                .add_enabled(index.is_some(), egui::Button::new("Move"))
                .clicked()
            {
                let to = self.stacks.move_to;
                self.move_selected_entry(to);
            }
            ui.separator();
            if ui
                .add_enabled(index.is_some(), egui::Button::new("Take out  [R]"))
                .clicked()
            {
                self.remove_selected_entry();
            }
            if ui
                .add_enabled(
                    index.is_some(),
                    egui::Button::new("Show in collection  [Enter]"),
                )
                .clicked()
            {
                self.show_selected_entry_in_collection();
            }
        });
        ui.separator();
        let scroll = std::mem::take(&mut self.stacks.scroll_to_selected);
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_source("stack-records")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for entry in &self.stacks.entries {
                    let selected = Some(entry.release_id) == self.stacks.selected;
                    let mut text = format!("{:>3}.  {}", entry.position, entry.artists);
                    if !entry.title.is_empty() {
                        text.push_str(&format!(" – {}", entry.title));
                    }
                    if !entry.labels.is_empty() {
                        text.push_str(&format!("    [{}]", entry.labels));
                    }
                    let response = ui.selectable_label(selected, text);
                    if response.clicked() {
                        clicked = Some(entry.release_id);
                    }
                    if selected && scroll {
                        response.scroll_to_me(None);
                    }
                }
            });
        if let Some(id) = clicked {
            self.stacks.selected = Some(id);
        }
        if let Some(entry) = self.selected_stack_entry() {
            self.stacks.move_to = entry.position;
        }
    }

    fn show_stack_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(dialog) = &mut self.stacks.dialog {
            let mut submit = false;
            let mut cancel = false;
            let title = if dialog.rename.is_some() {
                "Rename stack"
            } else {
                "New stack"
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    if dialog.rename.is_none() {
                        ui.label("Every label you print from now on goes on this stack.");
                    }
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut dialog.text)
                            .hint_text("Name, e.g. Box 3")
                            .desired_width(260.0),
                    );
                    if std::mem::take(&mut dialog.focus) {
                        response.request_focus();
                    }
                    if response.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                        if dialog.text.trim().is_empty() {
                            // Enter took the focus away, but there is no name yet
                            response.request_focus();
                        }
                    }
                    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                        cancel = true;
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!dialog.text.trim().is_empty(), egui::Button::new("OK"))
                            .clicked()
                        {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            if submit && !dialog.text.trim().is_empty() {
                let (rename, name) = (dialog.rename, dialog.text.trim().to_string());
                self.stacks.dialog = None;
                self.stack_op(match rename {
                    Some(id) => StackOp::Rename { id, name },
                    None => StackOp::Create { name },
                });
            } else if cancel {
                self.stacks.dialog = None;
            }
        }
        if let Some(id) = self.stacks.confirm_delete {
            let name = self
                .stacks
                .stacks
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            let (mut yes, mut no) = (false, false);
            egui::Window::new("Delete stack?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Delete the stack {name}? The records stay in the library."
                    ));
                    ui.horizontal(|ui| {
                        yes = ui.button("Delete  [Enter]").clicked();
                        no = ui.button("Keep  [Esc]").clicked();
                    });
                    ctx.input(|i| {
                        yes |= i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Y);
                        no |= i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::N);
                    });
                });
            if yes {
                self.stacks.confirm_delete = None;
                self.stack_op(StackOp::Delete(id));
            } else if no {
                self.stacks.confirm_delete = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::app_with_results;

    fn stack(id: i32, name: &str, active: bool, count: i64) -> StackInfo {
        StackInfo {
            id,
            name: name.into(),
            active,
            count,
        }
    }

    fn entry(position: u32, release_id: u32) -> StackEntry {
        StackEntry {
            position,
            release_id,
            artists: format!("Artist {release_id}"),
            title: format!("Title {release_id}"),
            labels: format!("Label (L{release_id})"),
        }
    }

    fn state(active: bool) -> StackState {
        StackState {
            stacks: vec![stack(1, "Box A", active, 3), stack(2, "Box B", false, 0)],
            shown: Some((1, vec![entry(1, 7), entry(2, 8), entry(3, 9)])),
            note: None,
        }
    }

    type Sent = std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, String, String)>>>;

    /// A backend that prints nowhere: it records what would be sent.
    fn fake_backend(answer: Result<String, String>) -> (ListBackend, Sent) {
        let sent: Sent = Default::default();
        let record = sent.clone();
        let backend = ListBackend {
            printers: std::sync::Arc::new(|| {
                Ok((vec!["Office".into(), "Other".into()], Some("Office".into())))
            }),
            print: std::sync::Arc::new(move |path, printer, title| {
                record.lock().unwrap().push((
                    path.to_path_buf(),
                    printer.to_string(),
                    title.to_string(),
                ));
                answer.clone()
            }),
        };
        (backend, sent)
    }

    /// Waits for what the threads report and takes it in.
    fn settle(app: &mut RecordShelfApp, until: impl Fn(&RecordShelfApp) -> bool) {
        for _ in 0..200 {
            app.drain_list_events();
            if until(app) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "nothing came back: {:?}",
            app.notice.as_ref().map(|n| &n.text)
        );
    }

    fn opened() -> (
        FileOpener,
        std::rc::Rc<std::cell::RefCell<Vec<std::path::PathBuf>>>,
    ) {
        let seen: std::rc::Rc<std::cell::RefCell<Vec<std::path::PathBuf>>> = Default::default();
        let record = seen.clone();
        (
            Box::new(move |p| {
                record.borrow_mut().push(p.to_path_buf());
                Ok(())
            }),
            seen,
        )
    }

    fn app() -> RecordShelfApp {
        let mut app = app_with_results(0);
        app.list_backend = fake_backend(Ok("request id is Office-7 (1 file(s))".into())).0;
        app.file_opener = opened().0;
        app.library_link = LibraryLink::Ready;
        app.tab = Tab::Stacks;
        app.stacks.apply(state(true));
        app
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn frame(app: &mut RecordShelfApp, events: Vec<egui::Event>) -> Vec<String> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            modifiers: events
                .iter()
                .find_map(|e| match e {
                    egui::Event::Key { modifiers, .. } => Some(*modifiers),
                    _ => None,
                })
                .unwrap_or_default(),
            events,
            ..Default::default()
        };
        let ctx = app.repaint.clone();
        let output = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.ui_stacks(ui));
        });
        let mut texts = Vec::new();
        fn walk(shape: &egui::epaint::Shape, texts: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(t) => texts.push(t.galley.text().to_string()),
                egui::epaint::Shape::Vec(v) => v.iter().for_each(|s| walk(s, texts)),
                _ => {}
            }
        }
        output
            .shapes
            .iter()
            .for_each(|c| walk(&c.shape, &mut texts));
        texts
    }

    fn plain(k: egui::Key) -> Vec<egui::Event> {
        vec![key(k, egui::Modifiers::NONE)]
    }

    #[test]
    fn the_selection_survives_an_update_of_the_same_stack_only() {
        let mut s = StacksState::default();
        s.apply(state(true));
        s.selected = Some(8);
        s.apply(state(true));
        assert_eq!(s.selected, Some(8));
        // the record was taken out
        let mut fewer = state(true);
        fewer.shown = Some((1, vec![entry(1, 7), entry(2, 9)]));
        s.apply(fewer);
        assert_eq!(s.selected, None);
        // another stack is shown
        s.selected = Some(7);
        let mut other = state(true);
        other.shown = Some((2, vec![entry(1, 7)]));
        s.apply(other);
        assert_eq!(s.selected, None, "a new list starts without a selection");
        assert!(s.loaded);
        // no stacks at all
        s.apply(StackState::default());
        assert!(s.shown.is_none() && s.entries.is_empty() && s.active().is_none());
    }

    #[test]
    fn the_answer_of_the_database_is_taken_over() {
        let mut app = app();
        let mut answer = state(false);
        answer.note = Some("Release 7 is number 4 on stack Box A".into());
        app.handle_library_response(LibraryResponse::Stacks(Ok(Box::new(answer))));
        assert!(app.stacks.active().is_none());
        assert_eq!(
            app.notice.as_ref().map(|n| (n.text.as_str(), n.is_error)),
            Some(("Release 7 is number 4 on stack Box A", false))
        );
        app.handle_library_response(LibraryResponse::Stacks(Err("boom".into())));
        let notice = app.notice.as_ref().unwrap();
        assert!(notice.is_error && notice.text.contains("boom"));
        assert_eq!(app.stacks.entries.len(), 3, "an error keeps what we know");
    }

    #[test]
    fn printed_labels_go_on_the_active_stack_when_stacking_is_automatic() {
        let mut app = app();
        assert!(app.settings.stack_auto_add, "automatic by default");
        let before = app.worker.pending();
        app.handle_response(Response::Printed {
            release_id: 7,
            result: Ok(()),
        });
        assert_eq!(app.worker.pending(), before + 1, "an Add was sent");

        // a failed print is not stacked
        let before = app.worker.pending();
        app.handle_response(Response::Printed {
            release_id: 7,
            result: Err("paused".into()),
        });
        assert_eq!(app.worker.pending(), before);

        // manual mode
        app.settings.stack_auto_add = false;
        app.handle_response(Response::Printed {
            release_id: 7,
            result: Ok(()),
        });
        assert_eq!(app.worker.pending(), before);

        // nothing to stack on
        app.settings.stack_auto_add = true;
        app.stacks.apply(state(false));
        app.handle_response(Response::Printed {
            release_id: 7,
            result: Ok(()),
        });
        assert_eq!(app.worker.pending(), before);

        // no database
        app.stacks.apply(state(true));
        app.library_link = LibraryLink::Off;
        app.handle_response(Response::Printed {
            release_id: 7,
            result: Ok(()),
        });
        assert_eq!(app.worker.pending(), before);
    }

    #[test]
    fn k_puts_a_record_on_the_stack_by_hand() {
        let mut app = app();
        app.stacks.apply(state(false));
        app.put_on_stack(7);
        let notice = app.notice.as_ref().unwrap();
        assert!(notice.is_error && notice.text.contains("No active stack"));
        app.stacks.apply(state(true));
        let before = app.worker.pending();
        app.put_on_stack(7);
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn auto_stack_is_switched_and_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = \"keep\"\n").unwrap();
        let mut app = app();
        app.config_path = Some(path.clone());
        app.toggle_auto_stack();
        assert!(!app.settings.stack_auto_add);
        let saved = Settings::load(&path).unwrap();
        assert!(!saved.stack_auto_add);
        assert_eq!(saved.token, "keep");
        app.toggle_auto_stack();
        assert!(Settings::load(&path).unwrap().stack_auto_add);
        app.library_link = LibraryLink::Off;
        app.toggle_auto_stack();
        assert!(app.notice.as_ref().unwrap().is_error);
        assert!(app.settings.stack_auto_add, "unchanged without a database");
    }

    #[test]
    fn the_status_bar_tells_which_stack_is_active() {
        let mut app = app();
        let (text, _) = app.stack_indicator().unwrap();
        assert_eq!(text, "Stack: Box A (3) auto [J]");
        app.settings.stack_auto_add = false;
        assert!(app.stack_indicator().unwrap().0.contains("manual [K]"));
        app.stacks.apply(state(false));
        assert_eq!(app.stack_indicator().unwrap().0, "No active stack [F3]");
        app.stacks.apply(StackState::default());
        assert!(
            app.stack_indicator().is_none(),
            "nothing to say without stacks"
        );
        app.stacks.apply(state(true));
        app.library_link = LibraryLink::Connecting;
        assert!(app.stack_indicator().is_none());
    }

    #[test]
    fn the_tab_lists_the_stacks_and_the_records_in_order() {
        let mut app = app();
        frame(&mut app, vec![]);
        let texts = frame(&mut app, vec![]);
        for wanted in [
            "● Box A  (3)",
            "Box B  (0)",
            "  1.  Artist 7 – Title 7    [Label (L7)]",
            "  3.  Artist 9 – Title 9    [Label (L9)]",
        ] {
            assert!(texts.iter().any(|t| t == wanted), "{wanted:?} in {texts:?}");
        }
        let pos = |name: &str| texts.iter().position(|t| t.contains(name)).unwrap();
        assert!(pos("Artist 7") < pos("Artist 8") && pos("Artist 8") < pos("Artist 9"));
    }

    #[test]
    fn an_empty_tab_explains_how_to_start() {
        let mut app = app();
        app.stacks.apply(StackState::default());
        let texts = frame(&mut app, vec![]);
        assert!(
            texts.iter().any(|t| t.starts_with("No stack yet")),
            "{texts:?}"
        );
    }

    #[test]
    fn the_arrow_keys_walk_through_the_stack() {
        let mut app = app();
        frame(&mut app, vec![]);
        frame(&mut app, plain(egui::Key::ArrowDown));
        assert_eq!(app.stacks.selected, Some(7));
        frame(&mut app, plain(egui::Key::ArrowDown));
        frame(&mut app, plain(egui::Key::ArrowDown));
        frame(&mut app, plain(egui::Key::ArrowDown));
        assert_eq!(app.stacks.selected, Some(9), "stops at the end");
        frame(&mut app, plain(egui::Key::ArrowUp));
        assert_eq!(app.stacks.selected, Some(8));
    }

    #[test]
    fn alt_and_the_arrows_move_the_record() {
        let mut app = app();
        app.stacks.selected = Some(8);
        let alt = egui::Modifiers::ALT;
        let before = app.worker.pending();
        frame(&mut app, vec![key(egui::Key::ArrowDown, alt)]);
        assert_eq!(app.worker.pending(), before + 1, "asked to move it");
        assert_eq!(app.stacks.selected, Some(8), "the record stays selected");
        // the first record cannot go up, the last cannot go down
        app.stacks.selected = Some(7);
        let before = app.worker.pending();
        frame(&mut app, vec![key(egui::Key::ArrowUp, alt)]);
        app.stacks.selected = Some(9);
        frame(&mut app, vec![key(egui::Key::ArrowDown, alt)]);
        assert_eq!(app.worker.pending(), before);
    }

    #[test]
    fn r_takes_the_record_out_and_selects_its_neighbour() {
        let mut app = app();
        app.stacks.selected = Some(8);
        let before = app.worker.pending();
        frame(&mut app, plain(egui::Key::R));
        assert_eq!(app.worker.pending(), before + 1);
        assert_eq!(app.stacks.selected, Some(9));
        // the last one: the one before it
        app.stacks.selected = Some(9);
        frame(&mut app, plain(egui::Key::Delete));
        assert_eq!(app.stacks.selected, Some(8));
        // nothing selected
        app.stacks.selected = None;
        let before = app.worker.pending();
        frame(&mut app, plain(egui::Key::R));
        assert_eq!(app.worker.pending(), before);
        assert!(app.notice.as_ref().unwrap().is_error);
    }

    #[test]
    fn n_asks_for_a_name_and_enter_starts_the_stack() {
        let mut app = app();
        frame(&mut app, plain(egui::Key::N));
        assert!(app.stacks.dialog.is_some());
        // while the window is open, letters are not commands
        let before = app.worker.pending();
        frame(&mut app, plain(egui::Key::R));
        frame(&mut app, plain(egui::Key::A));
        assert_eq!(app.worker.pending(), before);
        // an empty name is not accepted
        frame(&mut app, plain(egui::Key::Enter));
        assert!(app.stacks.dialog.is_some());
        app.stacks.dialog.as_mut().unwrap().text = "  Box 3 ".into();
        // the field has to have the focus before Enter makes it let go
        for _ in 0..3 {
            if app.stacks.dialog.is_some() {
                frame(&mut app, plain(egui::Key::Enter));
            }
        }
        assert!(app.stacks.dialog.is_none(), "the window closes");
        assert_eq!(app.worker.pending(), before + 1, "the stack is created");
    }

    #[test]
    fn esc_cancels_the_name_window() {
        let mut app = app();
        frame(&mut app, plain(egui::Key::N));
        frame(&mut app, plain(egui::Key::Escape));
        assert!(app.stacks.dialog.is_none());
    }

    #[test]
    fn renaming_starts_with_the_old_name() {
        let mut app = app();
        app.open_name_dialog(Some(1));
        let dialog = app.stacks.dialog.as_ref().unwrap();
        assert_eq!((dialog.rename, dialog.text.as_str()), (Some(1), "Box A"));
    }

    #[test]
    fn deleting_a_stack_asks_first() {
        let mut app = app();
        app.stacks.confirm_delete = Some(1);
        let before = app.worker.pending();
        frame(&mut app, plain(egui::Key::Escape));
        assert!(app.stacks.confirm_delete.is_none());
        assert_eq!(app.worker.pending(), before);
        app.stacks.confirm_delete = Some(1);
        frame(&mut app, plain(egui::Key::Enter));
        assert!(app.stacks.confirm_delete.is_none());
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn a_scanned_sticker_selects_the_record_in_the_stack() {
        let mut app = app();
        app.scan_in_stacks("8".into());
        assert_eq!(app.stacks.selected, Some(8));
        app.scan_in_stacks("12345".into());
        assert_eq!(app.stacks.selected, Some(8));
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("not on this stack"),
            "{:?}",
            app.notice.as_ref().map(|n| &n.text)
        );
        app.scan_in_stacks("not a number".into());
        assert!(app.notice.as_ref().unwrap().is_error);
    }

    #[test]
    fn enter_shows_the_record_in_the_collection() {
        let mut app = app();
        app.stacks.selected = Some(8);
        let before = app.worker.pending();
        frame(&mut app, plain(egui::Key::Enter));
        assert_eq!(app.tab, Tab::Collection);
        assert!(app.worker.pending() > before, "the record is asked for");
    }

    #[test]
    fn f3_opens_the_tab_and_the_tab_loads_the_stacks() {
        let mut app = app();
        app.tab = Tab::Add;
        app.stacks = StacksState::default();
        app.tab = Tab::Stacks;
        let before = app.worker.pending();
        app.on_tab_changed();
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn the_list_goes_to_the_chosen_printer() {
        let mut app = app();
        let (backend, sent) = fake_backend(Ok("request id is Other-3 (1 file(s))".into()));
        app.list_backend = backend;
        app.settings.list_printer = "Other".into();
        frame(&mut app, plain(egui::Key::P));
        settle(&mut app, |a| {
            a.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("sent to the printer"))
        });
        let notice = app.notice.as_ref().unwrap();
        assert!(
            !notice.is_error && notice.text.contains("request id is Other-3"),
            "{}",
            notice.text
        );
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (path, printer, title) = &sent[0];
        assert_eq!((printer.as_str(), title.as_str()), ("Other", "Stack Box A"));
        assert!(
            std::fs::read(path).unwrap().starts_with(b"%PDF"),
            "the file exists"
        );
    }

    #[test]
    fn without_a_choice_the_default_printer_is_used() {
        let mut app = app();
        let (backend, sent) = fake_backend(Ok(String::new()));
        app.list_backend = backend;
        // the tab asks CUPS for its printers
        frame(&mut app, vec![]);
        settle(&mut app, |a| a.list_default.is_some());
        assert_eq!(app.list_printers, ["Office", "Other"]);
        app.settings.list_printer = String::new();
        app.print_stack_list();
        settle(&mut app, |a| {
            a.notice.as_ref().is_some_and(|n| n.text.contains("sent"))
        });
        assert_eq!(sent.lock().unwrap()[0].1, "Office");
        assert_eq!(
            app.notice.as_ref().unwrap().text,
            "The list was sent to the printer"
        );
    }

    #[test]
    fn no_printer_to_use_is_said_clearly() {
        let mut app = app();
        let (backend, sent) = fake_backend(Ok(String::new()));
        app.list_backend = backend;
        app.list_printers = vec!["A".into(), "B".into()];
        app.list_default = None;
        app.print_stack_list();
        let notice = app.notice.as_ref().unwrap();
        assert!(
            notice.is_error && notice.text.contains("choose a printer"),
            "{}",
            notice.text
        );
        assert!(sent.lock().unwrap().is_empty());
        // nothing to print without a stack
        app.stacks.apply(StackState::default());
        app.print_stack_list();
        assert!(app.notice.as_ref().unwrap().text.contains("no stack"));
    }

    #[test]
    fn a_failed_print_is_reported() {
        let mut app = app();
        app.list_backend = fake_backend(Err("lp: The printer or class does not exist.".into())).0;
        app.settings.list_printer = "Gone".into();
        app.print_stack_list();
        settle(&mut app, |a| a.notice.as_ref().is_some_and(|n| n.is_error));
        let text = &app.notice.as_ref().unwrap().text;
        assert!(
            text.contains("Printing the list failed") && text.contains("does not exist"),
            "{text}"
        );
    }

    #[test]
    fn the_pdf_can_be_looked_at_first() {
        let mut app = app();
        let (opener, seen) = opened();
        app.file_opener = opener;
        frame(&mut app, plain(egui::Key::V));
        let seen = seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].file_name().unwrap(), "stack-Box_A.pdf");
        assert!(std::fs::read(&seen[0]).unwrap().starts_with(b"%PDF"));
    }

    #[test]
    fn the_printer_choice_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = \"keep\"\n").unwrap();
        let mut app = app();
        app.config_path = Some(path.clone());
        app.choose_list_printer("Other".into());
        assert_eq!(app.settings.list_printer, "Other");
        let saved = Settings::load(&path).unwrap();
        assert_eq!(
            (saved.list_printer.as_str(), saved.token.as_str()),
            ("Other", "keep")
        );
        app.choose_list_printer(String::new());
        assert_eq!(Settings::load(&path).unwrap().list_printer, "");
    }

    #[test]
    fn the_tab_offers_the_list_on_paper() {
        let mut app = app();
        app.list_printers = vec!["Office".into()];
        app.list_default = Some("Office".into());
        frame(&mut app, vec![]);
        let texts = frame(&mut app, vec![]);
        for wanted in [
            "List on paper",
            "Print list  [P]",
            "View PDF  [V]",
            "CUPS default (Office)",
        ] {
            assert!(texts.iter().any(|t| t == wanted), "{wanted:?} in {texts:?}");
        }
    }

    #[test]
    fn an_unavailable_cups_does_not_disturb() {
        let mut app = app();
        app.list_backend.printers = std::sync::Arc::new(|| Err("lpstat: not found".into()));
        frame(&mut app, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(50));
        app.drain_list_events();
        assert!(
            app.notice
                .as_ref()
                .is_none_or(|n| !n.text.contains("lpstat")),
            "no complaint before printing is tried"
        );
        assert!(app.list_printers.is_empty());
    }
}
