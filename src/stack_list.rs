//! A stack as a list on paper: a PDF (A4) with the records in order, to print
//! on a normal printer through CUPS (`lp`) or to look at first.

use std::path::{Path, PathBuf};
use std::process::Command;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{Context, Result, bail};
use printpdf::{Color, IndirectFontRef, Line, Mm, PdfDocument, PdfLayerReference, Point, Rgb};

use crate::label::{BOLD, REGULAR};
use crate::library::StackEntry;

const PAGE_W: f32 = 210.0;
const PAGE_H: f32 = 297.0;
const MARGIN: f32 = 15.0;
const ROW_H: f32 = 6.0;
const FONT_PT: f32 = 9.5;
/// The columns in mm: number, tick box, artist, title, label.
const COLUMNS: [f32; 5] = [10.0, 7.0, 54.0, 66.0, 43.0];
/// Room used by the title block on the first page, and by the column heads and the footer on all.
const TITLE_BLOCK: f32 = 16.0;
const HEAD_H: f32 = 8.0;
const FOOT_H: f32 = 10.0;

/// One line of the list, the texts cut to their columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub number: String,
    pub artist: String,
    pub title: String,
    pub label: String,
}

/// The list as it is laid out on pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sheet {
    pub title: String,
    pub subtitle: String,
    pub pages: Vec<Vec<Row>>,
}

