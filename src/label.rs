//! The sticker for a record sleeve: size, label / catalog number, artist,
//! title, a few tracks and a code (the Discogs release id, which is also the
//! primary key of our database) as QR code or Code 128 barcode.
//!
//! The label is drawn into a 1-bit bitmap at the printer's resolution (the
//! ZD411 prints 203 dpi, 8 dots per mm). The same bitmap is used for the
//! preview and, later, for printing, so what is seen is what comes out.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{Context, Result};
use barcoders::sym::code128::Code128;
use image::{GrayImage, Luma};
use qrcode::{Color, EcLevel, QrCode};

use crate::discogs::Release;

/// Dots per inch of the label printer (Zebra ZD411, 203 dpi version).
pub const DPI: f32 = 203.0;

const REGULAR: &[u8] = include_bytes!("../assets/fonts/DejaVuSansCondensed.ttf");
const BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSansCondensed-Bold.ttf");

const BLACK: Luma<u8> = Luma([0]);
const WHITE: Luma<u8> = Luma([255]);

/// Margin around the content in dots (2 mm). Printers are unreliable at the
/// very edge of the label.
const MARGIN: u32 = 16;

pub fn mm_to_dots(mm: f32) -> u32 {
    (mm * DPI / 25.4).round() as u32
}

/// Label size in dots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelSize {
    pub width: u32,
    pub height: u32,
}

impl LabelSize {
    pub fn from_mm(width_mm: f32, height_mm: f32) -> Self {
        Self {
            width: mm_to_dots(width_mm),
            height: mm_to_dots(height_mm),
        }
    }
}

/// How genres and styles are drawn as tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagStyle {
    /// A rounded frame with black text.
    Outline,
    /// A black rounded area with white text.
    Filled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeKind {
    Qr,
    Barcode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelTrack {
    pub position: String,
    pub title: String,
}

/// Everything printed on a label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelContent {
    pub release_id: u32,
    /// Diameter in inches (7, 10, 12) if known.
    pub size_inch: Option<u8>,
    pub label: String,
    pub catno: String,
    pub artist: String,
    pub title: String,
    /// The styles, drawn as tags when the options ask for them.
    pub tags: Vec<String>,
    pub tracks: Vec<LabelTrack>,
}

impl LabelContent {
    pub fn from_release(id: u32, release: &Release) -> Self {
        let first_label = release.labels.iter().flatten().next();
        let descriptions: Vec<&str> = release
            .formats
            .iter()
            .flatten()
            .flat_map(|f| f.descriptions.iter().flatten())
            .map(String::as_str)
            .collect();
        let size_inch = [(7u8, "7\""), (10, "10\""), (12, "12\"")]
            .into_iter()
            .find(|(_, tag)| descriptions.contains(tag))
            .map(|(size, _)| size)
            // an LP without a stated size is a 12 inch
            .or_else(|| descriptions.contains(&"LP").then_some(12));
        Self {
            release_id: id,
            size_inch,
            label: first_label.and_then(|l| l.name.clone()).unwrap_or_default(),
            catno: first_label
                .and_then(|l| l.catno.clone())
                .filter(|c| !c.eq_ignore_ascii_case("none"))
                .unwrap_or_default(),
            artist: crate::catalogue::join_artists(
                release
                    .artists
                    .iter()
                    .flatten()
                    .filter_map(|a| Some((a.name.as_deref()?, a.join.as_deref()))),
            ),
            title: release.title.clone().unwrap_or_default(),
            tags: tags_of(release.styles.iter().flatten()),
            tracks: release
                .tracklist
                .iter()
                .flatten()
                .filter(|t| t.title.as_deref().is_some_and(|s| !s.is_empty()))
                .map(|t| LabelTrack {
                    position: t.position.clone().unwrap_or_default(),
                    title: t.title.clone().unwrap_or_default(),
                })
                .collect(),
        }
    }
}

impl LabelContent {
    /// The label of a record that is in our database.
    pub fn from_stored(record: &crate::library::StoredRelease) -> Self {
        let (label, catno) = record.labels.first().cloned().unwrap_or_default();
        Self {
            release_id: record.id,
            size_inch: record
                .formats
                .iter()
                .find_map(|f| f.size.and_then(|s| u8::try_from(s).ok())),
            label,
            catno,
            artist: record.artist_display(),
            title: record.title.clone(),
            tags: tags_of(record.styles.iter()),
            tracks: record
                .tracks
                .iter()
                .map(|t| LabelTrack {
                    position: t.position.clone().unwrap_or_default(),
                    title: t.title.clone(),
                })
                .collect(),
        }
    }
}

/// The styles ("Dub Techno") as tags, without empty entries and repeats. The
/// genres are left out: they are broad ("Electronic") and say little.
fn tags_of<'a>(styles: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in styles {
        let tag = tag.trim();
        if !tag.is_empty() && !tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
            tags.push(tag.to_string());
        }
    }
    tags
}

struct Fonts {
    regular: FontRef<'static>,
    bold: FontRef<'static>,
}

impl Fonts {
    fn load() -> Result<Self> {
        Ok(Self {
            regular: FontRef::try_from_slice(REGULAR).context("load regular font")?,
            bold: FontRef::try_from_slice(BOLD).context("load bold font")?,
        })
    }
}

fn text_width(font: &FontRef, px: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(px));
    let mut width = 0.0;
    let mut previous = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(prev) = previous {
            width += scaled.kern(prev, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

/// Draws `text` in black with its top at `top`.
fn draw_text(img: &mut GrayImage, font: &FontRef, px: f32, x: f32, top: f32, text: &str) {
    draw_text_in(img, font, px, x, top, text, BLACK);
}

/// Draws `text` in `color` (white for text on a black area).
fn draw_text_in(
    img: &mut GrayImage,
    font: &FontRef,
    px: f32,
    x: f32,
    top: f32,
    text: &str,
    color: Luma<u8>,
) {
    let scale = PxScale::from(px);
    let scaled = font.as_scaled(scale);
    let baseline = top + scaled.ascent();
    let mut cursor = x;
    let mut previous = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(prev) = previous {
            cursor += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(cursor, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                if coverage > 0.45 {
                    let (x, y) = (
                        bounds.min.x as i32 + gx as i32,
                        bounds.min.y as i32 + gy as i32,
                    );
                    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
                        img.put_pixel(x as u32, y as u32, color);
                    }
                }
            });
        }
        cursor += scaled.h_advance(id);
        previous = Some(id);
    }
}

/// Shortens `text` with an ellipsis until it fits `max_width`.
fn ellipsize(font: &FontRef, px: f32, text: &str, max_width: f32) -> String {
    if text_width(font, px, text) <= max_width {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate: String = chars.iter().collect::<String>().trim_end().to_string() + "…";
        if text_width(font, px, &candidate) <= max_width {
            return candidate;
        }
    }
    String::new()
}

/// Largest size between `max_px` and `min_px` at which `text` fits on one
/// line; below `min_px` it is ellipsized.
fn fit_line(font: &FontRef, max_px: f32, min_px: f32, text: &str, max_width: f32) -> (f32, String) {
    let mut px = max_px;
    while px > min_px && text_width(font, px, text) > max_width {
        px -= 1.0;
    }
    (px, ellipsize(font, px, text, max_width))
}

/// "Label / CATNO" on one line: shrinks the text, and if that is not enough
/// shortens the label name, never the catalog number.
fn fit_label_line(
    font: &FontRef,
    max_px: f32,
    min_px: f32,
    label: &str,
    catno: &str,
    max_width: f32,
) -> (f32, String) {
    let join = |label: &str| match (label.is_empty(), catno.is_empty()) {
        (false, false) => format!("{label} / {catno}"),
        (false, true) => label.to_string(),
        (true, false) => catno.to_string(),
        (true, true) => String::new(),
    };
    let full = join(label);
    let mut px = max_px;
    while px > min_px && text_width(font, px, &full) > max_width {
        px -= 1.0;
    }
    if text_width(font, px, &full) <= max_width || label.is_empty() {
        return (px, ellipsize(font, px, &full, max_width));
    }
    let suffix = if catno.is_empty() {
        String::new()
    } else {
        format!(" / {catno}")
    };
    let room = (max_width - text_width(font, px, &suffix)).max(0.0);
    let short = ellipsize(font, px, label, room);
    if short.is_empty() {
        return (px, ellipsize(font, px, catno, max_width));
    }
    (px, format!("{short}{suffix}"))
}

