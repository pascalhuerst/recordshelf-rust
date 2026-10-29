# recordshelf-rust

Desktop GUI (egui/eframe) for cataloguing vinyl records via the Discogs API. A USB barcode scanner (read as a Linux evdev keyboard device) feeds release IDs; a Search tab finds releases and adds them to a Discogs collection folder. Linux only (evdev).

## Commands
- Build/run: `cargo run -- --device /dev/input/eventN --username <discogs-user> --token <token>`
- Check: `cargo check`, `cargo clippy`, `cargo fmt`
- No tests exist yet. Reading `/dev/input/event*` needs permissions (input group or root).

## Layout (src/, ~1350 lines, edition 2024)
- `main.rs` – clap `Args` (device, user-agent, token, currency, username), builds `DiscogsClient`, starts eframe, spawns the scanner thread, passes an mpsc `Receiver<ScanEvent>` to the app.
- `discogs.rs` – blocking `reqwest` client: `release`, `search`, `collection_folders`, `collection_instances`, `add_to_collection`. Also the models (`Release`, `SearchItem`, ...) and lenient JSON helpers (`val_to_*`, `deserialize_opt_*`) because Discogs returns inconsistent types.
- `barcode_reader.rs` – thread that reads evdev key events, buffers digits/`-./`, and on Enter parses the buffer as a numeric **Discogs release ID** (not a real EAN/UPC) and fetches it. Sends `ScanEvent::{Info, Result}` and calls `ctx.request_repaint()`.
- `ui.rs` – `RecordShelfApp` (`eframe::App`), state-driven. Tabs: Scan (shows last scan only) and Search (query/artist/release/catno fields, keyboard-driven result list, "Add to folder" window). Keys: Up/Down, Enter (load release / confirm folder), `A` (open folder picker / confirm), Esc.

## Conventions
- Errors: `anyhow::Result` with `.context(...)`. UI errors are shown as strings in `search_error` (also used for success messages).
- Stay with the existing style: plain structs, no extra abstraction layers; keep Discogs parsing tolerant.

## Known issues / watch-outs
- `main.rs` has a real-looking Discogs token and username as clap defaults, committed in git. Treat as leaked: rotate it and remove the defaults (use env/`--token` required).
- Default scanner device `/dev/input/event17` is machine-specific.
- All HTTP calls are blocking and run on the UI thread (search, release load, folder load, add) – the UI freezes during requests.
- `collection_instance` in `ui.rs` is never set to `Some`, so the branch in `add_selected_to_collection` is dead; duplicate detection lives in `prepare_folder_picker`, which opens the browser instead of moving the item.
- The "already in collection" URL-building code is duplicated in `ui.rs`.
- `collection_instances` in `discogs.rs` has duplicated parsing branches; `folder_id` is checked but unused.
- Scanner thread has no clean shutdown path beyond the `stop` flag (blocked in `fetch_events`).
- `Cargo.toml` enables eframe `wgpu` feature; eframe 0.27 uses `Box::new(...)` app creators.

## Working notes
- Not tracked by the parent dir (`/home/paso/development/recordshelf` is not a git repo); this directory has its own git repo. Only commit when asked.