/// Measures text the way the PDF viewer will draw it (`pt` is the font size).
struct Measure(FontRef<'static>);

impl Measure {
    fn new() -> Result<Self> {
        Ok(Self(
            FontRef::try_from_slice(REGULAR).context("load the font")?,
        ))
    }

    /// Width in mm of `text` at `pt` points.
    fn width_mm(&self, pt: f32, text: &str) -> f32 {
        // ab_glyph's pixel scale is the line height; the font size is the em
        let px = pt * self.0.height_unscaled() / self.0.units_per_em().unwrap_or(2048.0);
        let scaled = self.0.as_scaled(PxScale::from(px));
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
        width * 25.4 / 72.0
    }

    fn fit(&self, pt: f32, text: &str, max_mm: f32) -> String {
        if self.width_mm(pt, text) <= max_mm {
            return text.to_string();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let candidate = chars.iter().collect::<String>().trim_end().to_string() + "…";
            if self.width_mm(pt, &candidate) <= max_mm {
                return candidate;
            }
        }
        String::new()
    }
}

/// How many rows fit on a page (the first one has the title block).
fn rows_on_page(first: bool) -> usize {
    let free = PAGE_H - 2.0 * MARGIN - HEAD_H - FOOT_H - if first { TITLE_BLOCK } else { 0.0 };
    (free / ROW_H).floor() as usize
}

/// Lays the records out on pages.
pub fn sheet(name: &str, date: &str, entries: &[StackEntry]) -> Result<Sheet> {
    let measure = Measure::new()?;
    let rows: Vec<Row> = entries
        .iter()
        .map(|e| Row {
            number: e.position.to_string(),
            artist: measure.fit(FONT_PT, &e.artists, COLUMNS[2] - 2.0),
            title: measure.fit(FONT_PT, &e.title, COLUMNS[3] - 2.0),
            label: measure.fit(FONT_PT, &e.labels, COLUMNS[4]),
        })
        .collect();
    let mut pages: Vec<Vec<Row>> = Vec::new();
    let mut rest = rows.as_slice();
    loop {
        let take = rows_on_page(pages.is_empty()).min(rest.len());
        let (page, tail) = rest.split_at(take);
        pages.push(page.to_vec());
        rest = tail;
        if rest.is_empty() {
            break;
        }
    }
    Ok(Sheet {
        title: format!("Stack {name}"),
        subtitle: format!(
            "{} record{} · {date}",
            entries.len(),
            if entries.len() == 1 { "" } else { "s" }
        ),
        pages,
    })
}

/// The list as a PDF document.
pub fn render_pdf(sheet: &Sheet) -> Result<Vec<u8>> {
    let (doc, page1, layer1) = PdfDocument::new(&sheet.title, Mm(PAGE_W), Mm(PAGE_H), "list");
    let regular = doc
        .add_external_font(REGULAR)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("embed the font")?;
    let bold = doc
        .add_external_font(BOLD)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("embed the bold font")?;
    let total = sheet.pages.len();
    for (n, rows) in sheet.pages.iter().enumerate() {
        let layer = if n == 0 {
            doc.get_page(page1).get_layer(layer1)
        } else {
            let (page, layer) = doc.add_page(Mm(PAGE_W), Mm(PAGE_H), "list");
            doc.get_page(page).get_layer(layer)
        };
        draw_page(&layer, &regular, &bold, sheet, rows, n, total);
    }
    let mut out = std::io::BufWriter::new(Vec::new());
    doc.save(&mut out)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("write the PDF")?;
    out.into_inner()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("write the PDF")
}

fn text(layer: &PdfLayerReference, font: &IndirectFontRef, pt: f32, x: f32, y: f32, s: &str) {
    layer.use_text(s, pt, Mm(x), Mm(y), font);
}

fn line(layer: &PdfLayerReference, x1: f32, y1: f32, x2: f32, y2: f32) {
    layer.add_line(Line {
        points: vec![
            (Point::new(Mm(x1), Mm(y1)), false),
            (Point::new(Mm(x2), Mm(y2)), false),
        ],
        is_closed: false,
    });
}

fn draw_page(
    layer: &PdfLayerReference,
    regular: &IndirectFontRef,
    bold: &IndirectFontRef,
    sheet: &Sheet,
    rows: &[Row],
    index: usize,
    total: usize,
) {
    let black = Color::Rgb(Rgb::new(0.0, 0.0, 0.0, None));
    let grey = Color::Rgb(Rgb::new(0.75, 0.75, 0.75, None));
    layer.set_fill_color(black.clone());
    let mut top = PAGE_H - MARGIN;
    if index == 0 {
        text(layer, bold, 16.0, MARGIN, top - 7.0, &sheet.title);
        text(layer, regular, 9.0, MARGIN, top - 13.0, &sheet.subtitle);
        top -= TITLE_BLOCK;
    }
    // column heads
    let xs = {
        let mut x = MARGIN;
        let mut xs = [0.0; 5];
        for (i, w) in COLUMNS.iter().enumerate() {
            xs[i] = x;
            x += w;
        }
        xs
    };
    for (i, head) in ["#", "", "Artist", "Title", "Label / cat. no."]
        .iter()
        .enumerate()
    {
        let x = if i == 0 { xs[0] + 1.0 } else { xs[i] + 1.0 };
        text(layer, bold, 8.5, x, top - 5.5, head);
    }
    layer.set_outline_color(black.clone());
    layer.set_outline_thickness(0.6);
    line(
        layer,
        MARGIN,
        top - HEAD_H + 1.0,
        PAGE_W - MARGIN,
        top - HEAD_H + 1.0,
    );
    top -= HEAD_H;
    for row in rows {
        let baseline = top - ROW_H + 1.9;
        text(layer, bold, FONT_PT, xs[0] + 1.0, baseline, &row.number);
        text(layer, regular, FONT_PT, xs[2] + 1.0, baseline, &row.artist);
        text(layer, regular, FONT_PT, xs[3] + 1.0, baseline, &row.title);
        text(layer, regular, FONT_PT, xs[4] + 1.0, baseline, &row.label);
        // a box to tick when the record is put on the shelf
        layer.set_outline_color(black.clone());
        layer.set_outline_thickness(0.5);
        let (bx, by, side) = (xs[1] + 1.5, top - ROW_H + 1.4, 3.2);
        layer.add_line(Line {
            points: vec![
                (Point::new(Mm(bx), Mm(by)), false),
                (Point::new(Mm(bx + side), Mm(by)), false),
                (Point::new(Mm(bx + side), Mm(by + side)), false),
                (Point::new(Mm(bx), Mm(by + side)), false),
            ],
            is_closed: true,
        });
        layer.set_outline_color(grey.clone());
        layer.set_outline_thickness(0.25);
        line(layer, MARGIN, top - ROW_H, PAGE_W - MARGIN, top - ROW_H);
        top -= ROW_H;
    }
    layer.set_fill_color(black);
    text(
        layer,
        regular,
        8.0,
        MARGIN,
        MARGIN - 4.0,
        &format!("RecordShelf · page {} of {}", index + 1, total),
    );
}

/// Today's date as `2026-11-06` (UTC).
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil_date(secs / 86_400)
}