/// Word-wraps `text` into at most `max_lines` lines, ellipsizing the last one.
fn wrap(font: &FontRef, px: f32, text: &str, max_width: f32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if text_width(font, px, &candidate) <= max_width {
            current = candidate;
        } else if current.is_empty() {
            // a single word wider than the line
            current = ellipsize(font, px, word, max_width);
        } else {
            lines.push(std::mem::take(&mut current));
            if lines.len() == max_lines {
                break;
            }
            current = word.to_string();
        }
        if words.peek().is_none() {
            lines.push(std::mem::take(&mut current));
        }
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
    }
    // anything that did not fit ends the last line with an ellipsis
    let used: usize = lines.iter().map(|l| l.split_whitespace().count()).sum();
    if used < text.split_whitespace().count()
        && let Some(last) = lines.last_mut()
    {
        let with_dots = format!("{last}…");
        *last = ellipsize(font, px, &with_dots, max_width);
        if !last.ends_with('…') {
            *last = ellipsize(font, px, &format!("{last} …"), max_width);
        }
    }
    lines
}

/// A code as black/white modules, `true` = black, including quiet zone.
struct CodeBitmap {
    width: u32,
    height: u32,
    black: Vec<bool>,
}

impl CodeBitmap {
    /// Turned a quarter clockwise: a barcode that stands along a side.
    fn rotated(&self) -> CodeBitmap {
        let (width, height) = (self.height, self.width);
        let mut black = vec![false; (width * height) as usize];
        for y in 0..self.height {
            for x in 0..self.width {
                if self.black[(y * self.width + x) as usize] {
                    let (nx, ny) = (self.height - 1 - y, x);
                    black[(ny * width + nx) as usize] = true;
                }
            }
        }
        CodeBitmap {
            width,
            height,
            black,
        }
    }

    fn draw(&self, img: &mut GrayImage, left: u32, top: u32) {
        for y in 0..self.height {
            for x in 0..self.width {
                if self.black[(y * self.width + x) as usize] {
                    img.put_pixel(left + x, top + y, BLACK);
                }
            }
        }
    }
}

/// QR code of the bare id: numeric mode, version 1 (21x21 modules) for ids up
/// to 34 digits. `module` is the size of one module in dots, `quiet` the quiet
/// zone in modules.
fn qr_code(id: u32, module: u32, quiet: u32) -> Result<CodeBitmap> {
    let code = QrCode::with_error_correction_level(id.to_string(), EcLevel::M)
        .context("encode QR code")?;
    let modules = code.width() as u32;
    let side = (modules + 2 * quiet) * module;
    let colors = code.to_colors();
    let mut black = vec![false; (side * side) as usize];
    for my in 0..modules {
        for mx in 0..modules {
            if colors[(my * modules + mx) as usize] == Color::Dark {
                for dy in 0..module {
                    for dx in 0..module {
                        let (x, y) = ((mx + quiet) * module + dx, (my + quiet) * module + dy);
                        black[(y * side + x) as usize] = true;
                    }
                }
            }
        }
    }
    Ok(CodeBitmap {
        width: side,
        height: side,
        black,
    })
}

/// Code 128 barcode of the bare id (digits only, so code set C is used: two
/// digits per symbol, which keeps the code short). `module` is the width of
/// the narrowest bar in dots, the quiet zone is 10 modules on each side.
fn barcode(id: u32, module: u32, height: u32) -> Result<CodeBitmap> {
    let digits = id.to_string();
    // set C needs an even number of digits; an odd one starts in set B
    let data = if digits.len().is_multiple_of(2) {
        format!("\u{0106}{digits}")
    } else {
        format!("\u{0181}{}\u{0106}{}", &digits[..1], &digits[1..])
    };
    let modules = Code128::new(data)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("encode barcode")?
        .encode();
    let quiet = 10 * module;
    let width = modules.len() as u32 * module + 2 * quiet;
    let mut black = vec![false; (width * height) as usize];
    for (i, m) in modules.iter().enumerate() {
        if *m == 1 {
            for dx in 0..module {
                let x = quiet + i as u32 * module + dx;
                for y in 0..height {
                    black[(y * width + x) as usize] = true;
                }
            }
        }
    }
    Ok(CodeBitmap {
        width,
        height,
        black,
    })
}

/// The parts of a label that can be left out: (name in the settings, name to show).
pub const PARTS: [(&str, &str); 5] = [
    ("header", "Size, label, cat. no."),
    ("artist", "Artist"),
    ("title", "Title"),
    ("tracks", "Tracks"),
    ("code", "Code"),
];

/// Which parts of a label are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelParts {
    pub header: bool,
    pub artist: bool,
    pub title: bool,
    pub tracks: bool,
    pub code: bool,
}

impl Default for LabelParts {
    fn default() -> Self {
        Self {
            header: true,
            artist: true,
            title: true,
            tracks: true,
            code: true,
        }
    }
}

impl LabelParts {
    /// Everything except the parts named in `hidden` (unknown names are ignored).
    pub fn without(hidden: &[String]) -> Self {
        let shown = |name: &str| !hidden.iter().any(|h| h == name);
        Self {
            header: shown("header"),
            artist: shown("artist"),
            title: shown("title"),
            tracks: shown("tracks"),
            code: shown("code"),
        }
    }
}

/// Where a barcode goes (a QR code is always in the bottom right corner).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodeSide {
    /// Along the bottom edge, in the middle.
    #[default]
    Bottom,
    /// Standing along the right edge, in the middle; the text keeps to the left of it.
    Right,
}

/// How the code is drawn, besides its kind.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelOptions {
    pub kind: CodeKind,
    /// Where a barcode is.
    pub code_side: CodeSide,
    /// Height of the bars of a barcode in dots.
    pub barcode_height: u32,
    /// Width of the narrowest bar of a barcode in dots.
    pub barcode_module: u32,
    /// Small text in the bottom left corner (used by test labels).
    pub caption: Option<String>,
    /// Draw genres and styles as tags in this style (`None`: no tags).
    pub tag_style: Option<TagStyle>,
    /// Which parts are drawn.
    pub parts: LabelParts,
    /// All text is this many times as big as normal (1.0).
    pub font_scale: f32,
    /// Make the text as big as the content allows (up to `MAX_FONT_SCALE`);
    /// `font_scale` is then only the least size.
    pub font_auto: bool,
}

/// The text sizes the settings allow, as a factor of the normal size.
pub const MIN_FONT_SCALE: f32 = 0.6;
pub const MAX_FONT_SCALE: f32 = 3.0;

/// What did not fit on a label: the sign that the text should be smaller.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Overflow {
    pub tracks_hidden: usize,
    pub tags_hidden: usize,
    pub title_cut: bool,
}

impl Overflow {
    pub fn none(&self) -> bool {
        *self == Overflow::default()
    }
}

impl LabelOptions {
    pub fn new(kind: CodeKind) -> Self {
        Self {
            kind,
            code_side: CodeSide::Bottom,
            barcode_height: 24, // 3 mm; the printed tests read fine down to 2 mm
            barcode_module: 2,
            caption: None,
            tag_style: None,
            parts: LabelParts::default(),
            font_scale: 1.0,
            font_auto: false,
        }
    }
}

/// Whether the point (pixel centre) lies inside a rounded rectangle of size w x h.
fn in_rounded_rect(px: f32, py: f32, w: f32, h: f32, r: f32) -> bool {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let (cx, cy) = (px.clamp(r, w - r), py.clamp(r, h - r));
    let (dx, dy) = (px - cx, py - cy);
    px >= 0.0 && py >= 0.0 && px <= w && py <= h && dx * dx + dy * dy <= r * r
}

/// A rounded rectangle, as an area or as a frame `thickness` dots wide.
fn draw_pill(img: &mut GrayImage, x: u32, y: u32, w: u32, h: u32, filled: bool, thickness: f32) {
    let (wf, hf) = (w as f32, h as f32);
    let radius = (hf / 2.0).min(7.0);
    for yy in 0..h {
        for xx in 0..w {
            let (px, py) = (xx as f32 + 0.5, yy as f32 + 0.5);
            if !in_rounded_rect(px, py, wf, hf, radius) {
                continue;
            }
            let inside_frame = !filled
                && in_rounded_rect(
                    px - thickness,
                    py - thickness,
                    wf - 2.0 * thickness,
                    hf - 2.0 * thickness,
                    radius - thickness,
                );
            if !inside_frame && x + xx < img.width() && y + yy < img.height() {
                img.put_pixel(x + xx, y + yy, BLACK);
            }
        }
    }
}

