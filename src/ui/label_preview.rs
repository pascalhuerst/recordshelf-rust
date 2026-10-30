//! The label preview window (Shift+P): the label as it would be printed, drawn
//! from the same bitmap that goes to the printer. By default at its real size
//! on the screen, so it can be compared with a label held next to it.

use super::*;
use crate::label::{self, TagStyle};
use image::{GrayImage, imageops};

/// An open preview. The tag style can be changed in the window (T) to compare
/// the looks; printing from the window uses what is shown.
pub(super) struct LabelPreview {
    title: String,
    content: LabelContent,
    tag_style: Option<TagStyle>,
    /// The label bitmap as the printer gets it.
    source: GrayImage,
    texture: egui::TextureHandle,
    /// Shown at its real size (true) or enlarged to fill the window (false).
    real_size: bool,
    /// What the texture was made for; made again when this changes (zoom, screen).
    texture_key: TextureKey,
    /// Size on the screen in points.
    display: egui::Vec2,
    /// The frame in which the window was opened: the key press that opened it
    /// (Shift+P) must not also act inside the window.
    opened_frame: u64,
}

/// Everything the texture depends on besides the bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextureKey {
    pixels_per_point_milli: u32,
    zoom_milli: u32,
    real_size: bool,
    /// The window size, only relevant when enlarged.
    screen: (u32, u32),
    ppi_centi: u32,
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

/// The size in points of a label of `width_mm` x `height_mm` that looks as big
/// on the screen as the real label: `ppi` points make an inch (96 on a usual
/// screen), and the interface zoom (Ctrl +/-) is taken out again.
pub(super) fn real_size_points(width_mm: f32, height_mm: f32, ppi: f32, zoom: f32) -> egui::Vec2 {
    let points_per_mm = ppi / 25.4 / zoom.max(0.1);
    egui::vec2(width_mm * points_per_mm, height_mm * points_per_mm)
}

/// How much to enlarge the bitmap (screen points per dot) so that it fills most
/// of the window, but not more than 3 points per dot.
pub(super) fn fit_scale(screen: egui::Vec2, dots: (u32, u32)) -> f32 {
    (screen.x * 0.8 / dots.0.max(1) as f32)
        .min(screen.y * 0.7 / dots.1.max(1) as f32)
        .clamp(0.5, 3.0)
}

impl RecordShelfApp {
    /// The label of a record, drawn with the printer settings.
    fn render_label_bitmap(
        &self,
        content: &LabelContent,
        tag_style: Option<TagStyle>,
    ) -> anyhow::Result<GrayImage> {
        let settings = &self.settings;
        let size = label::LabelSize::from_mm(settings.label_width_mm, settings.label_height_mm);
        let mut options = label::LabelOptions::new(if settings.label_code == "qr" {
            CodeKind::Qr
        } else {
            CodeKind::Barcode
        });
        options.tag_style = tag_style;
        label::render_with(content, size, &options)
    }

    /// The picture for the screen, its size in points and the key it is valid for.
    fn label_view(
        &self,
        source: &GrayImage,
        ctx: &egui::Context,
        real_size: bool,
    ) -> (
        egui::ColorImage,
        egui::Vec2,
        egui::TextureOptions,
        TextureKey,
    ) {
        let ppp = ctx.pixels_per_point();
        let screen = ctx.screen_rect().size();
        let key = TextureKey {
            pixels_per_point_milli: (ppp * 1000.0).round() as u32,
            zoom_milli: (ctx.zoom_factor() * 1000.0).round() as u32,
            real_size,
            screen: if real_size {
                (0, 0)
            } else {
                (screen.x as u32, screen.y as u32)
            },
            ppi_centi: (self.settings.preview_ppi * 100.0).round() as u32,
        };
        if real_size {
            let points = real_size_points(
                self.settings.label_width_mm,
                self.settings.label_height_mm,
                self.settings.preview_ppi,
                ctx.zoom_factor(),
            );
            // one screen pixel per image pixel: smooth (the bitmap is 1-bit and
            // about twice as fine as the screen), and nothing is stretched
            let (w, h) = (
                (points.x * ppp).round().max(1.0) as u32,
                (points.y * ppp).round().max(1.0) as u32,
            );
            let small = imageops::resize(source, w, h, imageops::FilterType::Triangle);
            let image = egui::ColorImage::from_gray([w as usize, h as usize], small.as_raw());
            let display = egui::vec2(w as f32 / ppp, h as f32 / ppp);
            (image, display, egui::TextureOptions::NEAREST, key)
        } else {
            let scale = fit_scale(screen, (source.width(), source.height()));
            let image = egui::ColorImage::from_gray(
                [source.width() as usize, source.height() as usize],
                source.as_raw(),
            );
            let display = egui::vec2(
                source.width() as f32 * scale,
                source.height() as f32 * scale,
            );
            (image, display, egui::TextureOptions::NEAREST, key)
        }
    }

