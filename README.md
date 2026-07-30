# Nexus

A search engine written entirely in Rust: it started as a local desktop
full-text search tool and now also crawls the web — HTTP/HTTPS, robots.txt,
sitemaps, link graph, PageRank, deduplication, incremental re-crawling —
indexing filesystem files and web pages into one unified, BM25-plus-signals
ranked index, queryable from the CLI or over HTTP.

```
$ nexus add-folder ~/projects && nexus index
$ nexus crawl https://example.com --max-pages 500 --max-depth 4
$ nexus search "rust AND parser site:example.com" --limit 5 --explain
$ nexus search "rust ownership" --mode local   # local | web | both | tor
$ nexus search "!gh nexus search engine"   # bangs redirect to the external site
$ nexus crawl https://example.com --watch --interval 3600   # keep re-crawling on a schedule
$ nexus serve --bind 127.0.0.1:8080   # GET /search?q=...
$ nexus serve-ws --bind 127.0.0.1:8081   # WebSocket search-as-you-type
$ nexus tor --check   # Check Tor reachability
$ nexus benchmark --iterations 500   # Performance benchmark
$ nexus privacy-policy   # Show privacy configuration
```

## Features

### Local filesystem search (original core)
- Recursive crawling with configurable hidden-file/folder/extension filters, parallelized via Rayon
- Unicode-aware text pipeline: NFKC normalization, tokenization, stop-word removal
- Inverted index with term positions, persisted in a versioned, checksummed binary format
- Real-time file watching with incremental index updates (create/modify/delete)
- Prefix autocomplete (trie-based), recent/popular search history, "did you mean?" spelling suggestions

### Web crawling
- HTTP/HTTPS fetching with redirects, retries with exponential backoff, and a configurable user agent
- `robots.txt` (Allow/Disallow/Crawl-delay/Sitemap, longest-prefix-wins) and `sitemap.xml` (incl. nested sitemap indexes and plain-text sitemaps)
- A real crawl queue: priority scheduling (seed > sitemap > discovered), per-domain rate limiting, dedup, and disk persistence so a large crawl can be interrupted and resumed (`nexus crawl --resume`)
- Incremental re-crawling via conditional `GET` (`If-None-Match` / `If-Modified-Since`), so unchanged pages aren't re-indexed
- HTML content extraction (title, headings, paragraphs, meta description, image alt text, canonical URL, `<html lang>`, meta author) with script/style/nav/footer/ad-chrome stripped
- Rich document formats beyond HTML: Markdown, JSON, XML/RSS/Atom, and PDF (best-effort text extraction)
- Duplicate detection: exact content hashing plus SimHash + shingling for near-duplicates (mirrors, boilerplate-only differences)

### Ranking & link graph
- Link graph: incoming/outgoing edges with anchor text, built after each crawl
- PageRank via power iteration over the crawled graph
- Composite scoring: `BM25 × title/filename-match × exact-phrase × recency × PageRank × domain-quality × click-history × URL-match`
- Click-history log (`nexus click <query> <rank>`) feeding a real ranking signal
- `--explain` shows the full per-signal score breakdown

### Query language
`word`, `"exact phrase"`, `AND`/`OR`/`NOT`, `-negation`, `prefix*`, `wild?card`, `fuzzy~2`,
`ext:rs` / `filetype:pdf`, `path:src` / `inurl:async`, `name:main` / `intitle:rust`,
`size>100KB`, `modified<7d`, `site:example.com`, `before:2024-01-01`, `after:2022`,
`lang:en`, `author:jane`.

### Search modes: Local / Web / Hybrid / Tor
Every search — CLI, HTTP API, and both WebSocket servers — takes a `mode`:

