# recordshelf-rust

Desktop GUI (egui/eframe 0.27) for cataloguing vinyl via the Discogs API. A USB barcode scanner (read as a Linux evdev keyboard device) feeds codes; the search view finds releases and adds them to a Discogs collection folder. Linux only (evdev). Edition 2024.

## Commands
- Run (starts fullscreen; `--windowed` opts out, F11 toggles): `cargo run -- [--config PATH] [--token T] [--username U] [--device-name N] [--device PATH] [--currency C] [--user-agent UA]`
- Check: `cargo clippy --all-targets`, `cargo fmt`, `cargo test`
- Reading `/dev/input/event*` needs permissions (input group or root).

## Configuration
- Settings file: `~/.config/recordshelf/config.toml` (`config.rs`, written with mode 0600). Editable in the in-app Settings window; saving applies immediately (restarts scanner, reconfigures worker).
- CLI flags override the file for one run; token also via `DISCOGS_TOKEN`. Never hardcode credentials.
- UI zoom (egui's built-in Ctrl +/-/0) is saved as `zoom_factor` on every change via `Settings::save_zoom`, which only rewrites that key on disk (CLI overrides are not persisted).
- Scanner is found by device **name** (substring, case-insensitive); `device_path` is only a fallback.

## Architecture (src/)
- `main.rs` – clap `Args`, loads `Settings`, applies overrides, starts eframe.
- `config.rs` – `Settings` (serde/TOML) load/save.
- `barcode_reader.rs` – evdev: `list_devices`, `pick_device`, scanner thread (non-blocking, stoppable via flag). Only emits `ScanEvent::{Info, Code}`.
- `worker.rs` – single background thread owning the Discogs client. UI sends `Request`s, polls `Response`s (one response per request, FIFO; `Configure` has none). `pending()` drives the status-bar spinner. `ScanCode::classify`: 12–14 digits = UPC/EAN barcode (searched via `barcode=` param), shorter numbers = release id. A scan fills the results list (all pressings for a barcode, one row for a release id) and loads the first.
- `discogs.rs` – blocking `reqwest` client + serde models. Search items use lenient deserializers (Discogs returns mixed types). Collection instances come from `GET /users/{u}/collection/releases/{id}` (`releases[]` with `instance_id`, `folder_id`).
- Cover pictures: scans and searches share one results list; rows show `thumb`, details show the release `images` primary cover. Fetched lazily (only visible rows) by a separate image thread in `worker.rs` (not counted in `pending()`), only from discogs.com hosts, cached by URL in `ui.rs`, drawn via egui_extras image loaders.
- `ui.rs` – `RecordShelfApp`: single main view (search fields, results list, release details; keys: Up/Down browse, `A` add to the current folder, `R` remove, `M` move to another folder (asks which copies if there are several), `O` open in browser, `F` change folder, `Q` quit, Enter retries a failed load, Esc new search; the same commands are buttons showing their keys; logic in `controls.rs`), folder picker window, Settings window, bottom status bar. Never do network I/O on the UI thread; send a `Request`.

- `shops.rs`, `decks.rs`, `hhv.rs`, `shop_bridge.rs` – lookups of decks.de / hhv.de label codes via a browser userscript (`--shop-bridge`); `ShopEntry` → Discogs search plan (barcode, catno+label, catno, artist+title). 6-7 digit numbers are treated as hhv article numbers while the bridge runs. See `shops.md`. Personal tool, never to be published.

- `db/` – PostgreSQL schema for our own catalogue (setup.sql, numbered migrations, `test.sh` + `tests/schema.sql`), see `db/README.md` for the schema, conventions and how Discogs data maps onto it. Not wired into the app yet.

## Conventions
- All Discogs searches are restricted to `format=Vinyl` (only vinyl is of interest); this is applied in `SearchQuery::params`.
- Errors: `anyhow` with `.context(...)`; worker turns them into `String`s for the UI; the UI shows them as a `Notice` in the status bar.
- Browsing results shows details without Enter: the first `PREFETCH_LIMIT` (5) results are prefetched (release + collection status), later ones load when the selection rests for 250 ms. Releases are cached in `releases`/`requested`; Enter only retries a release whose load failed (`failed`). Keep this in mind for the 60 req/min Discogs limit.
- Add/remove are optimistic (`PENDING_INSTANCE` placeholder, instance removed locally) and always followed by `resync_collection`. Adding a release that is already owned, and every removal, asks for confirmation (Enter/Y yes, Esc/N no). The target folder (never "All", id 0) is stored as `folder_id` in the settings file.
- Owned releases are flagged (ownership cache `HashMap<release_id, Vec<CollectionInstance>>`); adding a duplicate copy is always allowed.
- Tests live in `#[cfg(test)]` modules next to the code; Discogs HTTP is tested against a local one-shot `TcpListener` server (`DiscogsClient.base` is overridable in-crate).
- Keep clippy and fmt clean; commit only when asked.

## Known gaps
- Settings saved from the window also persist any CLI/env overrides active in that run.
- Scanner reports letters upper case regardless of Shift.
- No automatic scanner reconnect if the device is unplugged.
- The worker is a single thread, so a slow request delays the ones queued behind it.
- UI code (`ui.rs`) has only helper-level tests; no egui interaction tests.
- `/home/paso/development/recordshelf` (parent) is not a git repo; this directory is.