    pub(super) fn open_label_preview(&mut self, content: LabelContent, title: String) {
        let tag_style = self.settings.tag_style();
        let source = match self.render_label_bitmap(&content, tag_style) {
            Ok(source) => source,
            Err(err) => {
                self.set_error(format!("Could not draw the label: {err:#}"));
                return;
            }
        };
        let (image, display, options, texture_key) = self.label_view(&source, &self.repaint, true);
        let texture = self.repaint.load_texture("label-preview", image, options);
        self.label_preview = Some(LabelPreview {
            title,
            content,
            tag_style,
            source,
            texture,
            real_size: true,
            texture_key,
            display,
            opened_frame: self.repaint.frame_nr(),
        });
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
        let Some(mut preview) = self.label_preview.take() else {
            return;
        };
        let style = next_tag_style(preview.tag_style);
        match self.render_label_bitmap(&preview.content, style) {
            Ok(source) => {
                preview.source = source;
                preview.tag_style = style;
                // the texture is made again for the new bitmap by `refresh_texture`
                preview.texture_key.pixels_per_point_milli = 0;
            }
            Err(err) => self.set_error(format!("Could not draw the label: {err:#}")),
        }
        self.label_preview = Some(preview);
    }

    /// Makes the texture again when the bitmap, the zoom or the window changed.
    fn refresh_texture(&mut self, ctx: &egui::Context) {
        let Some(preview) = self.label_preview.take() else {
            return;
        };
        let mut preview = preview;
        let (image, display, options, key) =
            self.label_view(&preview.source, ctx, preview.real_size);
        if key != preview.texture_key {
            preview.texture.set(image, options);
            preview.texture_key = key;
            preview.display = display;
        }
        self.label_preview = Some(preview);
    }