const TAG_PX_BASE: f32 = 15.0;
const TAG_HEIGHT_BASE: f32 = 22.0;
const TAG_PADDING_BASE: f32 = 8.0;
const TAG_GAP_BASE: f32 = 5.0;

/// One row of tags starting at `y`: as many as fit `max_width`; the rest is
/// counted ("+2"). Returns the height used (0 without tags).
fn draw_tags(
    img: &mut GrayImage,
    fonts: &Fonts,
    (left, y): (f32, f32),
    max_width: f32,
    tags: &[String],
    style: TagStyle,
    scale: f32,
) -> (f32, usize) {
    let (tag_px, tag_height, tag_padding, tag_gap) = (
        TAG_PX_BASE * scale,
        TAG_HEIGHT_BASE * scale,
        TAG_PADDING_BASE * scale,
        TAG_GAP_BASE * scale,
    );
    if tags.is_empty() {
        return (0.0, 0);
    }
    let filled = style == TagStyle::Filled;
    // white on black looks thin in the normal weight
    let font = if filled { &fonts.bold } else { &fonts.regular };
    let widths: Vec<f32> = tags
        .iter()
        .map(|t| text_width(font, tag_px, t) + 2.0 * tag_padding)
        .collect();
    // how many fit, leaving room for a "+N" if some do not
    let marker_width = |hidden: usize| text_width(&fonts.bold, tag_px, &format!("+{hidden}")) + 4.0;
    let mut shown = 0;
    let mut used = 0.0;
    for (i, w) in widths.iter().enumerate() {
        let needed = used + w + if i == 0 { 0.0 } else { tag_gap };
        let hidden_after = tags.len() - i - 1;
        let reserve = if hidden_after > 0 {
            tag_gap + marker_width(hidden_after)
        } else {
            0.0
        };
        if needed + reserve > max_width && i > 0 {
            break;
        }
        used = needed;
        shown = i + 1;
    }
    // a single tag wider than the row is shortened
    let mut x = left;
    for (i, tag) in tags.iter().take(shown).enumerate() {
        let mut label = tag.clone();
        let mut width = widths[i];
        if width > max_width {
            label = ellipsize(font, tag_px, tag, max_width - 2.0 * tag_padding);
            width = text_width(font, tag_px, &label) + 2.0 * tag_padding;
        }
        draw_pill(
            img,
            x as u32,
            y as u32,
            width.ceil() as u32,
            tag_height as u32,
            filled,
            2.0,
        );
        let text_top = y + (tag_height - tag_px * 1.2) / 2.0 - 0.5;
        let color = if filled { WHITE } else { BLACK };
        draw_text_in(img, font, tag_px, x + tag_padding, text_top, &label, color);
        x += width + tag_gap;
    }
    if shown < tags.len() {
        let marker = format!("+{}", tags.len() - shown);
        let text_top = y + (tag_height - tag_px * 1.2) / 2.0 - 0.5;
        draw_text(img, &fonts.bold, tag_px, x, text_top, &marker);
    }
    (tag_height, tags.len() - shown)
}

/// Draws the label. Black pixels are 0, the rest 255.
pub fn render(content: &LabelContent, size: LabelSize, kind: CodeKind) -> Result<GrayImage> {
    render_with(content, size, &LabelOptions::new(kind))
}

pub fn render_with(
    content: &LabelContent,
    size: LabelSize,
    options: &LabelOptions,
) -> Result<GrayImage> {
    render_fitted(content, size, options).map(|(img, _)| img)
}

/// Draws the label and tells the text size used (a factor of the normal size).
/// With `font_auto` that is the biggest size at which everything fits (not
/// less than `font_scale`); otherwise `font_scale`.
pub fn render_fitted(
    content: &LabelContent,
    size: LabelSize,
    options: &LabelOptions,
) -> Result<(GrayImage, f32)> {
    let least = options.font_scale.clamp(MIN_FONT_SCALE, MAX_FONT_SCALE);
    if !options.font_auto {
        return render_scaled(content, size, options, least).map(|(img, _)| (img, least));
    }
    // from big to small: the first size where nothing is left out
    let mut scale = MAX_FONT_SCALE;
    while scale > least {
        let (img, overflow) = render_scaled(content, size, options, scale)?;
        if overflow.none() {
            return Ok((img, scale));
        }
        scale = ((scale - 0.05) * 100.0).round() / 100.0;
    }
    render_scaled(content, size, options, least).map(|(img, _)| (img, least))
}

fn render_scaled(
    content: &LabelContent,
    size: LabelSize,
    options: &LabelOptions,
    scale: f32,
) -> Result<(GrayImage, Overflow)> {
    let mut overflow = Overflow::default();
    let kind = options.kind;
    let fonts = Fonts::load()?;
    let mut img = GrayImage::from_pixel(size.width, size.height, WHITE);
    let left = MARGIN as f32;
    let mut y = MARGIN as f32 - 2.0;

    // Where the code goes decides how much room is left for the tracks.
    let side_right =
        kind == CodeKind::Barcode && options.code_side == CodeSide::Right && options.parts.code;
    let code = match kind {
        CodeKind::Qr => qr_code(content.release_id, 4, 2)?,
        CodeKind::Barcode => barcode(
            content.release_id,
            options.barcode_module,
            options.barcode_height,
        )?,
    };
    let code = if side_right { code.rotated() } else { code };
    let (code_left, code_top) = match kind {
        // standing at the right edge, in the middle
        CodeKind::Barcode if side_right => (
            size.width - MARGIN + 4 - code.width,
            size.height.saturating_sub(code.height) / 2,
        ),
        // bottom right corner; the quiet zone may overlap the margin
        CodeKind::Qr => (
            size.width - MARGIN + 8 - code.width,
            size.height - MARGIN + 8 - code.height,
        ),
        // centred along the bottom
        CodeKind::Barcode => (
            (size.width - code.width) / 2,
            size.height - MARGIN + 4 - code.height,
        ),
    };

    // the text keeps left of a barcode that stands at the right edge
    let inner_width = (size.width - 2 * MARGIN) as f32
        - if side_right {
            code.width as f32 + 6.0
        } else {
            0.0
        };

    // Header: size on the left, label / catalog number on the right.
    let parts = options.parts;
    let header_px = 21.0 * scale;
    let size_text = content
        .size_inch
        .map(|s| format!("{s}\""))
        .unwrap_or_default();
    let size_width = text_width(&fonts.bold, header_px, &size_text);
    if parts.header && !size_text.is_empty() {
        draw_text(&mut img, &fonts.bold, header_px, left, y, &size_text);
    }
    if parts.header && !(content.label.is_empty() && content.catno.is_empty()) {
        let room = inner_width - size_width - 12.0;
        let (px, line) = fit_label_line(
            &fonts.regular,
            header_px,
            15.0 * scale,
            &content.label,
            &content.catno,
            room,
        );
        let width = text_width(&fonts.regular, px, &line);
        draw_text(
            &mut img,
            &fonts.regular,
            px,
            left + inner_width - width,
            y,
            &line,
        );
    }
    if parts.header {
        y += header_px * 1.25 + 4.0 * scale;
    }

    // Artist: as large as fits on one line.
    if parts.artist && !content.artist.is_empty() {
        let (px, line) = fit_line(
            &fonts.bold,
            32.0 * scale,
            20.0 * scale,
            &content.artist,
            inner_width,
        );
        draw_text(&mut img, &fonts.bold, px, left, y, &line);
        y += px * 1.2 + 2.0 * scale;
    }

    // Title: up to two lines.
    if parts.title && !content.title.is_empty() {
        let px = 22.0 * scale;
        let lines = wrap(&fonts.regular, px, &content.title, inner_width, 2);
        overflow.title_cut =
            lines.last().is_some_and(|l| l.ends_with('…')) && !content.title.ends_with('…');
        for line in lines {
            draw_text(&mut img, &fonts.regular, px, left, y, &line);
            y += px * 1.2;
        }
        y += 4.0 * scale;
    }

    // Genres and styles as tags.
    if let Some(style) = options.tag_style {
        let (used, hidden) = draw_tags(
            &mut img,
            &fonts,
            (left, y),
            inner_width,
            &content.tags,
            style,
            scale,
        );
        overflow.tags_hidden = hidden;
        if used > 0.0 {
            y += used + 5.0 * scale;
        }
    }

    // Tracks: as many as fit above (and beside) the code.
    let tracks: &[LabelTrack] = if parts.tracks { &content.tracks } else { &[] };
    let track_px: f32 = 19.0 * scale;
    let line_height = (track_px * 1.25).round();
    let position_width = tracks
        .iter()
        .map(|t| text_width(&fonts.bold, track_px, &t.position))
        .fold(0.0, f32::max);
    let bottom = (size.height - MARGIN) as f32;
    // without the code the whole height is free
    let code_draw_top = code_top;
    let code_top = if parts.code && !side_right {
        code_top
    } else {
        size.height
    };
    let code_top_f = code_top as f32;
    // (top, right edge) of every track line that fits
    let mut rows: Vec<(f32, f32)> = Vec::new();
    let mut row_y = y;
    for _ in tracks {
        if row_y + line_height > bottom + 2.0 {
            break;
        }
        match kind {
            // the barcode takes the full width of its rows: stop above it
            CodeKind::Barcode if row_y + line_height > code_top_f - 2.0 => break,
            _ => {}
        }
        // a QR code takes only the right part: lines beside it end before it
        let beside_qr = kind == CodeKind::Qr && row_y + line_height > code_top_f + 6.0;
        let right_edge = if beside_qr {
            code_left as f32 - 4.0
        } else {
            left + inner_width
        };
        rows.push((row_y, right_edge));
        row_y += line_height;
    }
    let hidden = tracks.len() - rows.len();
    overflow.tracks_hidden = hidden;
    let marker = format!("+{hidden}");
    let marker_width = text_width(&fonts.bold, track_px, &marker);
    let title_x = left + position_width + 6.0;
    let last = rows.len().saturating_sub(1);
    for (i, ((top, right_edge), track)) in rows.iter().zip(tracks).enumerate() {
        // tracks that did not fit are counted on the last line, not silently dropped
        let marker_here = hidden > 0 && i == last;
        let mut room = right_edge - title_x;
        if marker_here {
            room -= marker_width + 8.0;
        }
        draw_text(&mut img, &fonts.bold, track_px, left, *top, &track.position);
        let title = ellipsize(&fonts.regular, track_px, &track.title, room);
        draw_text(&mut img, &fonts.regular, track_px, title_x, *top, &title);
        if marker_here {
            draw_text(
                &mut img,
                &fonts.bold,
                track_px,
                right_edge - marker_width,
                *top,
                &marker,
            );
        }
    }

    if parts.code {
        code.draw(&mut img, code_left, code_draw_top);
    }
    if let Some(caption) = &options.caption {
        let px = 15.0;
        let top = (size.height - MARGIN) as f32 - px;
        draw_text(&mut img, &fonts.regular, px, left, top, caption);
    }
    Ok((img, overflow))
}

