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
            artist: release
                .artists
                .iter()
                .flatten()
                .filter_map(|a| a.name.as_deref())
                .collect::<Vec<_>>()
                .join(", "),
            title: release.title.clone().unwrap_or_default(),
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

/// Draws `text` with its top at `top`; returns the line height used.
fn draw_text(img: &mut GrayImage, font: &FontRef, px: f32, x: f32, top: f32, text: &str) {
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
                        img.put_pixel(x as u32, y as u32, BLACK);
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

/// Draws the label. Black pixels are 0, the rest 255.
pub fn render(content: &LabelContent, size: LabelSize, kind: CodeKind) -> Result<GrayImage> {
    let fonts = Fonts::load()?;
    let mut img = GrayImage::from_pixel(size.width, size.height, WHITE);
    let inner_width = (size.width - 2 * MARGIN) as f32;
    let left = MARGIN as f32;
    let mut y = MARGIN as f32 - 2.0;

    // Where the code goes decides how much room is left for the tracks.
    let code = match kind {
        CodeKind::Qr => qr_code(content.release_id, 4, 2)?,
        CodeKind::Barcode => barcode(content.release_id, 2, 40)?,
    };
    let (code_left, code_top) = match kind {
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

    // Header: size on the left, label / catalog number on the right.
    let header_px = 21.0;
    let size_text = content
        .size_inch
        .map(|s| format!("{s}\""))
        .unwrap_or_default();
    let size_width = text_width(&fonts.bold, header_px, &size_text);
    if !size_text.is_empty() {
        draw_text(&mut img, &fonts.bold, header_px, left, y, &size_text);
    }
    if !(content.label.is_empty() && content.catno.is_empty()) {
        let room = inner_width - size_width - 12.0;
        let (px, line) = fit_label_line(
            &fonts.regular,
            header_px,
            15.0,
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
    y += header_px * 1.25 + 4.0;

    // Artist: as large as fits on one line.
    if !content.artist.is_empty() {
        let (px, line) = fit_line(&fonts.bold, 32.0, 20.0, &content.artist, inner_width);
        draw_text(&mut img, &fonts.bold, px, left, y, &line);
        y += px * 1.2 + 2.0;
    }

    // Title: up to two lines.
    if !content.title.is_empty() {
        let px = 22.0;
        for line in wrap(&fonts.regular, px, &content.title, inner_width, 2) {
            draw_text(&mut img, &fonts.regular, px, left, y, &line);
            y += px * 1.2;
        }
        y += 4.0;
    }

    // Tracks: as many as fit above (and beside) the code.
    let track_px: f32 = 19.0;
    let line_height = (track_px * 1.25).round();
    let position_width = content
        .tracks
        .iter()
        .map(|t| text_width(&fonts.bold, track_px, &t.position))
        .fold(0.0, f32::max);
    let bottom = (size.height - MARGIN) as f32;
    let code_top_f = code_top as f32;
    // (top, right edge) of every track line that fits
    let mut rows: Vec<(f32, f32)> = Vec::new();
    let mut row_y = y;
    for _ in &content.tracks {
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
    let hidden = content.tracks.len() - rows.len();
    let marker = format!("+{hidden}");
    let marker_width = text_width(&fonts.bold, track_px, &marker);
    let title_x = left + position_width + 6.0;
    let last = rows.len().saturating_sub(1);
    for (i, ((top, right_edge), track)) in rows.iter().zip(&content.tracks).enumerate() {
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

    code.draw(&mut img, code_left, code_top);
    Ok(img)
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
        tracks: vec![
            track("A1", "Dawns Embrace"),
            track("A2", "Radiant Resurgence"),
            track("B1", "When Darkness Surrenders To The Light (Edit)"),
            track("B2", "When Darkness Surrenders To The Light (Dub)"),
        ],
    }
}

/// Label sizes in millimetres: the real ones (52 x 39) and the test labels (51 x 38).
pub const SIZES_MM: [(f32, f32); 2] = [(52.0, 39.0), (51.0, 38.0)];

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
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(files.len(), 4);
        for f in files {
            assert!(f.exists(), "{}", f.display());
        }
    }
}
