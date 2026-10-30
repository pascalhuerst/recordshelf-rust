//! The label picture beside the record info: the label as it would be printed,
//! drawn from the same bitmap that goes to the printer, at its real size on the
//! screen so it can be compared with a label held next to it.

use super::*;
use crate::label::{self, TagStyle};
use image::{GrayImage, imageops};

/// The picture for the record that is shown. Made again when the record, the
/// label settings, the zoom or the screen scale change.
pub(super) struct LabelPanel {
    content: LabelContent,
    key: PanelKey,
    texture: egui::TextureHandle,
    display: egui::Vec2,
    caption: String,
}

/// Everything the picture depends on besides the record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelKey {
    pixels_per_point_milli: u32,
    zoom_milli: u32,
    ppi_centi: u32,
    width_centi_mm: u32,
    height_centi_mm: u32,
    qr: bool,
    tags: String,
    hidden: Vec<String>,
}

/// What to draw: the picture, its size on the screen in points, and a caption.
pub(super) struct LabelView {
    pub texture: egui::TextureId,
    pub size: egui::Vec2,
    pub caption: String,
    /// The current settings, for the toggles: "off", "outline" or "filled" ...
    pub tags_choice: String,
    /// ... and "barcode" or "qr".
    pub code_choice: String,
    /// Whether the record has any styles to show as tags.
    pub has_tags: bool,
    /// Names of the parts left out.
    pub hidden: Vec<String>,
}

/// A change made with the toggles beside the picture.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct LabelAction {
    pub tags: Option<String>,
    pub code: Option<String>,
    /// A part to switch on or off: (name, shown).
    pub part: Option<(String, bool)>,
}

/// Space around the label panel, in points.
const PANEL_PADDING: f32 = 16.0;
/// Space on each side of the line that splits the details.
const SPLIT_MARGIN: f32 = 14.0;
/// The record info needs at least this much width.
const MIN_INFO_WIDTH: f32 = 260.0;
/// The panel is at least this wide, for the toggles.
const PANEL_MIN_WIDTH: f32 = 230.0;

/// How a details area of `available` width is split: the left part (cover and
/// info) and the right part with the label panel. The line goes through the
/// middle of the window, `middle` points from the left edge of the area, but
/// the left part gets at least what the cover and info need. `None` if the
/// right part would be too small for the picture.
pub(super) fn split_widths(
    available: f32,
    cover_width: f32,
    picture_width: f32,
    middle: f32,
) -> Option<(f32, f32)> {
    let left = (middle - SPLIT_MARGIN).max(cover_width + 12.0 + MIN_INFO_WIDTH);
    let right = available - left;
    let needed = picture_width + 2.0 * PANEL_PADDING + 2.0 * SPLIT_MARGIN;
    (right >= needed).then_some((left, right))
}

/// Draws the details in two parts with a line between them: `left` (cover and
/// info) and `right` (the label panel). Returns what `right` returns.
pub(super) fn split_pane<R>(
    ui: &mut egui::Ui,
    left_width: f32,
    right_width: f32,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut result = None;
    let spacing = ui.spacing().item_spacing;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let top = ui.cursor().top();
        let left_x = ui.cursor().left();
        ui.allocate_ui_with_layout(
            egui::vec2(left_width, 1.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing = spacing;
                ui.set_max_width(left_width);
                // the part takes its whole width, whatever the content needs
                ui.set_min_width(left_width);
                left(ui);
            },
        );
        let line_x = left_x + left_width + SPLIT_MARGIN;
        ui.add_space(2.0 * SPLIT_MARGIN);
        let pane_width = (right_width - 3.0 * SPLIT_MARGIN).max(1.0);
        let inner = ui.allocate_ui_with_layout(
            egui::vec2(pane_width, 1.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing = spacing;
                ui.set_max_width(pane_width);
                right(ui)
            },
        );
        result = Some(inner.inner);
        let bottom = ui.min_rect().bottom();
        ui.painter().vline(
            line_x,
            top..=bottom,
            ui.visuals().widgets.noninteractive.bg_stroke,
        );
    });
    result.expect("the right part is always drawn")
}