/// The date `days` after 1970-01-01 (proleptic Gregorian calendar).
fn civil_date(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Where the PDF of a stack is written.
pub fn pdf_path(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("recordshelf")
        .join(format!("stack-{safe}.pdf"))
}

/// Makes the PDF file of a stack.
pub fn write_pdf(path: &Path, name: &str, entries: &[StackEntry]) -> Result<()> {
    let bytes = render_pdf(&sheet(name, &today(), entries)?)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

/// The printers CUPS knows, and its default one.
pub fn cups_printers() -> Result<(Vec<String>, Option<String>)> {
    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("lpstat")
            .args(args)
            .output()
            .context("run lpstat (is CUPS installed?)")?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    Ok((
        parse_printers(&run(&["-p"])?),
        parse_default(&run(&["-d"])?),
    ))
}

/// The names in the output of `lpstat -p`: "printer NAME is idle. ..."
pub fn parse_printers(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix("printer "))
        .filter_map(|l| l.split_whitespace().next())
        .map(String::from)
        .collect()
}

/// The default in the output of `lpstat -d`.
pub fn parse_default(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|l| l.strip_prefix("system default destination:"))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// The printer to use: the one chosen, else CUPS' default, else the only one there is.
pub fn choose_printer(chosen: &str, printers: &[String], default: Option<&str>) -> Result<String> {
    let chosen = chosen.trim();
    if !chosen.is_empty() {
        return Ok(chosen.to_string());
    }
    if let Some(d) = default {
        return Ok(d.to_string());
    }
    match printers {
        [only] => Ok(only.clone()),
        [] => bail!("no printer found (is CUPS running?)"),
        _ => bail!("choose a printer first (Stacks tab, \"List printer\")"),
    }
}

/// Sends the file to the printer; returns what CUPS says ("request id is ...").
pub fn print_file(path: &Path, printer: &str, title: &str) -> Result<String> {
    let out = Command::new("lp")
        .arg("-d")
        .arg(printer)
        .arg("-t")
        .arg(title)
        .arg(path)
        .output()
        .context("run lp (is CUPS installed?)")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(position: u32) -> StackEntry {
        StackEntry {
            position,
            release_id: 1000 + position,
            artists: format!("Artist {position}"),
            title: format!("Title {position}"),
            labels: format!("Label (CAT {position})"),
        }
    }

    fn entries(n: u32) -> Vec<StackEntry> {
        (1..=n).map(entry).collect()
    }

    #[test]
    fn a_short_stack_is_one_page_in_order() {
        let sheet = sheet("Box 3", "2026-11-06", &entries(3)).unwrap();
        assert_eq!(sheet.pages.len(), 1);
        assert_eq!(sheet.title, "Stack Box 3");
        assert_eq!(sheet.subtitle, "3 records · 2026-11-06");
        let numbers: Vec<_> = sheet.pages[0].iter().map(|r| r.number.as_str()).collect();
        assert_eq!(numbers, ["1", "2", "3"]);
        assert_eq!(sheet.pages[0][1].artist, "Artist 2");
        assert_eq!(sheet.pages[0][1].label, "Label (CAT 2)");
    }

    #[test]
    fn a_long_stack_goes_over_several_pages_without_losing_a_record() {
        let n = 200;
        let sheet = sheet("Big", "2026-11-06", &entries(n)).unwrap();
        assert!(sheet.pages.len() >= 3, "{}", sheet.pages.len());
        assert_eq!(sheet.pages[0].len(), rows_on_page(true));
        assert!(
            sheet.pages[0].len() < sheet.pages[1].len(),
            "the title takes room"
        );
        let all: Vec<u32> = sheet
            .pages
            .iter()
            .flatten()
            .map(|r| r.number.parse().unwrap())
            .collect();
        assert_eq!(all, (1..=n).collect::<Vec<_>>());
        assert!(sheet.pages.iter().all(|p| !p.is_empty()));
    }

    #[test]
    fn an_empty_stack_still_makes_a_page() {
        let sheet = sheet("Empty", "2026-11-06", &[]).unwrap();
        assert_eq!(sheet.pages.len(), 1);
        assert!(sheet.pages[0].is_empty());
        assert_eq!(sheet.subtitle, "0 records · 2026-11-06");
        assert!(render_pdf(&sheet).unwrap().starts_with(b"%PDF"));
        let one = super::sheet("One", "2026-11-06", &entries(1)).unwrap();
        assert!(one.subtitle.starts_with("1 record ·"));
    }

    #[test]
    fn long_texts_are_cut_to_their_column() {
        let mut e = entry(1);
        e.artists = "An artist with a really very long name that cannot fit".into();
        e.title = "A title that is just as long as the artist name, and then some more".into();
        let sheet = sheet("X", "d", &[e]).unwrap();
        let row = &sheet.pages[0][0];
        assert!(row.artist.ends_with('…') && row.title.ends_with('…'));
        let m = Measure::new().unwrap();
        assert!(m.width_mm(FONT_PT, &row.artist) <= COLUMNS[2]);
        assert!(m.width_mm(FONT_PT, &row.title) <= COLUMNS[3]);
        // short ones stay as they are, also with umlauts
        assert_eq!(
            m.fit(FONT_PT, "Björk – Homogenic", 60.0),
            "Björk – Homogenic"
        );
    }

    #[test]
    fn the_measure_matches_the_size_of_the_text() {
        let m = Measure::new().unwrap();
        // at 10 pt a digit is about 5.5 pt wide in DejaVu Sans Condensed: ~1.9 mm
        let one = m.width_mm(10.0, "0");
        assert!((1.5..2.4).contains(&one), "{one}");
        let ten = m.width_mm(10.0, "0000000000");
        assert!((ten - 10.0 * one).abs() < 0.05);
        assert!(m.width_mm(20.0, "abc") > m.width_mm(10.0, "abc") * 1.9);
    }

    #[test]
    fn the_pdf_is_a_pdf_with_a_page_per_sheet_page() {
        let sheet = sheet("Box", "2026-11-06", &entries(120)).unwrap();
        let pdf = render_pdf(&sheet).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        assert!(pdf.windows(5).any(|w| w == b"%%EOF"));
        let text = String::from_utf8_lossy(&pdf);
        let pages = text.matches("/Type/Page").count() + text.matches("/Type /Page").count();
        // the page tree is "/Type /Pages", the pages themselves "/Type /Page"
        let tree = text.matches("/Type/Pages").count() + text.matches("/Type /Pages").count();
        assert_eq!(pages - tree, sheet.pages.len(), "pages in the file");
    }

    #[test]
    fn the_pdf_text_can_be_read_back() {
        // only where poppler's pdftotext is installed
        if Command::new("pdftotext").arg("-v").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("list.pdf");
        let mut e = entry(7);
        e.artists = "Björk".into();
        write_pdf(&path, "Box 3", &[e]).unwrap();
        let out = Command::new("pdftotext")
            .arg("-layout")
            .arg(&path)
            .arg("-")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        for wanted in [
            "Stack Box 3",
            "Björk",
            "Title 7",
            "Label (CAT 7)",
            "page 1 of 1",
        ] {
            assert!(text.contains(wanted), "{wanted:?} in {text}");
        }
    }

    #[test]
    fn dates_are_right() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(365), "1971-01-01");
        assert_eq!(civil_date(19_782), "2024-02-29");
        assert_eq!(civil_date(20_398), "2025-11-06");
        let today = today();
        assert_eq!(today.len(), 10);
        assert!(today.starts_with("20"));
    }

    #[test]
    fn the_file_name_is_safe() {
        let p = pdf_path("Box 3/äb:c");
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "stack-Box_3_äb_c.pdf");
        assert!(p.to_string_lossy().contains("recordshelf"));
    }

    #[test]
    fn writing_makes_the_directory_and_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/er/list.pdf");
        write_pdf(&path, "Box", &entries(2)).unwrap();
        assert!(std::fs::read(&path).unwrap().starts_with(b"%PDF"));
    }

    #[test]
    fn cups_output_is_read() {
        let p = "printer HP_LaserJet_Pro_3001-3008_ is idle.  enabled since Fri 30 Oct 2026 21:53:10 CET\nprinter Other disabled since Mon\n";
        assert_eq!(parse_printers(p), ["HP_LaserJet_Pro_3001-3008_", "Other"]);
        assert!(parse_printers("").is_empty());
        assert!(parse_printers("lpstat: No destinations added.").is_empty());
        assert_eq!(
            parse_default("system default destination: Office\n").as_deref(),
            Some("Office")
        );
        assert_eq!(parse_default("no system default destination\n"), None);
    }

    #[test]
    fn the_printer_is_chosen_in_a_sensible_order() {
        let two = vec!["A".to_string(), "B".to_string()];
        assert_eq!(choose_printer(" B ", &two, Some("A")).unwrap(), "B");
        assert_eq!(choose_printer("", &two, Some("A")).unwrap(), "A");
        assert_eq!(
            choose_printer("", &["Only".to_string()], None).unwrap(),
            "Only"
        );
        assert!(
            choose_printer("", &two, None)
                .unwrap_err()
                .to_string()
                .contains("choose a printer")
        );
        assert!(
            choose_printer("", &[], None)
                .unwrap_err()
                .to_string()
                .contains("no printer")
        );
    }
}
