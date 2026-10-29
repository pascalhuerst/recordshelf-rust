# RecordShelf database

PostgreSQL. Every row in `releases` is a record we own; the data comes from Discogs. Our own database is the catalogue; the Discogs collection stays the source of truth for what we own.

## Setup
1. Create the role and database once, as a Postgres superuser:
   ```
   psql -U postgres -v password="'choose-a-password'" -f db/setup.sql
   ```
2. Apply the migrations in order (the app's migration runner does this; by hand):
   ```
   for f in db/migrations/*.sql; do psql -U recordshelf -d recordshelf -v ON_ERROR_STOP=1 --single-transaction -f "$f"; done
   ```
3. Put the connection string in the settings file (like the Discogs token), e.g. `postgres://recordshelf:...@host/recordshelf`.

## Tests
`db/test.sh` creates a throwaway Postgres cluster in a temp directory, applies `setup.sql` and all migrations, and runs `db/tests/schema.sql` (defaults, constraints, triggers, cascades). It needs the Postgres server binaries and `psql`. Add checks there when a migration adds rules.

## Schema
| Table | Purpose |
|---|---|
| `releases` | one row per release we own: Discogs `id`, `master_id`, title, `released_on` + `released_precision`, generated `year`, country, genres, styles, notes, `copies`, timestamps |
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
- Files `db/migrations/NNNN_description.sql`, applied in number order, each in one transaction, forward only.
- Never edit a migration that has been applied anywhere; add a new one.
- The runner records what ran in `schema_migrations (version, name, checksum, applied_at)` and refuses to continue if an applied file changed.
- `setup.sql` is not a migration: it needs superuser rights and only runs once.