/// The size in points of a label of `width_mm` x `height_mm` that looks as big
/// on the screen as the real label: `ppi` points make an inch (96 on a usual
/// screen), and the interface zoom (Ctrl +/-) is taken out again.
pub(super) fn real_size_points(width_mm: f32, height_mm: f32, ppi: f32, zoom: f32) -> egui::Vec2 {
    let points_per_mm = ppi / 25.4 / zoom.max(0.1);
    egui::vec2(width_mm * points_per_mm, height_mm * points_per_mm)
}

fn tag_style_name(style: Option<TagStyle>) -> &'static str {
    match style {
        None => "off",
        Some(TagStyle::Outline) => "outlined",
        Some(TagStyle::Filled) => "filled",
    }
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
        options.parts = settings.label_parts();
        label::render_with(content, size, &options)
    }

    fn panel_key(&self, ctx: &egui::Context) -> PanelKey {
        let s = &self.settings;
        PanelKey {
            pixels_per_point_milli: (ctx.pixels_per_point() * 1000.0).round() as u32,
            zoom_milli: (ctx.zoom_factor() * 1000.0).round() as u32,
            ppi_centi: (s.preview_ppi * 100.0).round() as u32,
            width_centi_mm: (s.label_width_mm * 100.0).round() as u32,
            height_centi_mm: (s.label_height_mm * 100.0).round() as u32,
            qr: s.label_code == "qr",
            tags: s.label_tags.clone(),
            hidden: s.label_hidden.clone(),
        }
    }

    /// Brings the picture in line with the record and the settings and returns
    /// what to draw; `None` content (no record shown) means no picture.
    pub(super) fn label_view(
        &mut self,
        ctx: &egui::Context,
        content: Option<LabelContent>,
    ) -> Option<LabelView> {
        let Some(content) = content else {
            self.label_panel = None;
            return None;
        };
        let key = self.panel_key(ctx);
        let up_to_date = self
            .label_panel
            .as_ref()
            .is_some_and(|p| p.key == key && p.content == content);
        if !up_to_date {
            let bitmap = match self.render_label_bitmap(&content, self.settings.tag_style()) {
                Ok(bitmap) => bitmap,
                Err(err) => {
                    self.set_error(format!("Could not draw the label: {err:#}"));
                    self.label_panel = None;
                    return None;
                }
            };
            let ppp = ctx.pixels_per_point();
            let points = real_size_points(
                self.settings.label_width_mm,
                self.settings.label_height_mm,
                self.settings.preview_ppi,
                ctx.zoom_factor(),
            );
            // one screen pixel per picture pixel: the 203 dpi bitmap is scaled down
            // smoothly to what it covers on the screen, and nothing is stretched
            let (w, h) = (
                (points.x * ppp).round().max(1.0) as u32,
                (points.y * ppp).round().max(1.0) as u32,
            );
            let small = imageops::resize(&bitmap, w, h, imageops::FilterType::Triangle);
            let image = egui::ColorImage::from_gray([w as usize, h as usize], small.as_raw());
            let display = egui::vec2(w as f32 / ppp, h as f32 / ppp);
            let caption = format!(
                "Label {:.0} x {:.0} mm, {}, tags: {}",
                self.settings.label_width_mm,
                self.settings.label_height_mm,
                if key.qr { "QR code" } else { "Code 128" },
                tag_style_name(self.settings.tag_style()),
            );
            match &mut self.label_panel {
                Some(panel) => {
                    panel.texture.set(image, egui::TextureOptions::NEAREST);
                    panel.content = content;
                    panel.key = key;
                    panel.display = display;
                    panel.caption = caption;
                }
                None => {
                    let texture =
                        ctx.load_texture("label-panel", image, egui::TextureOptions::NEAREST);
                    self.label_panel = Some(LabelPanel {
                        content,
                        key,
                        texture,
                        display,
                        caption,
                    });
                }
            }
        }
        self.label_panel.as_ref().map(|p| LabelView {
            texture: p.texture.id(),
            size: p.display,
            caption: p.caption.clone(),
            tags_choice: self.settings.label_tags.clone(),
            code_choice: self.settings.label_code.clone(),
            has_tags: !p.content.tags.is_empty(),
            hidden: self.settings.label_hidden.clone(),
        })
    }

    /// The label content of the release shown on the Add records tab: only for
    /// releases in the collection (that is where labels are printed).
    pub(super) fn shown_release_label(&self) -> Option<LabelContent> {
        let id = self.selected_id()?;
        let owned = self
            .ownership
            .get(&id)
            .is_some_and(|copies| !copies.is_empty());
        let release = self.releases.get(&id)?;
        owned.then(|| LabelContent::from_release(id, release))
    }
}

