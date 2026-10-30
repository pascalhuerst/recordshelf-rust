//! The label preview window (Shift+P): the label as it would be printed, drawn
//! from the same bitmap that goes to the printer.

use super::*;
use crate::label::{self, TagStyle};

/// An open preview. The tag style can be changed in the window (T) to compare
/// the looks; printing from the window uses what is shown.
pub(super) struct LabelPreview {
    title: String,
    content: LabelContent,
    tag_style: Option<TagStyle>,
    texture: egui::TextureHandle,
    /// Size of the label in dots.
    size: (u32, u32),
    /// The frame in which the window was opened: the key press that opened it
    /// (Shift+P) must not also act inside the window.
    opened_frame: u64,
}

fn next_tag_style(style: Option<TagStyle>) -> Option<TagStyle> {
    match style {
        None => Some(TagStyle::Outline),
        Some(TagStyle::Outline) => Some(TagStyle::Filled),
        Some(TagStyle::Filled) => None,
    }
}

fn tag_style_name(style: Option<TagStyle>) -> &'static str {
    match style {
        None => "off",
        Some(TagStyle::Outline) => "outlined",
        Some(TagStyle::Filled) => "filled",
    }
}

impl RecordShelfApp {
    /// The label of a record, drawn with the printer settings, as an image for the screen.
    fn render_label_image(
        &self,
        content: &LabelContent,
        tag_style: Option<TagStyle>,
    ) -> anyhow::Result<(egui::ColorImage, (u32, u32))> {
        let settings = &self.settings;
        let size = label::LabelSize::from_mm(settings.label_width_mm, settings.label_height_mm);
        let mut options = label::LabelOptions::new(if settings.label_code == "qr" {
            CodeKind::Qr
        } else {
            CodeKind::Barcode
        });
        options.tag_style = tag_style;
        let img = label::render_with(content, size, &options)?;
        let dims = [img.width() as usize, img.height() as usize];
        Ok((
            egui::ColorImage::from_gray(dims, img.as_raw()),
            (size.width, size.height),
        ))
    }

    pub(super) fn open_label_preview(&mut self, content: LabelContent, title: String) {
        let tag_style = self.settings.tag_style();
        match self.render_label_image(&content, tag_style) {
            Ok((image, size)) => {
                let texture = self.repaint.load_texture(
                    "label-preview",
                    image,
                    egui::TextureOptions::NEAREST,
                );
                self.label_preview = Some(LabelPreview {
                    title,
                    content,
                    tag_style,
                    texture,
                    size,
                    opened_frame: self.repaint.frame_nr(),
                });
            }
            Err(err) => self.set_error(format!("Could not draw the label: {err:#}")),
        }
    }

    /// Shift+P on the Add records tab.
    pub(super) fn preview_selected_label(&mut self, id: u32) {
        let Some(release) = self.releases.get(&id) else {
            self.request_release(id);
            self.set_info("The release is still loading; press Shift+P again".to_string());
            return;
        };
        let content = LabelContent::from_release(id, release);
        let title = self.title_of(id);
        self.open_label_preview(content, title);
    }

    fn cycle_preview_tags(&mut self) {
        let Some(preview) = self.label_preview.take() else {
            return;
        };
        let style = next_tag_style(preview.tag_style);
        match self.render_label_image(&preview.content, style) {
            Ok((image, _)) => {
                let mut preview = preview;
                preview.texture.set(image, egui::TextureOptions::NEAREST);
                preview.tag_style = style;
                self.label_preview = Some(preview);
            }
            Err(err) => {
                self.label_preview = Some(preview);
                self.set_error(format!("Could not draw the label: {err:#}"));
            }
        }
    }

