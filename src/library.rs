//! Our own catalogue in PostgreSQL (see db/README.md): connection, schema
//! migrations, and storing / removing releases and their covers.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use postgres::{Client, NoTls};
use sha2::{Digest, Sha256};

use crate::catalogue::{CatalogueRelease, fold, join_artists};

/// Longest side of the stored thumbnail in pixels.
pub const THUMB_SIZE: u32 = 150;

/// The migrations this version of the app knows, in order. They are the same
/// files `db/db.sh` applies, with the same checksum (sha256 of the file).
/// `every_migration_file_is_embedded` fails when a file is missing here.
const MIGRATIONS: &[(i32, &str, &str)] = &[(
    1,
    "0001_init.sql",
    include_str!("../db/migrations/0001_init.sql"),
)];

fn checksum(sql: &str) -> String {
    Sha256::digest(sql.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What we know about a release in our database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub copies: i16,
    pub has_cover: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveOutcome {
    /// The release was not in the database before.
    pub created: bool,
}

/// One line of the list of all records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRow {
    pub id: u32,
    pub title: String,
    /// Artists as on the sleeve ("A feat. B").
    pub artists: String,
    /// "Label (CAT 1), Other Label".
    pub labels: String,
    pub year: Option<i16>,
    /// "12", 33 RPM, 180 g, EP, X-ray Colour"
    pub format: String,
    pub copies: i16,
    pub has_cover: bool,
    /// Everything searchable about the record, lower case without accents.
    pub haystack: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFormat {
    pub qty: i16,
    pub size: Option<i16>,
    pub rpm: Option<i16>,
    pub channels: String,
    pub weight_g: Option<i16>,
    pub text: Option<String>,
    pub descriptions: Vec<String>,
}

impl StoredFormat {
    /// `12", 33 RPM, 180 g, EP, X-ray Colour` (and "2 x" for several discs).
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.qty > 1 {
            parts.push(format!("{} x", self.qty));
        }
        let mut first = String::new();
        if let Some(size) = self.size {
            first.push_str(&format!("{size}\""));
        }
        if !first.is_empty() {
            parts.push(first);
        }
        if let Some(rpm) = self.rpm {
            parts.push(format!("{rpm} RPM"));
        }
        if let Some(weight) = self.weight_g {
            parts.push(format!("{weight} g"));
        }
        if self.channels != "stereo" {
            let mut channels = self.channels.clone();
            if let Some(c) = channels.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            parts.push(channels);
        }
        parts.extend(self.descriptions.iter().cloned());
        if let Some(text) = &self.text {
            parts.push(text.clone());
        }
        // "2 x 12"" reads better than "2 x, 12\""
        match parts.first() {
            Some(first) if first.ends_with(" x") && parts.len() > 1 => {
                let merged = format!("{first} {}", parts[1]);
                parts.splice(0..2, [merged]);
            }
            _ => {}
        }
        parts.join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTrack {
    pub position: Option<String>,
    pub title: String,
    pub duration_seconds: Option<i32>,
}

/// A record as stored in our database, with everything needed to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRelease {
    pub id: u32,
    pub master_id: Option<i32>,
    pub title: String,
    pub released_on: Option<String>,
    pub released_precision: Option<String>,
    pub year: Option<i16>,
    pub country: Option<String>,
    pub genres: Vec<String>,
    pub styles: Vec<String>,
    pub notes: Option<String>,
    pub discogs_uri: Option<String>,
    pub copies: i16,
    /// (name, what the sleeve puts after the name)
    pub artists: Vec<(String, Option<String>)>,
    /// (label, catalog number)
    pub labels: Vec<(String, String)>,
    pub formats: Vec<StoredFormat>,
    pub tracks: Vec<StoredTrack>,
    /// (role, name, tracks)
    pub credits: Vec<(String, String, Option<String>)>,
}

impl StoredRelease {
    pub fn artist_display(&self) -> String {
        join_artists(self.artists.iter().map(|(n, j)| (n.as_str(), j.as_deref())))
    }

    pub fn format_display(&self) -> String {
        self.formats
            .iter()
            .map(StoredFormat::summary)
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

#[cfg(test)]
impl StoredRelease {
    /// What reading a stored `CatalogueRelease` back gives (for tests that need a
    /// record without a database).
    pub fn from_catalogue(c: &CatalogueRelease) -> Self {
        Self {
            id: c.id as u32,
            master_id: c.master_id,
            title: c.title.clone(),
            released_on: c.released_on.clone(),
            released_precision: c.released_precision.map(str::to_string),
            year: c.released_on.as_ref().and_then(|d| d[..4].parse().ok()),
            country: c.country.clone(),
            genres: c.genres.clone(),
            styles: c.styles.clone(),
            notes: c.notes.clone(),
            discogs_uri: c.discogs_uri.clone(),
            copies: c.copies,
            artists: c
                .artists
                .iter()
                .map(|a| (a.name.clone(), a.join_text.clone()))
                .collect(),
            labels: c
                .labels
                .iter()
                .map(|l| (l.name.clone(), l.catalog_number.clone()))
                .collect(),
            formats: c
                .formats
                .iter()
                .map(|f| StoredFormat {
                    qty: f.qty,
                    size: f.size,
                    rpm: f.rpm,
                    channels: f.channels.to_string(),
                    weight_g: f.weight_g,
                    text: f.text.clone(),
                    descriptions: f.descriptions.clone(),
                })
                .collect(),
            tracks: c
                .tracks
                .iter()
                .map(|t| StoredTrack {
                    position: t.position.clone(),
                    title: t.title.clone(),
                    duration_seconds: t.duration_seconds,
                })
                .collect(),
            credits: Vec::new(),
        }
    }
}

/// "Label (CAT 1), Other Label" from (label, catalog number) pairs.
fn labels_display(labels: &[(String, String)]) -> String {
    labels
        .iter()
        .map(|(name, catno)| {
            if catno.is_empty() {
                name.clone()
            } else {
                format!("{name} ({catno})")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub struct Library {
    client: Client,
}

impl Library {
    /// Connects (plain TCP or unix socket, no TLS) and brings the schema up to
    /// date. Returns the number of migrations that had to be applied.
    pub fn connect(url: &str) -> Result<(Self, usize)> {
        let client = Client::connect(url, NoTls).context("connect to the database")?;
        let mut library = Self { client };
        let applied = library.migrate()?;
        Ok((library, applied))
    }

    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// Applies the migrations that are still pending. Refuses to go on if an
    /// applied migration was changed, or if the database was migrated by a
    /// newer version of the app.
    pub fn migrate(&mut self) -> Result<usize> {
        self.client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    version    integer PRIMARY KEY,
                    name       text NOT NULL,
                    checksum   text NOT NULL,
                    applied_at timestamptz NOT NULL DEFAULT now()
                )",
            )
            .context("create schema_migrations")?;
        let applied: HashMap<i32, String> = self
            .client
            .query("SELECT version, checksum FROM schema_migrations", &[])?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();

        let newest_known = MIGRATIONS.iter().map(|m| m.0).max().unwrap_or(0);
        if let Some(newer) = applied.keys().filter(|v| **v > newest_known).max() {
            bail!(
                "the database was migrated to version {newer} by a newer version of the app (this one knows up to {newest_known})"
            );
        }

        let mut count = 0;
        for (version, name, sql) in MIGRATIONS {
            let sum = checksum(sql);
            match applied.get(version) {
                Some(known) if *known == sum => continue,
                Some(_) => {
                    bail!("migration {name} was changed after it was applied to this database")
                }
                None => {}
            }
            let mut tx = self.client.transaction()?;
            tx.batch_execute(sql)
                .with_context(|| format!("apply migration {name}"))?;
            tx.execute(
                "INSERT INTO schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
                &[version, &name.to_string(), &sum],
            )?;
            tx.commit()?;
            count += 1;
        }
        Ok(count)
    }

    /// Copies and cover state of the given releases; releases we do not have are absent.
    pub fn status(&mut self, ids: &[u32]) -> Result<HashMap<u32, Entry>> {
        let ids: Vec<i32> = ids
            .iter()
            .filter_map(|id| i32::try_from(*id).ok())
            .collect();
        let rows = self.client.query(
            "SELECT r.id, r.copies, (c.release_id IS NOT NULL)
               FROM releases r LEFT JOIN release_covers c ON c.release_id = r.id
              WHERE r.id = ANY($1)",
            &[&ids],
        )?;
        Ok(rows
            .iter()
            .map(|row| {
                let id: i32 = row.get(0);
                (
                    id as u32,
                    Entry {
                        copies: row.get(1),
                        has_cover: row.get(2),
                    },
                )
            })
            .collect())
    }

    /// Stores a release with everything that belongs to it, replacing what was
    /// stored before. All or nothing.
    pub fn save_release(&mut self, r: &CatalogueRelease) -> Result<SaveOutcome> {
        let mut tx = self.client.transaction()?;
        let row = tx.query_one(
            "INSERT INTO releases (id, master_id, title, released_on, released_precision, country,
                                   genres, styles, notes, discogs_uri, copies)
             VALUES ($1, $2, $3, $4::text::date, $5::text::date_precision, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (id) DO UPDATE SET
                 master_id = EXCLUDED.master_id, title = EXCLUDED.title,
                 released_on = EXCLUDED.released_on, released_precision = EXCLUDED.released_precision,
                 country = EXCLUDED.country, genres = EXCLUDED.genres, styles = EXCLUDED.styles,
                 notes = EXCLUDED.notes, discogs_uri = EXCLUDED.discogs_uri, copies = EXCLUDED.copies
             RETURNING (xmax = 0)",
            &[
                &r.id,
                &r.master_id,
                &r.title,
                &r.released_on,
                &r.released_precision.map(str::to_string),
                &r.country,
                &r.genres,
                &r.styles,
                &r.notes,
                &r.discogs_uri,
                &r.copies,
            ],
        )?;
        let created: bool = row.get(0);

        // shared rows first: artists and labels are not owned by one release
        let artists = r
            .artists
            .iter()
            .chain(r.tracks.iter().flat_map(|t| t.artists.iter()))
            .map(|a| (a.id, &a.name))
            .chain(r.credits.iter().map(|c| (c.artist_id, &c.artist_name)));
        for (id, name) in artists {
            tx.execute(
                "INSERT INTO artists (id, name) VALUES ($1, $2)
                 ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
                &[&id, name],
            )?;
        }
        for label in &r.labels {
            tx.execute(
                "INSERT INTO labels (id, name) VALUES ($1, $2)
                 ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
                &[&label.id, &label.name],
            )?;
        }

        // what belongs to the release is replaced as a whole
        for table in [
            "release_artists",
            "release_labels",
            "release_formats",
            "tracks",
            "release_credits",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE release_id = $1"),
                &[&r.id],
            )?;
        }
        for (i, a) in r.artists.iter().enumerate() {
            tx.execute(
                "INSERT INTO release_artists (release_id, position, artist_id, join_text) VALUES ($1, $2, $3, $4)",
                &[&r.id, &(i as i16 + 1), &a.id, &a.join_text],
            )?;
        }
        for l in &r.labels {
            tx.execute(
                "INSERT INTO release_labels (release_id, label_id, catalog_number) VALUES ($1, $2, $3)",
                &[&r.id, &l.id, &l.catalog_number],
            )?;
        }
        for (i, f) in r.formats.iter().enumerate() {
            tx.execute(
                "INSERT INTO release_formats (release_id, position, qty, size, rpm, channels, weight_g, text, descriptions)
                 VALUES ($1, $2, $3, $4, $5, $6::text::channels, $7, $8, $9)",
                &[
                    &r.id,
                    &(i as i16 + 1),
                    &f.qty,
                    &f.size,
                    &f.rpm,
                    &f.channels.to_string(),
                    &f.weight_g,
                    &f.text,
                    &f.descriptions,
                ],
            )?;
        }
        for (i, t) in r.tracks.iter().enumerate() {
            let track_id: i64 = tx
                .query_one(
                    "INSERT INTO tracks (release_id, track_index, position, title, duration_seconds)
                     VALUES ($1, $2, $3, $4, $5) RETURNING id",
                    &[&r.id, &(i as i32), &t.position, &t.title, &t.duration_seconds],
                )?
                .get(0);
            for (j, a) in t.artists.iter().enumerate() {
                tx.execute(
                    "INSERT INTO track_artists (track_id, position, artist_id, join_text) VALUES ($1, $2, $3, $4)",
                    &[&track_id, &(j as i16 + 1), &a.id, &a.join_text],
                )?;
            }
        }
        for c in &r.credits {
            tx.execute(
                "INSERT INTO release_credits (release_id, artist_id, role, tracks) VALUES ($1, $2, $3, $4)",
                &[&r.id, &c.artist_id, &c.role, &c.tracks],
            )?;
        }
        tx.commit()?;
        Ok(SaveOutcome { created })
    }

    /// All records, for browsing and searching. Everything is loaded at once
    /// (a few thousand records are a few megabytes) so that searching is instant.
    pub fn list(&mut self) -> Result<Vec<RecordRow>> {
        use std::collections::BTreeMap;
        let base = self.client.query(
            "SELECT r.id, r.title, r.year, r.country, r.genres, r.styles, r.copies, (c.release_id IS NOT NULL)
               FROM releases r LEFT JOIN release_covers c ON c.release_id = r.id",
            &[],
        )?;
        let mut artists: BTreeMap<i32, Vec<(String, Option<String>)>> = BTreeMap::new();
        for row in self.client.query(
            "SELECT ra.release_id, a.name, ra.join_text FROM release_artists ra
               JOIN artists a ON a.id = ra.artist_id ORDER BY ra.release_id, ra.position",
            &[],
        )? {
            artists
                .entry(row.get(0))
                .or_default()
                .push((row.get(1), row.get(2)));
        }
        let mut labels: BTreeMap<i32, Vec<(String, String)>> = BTreeMap::new();
        for row in self.client.query(
            "SELECT rl.release_id, l.name, rl.catalog_number FROM release_labels rl
               JOIN labels l ON l.id = rl.label_id ORDER BY rl.release_id, l.name, rl.catalog_number",
            &[],
        )? {
            labels.entry(row.get(0)).or_default().push((row.get(1), row.get(2)));
        }
        let mut formats: BTreeMap<i32, Vec<StoredFormat>> = BTreeMap::new();
        for row in self.client.query(
            "SELECT release_id, qty, size, rpm, channels::text, weight_g, text, descriptions
               FROM release_formats ORDER BY release_id, position",
            &[],
        )? {
            formats.entry(row.get(0)).or_default().push(StoredFormat {
                qty: row.get(1),
                size: row.get(2),
                rpm: row.get(3),
                channels: row.get(4),
                weight_g: row.get(5),
                text: row.get(6),
                descriptions: row.get(7),
            });
        }
        let mut tracks: BTreeMap<i32, String> = BTreeMap::new();
        for row in self.client.query(
            "SELECT release_id, string_agg(title, ' ' ORDER BY track_index) FROM tracks GROUP BY release_id",
            &[],
        )? {
            tracks.insert(row.get(0), row.get(1));
        }

        let mut rows: Vec<RecordRow> = base
            .iter()
            .map(|row| {
                let id: i32 = row.get(0);
                let title: String = row.get(1);
                let year: Option<i16> = row.get(2);
                let country: Option<String> = row.get(3);
                let genres: Vec<String> = row.get(4);
                let styles: Vec<String> = row.get(5);
                let artist_list = artists.get(&id).cloned().unwrap_or_default();
                let label_list = labels.get(&id).cloned().unwrap_or_default();
                let artists_text =
                    join_artists(artist_list.iter().map(|(n, j)| (n.as_str(), j.as_deref())));
                let labels_text = labels_display(&label_list);
                let format = formats
                    .get(&id)
                    .map(|f| {
                        f.iter()
                            .map(StoredFormat::summary)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .unwrap_or_default();
                let haystack = fold(&format!(
                    "{title} {artists_text} {labels_text} {} {} {} {} {format} {}",
                    year.map(|y| y.to_string()).unwrap_or_default(),
                    country.unwrap_or_default(),
                    genres.join(" "),
                    styles.join(" "),
                    tracks.get(&id).cloned().unwrap_or_default(),
                ));
                RecordRow {
                    id: id as u32,
                    title,
                    artists: artists_text,
                    labels: labels_text,
                    year,
                    format,
                    copies: row.get(6),
                    has_cover: row.get(7),
                    haystack,
                }
            })
            .collect();
        rows.sort_by_cached_key(|r| (fold(&r.artists), fold(&r.title), r.id));
        Ok(rows)
    }

    /// One record with everything stored about it; `None` if it is not in the database.
    pub fn get_release(&mut self, id: u32) -> Result<Option<StoredRelease>> {
        let db_id = i32::try_from(id).context("release id out of range")?;
        let Some(base) = self.client.query_opt(
            "SELECT title, master_id, released_on::text, released_precision::text, year, country,
                    genres, styles, notes, discogs_uri, copies
               FROM releases WHERE id = $1",
            &[&db_id],
        )?
        else {
            return Ok(None);
        };
        let artists = self
            .client
            .query(
                "SELECT a.name, ra.join_text FROM release_artists ra JOIN artists a ON a.id = ra.artist_id
                  WHERE ra.release_id = $1 ORDER BY ra.position",
                &[&db_id],
            )?
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        let labels = self
            .client
            .query(
                "SELECT l.name, rl.catalog_number FROM release_labels rl JOIN labels l ON l.id = rl.label_id
                  WHERE rl.release_id = $1 ORDER BY l.name, rl.catalog_number",
                &[&db_id],
            )?
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        let formats = self
            .client
            .query(
                "SELECT qty, size, rpm, channels::text, weight_g, text, descriptions
                   FROM release_formats WHERE release_id = $1 ORDER BY position",
                &[&db_id],
            )?
            .iter()
            .map(|r| StoredFormat {
                qty: r.get(0),
                size: r.get(1),
                rpm: r.get(2),
                channels: r.get(3),
                weight_g: r.get(4),
                text: r.get(5),
                descriptions: r.get(6),
            })
            .collect();
        let tracks = self
            .client
            .query(
                "SELECT position, title, duration_seconds FROM tracks WHERE release_id = $1 ORDER BY track_index",
                &[&db_id],
            )?
            .iter()
            .map(|r| StoredTrack {
                position: r.get(0),
                title: r.get(1),
                duration_seconds: r.get(2),
            })
            .collect();
        let credits = self
            .client
            .query(
                "SELECT rc.role, a.name, rc.tracks FROM release_credits rc JOIN artists a ON a.id = rc.artist_id
                  WHERE rc.release_id = $1 ORDER BY rc.id",
                &[&db_id],
            )?
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();
        Ok(Some(StoredRelease {
            id,
            master_id: base.get(1),
            title: base.get(0),
            released_on: base.get(2),
            released_precision: base.get(3),
            year: base.get(4),
            country: base.get(5),
            genres: base.get(6),
            styles: base.get(7),
            notes: base.get(8),
            discogs_uri: base.get(9),
            copies: base.get(10),
            artists,
            labels,
            formats,
            tracks,
            credits,
        }))
    }

    /// The stored cover image as downloaded, if there is one.
    pub fn get_image(&mut self, id: u32) -> Result<Option<Vec<u8>>> {
        let id = i32::try_from(id).context("release id out of range")?;
        Ok(self
            .client
            .query_opt(
                "SELECT image FROM release_covers WHERE release_id = $1",
                &[&id],
            )?
            .map(|row| row.get(0)))
    }

    /// Thumbnails of the given releases (those that have a cover).
    pub fn get_thumbs(&mut self, ids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
        let ids: Vec<i32> = ids
            .iter()
            .filter_map(|id| i32::try_from(*id).ok())
            .collect();
        Ok(self
            .client
            .query(
                "SELECT release_id, thumb FROM release_covers WHERE release_id = ANY($1)",
                &[&ids],
            )?
            .iter()
            .map(|row| (row.get::<_, i32>(0) as u32, row.get(1)))
            .collect())
    }

    /// Removes a release and everything that belongs to it. False if it was not there.
    pub fn delete_release(&mut self, id: u32) -> Result<bool> {
        let id = i32::try_from(id).context("release id out of range")?;
        Ok(self
            .client
            .execute("DELETE FROM releases WHERE id = $1", &[&id])?
            > 0)
    }

    /// Stores the cover of a release we have: the image as downloaded and a
    /// thumbnail. False if the release is not in the database.
    pub fn save_cover(&mut self, id: u32, image: &[u8], source_url: Option<&str>) -> Result<bool> {
        let cover = make_cover(image)?;
        let id = i32::try_from(id).context("release id out of range")?;
        let rows = self.client.execute(
            "INSERT INTO release_covers (release_id, image, thumb, mime, source_url)
             SELECT $1, $2, $3, $4, $5 WHERE EXISTS (SELECT 1 FROM releases WHERE id = $1)
             ON CONFLICT (release_id) DO UPDATE SET
                 image = EXCLUDED.image, thumb = EXCLUDED.thumb, mime = EXCLUDED.mime,
                 source_url = EXCLUDED.source_url, created_at = now()",
            &[&id, &image, &cover.thumb, &cover.mime, &source_url],
        )?;
        Ok(rows > 0)
    }
}

pub struct Cover {
    pub mime: String,
    pub thumb: Vec<u8>,
}

/// Works out the type of an image and makes its thumbnail (JPEG, at most
/// `THUMB_SIZE` pixels on the longer side).
pub fn make_cover(image: &[u8]) -> Result<Cover> {
    let format = image::guess_format(image).context("unknown image format")?;
    let mime = match format {
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Png => "image/png",
        other => bail!("unsupported image format {other:?}"),
    };
    let decoded = image::load_from_memory_with_format(image, format).context("decode the cover")?;
    let thumb = image::DynamicImage::ImageRgb8(decoded.thumbnail(THUMB_SIZE, THUMB_SIZE).to_rgb8());
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85)
        .encode_image(&thumb)
        .context("encode the thumbnail")?;
    Ok(Cover {
        mime: mime.to_string(),
        thumb: bytes,
    })
}

/// A throwaway PostgreSQL server for tests, started once per test run from the
/// server binaries on this machine. Tests are skipped (with a note) if there are none.
#[cfg(test)]
pub(crate) mod testing {
    use std::{
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        sync::OnceLock,
        time::{Duration, Instant},
    };

    use super::*;

    pub struct TestServer {
        dir: PathBuf,
        port: u16,
        _child: Child,
    }

    fn find_binary(name: &str) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).map(|d| d.join(name)).collect())
            .unwrap_or_default();
        if let Ok(entries) = std::fs::read_dir("/usr/lib/postgresql") {
            candidates.extend(entries.flatten().map(|e| e.path().join("bin").join(name)));
        }
        candidates.into_iter().find(|p| p.is_file())
    }

    fn pid_alive(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Removes the directories of earlier test runs that are over.
    fn purge_stale() {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(pid) = name
                .strip_prefix("recordshelf-test-pg-")
                .and_then(|p| p.parse::<u32>().ok())
                && !pid_alive(pid)
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn start() -> Option<TestServer> {
        let postgres = find_binary("postgres")?;
        let initdb = find_binary("initdb")?;
        purge_stale();
        let dir = std::env::temp_dir().join(format!("recordshelf-test-pg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let data = dir.join("data");
        let status = Command::new(initdb)
            .args(["-D"])
            .arg(&data)
            .args(["-U", "postgres", "--auth=trust"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            listener.local_addr().ok()?.port()
        };
        let mut command = Command::new(postgres);
        command
            .arg("-D")
            .arg(&data)
            .arg("-k")
            .arg(&dir)
            .args(["-p", &port.to_string(), "-c", "listen_addresses="])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // the server stops when the test process ends
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = command.spawn().ok()?;
        let server = TestServer {
            dir,
            port,
            _child: child,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if Client::connect(&server.url("postgres"), NoTls).is_ok() {
                return Some(server);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }

    impl TestServer {
        pub fn url(&self, database: &str) -> String {
            let socket_dir = self.dir.to_string_lossy().replace('/', "%2F");
            format!("postgres://postgres@{socket_dir}:{}/{database}", self.port)
        }

        /// A new, empty database; returns its connection URL.
        pub fn new_database(&self, name: &str) -> String {
            let mut admin = Client::connect(&self.url("postgres"), NoTls).unwrap();
            admin
                .batch_execute(&format!("CREATE DATABASE {name}"))
                .unwrap();
            self.url(name)
        }
    }

    static SERVER: OnceLock<Option<TestServer>> = OnceLock::new();

    /// The shared server, or `None` if PostgreSQL is not installed here.
    pub fn server() -> Option<&'static TestServer> {
        SERVER
            .get_or_init(|| {
                // The server must be started by a thread that lives as long as the
                // test process: its death signal (PDEATHSIG) fires when the thread
                // that spawned it ends, and test threads end with their test.
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::Builder::new()
                    .name("test-postgres".into())
                    .spawn(move || {
                        let _ = tx.send(start());
                        loop {
                            std::thread::park();
                        }
                    })
                    .expect("spawn the test server thread");
                rx.recv().ok().flatten()
            })
            .as_ref()
    }

    /// A connected library on its own migrated database, or None to skip the test.
    pub fn library(name: &str) -> Option<(Library, String)> {
        let Some(server) = server() else {
            eprintln!("skipping {name}: no PostgreSQL server binaries found");
            return None;
        };
        let url = server.new_database(name);
        let (library, _) = Library::connect(&url).unwrap();
        Some((library, url))
    }

    pub fn raw(url: &str) -> Client {
        Client::connect(url, NoTls).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::discogs::Release;

    fn mapped(json: &str, copies: u16) -> CatalogueRelease {
        let release: Release = serde_json::from_str(json).unwrap();
        CatalogueRelease::from_release(release.id.unwrap(), &release, copies).unwrap()
    }

    const FULL: &str = r#"{"id": 9, "master_id": 77, "title": "Speicher 95 Tribute EP", "released": "2017-03", "year": 2017,
        "country": "DE", "genres": ["Electronic"], "styles": ["Techno", "Minimal"], "notes": "Pressed in DE",
        "uri": "https://www.discogs.com/release/9",
        "artists": [{"id": 1, "name": "Laurent Garnier", "join": "feat."}, {"id": 2, "name": "Someone"}],
        "labels": [{"id": 5, "name": "Kompakt Extra", "catno": "Kompakt Ex 095"}],
        "formats": [{"name": "Vinyl", "qty": "1", "descriptions": ["12\"", "33 ⅓ RPM", "EP", "180 Gram"], "text": "X-ray Colour"}],
        "extraartists": [{"id": 3, "name": "Producer Pete", "role": "Producer", "tracks": "A"}],
        "tracklist": [{"position": "A", "type_": "track", "title": "Doctor", "duration": "6:20"},
                      {"position": "B", "type_": "track", "title": "Crypt", "artists": [{"id": 4, "name": "Guest"}]}]}"#;

    fn count(c: &mut Client, sql: &str) -> i64 {
        c.query_one(sql, &[]).unwrap().get(0)
    }

    #[test]
    fn every_migration_file_is_embedded() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db/migrations");
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        files.sort();
        let embedded: Vec<&str> = MIGRATIONS.iter().map(|m| m.1).collect();
        assert_eq!(
            files, embedded,
            "add the new migration file to MIGRATIONS in library.rs"
        );
        for (version, name, _) in MIGRATIONS {
            assert_eq!(name[..4].parse::<i32>().unwrap(), *version, "{name}");
        }
    }

    #[test]
    fn the_checksum_matches_sha256sum() {
        // sha256 of "abc", as printed by `printf abc | sha256sum`
        assert_eq!(
            checksum("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn connecting_applies_the_migrations_once() {
        let Some(server) = server() else { return };
        let url = server.new_database("t_migrate");
        let (mut library, applied) = Library::connect(&url).unwrap();
        assert_eq!(applied, MIGRATIONS.len());
        assert_eq!(library.migrate().unwrap(), 0, "nothing left to apply");
        let mut c = raw(&url);
        assert_eq!(
            count(&mut c, "SELECT count(*) FROM schema_migrations"),
            MIGRATIONS.len() as i64
        );
        assert_eq!(
            c.query_one(
                "SELECT checksum FROM schema_migrations WHERE version = 1",
                &[]
            )
            .unwrap()
            .get::<_, String>(0),
            checksum(MIGRATIONS[0].2)
        );
        assert!(
            count(
                &mut c,
                "SELECT count(*) FROM pg_tables WHERE tablename = 'releases'"
            ) == 1
        );
        // reconnecting is fine too
        assert_eq!(Library::connect(&url).unwrap().1, 0);
    }

    #[test]
    fn a_database_made_by_db_sh_is_accepted() {
        // db.sh records the sha256 of the file; so does the app
        let Some(server) = server() else { return };
        let url = server.new_database("t_dbsh");
        let mut c = raw(&url);
        c.batch_execute(
            "CREATE TABLE schema_migrations (version integer PRIMARY KEY, name text NOT NULL, checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now())",
        )
        .unwrap();
        c.batch_execute(MIGRATIONS[0].2).unwrap();
        c.execute(
            "INSERT INTO schema_migrations (version, name, checksum) VALUES (1, '0001_init.sql', $1)",
            &[&checksum(MIGRATIONS[0].2)],
        )
        .unwrap();
        assert_eq!(Library::connect(&url).unwrap().1, 0);
    }

    #[test]
    fn a_changed_migration_is_refused() {
        let Some(server) = server() else { return };
        let url = server.new_database("t_changed");
        Library::connect(&url).unwrap();
        raw(&url)
            .execute(
                "UPDATE schema_migrations SET checksum = 'edited' WHERE version = 1",
                &[],
            )
            .unwrap();
        let err = format!("{:#}", Library::connect(&url).err().unwrap());
        assert!(err.contains("changed after it was applied"), "{err}");
    }

    #[test]
    fn a_database_from_a_newer_app_is_refused() {
        let Some(server) = server() else { return };
        let url = server.new_database("t_newer");
        Library::connect(&url).unwrap();
        raw(&url).execute("INSERT INTO schema_migrations (version, name, checksum) VALUES (99, '0099_future.sql', 'x')", &[]).unwrap();
        let err = format!("{:#}", Library::connect(&url).err().unwrap());
        assert!(err.contains("newer version"), "{err}");
    }

    #[test]
    fn an_unreachable_database_is_a_clear_error() {
        let err = format!(
            "{:#}",
            Library::connect("postgres://nobody@127.0.0.1:1/none")
                .err()
                .unwrap()
        );
        assert!(err.contains("connect to the database"), "{err}");
        assert!(Library::connect("not a url").is_err());
    }

    #[test]
    fn saves_a_release_with_everything_that_belongs_to_it() {
        let Some((mut library, url)) = library("t_save") else {
            return;
        };
        let outcome = library.save_release(&mapped(FULL, 2)).unwrap();
        assert!(outcome.created);
        let mut c = raw(&url);
        let r = c
            .query_one(
                "SELECT title, master_id, released_on::text, released_precision::text, year, country, genres, styles, notes, copies, discogs_uri FROM releases WHERE id = 9",
                &[],
            )
            .unwrap();
        assert_eq!(r.get::<_, String>(0), "Speicher 95 Tribute EP");
        assert_eq!(r.get::<_, i32>(1), 77);
        assert_eq!(r.get::<_, String>(2), "2017-03-01");
        assert_eq!(r.get::<_, String>(3), "month");
        assert_eq!(r.get::<_, i16>(4), 2017);
        assert_eq!(r.get::<_, String>(5), "DE");
        assert_eq!(r.get::<_, Vec<String>>(6), ["Electronic"]);
        assert_eq!(r.get::<_, Vec<String>>(7), ["Techno", "Minimal"]);
        assert_eq!(r.get::<_, String>(8), "Pressed in DE");
        assert_eq!(r.get::<_, i16>(9), 2);
        assert_eq!(r.get::<_, String>(10), "https://www.discogs.com/release/9");

        let artists = c.query("SELECT a.name, ra.position, ra.join_text FROM release_artists ra JOIN artists a ON a.id = ra.artist_id ORDER BY ra.position", &[]).unwrap();
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].get::<_, String>(0), "Laurent Garnier");
        assert_eq!(
            artists[0].get::<_, Option<String>>(2).as_deref(),
            Some("feat.")
        );
        assert_eq!(artists[1].get::<_, Option<String>>(2), None);
        let label = c.query_one("SELECT l.name, rl.catalog_number FROM release_labels rl JOIN labels l ON l.id = rl.label_id", &[]).unwrap();
        assert_eq!(
            (label.get::<_, String>(0), label.get::<_, String>(1)),
            ("Kompakt Extra".into(), "Kompakt Ex 095".into())
        );
        let f = c
            .query_one("SELECT qty, size, rpm, channels::text, weight_g, text, descriptions FROM release_formats", &[])
            .unwrap();
        assert_eq!(f.get::<_, i16>(0), 1);
        assert_eq!(
            (f.get::<_, Option<i16>>(1), f.get::<_, Option<i16>>(2)),
            (Some(12), Some(33))
        );
        assert_eq!(f.get::<_, String>(3), "stereo");
        assert_eq!(f.get::<_, Option<i16>>(4), Some(180));
        assert_eq!(
            f.get::<_, Option<String>>(5).as_deref(),
            Some("X-ray Colour")
        );
        assert_eq!(f.get::<_, Vec<String>>(6), ["EP"]);
        let tracks = c.query("SELECT track_index, position, title, duration_seconds FROM tracks ORDER BY track_index", &[]).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].get::<_, Option<i32>>(3), Some(380));
        assert_eq!(tracks[1].get::<_, Option<i32>>(3), None);
        assert_eq!(
            count(&mut c, "SELECT count(*) FROM track_artists"),
            1,
            "only the track with its own artist"
        );
        let credit = c.query_one("SELECT a.name, rc.role, rc.tracks FROM release_credits rc JOIN artists a ON a.id = rc.artist_id", &[]).unwrap();
        assert_eq!(
            (credit.get::<_, String>(0), credit.get::<_, String>(1)),
            ("Producer Pete".into(), "Producer".into())
        );
        assert_eq!(
            count(&mut c, "SELECT count(*) FROM artists"),
            4,
            "release, guest and credit artists"
        );
    }

    #[test]
    fn saving_again_replaces_what_belongs_to_the_release() {
        let Some((mut library, url)) = library("t_resave") else {
            return;
        };
        library.save_release(&mapped(FULL, 1)).unwrap();
        let mut c = raw(&url);
        c.execute("UPDATE releases SET created_at = now() - interval '1 day', updated_at = now() - interval '1 day'", &[]).unwrap();

        let smaller = r#"{"id": 9, "title": "Renamed", "released": "2017-03-10",
            "artists": [{"id": 2, "name": "Someone Renamed"}],
            "labels": [{"id": 5, "name": "Kompakt Extra Renamed", "catno": "K 1"}],
            "formats": [{"name": "Vinyl", "descriptions": ["7\""]}],
            "tracklist": [{"position": "1", "title": "Only"}]}"#;
        let outcome = library.save_release(&mapped(smaller, 3)).unwrap();
        assert!(!outcome.created, "the release was there already");

        let r = c.query_one("SELECT title, copies, released_precision::text, master_id, notes, genres, created_at < now() - interval '1 hour', updated_at > now() - interval '1 hour' FROM releases", &[]).unwrap();
        assert_eq!(r.get::<_, String>(0), "Renamed");
        assert_eq!(r.get::<_, i16>(1), 3);
        assert_eq!(r.get::<_, String>(2), "day");
        assert_eq!(
            r.get::<_, Option<i32>>(3),
            None,
            "fields that Discogs no longer gives are cleared"
        );
        assert_eq!(r.get::<_, Option<String>>(4), None);
        assert!(r.get::<_, Vec<String>>(5).is_empty());
        assert!(r.get::<_, bool>(6), "created_at stays");
        assert!(r.get::<_, bool>(7), "updated_at moves");
        assert_eq!(count(&mut c, "SELECT count(*) FROM release_artists"), 1);
        assert_eq!(count(&mut c, "SELECT count(*) FROM release_labels"), 1);
        assert_eq!(count(&mut c, "SELECT count(*) FROM release_formats"), 1);
        assert_eq!(count(&mut c, "SELECT count(*) FROM tracks"), 1);
        assert_eq!(count(&mut c, "SELECT count(*) FROM track_artists"), 0);
        assert_eq!(count(&mut c, "SELECT count(*) FROM release_credits"), 0);
        assert_eq!(
            c.query_one("SELECT name FROM artists WHERE id = 2", &[])
                .unwrap()
                .get::<_, String>(0),
            "Someone Renamed"
        );
        assert_eq!(
            c.query_one("SELECT name FROM labels WHERE id = 5", &[])
                .unwrap()
                .get::<_, String>(0),
            "Kompakt Extra Renamed"
        );
    }

    #[test]
    fn a_failing_save_changes_nothing() {
        let Some((mut library, url)) = library("t_atomic") else {
            return;
        };
        library.save_release(&mapped(FULL, 1)).unwrap();
        let mut broken = mapped(FULL, 5);
        broken.title = "Should not be stored".into();
        broken.released_on = Some("2017-13-45".into()); // rejected by the database, after other rows were written
        assert!(library.save_release(&broken).is_err());
        let mut c = raw(&url);
        let r = c
            .query_one("SELECT title, copies FROM releases WHERE id = 9", &[])
            .unwrap();
        assert_eq!(
            (r.get::<_, String>(0), r.get::<_, i16>(1)),
            ("Speicher 95 Tribute EP".into(), 1)
        );
        assert_eq!(count(&mut c, "SELECT count(*) FROM tracks"), 2);
        // and the connection is still usable
        library.save_release(&mapped(FULL, 2)).unwrap();
    }

    #[test]
    fn status_reports_copies_and_covers() {
        let Some((mut library, _)) = library("t_status") else {
            return;
        };
        library.save_release(&mapped(FULL, 2)).unwrap();
        library
            .save_release(&mapped(r#"{"id": 10, "title": "Other"}"#, 1))
            .unwrap();
        assert!(library.save_cover(10, &png(40, 40), None).unwrap());
        let status = library.status(&[9, 10, 11]).unwrap();
        assert_eq!(
            status[&9],
            Entry {
                copies: 2,
                has_cover: false
            }
        );
        assert_eq!(
            status[&10],
            Entry {
                copies: 1,
                has_cover: true
            }
        );
        assert!(!status.contains_key(&11), "unknown releases are absent");
        assert!(library.status(&[]).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_release_removes_its_data_but_keeps_shared_artists() {
        let Some((mut library, url)) = library("t_delete") else {
            return;
        };
        library.save_release(&mapped(FULL, 1)).unwrap();
        library
            .save_release(&mapped(
                r#"{"id": 10, "title": "Other", "artists": [{"id": 2, "name": "Someone"}]}"#,
                1,
            ))
            .unwrap();
        library.save_cover(9, &png(40, 40), None).unwrap();
        assert!(library.delete_release(9).unwrap());
        assert!(!library.delete_release(9).unwrap(), "already gone");
        assert!(!library.delete_release(12345).unwrap());
        let mut c = raw(&url);
        assert_eq!(count(&mut c, "SELECT count(*) FROM releases"), 1);
        for table in [
            "tracks",
            "release_formats",
            "release_labels",
            "release_credits",
            "release_covers",
            "track_artists",
        ] {
            assert_eq!(
                count(&mut c, &format!("SELECT count(*) FROM {table}")),
                0,
                "{table}"
            );
        }
        assert_eq!(
            count(&mut c, "SELECT count(*) FROM release_artists"),
            1,
            "the other release keeps its artist"
        );
        assert!(count(&mut c, "SELECT count(*) FROM artists WHERE id = 2") == 1);
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 40])
        });
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn covers_are_stored_with_a_thumbnail() {
        let Some((mut library, url)) = library("t_cover") else {
            return;
        };
        library.save_release(&mapped(FULL, 1)).unwrap();
        let original = jpeg(600, 400);
        assert!(
            library
                .save_cover(9, &original, Some("https://i.discogs.com/x.jpg"))
                .unwrap()
        );
        let mut c = raw(&url);
        let row = c
            .query_one(
                "SELECT image, thumb, mime, source_url FROM release_covers WHERE release_id = 9",
                &[],
            )
            .unwrap();
        assert_eq!(
            row.get::<_, Vec<u8>>(0),
            original,
            "the image is kept as downloaded"
        );
        assert_eq!(row.get::<_, String>(2), "image/jpeg");
        assert_eq!(
            row.get::<_, Option<String>>(3).as_deref(),
            Some("https://i.discogs.com/x.jpg")
        );
        let thumb = image::load_from_memory(&row.get::<_, Vec<u8>>(1)).unwrap();
        assert_eq!(
            (thumb.width(), thumb.height()),
            (150, 100),
            "scaled down, aspect ratio kept"
        );

        // replacing keeps one row
        assert!(library.save_cover(9, &png(100, 100), None).unwrap());
        assert_eq!(count(&mut c, "SELECT count(*) FROM release_covers"), 1);
        assert_eq!(
            c.query_one("SELECT mime FROM release_covers", &[])
                .unwrap()
                .get::<_, String>(0),
            "image/png"
        );
    }

    #[test]
    fn a_cover_for_an_unknown_release_is_not_stored() {
        let Some((mut library, url)) = library("t_cover_unknown") else {
            return;
        };
        assert!(!library.save_cover(404, &png(50, 50), None).unwrap());
        assert_eq!(
            count(&mut raw(&url), "SELECT count(*) FROM release_covers"),
            0
        );
    }

    #[test]
    fn broken_covers_are_rejected() {
        let Some((mut library, _)) = library("t_cover_bad") else {
            return;
        };
        library.save_release(&mapped(FULL, 1)).unwrap();
        assert!(library.save_cover(9, b"not an image", None).is_err());
        assert!(library.save_cover(9, &[], None).is_err());
        let truncated = &png(50, 50)[..30];
        assert!(library.save_cover(9, truncated, None).is_err());
    }

    #[test]
    fn thumbnails_are_never_larger_than_the_limit() {
        for (w, h) in [(600, 600), (300, 1000), (1000, 300), (50, 50)] {
            let cover = make_cover(&png(w, h)).unwrap();
            let thumb = image::load_from_memory(&cover.thumb).unwrap();
            assert!(
                thumb.width() <= THUMB_SIZE && thumb.height() <= THUMB_SIZE,
                "{w}x{h}"
            );
        }
        assert_eq!(make_cover(&jpeg(10, 10)).unwrap().mime, "image/jpeg");
        assert!(make_cover(b"GIF89a....").is_err(), "gif is not supported");
    }

    #[test]
    fn a_dropped_connection_is_noticed() {
        let Some((library, url)) = library("t_closed") else {
            return;
        };
        assert!(!library.is_closed());
        let mut admin = raw(&url);
        admin.execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = 't_closed' AND pid <> pg_backend_pid()", &[]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut library = library;
        let _ = library.status(&[1]); // the next use finds out
        assert!(library.is_closed());
    }

    // --- browsing ---------------------------------------------------------------

    fn two_records() -> (CatalogueRelease, CatalogueRelease) {
        (
            mapped(FULL, 2),
            mapped(
                r#"{"id": 20, "title": "Wonderscape Numinous", "released": "2024-11-22", "country": "AU",
                    "genres": ["Electronic"], "styles": ["Dub Techno"],
                    "artists": [{"id": 30, "name": "Altone", "join": "&"}, {"id": 31, "name": "Björk"}],
                    "labels": [{"id": 6, "name": "Primary Colours", "catno": "PCSS09"}],
                    "formats": [{"name": "Vinyl", "qty": "2", "descriptions": ["12\"", "33 ⅓ RPM", "Mono", "EP"], "text": "Green Marble"}],
                    "tracklist": [{"position": "A1", "title": "Self Replication"}, {"position": "B1", "title": "Adaptive Swarm", "duration": "5:10"}]}"#,
                1,
            ),
        )
    }

    #[test]
    fn lists_all_records_sorted_by_artist_and_title() {
        let Some((mut library, _)) = library("t_list") else {
            return;
        };
        let (garnier, altone) = two_records();
        library.save_release(&garnier).unwrap();
        library.save_release(&altone).unwrap();
        library.save_cover(20, &png(30, 30), None).unwrap();
        let rows = library.list().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            [20, 9],
            "Altone before Laurent Garnier"
        );
        let r = &rows[0];
        assert_eq!(r.title, "Wonderscape Numinous");
        assert_eq!(r.artists, "Altone & Björk");
        assert_eq!(r.labels, "Primary Colours (PCSS09)");
        assert_eq!(r.year, Some(2024));
        assert_eq!(r.format, "2 x 12\", 33 RPM, Mono, EP, Green Marble");
        assert_eq!((r.copies, r.has_cover), (1, true));
        assert!(!rows[1].has_cover);
        assert_eq!(rows[1].artists, "Laurent Garnier feat. Someone");
        assert_eq!(rows[1].copies, 2);
        // everything is searchable, without accents or case
        for word in [
            "wonderscape",
            "bjork",
            "pcss09",
            "dub techno",
            "adaptive swarm",
            "2024",
            "au",
            "green marble",
            "primary colours",
        ] {
            assert!(r.haystack.contains(word), "{word} in {}", r.haystack);
        }
        assert!(!r.haystack.contains("Björk"), "the haystack is folded");
    }

    #[test]
    fn an_empty_library_lists_nothing() {
        let Some((mut library, _)) = library("t_list_empty") else {
            return;
        };
        assert!(library.list().unwrap().is_empty());
    }

    #[test]
    fn a_record_without_artists_labels_or_tracks_is_listed() {
        let Some((mut library, _)) = library("t_list_sparse") else {
            return;
        };
        library
            .save_release(&mapped(r#"{"id": 5, "title": "Only A Title"}"#, 1))
            .unwrap();
        let rows = library.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (
                rows[0].artists.as_str(),
                rows[0].labels.as_str(),
                rows[0].format.as_str()
            ),
            ("", "", "")
        );
        assert_eq!(rows[0].year, None);
    }

    #[test]
    fn a_record_can_be_read_back_completely() {
        let Some((mut library, _)) = library("t_get") else {
            return;
        };
        library.save_release(&mapped(FULL, 2)).unwrap();
        let r = library.get_release(9).unwrap().unwrap();
        assert_eq!(
            (r.id, r.title.as_str(), r.copies, r.master_id),
            (9, "Speicher 95 Tribute EP", 2, Some(77))
        );
        assert_eq!(r.released_on.as_deref(), Some("2017-03-01"));
        assert_eq!(r.released_precision.as_deref(), Some("month"));
        assert_eq!(r.year, Some(2017));
        assert_eq!(r.artist_display(), "Laurent Garnier feat. Someone");
        assert_eq!(
            r.labels,
            [("Kompakt Extra".to_string(), "Kompakt Ex 095".to_string())]
        );
        assert_eq!(r.format_display(), "12\", 33 RPM, 180 g, EP, X-ray Colour");
        assert_eq!(r.tracks.len(), 2);
        assert_eq!(
            (
                r.tracks[0].position.as_deref(),
                r.tracks[0].duration_seconds
            ),
            (Some("A"), Some(380))
        );
        assert_eq!(
            r.credits,
            [(
                "Producer".to_string(),
                "Producer Pete".to_string(),
                Some("A".to_string())
            )]
        );
        assert_eq!(r.genres, ["Electronic"]);
        assert_eq!(r.notes.as_deref(), Some("Pressed in DE"));
        assert!(library.get_release(404).unwrap().is_none());
    }

    #[test]
    fn covers_and_thumbnails_can_be_read_back() {
        let Some((mut library, _)) = library("t_get_cover") else {
            return;
        };
        let (a, b) = two_records();
        library.save_release(&a).unwrap();
        library.save_release(&b).unwrap();
        let original = png(60, 40);
        library.save_cover(20, &original, None).unwrap();
        assert_eq!(library.get_image(20).unwrap(), Some(original));
        assert_eq!(library.get_image(9).unwrap(), None, "no cover stored");
        assert_eq!(library.get_image(404).unwrap(), None);
        let thumbs = library.get_thumbs(&[9, 20, 404]).unwrap();
        assert_eq!(thumbs.len(), 1, "only releases with a cover");
        assert_eq!(thumbs[0].0, 20);
        assert!(image::load_from_memory(&thumbs[0].1).is_ok());
        assert!(library.get_thumbs(&[]).unwrap().is_empty());
    }

    #[test]
    fn format_summaries_read_naturally() {
        let f =
            |qty, size, rpm, channels: &str, weight, text: Option<&str>, d: &[&str]| StoredFormat {
                qty,
                size,
                rpm,
                channels: channels.into(),
                weight_g: weight,
                text: text.map(str::to_string),
                descriptions: d.iter().map(|s| s.to_string()).collect(),
            };
        assert_eq!(
            f(1, Some(12), Some(33), "stereo", None, None, &["EP"]).summary(),
            "12\", 33 RPM, EP"
        );
        assert_eq!(
            f(2, Some(12), None, "stereo", Some(180), Some("Green"), &[]).summary(),
            "2 x 12\", 180 g, Green"
        );
        assert_eq!(
            f(1, None, None, "mono", None, None, &["Single"]).summary(),
            "Mono, Single"
        );
        assert_eq!(
            f(1, None, None, "quadraphonic", None, None, &[]).summary(),
            "Quadraphonic"
        );
        assert_eq!(f(1, None, None, "stereo", None, None, &[]).summary(), "");
        assert_eq!(
            f(3, None, None, "stereo", None, None, &["LP"]).summary(),
            "3 x LP"
        );
    }
}
