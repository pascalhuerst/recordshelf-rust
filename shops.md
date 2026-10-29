# Shop lookups (decks.de, hhv.de)

Records bought at decks.de or hhv.de carry a label barcode with the shop's own code, not a UPC/EAN. The shop's page maps that code to release data (catalog number, label, for hhv even the real barcode), which we then search on Discogs. Personal tool for importing a large collection; not meant to be published.

## Why a browser is involved
Both shops put a bot check in front of their pages (decks.de: Cloudflare; hhv.de: a JavaScript challenge that sets the `HHV-JS-CH` cookie), so only a real browser gets the page. The app does not try to get around the checks. Instead the browser does the shop request and hands the result to the app over localhost.

## Flow
1. The scanner reads a code. `shops::route_scan` decides where it goes:
   - `ABC/12` (letters/digits, a slash) is a **decks.de** code.
   - a 6-7 digit number is an **hhv.de** article number, but only while the bridge runs (see below).
   - anything else is a Discogs release id or UPC/EAN, as before.
2. The code is queued in the bridge (`shop_bridge.rs`).
3. The userscript on an open tab of that shop polls `GET /pending?shop=...`, runs the shop search and posts the raw HTML to `POST /result?shop=...&code=...&status=...`.
4. The app parses the page into a `ShopEntry` and checks it is the requested item.
5. The worker searches Discogs (`shops::search_plan`), first non-empty result wins:
   1. the real barcode, if the shop lists one (hhv `gtin`; tried as 12 and 13 digits, the shop pads it to 14)
   2. catalog number + label
   3. catalog number only
   4. artist + title (remarks in parentheses removed)
6. The candidates fill the results list, the first is loaded. `A` adds it to the current folder.

### The 6-digit ambiguity
hhv article numbers (e.g. `525768`) look exactly like Discogs release ids. With the bridge running, a 6-7 digit number is therefore always treated as hhv: if no hhv.de tab is connected the app says so instead of looking up a random Discogs release. Without `--shop-bridge` nothing changes.

## decks.de
Search: `/decks/workfloor/search_db.php?such=CNF+O1&wosuch=all&wassuch=atl&where=` (slash replaced by a space). Server-rendered `iso-8859-1` HTML, one block per hit; the shop also returns loosely matching blocks, so the block whose `data-code` equals the normalised scan (`cnf-o1`) is picked.

```html
<div class="oneLine tablenorm1" id="tab_cnfo1" data-code="cnf-o1">
  <div class="LArtist">...</div>        artist
  <div class="LTitel">...</div>         title, "(180G / ...)" remarks in a span
  <div class="LLabelcat">PCSS09</div>   label catalog number
  <div class="LLabel">Primary colours</div>   label name
</div>
```
Parser: `decks.rs`, fixture `tests/fixtures/decks_search_cnf_o1.html`.

## hhv.de
Search: `/katalog/filter/suche-S11?af=true&term=525768` redirects to the article page (`/records/artikel/<name>-525768`); the browser's `fetch` follows it. The article page embeds schema.org JSON-LD (`@graph` with a `Product` and a `MusicAlbum`):

| JSON-LD | Used as |
|---|---|
| `Product.sku` | shop code (must equal the scan) |
| `Product.gtin` (or `offers[0].gtin`) | real barcode, zero padded to 14 digits |
| `Product.mpn` | catalog number (`Kompakt Ex 095`) |
| `Product.brand.name` | label |
| `MusicAlbum.byArtist.name` / `.name` | artist / title (fallback: `Product.name` split at " - ") |

Parser: `hhv.rs`, fixture `tests/fixtures/hhv_product_525768.html`. A page without a `Product` (result list, bot check page) parses to nothing.

## Setup
1. `cargo run -- --shop-bridge` (port via `--shop-port`, default 47321; the old names `--decks-bridge`/`--decks-port` still work). The first run creates `~/.config/recordshelf/decks-token` (mode 0600; the name is historical, it is the bridge token for both shops) and writes `~/.config/recordshelf/recordshelf.user.js`.
2. Install `recordshelf.user.js` in Tampermonkey (new script, paste contents). Replace an older "RecordShelf decks.de bridge" script: the new one serves both shops. Reinstall whenever the token or port changes or `USERSCRIPT_VERSION` is bumped.
3. Open `https://www.decks.de/decks/` and/or `https://www.hhv.de/` in a tab, pass the bot check (accept cookies, browse a page) and keep the tab open.
4. The status bar shows "decks.de: connected" / "hhv.de: connected" while a tab polls.

## Bridge API (127.0.0.1 only, every request needs `?token=`)
| Request | Purpose |
|---|---|
| `GET /pending?shop=decks\|hhv` | Returns and clears the queued codes of that shop as a JSON array; counts as the shop being connected |
| `POST /result?shop=...&code=...&status=200` | Body is the raw result page HTML; `status` is the HTTP status the browser saw |

Without `shop` the request is treated as decks (userscripts from before hhv support).

## Troubleshooting
- "lookup failed (403)" / "no exact match ... reload the tab": the bot check expired or was not passed; reload the shop tab and try again.
- "lookup failed (no reply)": the tab could not reach the shop.
- "no hhv.de tab is connected": the tab is closed, the userscript is not installed/outdated, or the token/port is outdated.
- "No Discogs release found": the shop's data is not on Discogs in that form; search by hand with the fields above.
