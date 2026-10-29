//! A Discogs release as it is stored in our own database (see db/README.md):
//! only what is needed for daily use, normalised. This module is pure; the
//! database access is in `library`.

use anyhow::{Result, bail};

use crate::discogs::{Artist, Format, Release, Track};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistLink {
    pub id: i32,
    pub name: String,
    /// What the sleeve puts after this artist, before the next one.
    pub join_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelLink {
    pub id: i32,
    pub name: String,
    pub catalog_number: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatRow {
    pub qty: i16,
    pub size: Option<i16>,
    pub rpm: Option<i16>,
    /// `mono`, `stereo` or `quadraphonic`.
    pub channels: &'static str,
    pub weight_g: Option<i16>,
    pub text: Option<String>,
    pub descriptions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRow {
    pub position: Option<String>,
    pub title: String,
    pub duration_seconds: Option<i32>,
    /// Only where they differ from the release artists.
    pub artists: Vec<ArtistLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditRow {
    pub artist_id: i32,
    pub artist_name: String,
    pub role: String,
    pub tracks: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueRelease {
    pub id: i32,
    pub master_id: Option<i32>,
    pub title: String,
    /// "YYYY-MM-DD"; a partial date is its first day.
    pub released_on: Option<String>,
    /// `year`, `month` or `day`; set exactly when `released_on` is.
    pub released_precision: Option<&'static str>,
    pub country: Option<String>,
    pub genres: Vec<String>,
    pub styles: Vec<String>,
    pub notes: Option<String>,
    pub discogs_uri: Option<String>,
    pub copies: i16,
    pub artists: Vec<ArtistLink>,
    pub labels: Vec<LabelLink>,
    pub formats: Vec<FormatRow>,
    pub tracks: Vec<TrackRow>,
    pub credits: Vec<CreditRow>,
}

/// Artists as printed on the sleeve: each name followed by what the sleeve
/// puts before the next one ("A feat. B", "A & B", "A, B"). `None` joins as a comma.
pub fn join_artists<'a>(artists: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) -> String {
    let mut out = String::new();
    let mut pending_join: Option<Option<&str>> = None;
    for (name, join) in artists {
        if let Some(previous) = pending_join {
            match previous.map(str::trim).filter(|j| !j.is_empty()) {
                None => out.push_str(", "),
                Some(j) if matches!(j, "," | ";") => {
                    out.push_str(j);
                    out.push(' ');
                }
                Some(j) => {
                    out.push(' ');
                    out.push_str(j);
                    out.push(' ');
                }
            }
        }
        out.push_str(name);
        pending_join = Some(join);
    }
    out
}

/// Lower case without accents, for searching: "Björk" and "bjork" agree.
pub fn fold(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut out = String::with_capacity(text.len());
    for c in text
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
    {
        match c {
            'ß' => out.push_str("ss"),
            'æ' | 'Æ' => out.push_str("ae"),
            'œ' | 'Œ' => out.push_str("oe"),
            'ø' | 'Ø' => out.push('o'),
            'ł' | 'Ł' => out.push('l'),
            'đ' | 'Đ' => out.push('d'),
            'ð' | 'Ð' => out.push('d'),
            _ => out.extend(c.to_lowercase()),
        }
    }
    out
}

fn non_empty(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn id32(id: u32) -> Option<i32> {
    i32::try_from(id).ok()
}

impl CatalogueRelease {
    /// Normalises a Discogs release. `copies` is the number of copies in the
    /// Discogs collection (at least 1; a release we do not own is not stored).
    pub fn from_release(id: u32, release: &Release, copies: u16) -> Result<Self> {
        let Some(title) = non_empty(release.title.as_deref()) else {
            bail!("release {id} has no title");
        };
        let Some(db_id) = id32(id) else {
            bail!("release id {id} is out of range");
        };
        if copies == 0 {
            bail!("a release without copies is not stored");
        }

        let artists = artist_links(release.artists.iter().flatten());
        let (released_on, released_precision) =
            parse_released(release.released.as_deref(), release.year);

        Ok(Self {
            id: db_id,
            master_id: release.master_id.and_then(id32),
            title,
            released_on,
            released_precision,
            country: non_empty(release.country.as_deref()),
            genres: clean_list(release.genres.iter().flatten()),
            styles: clean_list(release.styles.iter().flatten()),
            notes: non_empty(release.notes.as_deref()),
            discogs_uri: non_empty(release.uri.as_deref()),
            copies: i16::try_from(copies).unwrap_or(i16::MAX),
            labels: label_links(release),
            formats: release
                .formats
                .iter()
                .flatten()
                .filter_map(format_row)
                .collect(),
            tracks: track_rows(release.tracklist.iter().flatten(), &artists),
            credits: credit_rows(release),
            artists,
        })
    }
}

fn clean_list<'a>(items: impl Iterator<Item = &'a String>) -> Vec<String> {
    items.filter_map(|s| non_empty(Some(s))).collect()
}

/// Artists in sleeve order, without duplicates and without those lacking an
/// id. The join text of the last artist is dropped (nothing follows it).
fn artist_links<'a>(artists: impl Iterator<Item = &'a Artist>) -> Vec<ArtistLink> {
    let mut out: Vec<ArtistLink> = Vec::new();
    for artist in artists {
        let (Some(id), Some(name)) = (artist.id.and_then(id32), non_empty(artist.name.as_deref()))
        else {
            continue;
        };
        if out.iter().any(|a| a.id == id) {
            continue;
        }
        out.push(ArtistLink {
            id,
            name,
            join_text: non_empty(artist.join.as_deref()),
        });
    }
    if let Some(last) = out.last_mut() {
        last.join_text = None;
    }
    out
}

fn label_links(release: &Release) -> Vec<LabelLink> {
    let mut out: Vec<LabelLink> = Vec::new();
    for label in release.labels.iter().flatten() {
        let (Some(id), Some(name)) = (label.id.and_then(id32), non_empty(label.name.as_deref()))
        else {
            continue;
        };
        // Discogs writes "none" when a release has no catalog number
        let catalog_number = non_empty(label.catno.as_deref())
            .filter(|c| !c.eq_ignore_ascii_case("none"))
            .unwrap_or_default();
        if out
            .iter()
            .any(|l| l.id == id && l.catalog_number == catalog_number)
        {
            continue;
        }
        out.push(LabelLink {
            id,
            name,
            catalog_number,
        });
    }
    out
}

fn credit_rows(release: &Release) -> Vec<CreditRow> {
    release
        .extraartists
        .iter()
        .flatten()
        .filter_map(|c| {
            Some(CreditRow {
                artist_id: c.id.and_then(id32)?,
                artist_name: non_empty(c.name.as_deref())?,
                role: non_empty(c.role.as_deref())?,
                tracks: non_empty(c.tracks.as_deref()),
            })
        })
        .collect()
}

/// Real tracks only: headings are skipped, an index track is replaced by its
/// sub tracks.
fn track_rows<'a>(
    tracks: impl Iterator<Item = &'a Track>,
    release_artists: &[ArtistLink],
) -> Vec<TrackRow> {
    fn flatten<'a>(track: &'a Track, out: &mut Vec<&'a Track>) {
        match track.kind.as_deref() {
            Some("heading") => {}
            _ => match track.sub_tracks.as_deref() {
                Some(subs) if !subs.is_empty() => subs.iter().for_each(|s| flatten(s, out)),
                _ => out.push(track),
            },
        }
    }
    let mut flat = Vec::new();
    tracks.for_each(|t| flatten(t, &mut flat));
    flat.into_iter()
        .filter_map(|t| {
            let title = non_empty(t.title.as_deref())?;
            let artists = artist_links(t.artists.iter().flatten());
            let same = artists
                .iter()
                .map(|a| a.id)
                .eq(release_artists.iter().map(|a| a.id));
            Some(TrackRow {
                position: non_empty(t.position.as_deref()),
                title,
                duration_seconds: t.duration.as_deref().and_then(parse_duration),
                artists: if same { Vec::new() } else { artists },
            })
        })
        .collect()
}

/// "3:45" -> 225, "1:02:03" -> 3723; anything else -> None.
pub fn parse_duration(text: &str) -> Option<i32> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut seconds: i64 = 0;
    for part in parts {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        seconds = seconds * 60 + part.parse::<i64>().ok()?;
    }
    i32::try_from(seconds).ok()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Reads a Discogs date. "2017-03-10" is a day, "2017-03" / "2017-03-00" a
/// month, "2017" / "2017-00-00" a year. Impossible parts fall back to the
/// coarser precision. Without a usable date the release year (if any) is used.
pub fn parse_released(
    released: Option<&str>,
    year: Option<u32>,
) -> (Option<String>, Option<&'static str>) {
    let from_year = |y: i32| (Some(format!("{y:04}-01-01")), Some("year"));
    if let Some(text) = released {
        let mut parts = text.trim().splitn(3, '-');
        let y = parts
            .next()
            .and_then(|p| p.parse::<i32>().ok())
            .filter(|y| (1000..=9999).contains(y));
        if let Some(y) = y {
            let m = parts
                .next()
                .and_then(|p| p.parse::<u32>().ok())
                .filter(|m| (1..=12).contains(m));
            if let Some(m) = m {
                let d = parts
                    .next()
                    .and_then(|p| p.parse::<u32>().ok())
                    .filter(|d| (1..=days_in_month(y, m)).contains(d));
                return match d {
                    Some(d) => (Some(format!("{y:04}-{m:02}-{d:02}")), Some("day")),
                    None => (Some(format!("{y:04}-{m:02}-01")), Some("month")),
                };
            }
            return from_year(y);
        }
    }
    match year
        .and_then(|y| i32::try_from(y).ok())
        .filter(|y| (1000..=9999).contains(y))
    {
        Some(y) => from_year(y),
        None => (None, None),
    }
}

/// A weight in grams from text such as "180 Gram", "140g" or "180 Gram / Green Marble".
pub fn parse_weight(text: &str) -> Option<i16> {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let number: String = chars[start..i].iter().collect();
            let mut j = i;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            let word: String = chars[j..]
                .iter()
                .take_while(|c| c.is_alphabetic())
                .collect();
            if matches!(word.as_str(), "g" | "gr" | "gram" | "grams" | "gm" | "gms")
                && let Ok(value) = number.parse::<i16>()
                && (20..=500).contains(&value)
            {
                return Some(value);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// One vinyl format of a release; other media (CD, cassette in a box set) are skipped.
fn format_row(format: &Format) -> Option<FormatRow> {
    if !format
        .name
        .as_deref()
        .is_some_and(|n| n.trim().eq_ignore_ascii_case("vinyl"))
    {
        return None;
    }
    let mut size = None;
    let mut rpm = None;
    let mut channels: Option<&'static str> = None;
    let mut weight = None;
    let mut rest: Vec<String> = Vec::new();
    for description in format.descriptions.iter().flatten() {
        let tag = description.trim();
        let lower = tag.to_lowercase();
        if tag.is_empty() {
            continue;
        }
        match lower.as_str() {
            "7\"" => size = size.or(Some(7)),
            "10\"" => size = size.or(Some(10)),
            "12\"" => size = size.or(Some(12)),
            "mono" => channels = channels.or(Some("mono")),
            "stereo" => channels = channels.or(Some("stereo")),
            "quadraphonic" => channels = channels.or(Some("quadraphonic")),
            _ if lower.ends_with("rpm")
                && ["33", "45", "78"].iter().any(|r| lower.starts_with(r)) =>
            {
                rpm = rpm.or(["33", "45", "78"]
                    .iter()
                    .find(|r| lower.starts_with(**r))
                    .and_then(|r| r.parse().ok()));
            }
            _ if parse_weight(tag).is_some() && lower.contains("gram") => {
                weight = weight.or(parse_weight(tag));
            }
            _ => rest.push(tag.to_string()),
        }
    }
    // an LP without a stated size is a 12 inch
    if size.is_none() && rest.iter().any(|t| t == "LP") {
        size = Some(12);
    }
    let text = non_empty(format.text.as_deref());
    if weight.is_none() {
        weight = text.as_deref().and_then(parse_weight);
    }
    Some(FormatRow {
        qty: format
            .qty
            .and_then(|q| i16::try_from(q).ok())
            .filter(|q| *q >= 1)
            .unwrap_or(1),
        size,
        rpm,
        channels: channels.unwrap_or("stereo"),
        weight_g: weight,
        text,
        descriptions: rest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(json: &str) -> Release {
        serde_json::from_str(json).unwrap()
    }

    fn full() -> Release {
        release(
            r#"{"id": 9, "master_id": 77, "title": " Speicher 95 Tribute EP ", "released": "2017-03-10", "year": 2017,
                "country": "DE", "genres": ["Electronic", ""], "styles": ["Techno"], "notes": "  ", "uri": "https://www.discogs.com/release/9",
                "artists": [{"id": 1, "name": "Laurent Garnier", "join": "feat."}, {"id": 2, "name": "Someone", "join": ","}],
                "labels": [{"id": 5, "name": "Kompakt Extra", "catno": "Kompakt Ex 095"}, {"id": 5, "name": "Kompakt Extra", "catno": "none"}],
                "formats": [{"name": "Vinyl", "qty": "1", "descriptions": ["12\"", "33 ⅓ RPM", "EP", "Stereo", "180 Gram"], "text": "X-ray Colour"},
                            {"name": "CD", "descriptions": ["Album"]}],
                "extraartists": [{"id": 3, "name": "Producer Pete", "role": "Producer", "tracks": "A1"}, {"id": 4, "name": "No Role"}],
                "tracklist": [{"position": "A", "type_": "track", "title": "Doctor", "duration": "6:20"},
                              {"position": "", "type_": "heading", "title": "Side B"},
                              {"position": "B", "type_": "track", "title": "Crypt", "duration": "", "artists": [{"id": 9, "name": "Guest"}]}]}"#,
        )
    }

    #[test]
    fn maps_a_whole_release() {
        let r = CatalogueRelease::from_release(9, &full(), 2).unwrap();
        assert_eq!((r.id, r.master_id, r.copies), (9, Some(77), 2));
        assert_eq!(r.title, "Speicher 95 Tribute EP");
        assert_eq!(
            (r.released_on.as_deref(), r.released_precision),
            (Some("2017-03-10"), Some("day"))
        );
        assert_eq!(r.country.as_deref(), Some("DE"));
        assert_eq!(r.genres, ["Electronic"], "empty entries are dropped");
        assert_eq!(r.styles, ["Techno"]);
        assert_eq!(r.notes, None, "blank notes are not stored");
        assert_eq!(r.artists.len(), 2);
        assert_eq!(r.artists[0].join_text.as_deref(), Some("feat."));
        assert_eq!(
            r.artists[1].join_text, None,
            "nothing follows the last artist"
        );
        assert_eq!(r.labels.len(), 2, "same label, two catalog numbers");
        assert_eq!(r.labels[0].catalog_number, "Kompakt Ex 095");
        assert_eq!(
            r.labels[1].catalog_number, "",
            "'none' is no catalog number"
        );
        assert_eq!(r.formats.len(), 1, "the CD is skipped");
        assert_eq!(r.tracks.len(), 2, "the heading is skipped");
        assert_eq!(r.tracks[0].duration_seconds, Some(380));
        assert_eq!(r.tracks[1].duration_seconds, None);
        assert_eq!(
            r.tracks[1].artists.len(),
            1,
            "a track artist that differs is kept"
        );
        assert!(r.tracks[0].artists.is_empty());
        assert_eq!(r.credits.len(), 1, "a credit needs a role");
        assert_eq!(r.credits[0].role, "Producer");
    }

    #[test]
    fn needs_a_title_a_valid_id_and_copies() {
        assert!(CatalogueRelease::from_release(1, &release(r#"{"title": "  "}"#), 1).is_err());
        assert!(CatalogueRelease::from_release(1, &release("{}"), 1).is_err());
        assert!(
            CatalogueRelease::from_release(u32::MAX, &release(r#"{"title": "T"}"#), 1).is_err()
        );
        assert!(CatalogueRelease::from_release(1, &release(r#"{"title": "T"}"#), 0).is_err());
        assert!(CatalogueRelease::from_release(1, &release(r#"{"title": "T"}"#), 1).is_ok());
    }

    #[test]
    fn partial_dates_keep_their_precision() {
        let p = |r: Option<&str>, y: Option<u32>| {
            let (d, prec) = parse_released(r, y);
            (d.unwrap_or_default(), prec.unwrap_or(""))
        };
        assert_eq!(p(Some("2017-03-10"), None), ("2017-03-10".into(), "day"));
        assert_eq!(p(Some("2017-03"), None), ("2017-03-01".into(), "month"));
        assert_eq!(p(Some("2017-03-00"), None), ("2017-03-01".into(), "month"));
        assert_eq!(p(Some("2017"), None), ("2017-01-01".into(), "year"));
        assert_eq!(p(Some("2017-00-00"), None), ("2017-01-01".into(), "year"));
        assert_eq!(p(Some(" 2017-03-10 "), None), ("2017-03-10".into(), "day"));
        assert_eq!(
            p(Some("2016-02-29"), None),
            ("2016-02-29".into(), "day"),
            "leap day"
        );
        assert_eq!(
            p(Some("2017-02-29"), None),
            ("2017-02-01".into(), "month"),
            "not a leap year"
        );
        assert_eq!(p(Some("1900-02-29"), None), ("1900-02-01".into(), "month"));
        assert_eq!(p(Some("2000-02-29"), None), ("2000-02-29".into(), "day"));
        assert_eq!(
            p(Some("2017-13-01"), None),
            ("2017-01-01".into(), "year"),
            "no such month"
        );
        assert_eq!(p(Some("2017-04-31"), None), ("2017-04-01".into(), "month"));
    }

    #[test]
    fn missing_or_broken_dates_fall_back_to_the_year() {
        assert_eq!(
            parse_released(None, Some(1999)),
            (Some("1999-01-01".into()), Some("year"))
        );
        assert_eq!(
            parse_released(Some("garbage"), Some(1999)),
            (Some("1999-01-01".into()), Some("year"))
        );
        assert_eq!(parse_released(Some(""), None), (None, None));
        assert_eq!(parse_released(None, None), (None, None));
        assert_eq!(
            parse_released(None, Some(0)),
            (None, None),
            "Discogs uses 0 for unknown"
        );
        assert_eq!(parse_released(Some("0000-00-00"), Some(0)), (None, None));
    }

    #[test]
    fn durations_become_seconds() {
        assert_eq!(parse_duration("3:45"), Some(225));
        assert_eq!(parse_duration(" 12:03 "), Some(723));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("0:00"), Some(0));
        for bad in ["", "345", "3:", ":45", "a:b", "3:45:10:1", "3:-5", "3.5"] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn weights_are_found_in_tags_and_text() {
        for (text, grams) in [
            ("180 Gram", 180),
            ("140g", 140),
            ("200 GR", 200),
            ("180 Gram / Green Marble", 180),
            ("Heavyweight 160 gm", 160),
        ] {
            assert_eq!(parse_weight(text), Some(grams), "{text}");
        }
        for text in [
            "",
            "Green Marble",
            "33 ⅓ RPM",
            "12\"",
            "5 Gram",
            "9999 Gram",
            "180",
            "180 Grammy",
        ] {
            assert_eq!(parse_weight(text), None, "{text}");
        }
    }

    fn vinyl(json: &str) -> FormatRow {
        let f: Format = serde_json::from_str(json).unwrap();
        format_row(&f).unwrap()
    }

    #[test]
    fn format_details_are_split_into_columns() {
        let row = vinyl(
            r#"{"name": "Vinyl", "qty": "1", "text": "X-ray Colour", "descriptions": ["12\"", "33 ⅓ RPM", "EP", "Stereo"]}"#,
        );
        assert_eq!(
            (row.qty, row.size, row.rpm, row.channels),
            (1, Some(12), Some(33), "stereo")
        );
        assert_eq!(row.weight_g, None);
        assert_eq!(row.text.as_deref(), Some("X-ray Colour"));
        assert_eq!(
            row.descriptions,
            ["EP"],
            "what became a column is not repeated"
        );

        let row = vinyl(
            r#"{"name": "Vinyl", "descriptions": ["7\"", "45 RPM", "Single", "Mono", "Reissue"]}"#,
        );
        assert_eq!(
            (row.size, row.rpm, row.channels),
            (Some(7), Some(45), "mono")
        );
        assert_eq!(row.descriptions, ["Single", "Reissue"]);

        let row = vinyl(r#"{"name": "Vinyl", "descriptions": ["10\"", "78 RPM", "Shellac"]}"#);
        assert_eq!((row.size, row.rpm), (Some(10), Some(78)));

        let row = vinyl(r#"{"name": "Vinyl", "descriptions": ["12\"", "Quadraphonic"]}"#);
        assert_eq!(row.channels, "quadraphonic");
    }

    #[test]
    fn channels_default_to_stereo_and_lp_means_twelve_inch() {
        let row = vinyl(r#"{"name": "Vinyl", "descriptions": ["LP", "Album"]}"#);
        assert_eq!((row.size, row.channels), (Some(12), "stereo"));
        assert_eq!(row.descriptions, ["LP", "Album"], "LP stays as a tag");
        let row = vinyl(r#"{"name": "Vinyl", "descriptions": ["EP"]}"#);
        assert_eq!(row.size, None, "no size is guessed for an EP");
        let row = vinyl(r#"{"name": "Vinyl", "descriptions": ["7\"", "LP"]}"#);
        assert_eq!(row.size, Some(7), "a stated size wins");
    }

    #[test]
    fn weight_comes_from_a_tag_or_the_text() {
        let row = vinyl(r#"{"name": "Vinyl", "descriptions": ["12\"", "180 Gram"]}"#);
        assert_eq!(row.weight_g, Some(180));
        assert!(row.descriptions.is_empty());
        let row = vinyl(
            r#"{"name": "Vinyl", "descriptions": ["12\"", "EP"], "text": "180 Gram / Green Marble"}"#,
        );
        assert_eq!(row.weight_g, Some(180));
        assert_eq!(
            row.text.as_deref(),
            Some("180 Gram / Green Marble"),
            "the text stays as it is"
        );
        let row = vinyl(r#"{"name": "Vinyl", "descriptions": ["12\"", "EP"]}"#);
        assert_eq!(row.weight_g, None);
    }

    #[test]
    fn qty_defaults_to_one() {
        assert_eq!(vinyl(r#"{"name": "Vinyl"}"#).qty, 1);
        assert_eq!(vinyl(r#"{"name": "Vinyl", "qty": "3"}"#).qty, 3);
        assert_eq!(vinyl(r#"{"name": "Vinyl", "qty": "0"}"#).qty, 1);
        assert_eq!(vinyl(r#"{"name": "Vinyl", "qty": "junk"}"#).qty, 1);
    }

    #[test]
    fn only_vinyl_formats_are_kept() {
        let r = release(
            r#"{"title": "T", "formats": [{"name": "CD"}, {"name": "Vinyl", "descriptions": ["7\""]}, {"name": "Cassette"}, {"name": "vinyl"}]}"#,
        );
        let c = CatalogueRelease::from_release(1, &r, 1).unwrap();
        assert_eq!(c.formats.len(), 2);
        let r = release(r#"{"title": "T", "formats": [{"name": "CD"}]}"#);
        assert!(
            CatalogueRelease::from_release(1, &r, 1)
                .unwrap()
                .formats
                .is_empty()
        );
    }

    #[test]
    fn artists_are_deduplicated_and_need_an_id() {
        let r = release(
            r#"{"title": "T", "artists": [{"id": 1, "name": "A", "join": "&"}, {"name": "No id"}, {"id": 1, "name": "A again"}, {"id": 2, "name": "B", "join": " , "}]}"#,
        );
        let c = CatalogueRelease::from_release(1, &r, 1).unwrap();
        assert_eq!(c.artists.iter().map(|a| a.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(c.artists[0].join_text.as_deref(), Some("&"));
        assert_eq!(c.artists[0].name, "A");
    }

    #[test]
    fn index_tracks_are_replaced_by_their_sub_tracks() {
        let r = release(
            r#"{"title": "T", "tracklist": [
                {"position": "A", "type_": "index", "title": "Suite", "sub_tracks": [
                    {"position": "A.1", "title": "Part 1", "duration": "1:00"}, {"position": "A.2", "title": "Part 2"}]},
                {"position": "B", "type_": "track", "title": "Last"},
                {"position": "", "type_": "heading", "title": "Bonus"},
                {"position": "C", "type_": "track", "title": ""}]}"#,
        );
        let c = CatalogueRelease::from_release(1, &r, 1).unwrap();
        let titles: Vec<_> = c.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Part 1", "Part 2", "Last"]);
        assert_eq!(c.tracks[0].position.as_deref(), Some("A.1"));
    }

    #[test]
    fn track_artists_equal_to_the_release_artists_are_not_repeated() {
        let r = release(
            r#"{"title": "T", "artists": [{"id": 1, "name": "A"}],
            "tracklist": [{"position": "1", "title": "Same", "artists": [{"id": 1, "name": "A"}]},
                          {"position": "2", "title": "Other", "artists": [{"id": 2, "name": "B"}]}]}"#,
        );
        let c = CatalogueRelease::from_release(1, &r, 1).unwrap();
        assert!(c.tracks[0].artists.is_empty());
        assert_eq!(c.tracks[1].artists[0].id, 2);
    }

    #[test]
    fn a_sparse_release_maps_without_panicking() {
        let c =
            CatalogueRelease::from_release(5, &release(r#"{"title": "Only A Title"}"#), 1).unwrap();
        assert!(c.artists.is_empty() && c.labels.is_empty() && c.formats.is_empty());
        assert!(c.tracks.is_empty() && c.credits.is_empty());
        assert_eq!((c.released_on, c.released_precision), (None, None));
    }

    #[test]
    fn artists_are_joined_as_on_the_sleeve() {
        assert_eq!(
            join_artists([("Alexander Kowalski", Some("&")), ("Rebar", None)]),
            "Alexander Kowalski & Rebar"
        );
        assert_eq!(
            join_artists([("A", Some("feat.")), ("B", None)]),
            "A feat. B"
        );
        assert_eq!(
            join_artists([("A", Some(",")), ("B", Some(",")), ("C", None)]),
            "A, B, C"
        );
        assert_eq!(
            join_artists([("A", None), ("B", None)]),
            "A, B",
            "no join text: a comma"
        );
        assert_eq!(
            join_artists([("A", Some(" ")), ("B", None)]),
            "A, B",
            "blank join text: a comma"
        );
        assert_eq!(join_artists([("Solo", None)]), "Solo");
        assert_eq!(
            join_artists([("A", Some("vs.")), ("B", Some("/")), ("C", None)]),
            "A vs. B / C"
        );
        assert_eq!(join_artists(std::iter::empty()), "");
    }

    #[test]
    fn folding_ignores_case_and_accents() {
        assert_eq!(fold("Björk Guðmundsdóttir"), "bjork gudmundsdottir");
        assert_eq!(fold("KULØR"), "kulor");
        assert_eq!(fold("Müller / Mueller"), "muller / mueller");
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("Æon Œuvre Łódź"), "aeon oeuvre lodz");
        assert_eq!(fold("Dub Techno 12\""), "dub techno 12\"");
        assert_eq!(fold(""), "");
        assert_eq!(fold("ÉÈÊË éèêë"), "eeee eeee");
    }
}
