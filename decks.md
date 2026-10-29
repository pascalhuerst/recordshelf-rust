# decks.de lookup

Records bought at decks.de carry a label barcode with the shop's internal code (e.g. `CNF/O1`). The shop's search page maps that code to the label's catalog number (e.g. `PCSS09`), which we then search on Discogs. This was built for a one-off import of a large collection and is not meant to be published.

## Why a browser is involved
`https://www.decks.de/decks/workfloor/search_db.php?such=CNF+O1&wosuch=all&wassuch=atl&where=` is behind a Cloudflare bot check; only a real browser passes it. The app does not try to get around the check. Instead the browser does the shop request and hands the result to the app over localhost.

## Flow
1. The scanner reads `CNF/O1`. `decks::is_decks_code` recognises `<alnum>/<alnum>` with at least one letter in the first part.
2. The app queues the code in the bridge (`decks_bridge.rs`).
3. The userscript on a decks.de tab polls `GET /pending`, runs the shop search with the code (`/` replaced by a space) and posts the raw HTML to `POST /result?code=...&status=...`.
4. The app parses the page (`decks::parse_results`) and picks the block whose `data-code` equals the normalised scan (`cnf-o1`). The shop also returns loosely matching blocks (e.g. `cnf-35`), so the exact match matters. No exact match is shown as an error.
5. The worker searches Discogs (`decks::search_plan`), first non-empty result wins:
   1. catalog number + label
   2. catalog number only
   3. artist + title (remarks in parentheses removed)
6. The candidates appear in the Search tab, the first is loaded with the collection check. `A` adds it to a folder.

## Result page structure (as of Oct 2026)
Server-rendered HTML, `iso-8859-1`, one block per release:

```html
<div class="oneLine tablenorm1" id="tab_cnfo1" data-code="cnf-o1">
  <div class="LArtist">...</div>    artist
  <div class="LTitel">...</div>     title, with "(180G / ...)" remarks in a span
  <div class="LLabelcat">PCSS09</div>   label catalog number
  <div class="LLabel">Primary colours</div>   label name
</div>
```
If the shop changes its markup, fix `parse_results` and the fixture `tests/fixtures/decks_search_cnf_o1.html`.

## Setup
1. `cargo run -- --decks-bridge` (port via `--decks-port`, default 47321). The first run creates `~/.config/recordshelf/decks-token` (mode 0600) and writes `~/.config/recordshelf/decks.user.js`.
2. Install `decks.user.js` in Tampermonkey (new script, paste contents). Re-install it if the token or port changes.
3. Open `https://www.decks.de/decks/` in a tab, pass the Cloudflare check and keep the tab open.
4. The status bar shows "decks: browser connected" while the userscript polls.

## Bridge API (127.0.0.1 only, every request needs `?token=`)
| Request | Purpose |
|---|---|
| `GET /pending` | Returns and clears the queued codes as a JSON array |
| `POST /result?code=CNF%2FO1&status=200` | Body is the raw result page HTML; `status` is the HTTP status the browser saw |

## Troubleshooting
- "lookup failed (403)": the Cloudflare clearance expired; reload the decks.de tab.
- "lookup failed (no reply)": the tab could not reach the shop (offline, or the tab is not on decks.de).
- "browser not connected": the tab is closed, the userscript is not installed, or the token/port in the script is outdated.
- "no exact match": the shop did not return a block with that code; search it manually.
- "No Discogs release found": the catalog number may be formatted differently on Discogs; use the Search tab.
