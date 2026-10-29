# RecordShelf database

PostgreSQL. Every row in `releases` is a record we own; the data comes from Discogs. Our own database is the catalogue; the Discogs collection stays the source of truth for what we own.

## Setup
Everything goes through `db/db.sh` (bash and `psql` are all it needs; it takes the usual psql connection options and passwords from `PGPASSWORD`, `~/.pgpass` or trust authentication).

```
db/db.sh bootstrap -h lightdesk.local -U postgres     # once: role, database, tables
db/db.sh status    -h lightdesk.local -U postgres     # which migrations are applied
db/db.sh migrate   -h lightdesk.local -U postgres     # apply pending migrations later
db/db.sh drop      -h lightdesk.local -U postgres     # delete the database and its role again
```

- **`bootstrap`** needs a superuser. It creates the role `recordshelf` (login, with a generated password that is printed once at the end, or `RECORDSHELF_DB_PASSWORD` if you set it), the database `recordshelf` owned by it, and applies all migrations. Everything the migrations create belongs to the role. It only creates what is missing, so running it again is safe and never changes an existing role's password.
- **`migrate`** applies pending migrations; it works with the superuser or with the `recordshelf` role itself.
- **`status`** lists every migration as `applied` or `pending`.
- **`drop`** needs a superuser too. It shows what will be deleted (size, number of tables), and you must type the database name to confirm; `--confirm NAME` does that without a prompt, for scripts. Connected sessions are disconnected, everything in the database is lost. The role is dropped as well, unless `--keep-role` is given or it owns other databases. The `postgres` and template databases are refused. A dump first: `pg_dump -h HOST -U postgres recordshelf > recordshelf.sql`.
- `-d` and `-r` choose another database or role name (defaults `recordshelf`).

**Credentials go into the settings file automatically.** After creating the role, `bootstrap` writes `database_url = "postgres://recordshelf:<password>@host/recordshelf"` into `~/.config/recordshelf/config.toml` (created readable by you only; other settings in the file are kept, special characters in the password are percent-encoded). The generated password is then only stored there, and it is not printed. `--settings FILE` uses another file, `--no-settings` leaves files alone and prints the connection string instead (the generated password is shown once). If the role already existed, its password is not known, so nothing is written; set `database_url` yourself (or `drop` and bootstrap again). `drop` removes a `database_url` that points at the dropped database. The app reads the setting (also editable in the settings dialog, masked) and uses the database, see below.

## Tests
`db/test.sh` creates a throwaway Postgres cluster in a temp directory and runs `db/tests/bootstrap.sh` (the tooling: fresh and repeated bootstrap, status, new, edited and failing migrations, passwords, bad input) and `db/tests/schema.sql` (defaults, constraints, triggers, cascades on a database made by the tooling). It needs the Postgres server binaries and `psql`. Add checks when a migration adds rules.

## How the app uses it
With `database_url` set, the app connects at startup, brings the schema up to date by itself (the same migrations and checksums as `db.sh`; a database migrated by a newer app is refused) and keeps the library in line with the Discogs collection. Without a URL nothing changes.
- **Sync rule.** For a release whose data and collection status are known: if you own copies on Discogs, the release is stored with `copies` = that number (data refreshed from Discogs); if you own none and it is in the database, it is deleted (cascade). Releases you do not own are never stored.
- **Auto sync** (default; setting `library_auto_sync`, key `U`): this happens by itself for the results of every scan or search, and after adding or removing a copy with `A`/`R`. **Manual** mode: press `S` to sync the selected release (also refreshes its data). The status bar shows the mode; the details show "In the library (2x)" or what differs.
- **Covers.** After a release is stored, its main cover is downloaded (with the Discogs credentials) and stored as downloaded plus a 150 px JPEG thumbnail in `release_covers`.
- **Search Collection tab** lists everything in the database (loaded at once, searched in memory, so it is instant for a few thousand records), shows the stored details and covers, prints labels from the stored data and looks up scanned stickers (the number on our labels is the release id) in the database only. **E** edits `my_notes`; **D** deletes a record from the library only (it stays in the Discogs collection, so auto sync can add it again).
- **Mapping.** Done by `src/catalogue.rs` as described below; the SQL is in `src/library.rs`. The migrations are embedded in the app (`MIGRATIONS` in `library.rs`): **a new migration file must be added to that list too** (a test fails if one is missing).
- **Tests.** The Rust tests start a throwaway PostgreSQL from the server binaries on the machine (and stop it with the test run); without them those tests are skipped.