/// The sample with styles.
pub fn sample_content_with_tags() -> LabelContent {
    LabelContent {
        tags: ["Electronic", "House", "Disco", "Edits"]
            .map(String::from)
            .to_vec(),
        ..sample_content()
    }
}

/// A sample (from a real sleeve) for trying out layouts without Discogs.
pub fn sample_content() -> LabelContent {
    let track = |p: &str, t: &str| LabelTrack {
        position: p.to_string(),
        title: t.to_string(),
    };
    LabelContent {
        release_id: 32474433,
        size_inch: Some(12),
        label: "Made Of Concrete".to_string(),
        catno: "MOC040".to_string(),
        artist: "Alexander Kowalski & Rebar".to_string(),
        title: "When Darkness Surrenders To The Light".to_string(),
        tags: Vec::new(),
        tracks: vec![
            track("A1", "Dawns Embrace"),
            track("A2", "Radiant Resurgence"),
            track("B1", "When Darkness Surrenders To The Light (Edit)"),
            track("B2", "When Darkness Surrenders To The Light (Dub)"),
        ],
    }
}

/// Label sizes in millimetres: the real ones (52 x 39, 76 x 51 on the ZD421) and the test labels (51 x 38).
pub const SIZES_MM: [(f32, f32); 3] = [(52.0, 39.0), (51.0, 38.0), (76.0, 51.0)];