    pub(super) fn show_label_preview(&mut self, ctx: &egui::Context) {
        if self.label_preview.is_none() {
            return;
        }
        self.refresh_texture(ctx);
        let Some(preview) = &self.label_preview else {
            return;
        };
        let image_size = preview.display;
        let texture_id = preview.texture.id();
        let title = preview.title.clone();
        let caption = format!(
            "{:.0} x {:.0} mm, {}, style tags: {}, {}",
            self.settings.label_width_mm,
            self.settings.label_height_mm,
            if self.settings.label_code == "qr" {
                "QR code"
            } else {
                "Code 128"
            },
            tag_style_name(preview.tag_style),
            if preview.real_size {
                "real size"
            } else {
                "enlarged"
            },
        );
        let (mut print, mut close, mut cycle, mut zoom) = (false, false, false, false);
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
                    zoom = ui.button("Real size / enlarged  [Z]").clicked();
                    close = ui.button("Close  [Esc]").clicked();
                });
            });
        let fresh = self
            .label_preview
            .as_ref()
            .is_some_and(|p| p.opened_frame == ctx.frame_nr());
        let (p, enter, esc, t, z) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::P) && !i.modifiers.command,
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::T) && !i.modifiers.command,
                i.key_pressed(egui::Key::Z) && !i.modifiers.command,
            )
        });
        let (p, enter, esc, t, z) = if fresh {
            (false, false, false, false, false)
        } else {
            (p, enter, esc, t, z)
        };
        if print || p || enter {
            self.print_preview();
        } else if cycle || t {
            self.cycle_preview_tags();
        } else if zoom || z {
            if let Some(preview) = &mut self.label_preview {
                preview.real_size = !preview.real_size;
            }
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
        let image = |style| app.render_label_bitmap(&content, style).unwrap().into_raw();
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
        let bitmap = app.render_label_bitmap(&content, None).unwrap();
        assert_eq!((bitmap.width(), bitmap.height()), (408, 304), "51 x 38 mm");
        app.settings.label_width_mm = 52.0;
        app.settings.label_height_mm = 39.0;
        let bitmap = app.render_label_bitmap(&content, None).unwrap();
        assert_eq!((bitmap.width(), bitmap.height()), (416, 312));
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

    // --- real size -------------------------------------------------------------------------

    fn close_to(a: f32, b: f32) -> bool {
        (a - b).abs() < 1.0
    }

    #[test]
    fn real_size_follows_the_physical_size_of_the_label() {
        // 96 points make an inch: 51 mm are 2.008 inch
        let size = real_size_points(51.0, 38.0, 96.0, 1.0);
        assert!(
            close_to(size.x, 192.76) && close_to(size.y, 143.62),
            "{size:?}"
        );
        let size = real_size_points(52.0, 39.0, 96.0, 1.0);
        assert!(
            close_to(size.x, 196.54) && close_to(size.y, 147.40),
            "{size:?}"
        );
        // a sharper screen setting means more points per inch, so a bigger picture
        assert!(close_to(real_size_points(51.0, 38.0, 120.0, 1.0).x, 240.94));
        // the interface zoom is taken out: with Ctrl + the points get bigger, so fewer are needed
        assert!(close_to(real_size_points(51.0, 38.0, 96.0, 2.0).x, 96.38));
        assert!(
            real_size_points(51.0, 38.0, 96.0, 0.0).x.is_finite(),
            "no division by zero"
        );
    }

    #[test]
    fn the_enlarged_view_fits_the_window_up_to_three_times() {
        assert_eq!(fit_scale(egui::vec2(10000.0, 10000.0), (408, 304)), 3.0);
        let scale = fit_scale(egui::vec2(800.0, 600.0), (408, 304));
        assert!(scale < 3.0 && scale > 1.0, "{scale}");
        assert!(408.0 * scale <= 800.0 * 0.8 + 0.5 && 304.0 * scale <= 600.0 * 0.7 + 0.5);
        assert_eq!(
            fit_scale(egui::vec2(10.0, 10.0), (408, 304)),
            0.5,
            "never tiny"
        );
        assert!(fit_scale(egui::vec2(800.0, 600.0), (0, 0)).is_finite());
    }

    #[test]
    fn the_preview_starts_at_real_size() {
        let mut app = collection_app();
        app.open_label_preview(LabelContent::from_stored(&stored_with_styles()), "t".into());
        let preview = app.label_preview.as_ref().unwrap();
        assert!(preview.real_size);
        assert!(
            close_to(preview.display.x, 192.76) && close_to(preview.display.y, 143.62),
            "{:?}",
            preview.display
        );
        // one screen pixel per picture pixel
        let [w, h] = preview.texture.size();
        assert!(
            (w as f32 - preview.display.x).abs() <= 1.0
                && (h as f32 - preview.display.y).abs() <= 1.0,
            "{w}x{h}"
        );
        assert!(w < 408, "smaller than the 203 dpi bitmap, not upscaled");
    }

    #[test]
    fn z_switches_between_real_size_and_enlarged() {
        let mut app = collection_app();
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[egui::Key::P], true);
        frame_with(&mut app, &[], false);
        let real = app.label_preview.as_ref().unwrap().display;
        frame_with(&mut app, &[egui::Key::Z], false);
        frame_with(&mut app, &[], false);
        let preview = app.label_preview.as_ref().unwrap();
        assert!(!preview.real_size);
        assert!(
            preview.display.x > real.x * 2.0,
            "{:?} vs {real:?}",
            preview.display
        );
        assert_eq!(
            preview.texture.size(),
            [408, 304],
            "enlarged: the bitmap itself, shown larger"
        );
        frame_with(&mut app, &[egui::Key::Z], false);
        frame_with(&mut app, &[], false);
        let preview = app.label_preview.as_ref().unwrap();
        assert!(preview.real_size && close_to(preview.display.x, real.x));
        assert!(app.label_preview.is_some(), "Z does not close or print");
    }

    #[test]
    fn real_size_stays_real_when_the_interface_is_zoomed() {
        let mut app = collection_app();
        app.open_label_preview(LabelContent::from_stored(&stored_with_styles()), "t".into());
        let before = app.label_preview.as_ref().unwrap().texture.size();
        app.repaint.set_zoom_factor(2.0);
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[], false);
        let preview = app.label_preview.as_ref().unwrap();
        // half the points, but twice the pixels per point: the same physical size
        assert!(close_to(preview.display.x, 96.38), "{:?}", preview.display);
        assert_eq!(
            preview.texture.size(),
            before,
            "the same number of screen pixels"
        );
        app.repaint.set_zoom_factor(1.0);
    }

    #[test]
    fn the_preview_scale_setting_changes_the_size() {
        let mut app = collection_app();
        app.settings.preview_ppi = 120.0;
        app.open_label_preview(LabelContent::from_stored(&stored_with_styles()), "t".into());
        assert!(close_to(
            app.label_preview.as_ref().unwrap().display.x,
            240.94
        ));
        // changing it while the window is open is picked up
        app.settings.preview_ppi = 96.0;
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[], false);
        assert!(close_to(
            app.label_preview.as_ref().unwrap().display.x,
            192.76
        ));
    }

    #[test]
    fn trying_another_tag_style_redraws_the_picture() {
        let mut app = collection_app();
        frame_with(&mut app, &[], false);
        frame_with(&mut app, &[egui::Key::P], true);
        frame_with(&mut app, &[], false);
        let before = app
            .label_preview
            .as_ref()
            .unwrap()
            .source
            .clone()
            .into_raw();
        frame_with(&mut app, &[egui::Key::T], false);
        frame_with(&mut app, &[], false);
        let preview = app.label_preview.as_ref().unwrap();
        assert_ne!(
            preview.source.as_raw(),
            &before,
            "the bitmap shows the tags now"
        );
        assert_ne!(
            preview.texture_key.pixels_per_point_milli, 0,
            "the texture was made again"
        );
    }
}