/// The label panel: the picture with its caption and the toggles for the style
/// tags and the code, with space around it. Returns what was changed.
pub(super) fn draw_label_panel(ui: &mut egui::Ui, view: &LabelView) -> LabelAction {
    let mut action = LabelAction::default();
    let content_width = view.size.x.max(PANEL_MIN_WIDTH);
    // the panel sits in the middle of the space it is given
    let indent = ((ui.available_width() - content_width - 2.0 * PANEL_PADDING) / 2.0).max(0.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(indent);
        egui::Frame::none()
            .inner_margin(egui::Margin::same(PANEL_PADDING))
            .show(ui, |ui| {
                ui.set_width(content_width);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.label(RichText::new("Label").strong());
                    ui.add_space(4.0);
                    egui::Frame::none()
                        .stroke(egui::Stroke::new(1.0_f32, Color32::GRAY))
                        .show(ui, |ui| {
                            ui.add(egui::Image::new((view.texture, view.size)));
                        });
                    ui.label(RichText::new(&view.caption).small());
                    ui.add_space(8.0);

                    let choose = |ui: &mut egui::Ui,
                                  caption: &str,
                                  current: &str,
                                  options: &[(&str, &str)]| {
                        let mut picked = None;
                        ui.horizontal(|ui| {
                            ui.label(format!("{caption}:"));
                            for (value, name) in options {
                                if ui.selectable_label(current == *value, *name).clicked()
                                    && current != *value
                                {
                                    picked = Some(value.to_string());
                                }
                            }
                        });
                        picked
                    };
                    action.tags = choose(
                        ui,
                        "Style tags",
                        &view.tags_choice,
                        &[
                            ("off", "Off"),
                            ("outline", "Outlined"),
                            ("filled", "Filled"),
                        ],
                    );
                    if view.tags_choice != "off" && !view.has_tags {
                        ui.label(RichText::new("(this record has no styles)").small().weak());
                    }
                    ui.add_space(4.0);
                    ui.label("Show:");
                    ui.horizontal_wrapped(|ui| {
                        for (name, title) in label::PARTS {
                            let mut shown = !view.hidden.iter().any(|h| h == name);
                            if ui.checkbox(&mut shown, title).changed() {
                                action.part = Some((name.to_string(), shown));
                            }
                        }
                    });
                    ui.add_space(4.0);
                    action.code = choose(
                        ui,
                        "Code",
                        &view.code_choice,
                        &[("barcode", "Barcode"), ("qr", "QR code")],
                    );
                });
            });
    });
    action
}

impl RecordShelfApp {
    /// Applies a choice made with the toggles: the label settings change (they
    /// are what the next print uses) and are remembered in the settings file.
    pub(super) fn apply_label_action(&mut self, action: LabelAction) {
        if action == LabelAction::default() {
            return;
        }
        if let Some(tags) = &action.tags {
            self.settings.label_tags = tags.clone();
        }
        if let Some(code) = &action.code {
            self.settings.label_code = code.clone();
        }
        if let Some((name, shown)) = &action.part {
            self.settings.label_hidden.retain(|h| h != name);
            if !shown {
                self.settings.label_hidden.push(name.clone());
            }
        }
        let (tags, code, hidden) = (
            self.settings.label_tags.clone(),
            self.settings.label_code.clone(),
            self.settings.label_hidden.clone(),
        );
        if let Some(path) = &self.config_path
            && let Err(err) = Settings::update_on_disk(path, |s| {
                s.label_tags = tags;
                s.label_code = code;
                s.label_hidden = hidden;
            })
        {
            self.set_error(format!("{err:#}"));
        }
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
        app.collection.focus_search = false;
        app
    }