| Mode | What it searches | Notes |
|---|---|---|
| `local` | Filesystem-indexed documents only | Shows a 🔒 badge ("nothing leaves your machine"); file type filter chips (All/PDF/Markdown/Code/Docs) appear in this mode |
| `web` (default) | Web-crawled documents only, **excluding** `.onion` addresses | Favicon, domain, title, snippet, Visit/Cache buttons |
| `both` (hybrid) | Both, merged into one ranked list | Local results get a `local_boost` multiplier (default 1.2x); results get a 💻/🌐 badge; if a local file and a web page are ≥80% similar content (SimHash), only the local one is shown |
| `tor` | Web-crawled documents whose URL is a `.onion` address, and **only** those | Never merged into `web` or `both` — a `.onion` link needs Tor Browser specifically, so mixing it into an ordinary result list would be confusing and a genuine accidental-click risk |

`SearchMode::from_query_param` accepts a few synonyms (`fs`/`pc` for local,
`hybrid` for both, `onion` for tor) and falls back to `web` for anything
unrecognized. The CLI cycles through modes via `--mode`; the frontend's
mode-toggle button cycles `local → web → both → tor → local` on click and
re-runs the current query immediately.

### `GET /open` — opening local results
Local-mode result cards have an "Open" button that hits `GET
/open?path=<path>`, which shells out to `xdg-open` (Linux), `open`
(macOS), or `cmd /C start` (Windows) to open the file in its default
application. Path-traversal protection works by *not* trying to sanitize
an arbitrary path string (an easy category of bugs to get subtly wrong)
— instead, `/open` and `/view/` only ever serve a path that is already
present in the index (validated via `DocumentStore::id_for_path`). A
request for `/open?path=../../../etc/passwd` just 404s, because that
path was never indexed, regardless of how the traversal is encoded. (An
earlier version of `/view/` had no such protection at all — it decoded
and read whatever path it was given, checking only that the file
existed. Fixed as part of this pass.)

### Search API
`GET /search?q=...&mode=...&limit=...&offset=...`, `GET /health`, `GET
/open?path=...`, and `GET /opensearch.xml`, served over plain HTTP
(`tiny_http`), returning JSON with per-result snippets, scores, source
type (`is_web`/`is_onion`/`source_type`), and (for web results)
domain/favicon. `/health` also reports whether a Tor proxy is configured
(`tor_configured`) — a fast config check, not a live network probe;
`nexus tor --check` does the real reachability check. Rate limiting is
built in (token bucket, configurable per-IP and per-key), with query
length and pagination depth validation.

### WebSocket search-as-you-type server
`nexus serve-ws` starts `api::websocket::start_ws_server` — the real,
wired-up implementation, mode-aware like everything else. **Note:**
there is a second, near-identical implementation at
`network/websocket.rs` that is *not* called from anywhere in this
codebase (verified: `WebSocketServer::start` has zero call sites outside
its own module). It's compiled and has its own test suite, but it's dead
code — almost certainly left over from an earlier pass that built two
versions and only wired up one. It's kept in sync (mode support, tests
passing) rather than silently bit-rotting, but it should be deleted or
consolidated with `api/websocket.rs` in a follow-up; shipping two parallel
implementations of the same server is exactly the kind of thing that
causes a real bug fix to land in the wrong copy, which is close to what
almost happened here.

### Bangs
`!g rust ownership`, `rust talks !yt`, `borrow !so checker error` — a
recognized `!trigger` anywhere in the query redirects straight to that
site's own search instead of running against the local index (18
built-ins: Google, DuckDuckGo, Bing, Wikipedia, YouTube, GitHub, Stack
Overflow, Reddit, npm, crates.io, docs.rs, MDN, Twitter/X, Amazon, Google
Maps/Images, arXiv, Wolfram Alpha). The CLI prints the resolved URL; the
HTTP API issues a real `302` redirect with a `Location` header. Custom
bangs can be registered via `BangTable::add_custom`.