/// Writes previews of all sizes and code kinds into `dir` (enlarged 3x, so they
/// can be looked at on a screen) and returns the file names.
pub fn write_previews(
    dir: &std::path::Path,
    content: &LabelContent,
) -> Result<Vec<std::path::PathBuf>> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut files = Vec::new();
    for (w, h) in SIZES_MM {
        for (kind, name) in [(CodeKind::Qr, "qr"), (CodeKind::Barcode, "barcode")] {
            let img = render(content, LabelSize::from_mm(w, h), kind)?;
            let big = image::imageops::resize(
                &img,
                img.width() * 3,
                img.height() * 3,
                image::imageops::FilterType::Nearest,
            );
            let path = dir.join(format!("label-{w}x{h}-{name}.png"));
            big.save(&path)
                .with_context(|| format!("write {}", path.display()))?;
            files.push(path);
        }
        // the same label with genres and styles as tags
        for (style, name) in [(TagStyle::Outline, "outline"), (TagStyle::Filled, "filled")] {
            let mut options = LabelOptions::new(CodeKind::Barcode);
            options.tag_style = Some(style);
            let img = render_with(
                &sample_content_with_tags(),
                LabelSize::from_mm(w, h),
                &options,
            )?;
            let big = image::imageops::resize(
                &img,
                img.width() * 3,
                img.height() * 3,
                image::imageops::FilterType::Nearest,
            );
            let path = dir.join(format!("label-{w}x{h}-barcode-tags-{name}.png"));
            big.save(&path)
                .with_context(|| format!("write {}", path.display()))?;
            files.push(path);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink_in(img: &GrayImage, y0: u32, y1: u32) -> usize {
        (y0..y1.min(img.height()))
            .flat_map(|y| (0..img.width()).map(move |x| (x, y)))
            .filter(|&(x, y)| img.get_pixel(x, y).0[0] < 128)
            .count()
    }

    fn render_without(part: &str, kind: CodeKind) -> GrayImage {
        let mut options = LabelOptions::new(kind);
        options.parts = LabelParts::without(&[part.to_string()]);
        render_with(&sample_content(), SIZE_TEST, &options).unwrap()
    }

    const SIZE_ZD421: LabelSize = LabelSize {
        width: 607,
        height: 408,
    };

    /// The barcode standing at the right edge, turned back upright.
    fn upright_right_band(img: &GrayImage, band: u32) -> GrayImage {
        let part = image::imageops::crop_imm(
            img,
            img.width() - 16 - band - 24,
            0,
            band + 40,
            img.height(),
        )
        .to_image();
        image::imageops::rotate270(&part)
    }

    #[test]
    fn the_barcode_can_stand_at_the_right_edge() {
        let content = sample_content();
        for (size, mm) in [(SIZE_ZD421, 3.0_f32), (SIZE_ZD421, 6.0), (SIZE_REAL, 3.0)] {
            let mut options = LabelOptions::new(CodeKind::Barcode);
            options.code_side = CodeSide::Right;
            options.barcode_height = (mm * 8.0) as u32;
            let img = render_with(&content, size, &options).unwrap();
            let band = options.barcode_height;
            let upright = upright_right_band(&img, band);
            assert_eq!(
                decode(&upright).as_deref(),
                Some("32474433"),
                "the barcode reads at the right edge ({mm} mm)"
            );
            // the band is as wide as the bars are high: inside the margin, centred vertically
            let columns: Vec<u32> = (0..img.width())
                .filter(|&x| {
                    (0..img.height())
                        .filter(|&y| img.get_pixel(x, y).0[0] < 128)
                        .count()
                        > img.height() as usize / 6
                })
                .collect();
            assert!(!columns.is_empty());
            assert!(
                *columns.last().unwrap() <= size.width - MARGIN + 4,
                "inside the margin"
            );
            let rows_with_ink: Vec<u32> = (0..img.height())
                .filter(|&y| {
                    (img.width() - MARGIN - band..img.width() - MARGIN)
                        .any(|x| img.get_pixel(x, y).0[0] < 128)
                })
                .collect();
            let (first, last) = (rows_with_ink[0], *rows_with_ink.last().unwrap());
            assert!(
                first > 10 && last < img.height() - 10,
                "centred: {first}..{last}"
            );
            assert!((first as i32 - (img.height() - 1 - last) as i32).abs() < 12);
        }
    }

    #[test]
    fn text_keeps_left_of_a_barcode_at_the_right_edge() {
        let mut content = sample_content();
        content.artist = "A very long artist name that needs the whole width".into();
        let mut options = LabelOptions::new(CodeKind::Barcode);
        options.code_side = CodeSide::Right;
        options.barcode_height = 48;
        let img = render_with(&content, SIZE_ZD421, &options).unwrap();
        let band_left = SIZE_ZD421.width - MARGIN + 4 - 48;
        // nothing but the barcode right of the text area, and a gap before it
        for x in band_left - 4..band_left {
            for y in 0..img.height() {
                assert_eq!(
                    img.get_pixel(x, y).0[0],
                    255,
                    "ink at {x},{y} touches the barcode"
                );
            }
        }
        // the tracks may use the whole height now: more lines than with the barcode at the bottom
        content.tracks = (1..=12)
            .map(|i| LabelTrack {
                position: format!("A{i}"),
                title: format!("Track {i}"),
            })
            .collect();
        options.barcode_height = 80;
        let (_, right) = render_scaled(&content, SIZE_REAL, &options, 1.0).unwrap();
        let mut at_bottom = LabelOptions::new(CodeKind::Barcode);
        at_bottom.barcode_height = 80;
        let (_, bottom) = render_scaled(&content, SIZE_REAL, &at_bottom, 1.0).unwrap();
        assert!(
            right.tracks_hidden < bottom.tracks_hidden,
            "{right:?} vs {bottom:?}"
        );
    }

    #[test]
    fn the_side_is_ignored_for_a_qr_code_and_a_hidden_code() {
        let content = sample_content();
        let mut qr = LabelOptions::new(CodeKind::Qr);
        let plain = render_with(&content, SIZE_ZD421, &qr).unwrap();
        qr.code_side = CodeSide::Right;
        assert_eq!(
            render_with(&content, SIZE_ZD421, &qr).unwrap().as_raw(),
            plain.as_raw()
        );
        let mut hidden = LabelOptions::new(CodeKind::Barcode);
        hidden.parts.code = false;
        let without = render_with(&content, SIZE_ZD421, &hidden).unwrap();
        hidden.code_side = CodeSide::Right;
        assert_eq!(
            render_with(&content, SIZE_ZD421, &hidden).unwrap().as_raw(),
            without.as_raw()
        );
    }

    #[test]
    fn the_bars_can_be_higher() {
        let content = sample_content();
        let mut low = LabelOptions::new(CodeKind::Barcode);
        low.barcode_height = 16;
        let mut high = low.clone();
        high.barcode_height = 64;
        let ink = |o: &LabelOptions| {
            let img = render_with(&content, SIZE_ZD421, o).unwrap();
            ink_in(&img, 300, 408)
        };
        assert!(ink(&high) > ink(&low) * 2);
    }

    #[test]
    fn filled_tags_are_set_in_bold() {
        let fonts = Fonts::load().unwrap();
        let tags = vec!["Techno".to_string(), "Minimal".to_string()];
        let width = |style| {
            let mut img = GrayImage::from_pixel(400, 40, WHITE);
            draw_tags(&mut img, &fonts, (4.0, 4.0), 390.0, &tags, style, 1.0);
            (0..400)
                .rev()
                .find(|&x| (0..40).any(|y| img.get_pixel(x, y).0[0] < 128))
                .unwrap()
        };
        assert!(
            width(TagStyle::Filled) > width(TagStyle::Outline),
            "bold is wider"
        );
        // white text in the pill is still there
        let mut img = GrayImage::from_pixel(400, 40, WHITE);
        draw_tags(
            &mut img,
            &fonts,
            (4.0, 4.0),
            390.0,
            &tags,
            TagStyle::Filled,
            1.0,
        );
        let whites = (10..60)
            .flat_map(|x| (6..24).map(move |y| (x, y)))
            .filter(|&(x, y)| img.get_pixel(x, y).0[0] == 255)
            .count();
        assert!(whites > 30, "{whites}");
    }

    #[test]
    fn bigger_text_on_request() {
        let content = sample_content();
        let normal =
            render_with(&content, SIZE_ZD421, &LabelOptions::new(CodeKind::Barcode)).unwrap();
        let mut options = LabelOptions::new(CodeKind::Barcode);
        options.font_scale = 1.5;
        let (big, scale) = render_fitted(&content, SIZE_ZD421, &options).unwrap();
        assert_eq!(scale, 1.5);
        assert!(
            ink_in(&big, 0, 120) > ink_in(&normal, 0, 120) * 3 / 2,
            "the header, artist and title cover more of the top"
        );
        // a size outside the range is brought into it
        options.font_scale = 9.0;
        assert_eq!(
            render_fitted(&content, SIZE_ZD421, &options).unwrap().1,
            MAX_FONT_SCALE
        );
        options.font_scale = 0.0;
        assert_eq!(
            render_fitted(&content, SIZE_ZD421, &options).unwrap().1,
            MIN_FONT_SCALE
        );
    }

    #[test]
    fn the_automatic_size_is_the_biggest_that_fits() {
        let content = sample_content();
        for (size, kind) in [
            (SIZE_ZD421, CodeKind::Barcode),
            (SIZE_ZD421, CodeKind::Qr),
            (SIZE_REAL, CodeKind::Barcode),
            (SIZE_TEST, CodeKind::Qr),
        ] {
            let mut options = LabelOptions::new(kind);
            options.font_auto = true;
            let (img, scale) = render_fitted(&content, size, &options).unwrap();
            assert!((1.0..=MAX_FONT_SCALE).contains(&scale), "{scale}");
            let (_, at_scale) = render_scaled(&content, size, &options, scale).unwrap();
            assert!(
                at_scale.none() || scale == 1.0,
                "{size:?} {kind:?}: {at_scale:?}"
            );
            if scale < MAX_FONT_SCALE {
                let (_, bigger) = render_scaled(&content, size, &options, scale + 0.05).unwrap();
                assert!(!bigger.none(), "{size:?} {kind:?}: {scale} could be bigger");
            }
            assert_eq!(
                decode(&img).as_deref(),
                Some("32474433"),
                "the code still reads at {scale}"
            );
        }
        // the bigger label allows bigger text than the small one
        let mut options = LabelOptions::new(CodeKind::Barcode);
        options.font_auto = true;
        let big = render_fitted(&content, SIZE_ZD421, &options).unwrap().1;
        let small = render_fitted(&content, SIZE_REAL, &options).unwrap().1;
        assert!(big > small, "{big} vs {small}");
    }

    #[test]
    fn the_automatic_size_never_goes_below_the_chosen_one() {
        let mut content = sample_content();
        content.tracks = (1..=30)
            .map(|i| LabelTrack {
                position: format!("A{i}"),
                title: format!("Track {i}"),
            })
            .collect();
        let mut options = LabelOptions::new(CodeKind::Barcode);
        options.font_auto = true;
        options.font_scale = 1.2;
        // 30 tracks never fit: the least size is used
        assert_eq!(render_fitted(&content, SIZE_REAL, &options).unwrap().1, 1.2);
    }

    #[test]
    fn what_does_not_fit_is_reported() {
        let mut content = sample_content_with_tags();
        content.title = "A very long title ".repeat(8);
        let mut options = LabelOptions::new(CodeKind::Barcode);
        options.tag_style = Some(TagStyle::Outline);
        content.tags = (1..=12).map(|i| format!("Style number {i}")).collect();
        let (_, overflow) = render_scaled(&content, SIZE_TEST, &options, 1.0).unwrap();
        assert!(
            overflow.title_cut && overflow.tags_hidden > 0,
            "{overflow:?}"
        );
        content.tracks = (1..=30)
            .map(|i| LabelTrack {
                position: format!("A{i}"),
                title: format!("Track {i}"),
            })
            .collect();
        let (_, many) = render_scaled(&content, SIZE_TEST, &options, 1.0).unwrap();
        assert!(many.tracks_hidden > 0, "{many:?}");
        let (_, fine) = render_scaled(
            &sample_content(),
            SIZE_ZD421,
            &LabelOptions::new(CodeKind::Barcode),
            1.0,
        )
        .unwrap();
        assert!(fine.none(), "{fine:?}");
    }

    #[test]
    fn every_part_can_be_left_out() {
        for kind in [CodeKind::Barcode, CodeKind::Qr] {
            let full = render_with(&sample_content(), SIZE_TEST, &LabelOptions::new(kind)).unwrap();
            for (name, _) in PARTS {
                let img = render_without(name, kind);
                assert!(
                    ink_in(&img, 0, img.height()) < ink_in(&full, 0, full.height()),
                    "{name} ({kind:?}) leaves something out"
                );
            }
        }
    }

    #[test]
    fn leaving_out_a_part_keeps_the_others() {
        let no_code = render_without("code", CodeKind::Barcode);
        assert_eq!(decode(&no_code), None, "no code to read");
        let with_code = render_with(
            &sample_content(),
            SIZE_TEST,
            &LabelOptions::new(CodeKind::Barcode),
        )
        .unwrap();
        // the top of the label is the same: the header, artist and title stay
        assert_eq!(ink_in(&no_code, 0, 120), ink_in(&with_code, 0, 120));
        // without the header everything moves up, so the title is higher
        let no_header = render_without("header", CodeKind::Barcode);
        assert_ne!(ink_in(&no_header, 0, 40), ink_in(&with_code, 0, 40));
    }

    #[test]
    fn without_the_code_the_tracks_may_use_the_room() {
        let mut content = sample_content();
        content.tracks = (1..=12)
            .map(|i| LabelTrack {
                position: format!("A{i}"),
                title: format!("Track {i}"),
            })
            .collect();
        let mut options = LabelOptions::new(CodeKind::Barcode);
        let with = render_with(&content, SIZE_TEST, &options).unwrap();
        options.parts.code = false;
        let without = render_with(&content, SIZE_TEST, &options).unwrap();
        // more track lines fit: the lowest left corner (beside the barcode) is
        // empty with the code, and holds a track position without it
        let corner = |img: &GrayImage| {
            (270..296u32)
                .flat_map(|y| (MARGIN..MARGIN + 30).map(move |x| (x, y)))
                .filter(|&(x, y)| img.get_pixel(x, y).0[0] < 128)
                .count()
        };
        assert_eq!(corner(&with), 0);
        assert!(corner(&without) > 0);
    }

    const SIZE_REAL: LabelSize = LabelSize {
        width: 416,
        height: 312,
    };
    const SIZE_TEST: LabelSize = LabelSize {
        width: 408,
        height: 304,
    };

    fn decode(img: &GrayImage) -> Option<String> {
        rxing::helpers::detect_in_luma(img.as_raw().clone(), img.width(), img.height(), None)
            .ok()
            .map(|r| r.getText().to_string())
    }

    fn black_bounds(img: &GrayImage) -> (u32, u32, u32, u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for (x, y, p) in img.enumerate_pixels() {
            if p.0[0] < 128 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        (x0, y0, x1, y1)
    }

    fn assert_inside_safe_area(img: &GrayImage, what: &str) {
        let (x0, y0, x1, y1) = black_bounds(img);
        let safe = 8;
        assert!(
            x0 >= safe && y0 >= safe,
            "{what}: ink at the left/top edge ({x0},{y0})"
        );
        assert!(
            x1 < img.width() - safe && y1 < img.height() - safe,
            "{what}: ink at the right/bottom edge ({x1},{y1}) of {}x{}",
            img.width(),
            img.height()
        );
    }

    #[test]
    fn millimetres_to_dots_at_203_dpi() {
        assert_eq!(mm_to_dots(52.0), 416);
        assert_eq!(mm_to_dots(39.0), 312);
        assert_eq!(mm_to_dots(51.0), 408);
        assert_eq!(mm_to_dots(38.0), 304);
        assert_eq!(LabelSize::from_mm(52.0, 39.0), SIZE_REAL);
    }

    #[test]
    fn the_code_can_be_read_back_from_the_bitmap() {
        for size in [SIZE_REAL, SIZE_TEST] {
            for kind in [CodeKind::Qr, CodeKind::Barcode] {
                let img = render(&sample_content(), size, kind).unwrap();
                assert_eq!(
                    decode(&img).as_deref(),
                    Some("32474433"),
                    "{kind:?} on {}x{}",
                    size.width,
                    size.height
                );
            }
        }
    }

    #[test]
    fn ids_of_any_length_encode_and_decode() {
        // odd and even digit counts take different Code 128 paths
        for id in [
            1u32,
            12,
            123,
            1234,
            99999,
            123456,
            1234567,
            38_000_001,
            4_000_000_000,
        ] {
            let content = LabelContent {
                release_id: id,
                ..sample_content()
            };
            for kind in [CodeKind::Qr, CodeKind::Barcode] {
                let img = render(&content, SIZE_REAL, kind).unwrap();
                assert_eq!(decode(&img), Some(id.to_string()), "id {id} as {kind:?}");
            }
        }
    }

    #[test]
    fn everything_stays_inside_the_safe_area() {
        for size in [SIZE_REAL, SIZE_TEST] {
            for kind in [CodeKind::Qr, CodeKind::Barcode] {
                let img = render(&sample_content(), size, kind).unwrap();
                assert_inside_safe_area(&img, &format!("sample {kind:?}"));
            }
        }
    }

    fn crowded_content() -> LabelContent {
        LabelContent {
            release_id: 7654321,
            size_inch: Some(7),
            label: "A Very Long Label Name That Goes On And On Records".to_string(),
            catno: "ABCDEFGH-0042".to_string(),
            artist: "An Extremely Long Artist Name Featuring Several Other Very Long Artist Names".to_string(),
            title: "An Equally Long Release Title That Needs More Than Two Lines To Be Shown In Full Here".to_string(),
            tags: Vec::new(),
            tracks: (1..=20)
                .map(|i| LabelTrack {
                    position: format!("{}{}", if i <= 10 { "A" } else { "B" }, i),
                    title: format!("Track number {i} with a rather long title (Extended Remix Version)"),
                })
                .collect(),
        }
    }

    #[test]
    fn crowded_content_is_cut_to_fit_and_still_scans() {
        for size in [SIZE_REAL, SIZE_TEST] {
            for kind in [CodeKind::Qr, CodeKind::Barcode] {
                let img = render(&crowded_content(), size, kind).unwrap();
                assert_inside_safe_area(&img, &format!("crowded {kind:?}"));
                assert_eq!(decode(&img).as_deref(), Some("7654321"), "{kind:?}");
            }
        }
    }

    #[test]
    fn empty_content_renders_just_the_code() {
        let content = LabelContent {
            release_id: 42,
            size_inch: None,
            label: String::new(),
            catno: String::new(),
            artist: String::new(),
            title: String::new(),
            tags: Vec::new(),
            tracks: Vec::new(),
        };
        for kind in [CodeKind::Qr, CodeKind::Barcode] {
            let img = render(&content, SIZE_REAL, kind).unwrap();
            assert_eq!(decode(&img).as_deref(), Some("42"));
            assert_inside_safe_area(&img, "empty");
        }
    }

    #[test]
    fn accents_and_umlauts_are_drawn() {
        let mut content = sample_content();
        content.artist = "Björk Guðmundsdóttir".to_string();
        let plain = render(&sample_content(), SIZE_REAL, CodeKind::Qr).unwrap();
        let accented = render(&content, SIZE_REAL, CodeKind::Qr).unwrap();
        assert_ne!(plain.as_raw(), accented.as_raw());
        assert_inside_safe_area(&accented, "accents");
    }

    #[test]
    fn rendering_is_deterministic_and_black_and_white_only() {
        let a = render(&sample_content(), SIZE_REAL, CodeKind::Barcode).unwrap();
        let b = render(&sample_content(), SIZE_REAL, CodeKind::Barcode).unwrap();
        assert_eq!(a.as_raw(), b.as_raw());
        assert!(a.as_raw().iter().all(|p| *p == 0 || *p == 255));
    }

    #[test]
    fn qr_modules_are_whole_dots() {
        // a QR scaled by a fractional factor would blur on the printer
        let qr = qr_code(32474433, 4, 2).unwrap();
        assert_eq!(qr.width % 4, 0);
        assert_eq!(qr.width, (21 + 4) * 4, "version 1 for a short number");
        assert_eq!(qr.width, qr.height);
    }

    #[test]
    fn barcode_fits_the_label_width() {
        let code = barcode(32474433, 2, 40).unwrap();
        assert!(code.width < SIZE_TEST.width - 2 * MARGIN, "{}", code.width);
        assert_eq!(code.height, 40);
    }

    #[test]
    fn ellipsize_and_fit() {
        let fonts = Fonts::load().unwrap();
        let long = "A rather long line of text that cannot possibly fit";
        let cut = ellipsize(&fonts.regular, 20.0, long, 150.0);
        assert!(cut.ends_with('…'));
        assert!(text_width(&fonts.regular, 20.0, &cut) <= 150.0);
        assert_eq!(ellipsize(&fonts.regular, 20.0, "short", 150.0), "short");
        assert_eq!(ellipsize(&fonts.regular, 20.0, long, 1.0), "");

        let (px, line) = fit_line(&fonts.bold, 32.0, 20.0, "Alexander Kowalski", 400.0);
        assert_eq!((px, line.as_str()), (32.0, "Alexander Kowalski"));
        let (px, line) = fit_line(&fonts.bold, 32.0, 20.0, "Alexander Kowalski & Rebar", 300.0);
        assert!((20.0..32.0).contains(&px), "shrunk to {px}");
        assert!(text_width(&fonts.bold, px, &line) <= 300.0);
    }

    #[test]
    fn the_label_line_keeps_the_catalog_number() {
        let fonts = Fonts::load().unwrap();
        let (_, short) = fit_label_line(
            &fonts.regular,
            21.0,
            15.0,
            "Made Of Concrete",
            "MOC040",
            330.0,
        );
        assert_eq!(short, "Made Of Concrete / MOC040");
        let (px, cut) = fit_label_line(
            &fonts.regular,
            21.0,
            15.0,
            "A Very Long Label Name That Goes On And On Records",
            "ABCDEFGH-0042",
            250.0,
        );
        assert!(cut.ends_with(" / ABCDEFGH-0042"), "{cut}");
        assert!(cut.contains('…'), "{cut}");
        assert!(text_width(&fonts.regular, px, &cut) <= 250.0);
        let (_, only_label) = fit_label_line(&fonts.regular, 21.0, 15.0, "Label", "", 330.0);
        assert_eq!(only_label, "Label");
        let (_, only_catno) = fit_label_line(&fonts.regular, 21.0, 15.0, "", "CAT 1", 330.0);
        assert_eq!(only_catno, "CAT 1");
        assert_eq!(
            fit_label_line(&fonts.regular, 21.0, 15.0, "", "", 330.0).1,
            ""
        );
    }

    #[test]
    fn wrap_limits_lines_and_marks_cut_text() {
        let fonts = Fonts::load().unwrap();
        let text = "When Darkness Surrenders To The Light And Then Some More Words Follow";
        let lines = wrap(&fonts.regular, 22.0, text, 250.0, 2);
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|l| text_width(&fonts.regular, 22.0, l) <= 250.0)
        );
        assert!(lines[1].ends_with('…'), "{lines:?}");
        let short = wrap(&fonts.regular, 22.0, "Short title", 250.0, 2);
        assert_eq!(short, vec!["Short title".to_string()]);
        assert!(wrap(&fonts.regular, 22.0, "", 250.0, 2).is_empty());
        let long_word = wrap(
            &fonts.regular,
            22.0,
            "Pneumonoultramicroscopicsilicovolcanoconiosis",
            150.0,
            2,
        );
        assert!(
            long_word
                .iter()
                .all(|l| text_width(&fonts.regular, 22.0, l) <= 150.0)
        );
    }

    fn release(json: &str) -> Release {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn content_from_a_release() {
        let r = release(
            r#"{"title": "Wonderous Numinous", "artists": [{"name": "Altone"}, {"name": "Other"}],
                "labels": [{"name": "Primary [colours]", "catno": "PCSS09"}, {"name": "Second", "catno": "X1"}],
                "formats": [{"name": "Vinyl", "descriptions": ["12\"", "33 ⅓ RPM", "EP"]}],
                "tracklist": [{"position": "A1", "title": "Self Replication"}, {"position": "", "title": ""},
                              {"position": "B1", "title": "Adaptive Swarm"}]}"#,
        );
        let c = LabelContent::from_release(5, &r);
        assert_eq!(c.release_id, 5);
        assert_eq!(c.size_inch, Some(12));
        assert_eq!(
            (c.label.as_str(), c.catno.as_str()),
            ("Primary [colours]", "PCSS09")
        );
        assert_eq!(c.artist, "Altone, Other");
        assert_eq!(c.title, "Wonderous Numinous");
        assert_eq!(c.tracks.len(), 2, "empty rows are skipped");
        assert_eq!(c.tracks[1].position, "B1");
    }

    #[test]
    fn size_comes_from_the_format_tags() {
        let size = |descriptions: &str| {
            let r = release(&format!(
                r#"{{"formats": [{{"name": "Vinyl", "descriptions": {descriptions}}}]}}"#
            ));
            LabelContent::from_release(1, &r).size_inch
        };
        assert_eq!(size(r#"["7\"", "45 RPM"]"#), Some(7));
        assert_eq!(size(r#"["10\""]"#), Some(10));
        assert_eq!(
            size(r#"["LP", "Album"]"#),
            Some(12),
            "an LP without a size is a 12 inch"
        );
        assert_eq!(size(r#"["EP"]"#), None);
        assert_eq!(size("[]"), None);
    }

    #[test]
    fn missing_data_gives_empty_fields_and_none_catno_is_dropped() {
        let r = release(r#"{"labels": [{"name": "Label", "catno": "none"}]}"#);
        let c = LabelContent::from_release(1, &r);
        assert_eq!(c.catno, "");
        assert_eq!(c.label, "Label");
        let c = LabelContent::from_release(1, &release("{}"));
        assert!(
            c.label.is_empty() && c.artist.is_empty() && c.title.is_empty() && c.tracks.is_empty()
        );
    }

    #[test]
    fn previews_are_written_for_every_size_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        let files = write_previews(dir.path(), &sample_content()).unwrap();
        assert_eq!(
            files.len(),
            12,
            "three sizes: qr, barcode, and the barcode with tags in two styles"
        );
        for f in files {
            assert!(f.exists(), "{}", f.display());
        }
    }

    #[test]
    fn barcode_height_and_module_are_adjustable() {
        let content = sample_content();
        let mut options = LabelOptions::new(CodeKind::Barcode);
        for height in [16, 24, 40] {
            options.barcode_height = height;
            let img = render_with(&content, SIZE_TEST, &options).unwrap();
            assert_eq!(decode(&img).as_deref(), Some("32474433"), "height {height}");
            assert_inside_safe_area(&img, &format!("height {height}"));
        }
        options.barcode_height = 24;
        options.barcode_module = 3;
        let img = render_with(&content, SIZE_TEST, &options).unwrap();
        assert_eq!(decode(&img).as_deref(), Some("32474433"));
        let narrow = barcode(32474433, 2, 24).unwrap();
        let wide = barcode(32474433, 3, 24).unwrap();
        assert!(wide.width > narrow.width);
    }

    #[test]
    fn a_shorter_barcode_leaves_more_room_for_tracks() {
        let mut content = crowded_content();
        content.tracks.truncate(20);
        let ink_above = |height: u32| {
            let mut o = LabelOptions::new(CodeKind::Barcode);
            o.barcode_height = height;
            let img = render_with(&content, SIZE_TEST, &o).unwrap();
            // ink beside the place where a tall barcode starts: track lines continue there only if it is short
            let band = img
                .enumerate_pixels()
                .filter(|(x, y, p)| p.0[0] < 128 && *y > 253 && *y < 272 && *x > 16 && *x < 80);
            band.count()
        };
        assert!(ink_above(16) > ink_above(40));
    }

    #[test]
    fn the_caption_is_drawn_in_the_bottom_left_corner() {
        let mut options = LabelOptions::new(CodeKind::Barcode);
        let plain = render_with(&sample_content(), SIZE_TEST, &options).unwrap();
        options.caption = Some("4.0mm".to_string());
        let captioned = render_with(&sample_content(), SIZE_TEST, &options).unwrap();
        let differing: Vec<_> = plain
            .enumerate_pixels()
            .filter(|(x, y, p)| captioned.get_pixel(*x, *y) != *p)
            .collect();
        assert!(!differing.is_empty());
        assert!(
            differing.iter().all(|(x, y, _)| *x < 100 && *y > 260),
            "caption must stay in its corner"
        );
        assert_eq!(decode(&captioned).as_deref(), Some("32474433"));
    }

    #[test]
    fn a_stored_record_gives_the_same_label_as_the_release_it_came_from() {
        let json = r#"{"id": 9, "title": "Wonderscape", "released": "2024", "artists": [{"id": 1, "name": "Alexander Kowalski", "join": "&"}, {"id": 2, "name": "Rebar"}],
            "labels": [{"id": 5, "name": "Made Of Concrete", "catno": "MOC040"}],
            "formats": [{"name": "Vinyl", "descriptions": ["12\""]}],
            "tracklist": [{"position": "A1", "title": "Dawns Embrace"}, {"position": "A2", "title": "Radiant Resurgence"}]}"#;
        let release: Release = serde_json::from_str(json).unwrap();
        let from_discogs = LabelContent::from_release(9, &release);
        assert_eq!(
            from_discogs.artist, "Alexander Kowalski & Rebar",
            "joined as on the sleeve"
        );
        let stored = crate::library::StoredRelease::from_catalogue(
            &crate::catalogue::CatalogueRelease::from_release(9, &release, 1).unwrap(),
        );
        assert_eq!(LabelContent::from_stored(&stored), from_discogs);
    }

    // --- tags ------------------------------------------------------------------------------

    fn with_tags(tags: &[&str]) -> LabelContent {
        LabelContent {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..sample_content()
        }
    }

    fn tagged(style: TagStyle) -> LabelOptions {
        let mut o = LabelOptions::new(CodeKind::Barcode);
        o.tag_style = Some(style);
        o
    }

    #[test]
    fn only_the_styles_become_tags_without_repeats() {
        let release: Release = serde_json::from_str(
            r#"{"title": "T", "genres": ["Electronic", "Jazz"], "styles": ["House", "", " Disco ", "house"]}"#,
        )
        .unwrap();
        let content = LabelContent::from_release(1, &release);
        assert_eq!(
            content.tags,
            ["House", "Disco"],
            "no genres, no empty ones, no repeats"
        );
        let stored = crate::library::StoredRelease::from_catalogue(
            &crate::catalogue::CatalogueRelease::from_release(1, &release, 1).unwrap(),
        );
        assert_eq!(LabelContent::from_stored(&stored).tags, content.tags);
        assert!(
            LabelContent::from_release(1, &Release::default())
                .tags
                .is_empty()
        );
        let only_genres: Release =
            serde_json::from_str(r#"{"title": "T", "genres": ["Rock"]}"#).unwrap();
        assert!(
            LabelContent::from_release(1, &only_genres).tags.is_empty(),
            "no styles, no tags"
        );
    }

    #[test]
    fn tags_are_only_drawn_when_asked_for() {
        let content = with_tags(&["House", "Disco"]);
        let off = render_with(&content, SIZE_TEST, &LabelOptions::new(CodeKind::Barcode)).unwrap();
        let plain = render(&sample_content(), SIZE_TEST, CodeKind::Barcode).unwrap();
        assert_eq!(
            off.as_raw(),
            plain.as_raw(),
            "without a tag style the label is as before"
        );
        let on = render_with(&content, SIZE_TEST, &tagged(TagStyle::Outline)).unwrap();
        assert_ne!(on.as_raw(), plain.as_raw());
        // no tags, no change either
        let none = render_with(&sample_content(), SIZE_TEST, &tagged(TagStyle::Outline)).unwrap();
        assert_eq!(none.as_raw(), plain.as_raw());
    }

    #[test]
    fn both_styles_keep_the_code_readable_and_inside_the_margins() {
        for style in [TagStyle::Outline, TagStyle::Filled] {
            for size in [SIZE_REAL, SIZE_TEST] {
                for kind in [CodeKind::Barcode, CodeKind::Qr] {
                    let mut o = tagged(style);
                    o.kind = kind;
                    let img = render_with(&sample_content_with_tags(), size, &o).unwrap();
                    assert_eq!(
                        decode(&img).as_deref(),
                        Some("32474433"),
                        "{style:?} {kind:?}"
                    );
                    assert_inside_safe_area(&img, &format!("{style:?} {kind:?}"));
                }
            }
        }
    }

    #[test]
    fn filled_tags_are_black_areas_and_outlined_ones_are_frames() {
        let content = with_tags(&["House"]);
        let black = |style| {
            let img = render_with(&content, SIZE_TEST, &tagged(style)).unwrap();
            img.pixels().filter(|p| p.0[0] < 128).count()
        };
        let base = render(&sample_content(), SIZE_TEST, CodeKind::Barcode)
            .unwrap()
            .pixels()
            .filter(|p| p.0[0] < 128)
            .count();
        let (outline, filled) = (black(TagStyle::Outline), black(TagStyle::Filled));
        assert!(
            outline > base && filled > outline,
            "{base} < {outline} < {filled}"
        );
    }

    #[test]
    fn white_text_shows_up_inside_a_filled_tag() {
        let img = render_with(
            &with_tags(&["Electronic"]),
            SIZE_TEST,
            &tagged(TagStyle::Filled),
        )
        .unwrap();
        // the tag row: find the black area and count white pixels inside it
        let row = (100..200)
            .find(|y| (16..60).all(|x| img.get_pixel(x, *y).0[0] < 128))
            .expect("a filled tag row");
        let white_inside = (16..90)
            .filter(|x| img.get_pixel(*x, row + 4).0[0] > 128)
            .count()
            + (16..90)
                .filter(|x| img.get_pixel(*x, row + 8).0[0] > 128)
                .count();
        assert!(white_inside > 0, "the lettering is white on the black tag");
    }

    #[test]
    fn too_many_tags_are_counted_not_cut_off() {
        let many = [
            "Electronic",
            "Jazz",
            "Funk / Soul",
            "Experimental",
            "Ambient",
            "Drum n Bass",
            "Reggae",
            "Rock",
        ];
        let o = tagged(TagStyle::Outline);
        let img = render_with(&with_tags(&many), SIZE_TEST, &o).unwrap();
        assert_inside_safe_area(&img, "many tags");
        assert_eq!(decode(&img).as_deref(), Some("32474433"));
        // at least the first tag fits, and the row never grows beyond one line
        let few = render_with(&with_tags(&many[..2]), SIZE_TEST, &o).unwrap();
        let rows_with_ink = |img: &GrayImage| {
            (112..150)
                .filter(|y| (16..392).any(|x| img.get_pixel(x, *y).0[0] < 128))
                .count()
        };
        assert_eq!(rows_with_ink(&img), rows_with_ink(&few), "one row of tags");
    }

    #[test]
    fn a_very_long_tag_is_shortened_to_fit() {
        let long = "An Extremely Long Style Name That Is Far Wider Than The Whole Label Itself";
        let img = render_with(&with_tags(&[long]), SIZE_TEST, &tagged(TagStyle::Outline)).unwrap();
        assert_inside_safe_area(&img, "long tag");
        assert_eq!(decode(&img).as_deref(), Some("32474433"));
    }

    #[test]
    fn tags_leave_the_track_list_intact() {
        // the sample's four tracks still fit with a row of tags
        let o = tagged(TagStyle::Outline);
        let with = render_with(&sample_content_with_tags(), SIZE_TEST, &o).unwrap();
        let without = render(&sample_content(), SIZE_TEST, CodeKind::Barcode).unwrap();
        let ink = |img: &GrayImage, y0: u32, y1: u32| {
            (y0..y1)
                .flat_map(|y| (16..392).map(move |x| (x, y)))
                .filter(|(x, y)| img.get_pixel(*x, *y).0[0] < 128)
                .count()
        };
        // below the title there is more ink with tags (the tags), and the last track line is still drawn
        assert!(ink(&with, 120, 260) > ink(&without, 120, 260));
        assert!(ink(&with, 205, 235) > 100, "the last track is still there");
    }

    #[test]
    fn the_rounded_rectangle_has_round_corners() {
        assert!(in_rounded_rect(10.0, 10.0, 20.0, 20.0, 5.0));
        assert!(
            !in_rounded_rect(0.2, 0.2, 20.0, 20.0, 5.0),
            "the corner is cut"
        );
        assert!(
            in_rounded_rect(5.0, 0.5, 20.0, 20.0, 5.0),
            "the straight edge is not"
        );
        assert!(!in_rounded_rect(-1.0, 5.0, 20.0, 20.0, 5.0));
        assert!(!in_rounded_rect(21.0, 5.0, 20.0, 20.0, 5.0));
    }
}