    pub(super) fn show_label_preview(&mut self, ctx: &egui::Context) {
        let Some(preview) = &self.label_preview else {
            return;
        };
        let (width, height) = preview.size;
        // as large as the window allows, up to 3 screen points per dot
        let screen = ctx.screen_rect().size();
        let scale = (screen.x * 0.8 / width as f32)
            .min(screen.y * 0.7 / height as f32)
            .clamp(0.5, 3.0);
        let image_size = egui::vec2(width as f32 * scale, height as f32 * scale);
        let texture_id = preview.texture.id();
        let title = preview.title.clone();
        let caption = format!(
            "{:.0} x {:.0} mm, {}, style tags: {}",
            self.settings.label_width_mm,
            self.settings.label_height_mm,
            if self.settings.label_code == "qr" {
                "QR code"
            } else {
                "Code 128"
            },
            tag_style_name(preview.tag_style),
        );
        let (mut print, mut close, mut cycle) = (false, false, false);
        let mut open = true;
        egui::Window::new(format!("Label: {title}"))
            .id(egui::Id::new("label-preview-window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .stroke(egui::Stroke::new(1.0_f32, Color32::GRAY))
                    .show(ui, |ui| {
                        ui.add(egui::Image::new((texture_id, image_size)));
                    });
                ui.label(RichText::new(caption).small());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    print = ui.button("Print  [P / Enter]").clicked();
                    cycle = ui.button("Tags  [T]").clicked();
                    close = ui.button("Close  [Esc]").clicked();
                });
            });
        let fresh = self
            .label_preview
            .as_ref()
            .is_some_and(|p| p.opened_frame == ctx.frame_nr());
        let (p, enter, esc, t) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::P) && !i.modifiers.command,
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::T) && !i.modifiers.command,
            )
        });
        let (p, enter, esc, t) = if fresh {
            (false, false, false, false)
        } else {
            (p, enter, esc, t)
        };
        if print || p || enter {
            self.print_preview();
        } else if cycle || t {
            self.cycle_preview_tags();
        } else if close || esc || !open {
            self.label_preview = None;
        }
    }

    fn print_preview(&mut self) {
        let Some(preview) = self.label_preview.take() else {
            return;
        };
        self.print_content(preview.content, preview.tag_style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{StoredFormat, StoredRelease};
    use crate::ui::tests::{app_ready_to_add, app_with_results, copy};

    fn stored_with_styles() -> Box<StoredRelease> {
        Box::new(StoredRelease {
            id: 1,
            master_id: None,
            title: "Wonderscape".into(),
            released_on: None,
            released_precision: None,
            year: None,
            country: None,
            genres: vec!["Electronic".into()],
            styles: vec!["Dub Techno".into(), "Ambient".into()],
            notes: None,
            my_notes: None,
            discogs_uri: None,
            copies: 1,
            artists: vec![("Altone".into(), None)],
            labels: vec![("Primary Colours".into(), "PCSS09".into())],
            formats: vec![StoredFormat {
                qty: 1,
                size: Some(12),
                rpm: None,
                channels: "stereo".into(),
                weight_g: None,
                text: None,
                descriptions: vec![],
            }],
            tracks: vec![],
            credits: vec![],
        })
    }

    fn collection_app() -> RecordShelfApp {
        let mut app = app_with_results(0);
        app.library_link = LibraryLink::Ready;
        app.tab = Tab::Collection;
        app.settings.printer_host = "127.0.0.1".into();
        app.collection.set_rows(vec![crate::library::RecordRow {
            id: 1,
            title: "Wonderscape".into(),
            artists: "Altone".into(),
            labels: String::new(),
            year: None,
            format: String::new(),
            copies: 1,
            has_cover: false,
            haystack: "wonderscape altone".into(),
        }]);
        app.collection.details = Some(stored_with_styles());
        // the letter keys only work when the search field is not being typed in
        app.collection.focus_search = false;
        app
    }

    fn frame_with(app: &mut RecordShelfApp, keys: &[egui::Key], shift: bool) {
        let modifiers = if shift {
            egui::Modifiers::SHIFT
        } else {
            egui::Modifiers::NONE
        };
        let raw = egui::RawInput {
            modifiers,
            events: keys
                .iter()
                .map(|key| egui::Event::Key {
                    key: *key,
                    physical_key: Some(*key),
                    pressed: true,
                    repeat: false,
                    modifiers,
                })
                .collect(),
            ..Default::default()
        };
        let ctx = app.repaint.clone();
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| match app.tab {
                Tab::Add => app.ui_main(ui),
                Tab::Collection => app.ui_collection(ui),
            });
            app.show_label_preview(ctx);
        });
    }

    #[test]
    fn shift_p_opens_the_preview_and_only_p_prints() {
        let mut app = collection_app();
        frame_with(&mut app, &[], false);
        let before = app.worker.pending();
        frame_with(&mut app, &[egui::Key::P], true);
        assert!(app.label_preview.is_some(), "Shift+P opens the preview");
        assert_eq!(app.worker.pending(), before, "and does not print");
        assert!(
            app.label_preview
                .as_ref()
                .unwrap()
                .title
                .contains("Altone - Wonderscape")
        );

        // a plain P prints and opens nothing
        let mut app = collection_app();
        frame_with(&mut app, &[], false);
        let before = app.worker.pending();
        frame_with(&mut app, &[egui::Key::P], false);
        assert!(app.label_preview.is_none());
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn the_key_that_opens_the_window_does_not_act_inside_it() {
        let mut app = collection_app();
        frame_with(&mut app, &[], false);
        let before = app.worker.pending();
        frame_with(&mut app, &[egui::Key::P], true);
        frame_with(&mut app, &[], false);
        assert!(app.label_preview.is_some(), "still open in the next frame");
        assert_eq!(app.worker.pending(), before, "nothing was printed");
    }

    #[test]
    fn the_window_prints_closes_and_cycles_the_tags() {
        let mut app = collection_app();
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[egui::Key::P], true);
        frame_with(&mut app, &[], false);
        assert_eq!(
            app.label_preview.as_ref().unwrap().tag_style,
            None,
            "the setting is off"
        );
        // T tries the next style
        frame_with(&mut app, &[egui::Key::T], false);
        assert_eq!(
            app.label_preview.as_ref().unwrap().tag_style,
            Some(TagStyle::Outline)
        );
        frame_with(&mut app, &[egui::Key::T], false);
        assert_eq!(
            app.label_preview.as_ref().unwrap().tag_style,
            Some(TagStyle::Filled)
        );
        frame_with(&mut app, &[egui::Key::T], false);
        assert_eq!(app.label_preview.as_ref().unwrap().tag_style, None);
        // Esc closes without printing
        let before = app.worker.pending();
        frame_with(&mut app, &[egui::Key::Escape], false);
        assert!(app.label_preview.is_none());
        assert_eq!(app.worker.pending(), before);
        // Enter prints and closes
        frame_with(&mut app, &[egui::Key::P], true);
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[egui::Key::Enter], false);
        assert!(app.label_preview.is_none());
        assert_eq!(app.worker.pending(), before + 1);
    }

    #[test]
    fn printing_from_the_window_uses_the_style_shown() {
        let app = collection_app();
        let content = LabelContent::from_stored(&stored_with_styles());
        assert_eq!(content.tags, ["Dub Techno", "Ambient"]);
        for style in [None, Some(TagStyle::Outline), Some(TagStyle::Filled)] {
            let mut job = app.print_job(content.clone());
            job.tag_style = style;
            assert_eq!(job.tag_style, style);
        }
        // and the pictures differ between the styles
        let image = |style| app.render_label_image(&content, style).unwrap().0.pixels;
        assert_ne!(image(None), image(Some(TagStyle::Outline)));
        assert_ne!(
            image(Some(TagStyle::Outline)),
            image(Some(TagStyle::Filled))
        );
    }

    #[test]
    fn the_preview_follows_the_label_settings() {
        let mut app = collection_app();
        let content = LabelContent::from_stored(&stored_with_styles());
        let (_, size) = app.render_label_image(&content, None).unwrap();
        assert_eq!(size, (408, 304), "51 x 38 mm");
        app.settings.label_width_mm = 52.0;
        app.settings.label_height_mm = 39.0;
        let (image, size) = app.render_label_image(&content, None).unwrap();
        assert_eq!(size, (416, 312));
        assert_eq!(image.size, [416, 312]);
        // the tag style of the settings is where the window starts
        app.settings.label_tags = "filled".into();
        app.open_label_preview(content, "x".into());
        assert_eq!(
            app.label_preview.as_ref().unwrap().tag_style,
            Some(TagStyle::Filled)
        );
    }

    #[test]
    fn the_preview_on_the_add_tab_needs_the_release_data() {
        let mut app = app_ready_to_add();
        app.releases.clear();
        app.requested.clear();
        app.run_command(Command::Preview);
        assert!(app.label_preview.is_none());
        assert!(app.requested.contains(&1), "the release is requested");
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("still loading"))
        );
        app.releases.insert(1, crate::discogs::Release::default());
        app.run_command(Command::Preview);
        assert!(app.label_preview.is_some());
    }

    #[test]
    fn the_preview_does_not_need_ownership() {
        let mut app = app_ready_to_add();
        app.releases.insert(1, crate::discogs::Release::default());
        app.ownership.insert(1, Vec::new());
        app.run_command(Command::Preview);
        assert!(
            app.label_preview.is_some(),
            "any loaded release can be previewed"
        );
    }

    #[test]
    fn the_keys_of_the_tabs_are_off_while_the_window_is_open() {
        // collection tab: D would ask to delete
        let mut app = collection_app();
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[egui::Key::P], true);
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[egui::Key::D], false);
        assert!(app.collection.confirm_delete.is_none());
        assert!(app.label_preview.is_some());

        // add tab: A would add the release
        let mut app = app_ready_to_add();
        app.ownership.insert(1, Vec::new());
        app.releases.insert(1, crate::discogs::Release::default());
        app.run_command(Command::Preview);
        frame_with(&mut app, &[], false);
        let before = app.worker.pending();
        frame_with(&mut app, &[egui::Key::A], false);
        assert_eq!(
            app.worker.pending(),
            before,
            "nothing was added behind the window"
        );
        let _ = copy(1, 1);
    }

    #[test]
    fn shift_with_another_command_key_does_not_run_the_plain_command() {
        let mut app = app_ready_to_add();
        app.ownership.insert(1, Vec::new());
        app.releases.insert(1, crate::discogs::Release::default());
        frame_with(&mut app, &[], false);
        let before = app.worker.pending();
        frame_with(&mut app, &[egui::Key::A], true);
        assert_eq!(app.worker.pending(), before, "Shift+A is not A");
    }

    #[test]
    fn the_window_can_be_drawn_on_both_tabs() {
        let mut app = collection_app();
        app.open_label_preview(LabelContent::from_stored(&stored_with_styles()), "t".into());
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[], false);
        let mut app = app_ready_to_add();
        app.open_label_preview(crate::label::sample_content_with_tags(), "t".into());
        frame_with(&mut app, &[], false);
        assert!(app.label_preview.is_some());
    }

    #[test]
    fn the_tag_styles_cycle_through_all_three() {
        assert_eq!(next_tag_style(None), Some(TagStyle::Outline));
        assert_eq!(
            next_tag_style(Some(TagStyle::Outline)),
            Some(TagStyle::Filled)
        );
        assert_eq!(next_tag_style(Some(TagStyle::Filled)), None);
        assert_eq!(tag_style_name(None), "off");
    }
}