## Schema
| Table | Purpose |
|---|---|
| `releases` | one row per release we own: Discogs `id`, `master_id`, title, `released_on` + `released_precision`, generated `year`, country, genres, styles, `notes` (Discogs pressing notes, refreshed by sync), `my_notes` (our own, never touched by sync), `copies`, timestamps |
| `release_covers` | our own copy of the cover (`image`) and a thumbnail (`thumb`) |
| `artists`, `release_artists` | artists in sleeve order; `join_text` is what the sleeve puts after the name ("&", ",", "feat.") |
| `labels`, `release_labels` | labels and catalog numbers (a release can have several of either) |
| `release_formats` | physical details per format: `qty`, `size` (7/10/12), `rpm`, `channels`, `weight_g`, free `text`, remaining `descriptions` |
| `tracks`, `track_artists` | tracklist; track artists only where they differ from the release artists |
| `release_credits` | producer, remixer, ... with the tracks they apply to (free text) |

## Conventions
- **Ids** of releases, artists and labels are the Discogs ids. Other tables use identity columns.
- **Timestamps** are `timestamptz`. `created_at`: row created. `updated_at`: last sync with Discogs (set by a trigger on `releases`).
- **Everything is vinyl.** No format name, no media type. `size` is the diameter in inches (7, 10, 12), `weight_g` in grams, both plain numbers; `NULL` when Discogs does not say.
- **`channels`** defaults to `stereo` (most pressings do not say); Discogs "Mono" gives `mono`, "Quadraphonic" gives `quadraphonic`.
- **Partial dates.** Discogs releases may only have a year or a month. The date is stored as its first day and `released_precision` (`year`/`month`/`day`) says how much is known; both are set or both are `NULL`.
- **Copies.** `releases.copies` follows the number of copies in the Discogs collection. A release with no copies left is deleted; its tracks, formats, credits and cover go with it (`ON DELETE CASCADE`). Artists and labels stay.
- No condition and no collection folder are stored.

## Filling the tables from Discogs (for the sync code)
- `released`: `2017-03-10` is day precision, `2017-03` or `2017-03-00` month, `2017` or `2017-00-00` year. If only the release's `year` is known, use January 1st with year precision.
- Format: one `release_formats` row per vinyl format entry, `qty` from Discogs' `qty`; non-vinyl entries are skipped.
  - descriptions `7"`, `10"`, `12"` give `size`; `LP` without a size gives 12.
  - `33 ⅓ RPM`, `45 RPM`, `78 RPM` give `rpm`.
  - `Mono`, `Stereo`, `Quadraphonic` give `channels`.
  - `180 Gram` (and similar) gives `weight_g`; the same may appear inside the free text.
  - Everything left over (EP, Album, Reissue, Limited Edition, ...) goes to `descriptions`; the free text goes to `text`.
- Artists: Discogs' `join` field is `join_text` (empty means `NULL`). Track-level `artists` go to `track_artists`, `extraartists` to `release_credits` (`tracks` is the free text of the credit).
- Tracklist: `track_index` counts the real tracks from 0; heading and index rows are skipped. Durations "3:45" become seconds.

## Migrations
- Files `db/migrations/NNNN_description.sql` (four digits, lower case), applied in number order, forward only. Numbers must be unique.
- Each migration runs in one transaction together with its row in `schema_migrations (version, name, checksum, applied_at)`: it is applied completely or not at all.
- A migration that was changed after it was applied is refused (checksum). Never edit an applied migration; add a new one.
- Run `db/test.sh` after adding one.