### Redirect chain tracking
Every hop of every redirect Nexus follows is recorded (via a custom
`reqwest` redirect policy) and stored on `WebPageMeta::redirect_chain`,
alongside the negotiated `http_version` per request — useful for auditing
redirect loops/chains and confirming HTTP/2 is actually being negotiated
(it is, automatically via ALPN, whenever the server supports it — reqwest
0.11's HTTP/2 support has no separate feature flag to enable).

### Crawl budget per domain
`max_pages_per_domain` (default 100) caps how many pages a single crawl
run will fetch from any one domain, independent of the overall
`max_pages` budget — so one large or infinite-link site (calendar pages,
faceted search, a misbehaving CMS) can't consume the entire crawl and
spam the index with pages from a single source.

### RSS/Atom feed discovery + crawl scheduling
Feeds are discovered two ways: well-known paths tried at seed time
(`/feed`, `/feed.xml`, `/rss.xml`, `/atom.xml`, `/rss`) and `<link
rel="alternate" type="application/rss+xml|atom+xml">` tags found while
crawling. Feed items (RSS's bare `<link>text</link>` and Atom's
`<link href>`, preferring `rel="alternate"`) are enqueued at their own
priority tier — fresher than an arbitrary discovered link, since a feed
usually means "this changed recently." For scheduling, `nexus crawl <url>
--watch --interval 3600` loops the crawl on a fixed interval; this makes
the process itself loop rather than daemonizing, so run it under a
process supervisor (systemd, a container restart policy, etc.) for
unattended/restart-on-crash operation.

### Frontend: mode toggle, search history, saved searches, export
The bundled frontend (`frontend/`) implements the mode toggle described
above (per-mode accent colors, an icon/label transition on switch,
mode-specific result card layouts — breadcrumb+Open for local, favicon+
domain+Visit/Cache for web, source badges for hybrid, an "Open a .onion
address?" confirm modal for Tor), plus three client-side-only
conveniences on top of the search API: a **history** panel of recent
queries (click to re-run, persisted in `localStorage`), **saved
searches** (name a query, including its active filters, and reload it
later), and **export** buttons that download the currently-loaded
result set as JSON or CSV. The history/saved/export features don't touch
the backend at all — pure client-side convenience on top of `GET
/search`. Bangs are detected client-side by a cheap heuristic (does the
query contain a `!word` token) to decide whether to do a full page
navigation instead of a `fetch()` — a real bang redirects to an
external, cross-origin site with no CORS headers of its own, which
`fetch()` can't read the response of, while a native browser navigation
follows redirects transparently regardless of origin. The canonical bang
table stays server-side only; an unrecognized `!word` just falls through
to an ordinary search.

### Privacy & security
- Privacy-first defaults: sponsored results blocked, filter bubble disabled,
  queries anonymized, telemetry off, auto-delete history after 90 days
- `nexus privacy-policy` shows current privacy configuration
- Optional API authentication with key-based access
- CORS origin restrictions configurable via `[security]` config section

### Tor / SOCKS5 proxy for private crawling
- `nexus tor` command to check Tor reachability and manage proxy settings
- `[tor]` config section: `enabled`, `proxy_host`, `proxy_port`,
  `identity_rotation_minutes`
- SOCKS5 proxy support in the HTTP client — all crawl traffic can be routed
  through Tor for anonymity

### OpenSearch integration
`GET /opensearch.xml` serves an OpenSearch 1.1 description document, allowing
browsers and search clients to auto-discover and query the Nexus search API.

## Architecture

```
Filesystem (fs, watcher) ──┐
Web (web::crawler) ────────┼──> html / formats extraction ──> text pipeline ──> index
                            │                                        │
                     dedup::DuplicateIndex <───────────────────────┘
                            │
                    webdoc (link graph, PageRank) ──> ranking ──> search::engine ──> cli / api
```

### Module layout

| Module         | Responsibility                                                        |
|----------------|------------------------------------------------------------------------|
| `error`        | Central `NexusError` type and `Result` alias                          |
| `config`       | TOML configuration model (ranking + web-crawl tunables), load/save     |
| `fs`           | Recursive local directory crawler with filtering                       |
| `web`          | HTTP client, robots.txt, sitemap.xml, URL canonicalization, crawl queue, crawler orchestrator |
| `html`         | HTML content extraction (title/headings/paragraphs/meta/alt/links)     |
| `formats`      | Markdown/JSON/XML/PDF text extraction                                  |
| `dedup`        | Exact hashing + SimHash/shingling near-duplicate detection (now applied to local files too, not just web crawls — see hybrid mode) |
| `document`     | `Document` / `DocumentMetadata`                                        |
| `webdoc`       | Per-page crawl metadata, link graph, PageRank                          |
| `text`         | Unicode normalization, tokenizer, stop-words                           |
| `index`        | Vocabulary, postings, inverted index, document store, web metadata     |
| `storage`      | Versioned, checksummed binary persistence + web-page content cache     |
| `ranking`      | BM25, TF-IDF, and the composite boost formula                          |
| `query`        | Query language: AST, lexer, recursive-descent parser                   |
| `search`       | Query evaluation engine, `SearchMode` (Local/Web/Hybrid/Tor) filtering, best-window snippet highlighting |
| `clicks`       | Click-history log (ranking signal)                                     |
| `autocomplete` | Trie-based prefix suggestions + search history                         |
| `spellcheck`   | Levenshtein distance, "did you mean" suggestions                       |
| `stats`        | Index statistics and human-readable reporting                          |
| `watcher`      | Real-time filesystem watching (`notify`) + incremental updates          |
| `api`          | HTTP search API (`tiny_http`), rate limiting, OpenSearch XML, WebSocket handler |
| `cli`          | Argument parsing (`clap`) and command handlers                         |
| `network`      | WebSocket server, Tor/SOCKS5 proxy, TLS configuration                  |
| `privacy`      | X25519 key agreement, ChaCha20-Poly1305 AEAD (via the audited `chacha20poly1305` crate), session management, encrypted transport |
| `bangs`        | `!trigger` shortcut table and resolution (CLI + API redirect)          |
| `browser`      | WASM bindings (`wasm-bindgen`), IndexedDB persistence, Web Worker support |

## Building

```
cargo build --release
```

The compiled binary is at `target/release/nexus`. Requires Rust 1.75+ (stable). No unsafe code.

## Configuration

Nexus stores its configuration as TOML (default: `~/.config/nexus/config.toml`
on Linux, or the platform equivalent). Configuration sections:

| Section         | Controls                                                               |
|-----------------|------------------------------------------------------------------------|
| `[ranking]`     | BM25 params, boost weights (incl. `local_boost`, `hybrid_dedup_min_similarity` for hybrid mode), trusted/spam domain lists |
| `[web_crawl]`   | Crawl politeness delay, page/depth budgets, per-domain page budget, feed discovery toggle, allowed domains, etc |
| `[privacy]`     | Sponsored result blocking, filter bubble, query anonymization, telemetry, auto-delete history |
| `[security]`    | API key auth, TLS version, certificate pinning, CORS origins           |
| `[websocket]`   | WebSocket server enable/disable, bind address, connection limits       |
| `[tor]`         | Tor SOCKS5 proxy enable/disable, proxy address, identity rotation      |

All paths (index, content cache, click log, crawl queue) can be overridden
with `--config` pointing at an alternate file.

## Testing

```
cargo test
```

186 unit tests cover the tokenizer, query parser (including every operator,
including bangs), ranking functions (including PageRank, domain quality,
and click-history integration tests against a real `Index`), the four
`SearchMode` filters end-to-end against a real `Index` (local-only,
web-only-excludes-onion, tor-only, hybrid merge + local boost + >80%
cross-source dedup, dedup non-interference when content genuinely
differs), `/open`'s path-traversal protection (rejects `..`, rejects
paths not present in the index, rejects web-document "paths" which are
really URLs), storage round-tripping/corruption detection, robots.txt
precedence rules, sitemap parsing (including malformed input), RSS/Atom
feed parsing (both formats, malformed input), URL canonicalization, the
crawl queue's rate limiting and persistence, SimHash near-duplicate
detection (including the register-on-reindex fix), HTML extraction
(including noise stripping and feed-link discovery), rich-format
extraction, the audited AEAD crypto layer (round-trip, tamper/wrong-key/
wrong-nonce rejection), session key agreement/expiry, Tor `.onion`
address matching, TLS config validation, and — via a real WebSocket
client connecting to a real socket — the search-as-you-type protocol's
correctness under query narrowing.

The full pipeline has also been exercised end-to-end against a local HTTP
test server: seeding → robots.txt → sitemap discovery → rate-limited
crawling with resume → HTML extraction → indexing → link graph → PageRank →
composite-ranked search (including `site:` filtering) → snippet generation
→ the HTTP search API → click recording — all confirmed working together,
alongside a regression check that local filesystem indexing still works
side-by-side with crawled web pages in the same index.

## Notes on scope

This build adds a genuine, tested web crawler, HTML/rich-format extraction,
link graph + PageRank, duplicate detection (now spanning local files too),
composite ranking, expanded query operators, bangs, RSS/Atom feed
discovery, per-domain crawl budgets, redirect chain tracking, a search
API, a WebSocket search-as-you-type server, a privacy/session layer built
on audited crypto primitives, a four-mode search toggle (Local/Web/
Hybrid/Tor) with real path-traversal-protected local file opening, and a
frontend with history/saved-searches/export/mode-switching — everything
listed above is real, integrated code with passing tests, not stubs.

Known housekeeping item: `network/websocket.rs` is dead code (a second,
unused WebSocket server implementation — see the WebSocket section
above). It's kept passing its own tests rather than left to bit-rot, but
should be deleted or merged with `api/websocket.rs`, which is the one
actually wired to `nexus serve-ws`.

### The larger feature wishlist

A much longer list of "what's missing vs. Google/DDG" — headless-browser
JS rendering, anti-bot evasion, semantic/vector search, E-E-A-T signals,
dwell-time telemetry, instant answers/knowledge panels, image search
(OCR), news tabs, safe search, language detection, `.onion` v3 resolution
plus circuit isolation and identity rotation wiring, seed-list bootstrap,
Office/PDF/email/SQLite/browser-history/EXIF indexing, OS-level deep
search integration (Windows Search/registry, macOS Spotlight/FSEvents,
Linux journal), file previews, tag systems, distributed sharded indexing,
activating the WASM target as a real browser extension, index
compression, result caching, and A/B ranking experiments — was proposed
alongside the four-mode toggle in the same request. None of it is in this
build. Each of those is realistically its own multi-week-to-multi-month
project on its own (a correct headless-Chromium crawl pipeline and a
correct OCR pipeline are not smaller undertakings than everything else in
this repository combined), and faking any of them with code that doesn't
actually work would be worse than not having them — a search engine that
silently returns empty/wrong results for "OCR search" is worse than one
that doesn't claim to support it at all.

If you want to pursue any of these, the module boundaries here are built
so they're additive rather than requiring a rewrite:
- **Rich local file formats** (Office/PDF/email/SQLite/EXIF) plug into
  `formats/` and `document/` the same way Markdown/JSON/XML/PDF do now —
  add a `DocumentFormat` variant and an extractor function; `fs/crawler.rs`
  already dispatches on extension.
- **Semantic/vector search** would sit alongside `ranking::score_document`
  as another signal — the composite-score architecture (multiply in a new
  boost factor, gated by config) is exactly what would host it, the same
  way PageRank and click-history were added.
- **Distributed indexing** — the crawl queue/scheduler split
  (`web::queue::CrawlQueue`) is already the seam a multi-worker version
  would parallelize across (see its module doc comment).
- **Browser extension** — `browser/` (WASM/IndexedDB/Web Worker, behind
  the `wasm` Cargo feature) is the closest existing piece, but has not
  been exercised end-to-end in a real browser in this pass — only
  compiled. Treat it as a starting point, not a finished extension.
- **TLS certificate pinning** (`network::tls`) validates a *desired*
  policy shape only; it is not wired into an active enforcement path in
  the HTTP client. See `PRIVACY.md` for the current, accurate scope of
  every privacy/network component.
- **Tor circuit isolation / identity rotation** — `identity_rotation_minutes`
  exists in config but isn't wired to anything that actually rotates a
  circuit; `network::tor` currently configures a single static SOCKS5
  proxy for the whole crawl.

### Crawl scheduling, one more time
`nexus crawl <url> --watch --interval N` loops the process itself. It is
not a cron daemon/distributed scheduler — for unattended operation, run
it under a process supervisor (systemd, a container restart policy,
etc.).