    fn frame(app: &mut RecordShelfApp) {
        let ctx = app.repaint.clone();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| match app.tab {
                Tab::Add => app.ui_main(ui),
                Tab::Collection => app.ui_collection(ui),
                Tab::Stacks => app.ui_stacks(ui),
            });
        });
    }

    fn close_to(a: f32, b: f32) -> bool {
        (a - b).abs() < 1.0
    }

    fn content() -> LabelContent {
        LabelContent::from_stored(&stored_with_styles())
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
    fn no_record_means_no_picture() {
        let mut app = collection_app();
        let ctx = app.repaint.clone();
        assert!(app.label_view(&ctx, Some(content())).is_some());
        assert!(app.label_panel.is_some());
        assert!(app.label_view(&ctx, None).is_none());
        assert!(
            app.label_panel.is_none(),
            "the picture goes away with the record"
        );
    }

    #[test]
    fn the_picture_is_at_real_size_and_not_upscaled() {
        let mut app = collection_app();
        let ctx = app.repaint.clone();
        let view = app.label_view(&ctx, Some(content())).unwrap();
        assert!(
            close_to(view.size.x, 192.76) && close_to(view.size.y, 143.62),
            "{:?}",
            view.size
        );
        let [w, h] = app.label_panel.as_ref().unwrap().texture.size();
        assert!(
            (w as f32 - view.size.x).abs() <= 1.0 && (h as f32 - view.size.y).abs() <= 1.0,
            "one pixel per pixel: {w}x{h}"
        );
        assert!(w < 408, "smaller than the 203 dpi bitmap");
        assert!(
            view.caption.contains("51 x 38 mm")
                && view.caption.contains("Code 128")
                && view.caption.contains("tags: off"),
            "{}",
            view.caption
        );
    }

    #[test]
    fn the_picture_follows_the_label_settings() {
        let mut app = collection_app();
        let ctx = app.repaint.clone();
        let first = app.label_view(&ctx, Some(content())).unwrap();
        // a bigger label
        app.settings.label_width_mm = 52.0;
        app.settings.label_height_mm = 39.0;
        let second = app.label_view(&ctx, Some(content())).unwrap();
        assert!(second.size.x > first.size.x && close_to(second.size.x, 196.54));
        // QR instead of the barcode, and tags
        app.settings.label_code = "qr".into();
        app.settings.label_tags = "filled".into();
        let third = app.label_view(&ctx, Some(content())).unwrap();
        assert!(
            third.caption.contains("QR code") && third.caption.contains("tags: filled"),
            "{}",
            third.caption
        );
        // the screen scale
        app.settings.preview_ppi = 120.0;
        let fourth = app.label_view(&ctx, Some(content())).unwrap();
        assert!(
            close_to(fourth.size.x, 196.54 * 120.0 / 96.0),
            "{:?}",
            fourth.size
        );
    }

    #[test]
    fn the_picture_is_only_made_again_when_something_changed() {
        let mut app = collection_app();
        let ctx = app.repaint.clone();
        app.label_view(&ctx, Some(content())).unwrap();
        let handle = app.label_panel.as_ref().unwrap().texture.id();
        for _ in 0..3 {
            app.label_view(&ctx, Some(content())).unwrap();
        }
        assert_eq!(
            app.label_panel.as_ref().unwrap().texture.id(),
            handle,
            "the same texture is kept (and reused when it changes)"
        );
        let mut other = content();
        other.title = "Another".into();
        app.label_view(&ctx, Some(other.clone())).unwrap();
        assert_eq!(
            app.label_panel.as_ref().unwrap().content,
            other,
            "a different record is drawn"
        );
        assert_eq!(
            app.label_panel.as_ref().unwrap().texture.id(),
            handle,
            "into the same texture"
        );
    }

    #[test]
    fn real_size_stays_real_when_the_interface_is_zoomed() {
        let mut app = collection_app();
        let ctx = app.repaint.clone();
        app.label_view(&ctx, Some(content())).unwrap();
        let before = app.label_panel.as_ref().unwrap().texture.size();
        ctx.set_zoom_factor(2.0);
        // the zoom takes effect with the next frame
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let view = app.label_view(&ctx, Some(content())).unwrap();
        // half the points, but twice the pixels per point: the same physical size
        assert!(close_to(view.size.x, 96.38), "{:?}", view.size);
        assert_eq!(app.label_panel.as_ref().unwrap().texture.size(), before);
        ctx.set_zoom_factor(1.0);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
    }

    #[test]
    fn the_style_tags_setting_changes_the_picture() {
        let mut app = collection_app();
        let bitmap = |app: &RecordShelfApp| {
            app.render_label_bitmap(&content(), app.settings.tag_style())
                .unwrap()
                .into_raw()
        };
        let off = bitmap(&app);
        app.settings.label_tags = "outline".into();
        let outline = bitmap(&app);
        app.settings.label_tags = "filled".into();
        let filled = bitmap(&app);
        assert_ne!(off, outline);
        assert_ne!(outline, filled);
    }

    #[test]
    fn the_collection_tab_shows_the_picture_beside_the_details() {
        let mut app = collection_app();
        assert!(app.label_panel.is_none());
        frame(&mut app);
        frame(&mut app);
        assert!(
            app.label_panel.is_some(),
            "a record is shown: its label is too"
        );
        // the record goes away: so does the picture
        app.collection.details = None;
        frame(&mut app);
        assert!(app.label_panel.is_none());
    }

    #[test]
    fn the_add_tab_shows_it_for_a_shown_release_with_a_copy() {
        let mut app = app_ready_to_add();
        app.releases.insert(1, crate::discogs::Release::default());
        app.ownership.insert(1, vec![copy(10, 1)]);
        frame(&mut app);
        frame(&mut app);
        assert!(app.label_panel.is_some());
        assert!(app.shown_release_label().is_some());

        // a release we do not own gets no label
        app.ownership.insert(1, Vec::new());
        frame(&mut app);
        assert!(app.label_panel.is_none());
        assert!(app.shown_release_label().is_none());

        // collection status not known yet
        app.ownership.clear();
        assert!(app.shown_release_label().is_none());
        // nothing selected
        app.ownership.insert(1, vec![copy(10, 1)]);
        app.selected_result = None;
        assert!(app.shown_release_label().is_none());
        frame(&mut app);
        assert!(app.label_panel.is_none());
        // release data not loaded yet
        app.selected_result = Some(0);
        app.releases.clear();
        assert!(app.shown_release_label().is_none());
    }

    #[test]
    fn what_is_shown_is_what_p_prints() {
        // the panel is drawn from the same content and settings as a print job
        let mut app = app_ready_to_add();
        app.settings.label_tags = "outline".into();
        let release: crate::discogs::Release = serde_json::from_str(
            r#"{"title": "T", "artists": [{"id": 1, "name": "A"}], "styles": ["House"], "formats": [{"name": "Vinyl", "descriptions": ["12\""]}]}"#,
        )
        .unwrap();
        app.releases.insert(1, release);
        app.ownership.insert(1, vec![copy(10, 1)]);
        let shown = app.shown_release_label().unwrap();
        let job = app.print_job(shown.clone());
        assert_eq!(job.content, shown);
        assert_eq!(job.tag_style, Some(TagStyle::Outline));
        assert_eq!(shown.tags, ["House"]);
    }

    #[test]
    fn the_panel_can_be_drawn_with_a_qr_code_and_a_big_label() {
        let mut app = collection_app();
        app.settings.label_code = "qr".into();
        app.settings.label_width_mm = 56.0;
        app.settings.label_height_mm = 50.0;
        frame(&mut app);
        frame(&mut app);
        assert!(app.label_panel.is_some());
    }

    // --- layout --------------------------------------------------------------------------------

    /// A frame of the tab in a window of the given size; returns the rectangle of
    /// the label picture (if drawn) and the rectangles of all text.
    fn layout_frame(
        app: &mut RecordShelfApp,
        width: f32,
        height: f32,
    ) -> (Option<egui::Rect>, Vec<(String, egui::Rect)>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, height),
            )),
            ..Default::default()
        };
        let ctx = app.repaint.clone();
        let output = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| match app.tab {
                Tab::Add => app.ui_main(ui),
                Tab::Collection => app.ui_collection(ui),
                Tab::Stacks => app.ui_stacks(ui),
            });
        });
        let panel_texture = app.label_panel.as_ref().map(|p| p.texture.id());
        let (mut picture, mut texts) = (None, Vec::new());
        fn walk(
            shape: &egui::epaint::Shape,
            texture: Option<egui::TextureId>,
            picture: &mut Option<egui::Rect>,
            texts: &mut Vec<(String, egui::Rect)>,
        ) {
            match shape {
                egui::epaint::Shape::Rect(rect) if Some(rect.fill_texture_id) == texture => {
                    *picture = Some(rect.rect);
                }
                egui::epaint::Shape::Text(t) => {
                    texts.push((
                        t.galley.text().to_string(),
                        t.galley.rect.translate(t.pos.to_vec2()),
                    ));
                }
                egui::epaint::Shape::Vec(shapes) => {
                    shapes.iter().for_each(|s| walk(s, texture, picture, texts))
                }
                _ => {}
            }
        }
        for clipped in &output.shapes {
            walk(&clipped.shape, panel_texture, &mut picture, &mut texts);
        }
        (picture, texts)
    }

    fn long_record() -> Box<StoredRelease> {
        let mut record = stored_with_styles();
        record.tracks = (1..=6)
            .map(|i| crate::library::StoredTrack {
                position: Some(format!("A{i}")),
                title: format!("A fairly long track title number {i} (Extended Mix)"),
                duration_seconds: Some(300),
            })
            .collect();
        record
    }

    #[test]
    fn the_picture_sits_at_the_right_of_the_record_info() {
        let mut app = collection_app();
        app.collection.details = Some(long_record());
        layout_frame(&mut app, 1300.0, 900.0);
        let (picture, texts) = layout_frame(&mut app, 1300.0, 900.0);
        let picture = picture.expect("the label picture is drawn");
        assert!(
            picture.max.x <= 1300.0 + 0.5,
            "inside the window: {picture:?}"
        );
        assert!(
            picture.min.x > 400.0,
            "beyond the info (the list is at the left): {picture:?}"
        );
        assert!(
            picture.max.x < 1300.0 - 8.0 && picture.min.y > 8.0,
            "out of the corner: {picture:?}"
        );
        let info: Vec<_> = texts
            .iter()
            .filter(|(t, _)| {
                t.starts_with("Title:") || t.starts_with("A3") || t.starts_with("  A3")
            })
            .collect();
        assert!(
            !info.is_empty(),
            "the info is drawn: {:?}",
            texts.iter().map(|t| &t.0).collect::<Vec<_>>()
        );
        for (text, rect) in &info {
            assert!(
                rect.max.x <= picture.min.x + 0.5,
                "{text:?} ({rect:?}) does not run into the picture {picture:?}"
            );
        }
    }

    #[test]
    fn long_info_wraps_instead_of_running_under_the_picture() {
        let mut app = collection_app();
        let mut record = long_record();
        record.title = "A really very long title that goes on and on and on and on and on".into();
        record.styles = (1..=14).map(|i| format!("Style number {i}")).collect();
        app.collection.details = Some(record);
        layout_frame(&mut app, 1100.0, 900.0);
        let (picture, texts) = layout_frame(&mut app, 1100.0, 900.0);
        let picture = picture.expect("drawn");
        for (text, rect) in &texts {
            // everything that belongs to the info (below the list) stays left of the picture
            if rect.min.y > picture.min.y - 1.0
                && rect.max.y < picture.max.y + 1.0
                && rect.min.x < picture.min.x
            {
                assert!(
                    rect.max.x <= picture.min.x + 0.5,
                    "{text:?} {rect:?} overlaps {picture:?}"
                );
            }
        }
    }

    #[test]
    fn in_a_narrow_window_the_picture_gives_way_to_the_info() {
        let mut app = collection_app();
        app.collection.details = Some(long_record());
        layout_frame(&mut app, 640.0, 900.0);
        let (picture, texts) = layout_frame(&mut app, 640.0, 900.0);
        assert!(picture.is_none(), "no room beside the info: {picture:?}");
        assert!(
            texts.iter().any(|(t, _)| t.starts_with("Title:")),
            "the info is still there"
        );
    }

    #[test]
    fn the_picture_is_there_on_the_add_tab_too() {
        let mut app = app_ready_to_add();
        app.releases.insert(
            1,
            serde_json::from_str(
                r#"{"id": 1, "title": "Wonderscape", "artists": [{"id": 1, "name": "Altone"}]}"#,
            )
            .unwrap(),
        );
        app.ownership.insert(1, vec![copy(10, 1)]);
        layout_frame(&mut app, 1300.0, 900.0);
        let (picture, _) = layout_frame(&mut app, 1300.0, 900.0);
        let picture = picture.expect("drawn on the Add records tab");
        assert!(
            picture.min.x > 400.0 && picture.max.x < 1300.0 - 8.0,
            "{picture:?}"
        );
    }

    #[test]
    fn the_picture_is_in_the_middle_of_the_right_half_of_the_window() {
        for app in [
            {
                let mut a = collection_app();
                a.collection.details = Some(long_record());
                a
            },
            {
                let mut a = app_ready_to_add();
                a.releases.insert(
                    1,
                    serde_json::from_str(r#"{"id": 1, "title": "W", "artists": []}"#).unwrap(),
                );
                a.ownership.insert(1, vec![copy(10, 1)]);
                a
            },
        ] {
            let mut app = app;
            layout_frame(&mut app, 1400.0, 900.0);
            let (picture, _) = layout_frame(&mut app, 1400.0, 900.0);
            let picture = picture.expect("drawn");
            let centre = picture.center().x;
            assert!(
                (centre - 1050.0).abs() < 30.0,
                "centre {centre} should be near 1050, the middle of the right half: {picture:?}"
            );
        }
    }

    #[test]
    fn the_toggles_are_beside_the_picture() {
        let mut app = collection_app();
        app.collection.details = Some(long_record());
        layout_frame(&mut app, 1300.0, 900.0);
        let (picture, texts) = layout_frame(&mut app, 1300.0, 900.0);
        let picture = picture.unwrap();
        for name in ["Outlined", "Filled", "Barcode", "QR code"] {
            let (_, rect) = texts
                .iter()
                .find(|(t, _)| t == name)
                .unwrap_or_else(|| panic!("{name} toggle"));
            assert!(
                rect.min.x >= picture.min.x - 0.5,
                "{name} aligned with the picture"
            );
        }
    }

    #[test]
    fn toggles_change_and_remember_the_label_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = \"keep\"\n").unwrap();
        let mut app = collection_app();
        app.config_path = Some(path.clone());
        app.apply_label_action(LabelAction {
            tags: Some("filled".into()),
            code: Some("qr".into()),
            part: Some(("tracks".into(), false)),
        });
        assert_eq!(app.settings.label_hidden, ["tracks"]);
        assert!(!app.settings.label_parts().tracks);
        assert_eq!(app.settings.label_tags, "filled");
        assert_eq!(app.settings.label_code, "qr");
        let saved = Settings::load(&path).unwrap();
        assert_eq!(saved.label_tags, "filled");
        assert_eq!(saved.label_code, "qr");
        assert_eq!(saved.token, "keep");
        assert_eq!(saved.label_hidden, ["tracks"]);
        app.apply_label_action(LabelAction {
            part: Some(("tracks".into(), true)),
            ..LabelAction::default()
        });
        assert!(app.settings.label_hidden.is_empty());
        app.apply_label_action(LabelAction {
            tags: Some("filled".into()),
            ..LabelAction::default()
        });
        app.apply_label_action(LabelAction::default());
        assert_eq!(app.settings.label_tags, "filled");
    }

    #[test]
    fn leaving_a_part_out_redraws_the_picture() {
        let mut app = collection_app();
        app.collection.details = Some(long_record());
        let bitmap = |app: &RecordShelfApp| {
            app.render_label_bitmap(&crate::label::sample_content(), None)
                .unwrap()
        };
        let full = bitmap(&app);
        app.apply_label_action(LabelAction {
            part: Some(("artist".into(), false)),
            ..LabelAction::default()
        });
        assert_ne!(bitmap(&app).as_raw(), full.as_raw());
        layout_frame(&mut app, 1300.0, 900.0);
        let (_, texts) = layout_frame(&mut app, 1300.0, 900.0);
        assert!(texts.iter().any(|(t, _)| t == "Artist"), "checkbox shown");
        assert!(app.label_panel.as_ref().unwrap().key.hidden == ["artist"]);
    }

    #[test]
    fn the_split_needs_room() {
        assert!(split_widths(1300.0, 220.0, 193.0, 650.0).is_some());
        assert!(split_widths(700.0, 220.0, 193.0, 350.0).is_none());
        let (left, right) = split_widths(1300.0, 220.0, 193.0, 650.0).unwrap();
        assert!((left + right - 1300.0).abs() < 0.01);
    }
}
