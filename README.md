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

### Search API
`GET /search?q=...&limit=...&offset=...`, `GET /health`, and `GET /opensearch.xml`,
served over plain HTTP (`tiny_http`), returning JSON with per-result snippets,
scores, and (for web results) domain/title. Rate limiting is built in (token
bucket, configurable per-IP and per-key), with query length and pagination
depth validation.

### WebSocket search-as-you-type server
A dedicated WebSocket server (`nexus serve-ws`) providing real-time search
results as the user types. Supports debouncing (150ms coalescing), session
tracking, and cancel messages. Sends the full current result set for each
debounced query rather than diffing against previously-sent document IDs —
an earlier version did the latter and had a real staleness bug where a
document that matched an earlier, broader keystroke would never be resent
for a later, narrower query even though it still matched. Rate limited
through the shared token bucket.

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

### Frontend: search history, saved searches, export
The bundled frontend (`frontend/`) adds three client-side-only features on
top of the search API: a **history** panel of recent queries (click to
re-run, persisted in `localStorage`), **saved searches** (name a query,
including its active filters, and reload it later), and **export** buttons
that download the currently-loaded result set as JSON or CSV. None of
this touches the backend — it's pure client-side convenience on top of
`GET /search`.

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
| `dedup`        | Exact hashing + SimHash/shingling near-duplicate detection             |
| `document`     | `Document` / `DocumentMetadata`                                        |
| `webdoc`       | Per-page crawl metadata, link graph, PageRank                          |
| `text`         | Unicode normalization, tokenizer, stop-words                           |
| `index`        | Vocabulary, postings, inverted index, document store, web metadata     |
| `storage`      | Versioned, checksummed binary persistence + web-page content cache     |
| `ranking`      | BM25, TF-IDF, and the composite boost formula                          |
| `query`        | Query language: AST, lexer, recursive-descent parser                   |
| `search`       | Query evaluation engine + best-window snippet highlighting             |
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
| `[ranking]`     | BM25 params, boost weights, trusted/spam domain lists                  |
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

167 unit tests cover the tokenizer, query parser (including every operator,
including bangs), ranking functions (including PageRank, domain quality,
and click-history integration tests against a real `Index`), storage
round-tripping/corruption detection, robots.txt precedence rules, sitemap
parsing (including malformed input), RSS/Atom feed parsing (both formats,
malformed input), URL canonicalization, the crawl queue's rate limiting
and persistence, SimHash near-duplicate detection, HTML extraction
(including noise stripping and feed-link discovery), rich-format
extraction, the audited AEAD crypto layer (round-trip, tamper/wrong-key/
wrong-nonce rejection), session key agreement/expiry, Tor `.onion` address
matching, TLS config validation, and — via a real WebSocket client
connecting to a real socket — the search-as-you-type protocol's
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
link graph + PageRank, duplicate detection, composite ranking, expanded
query operators, bangs, RSS/Atom feed discovery, per-domain crawl budgets,
redirect chain tracking, a search API, a WebSocket search-as-you-type
server, a privacy/session layer built on audited crypto primitives, and a
frontend with history/saved-searches/export — everything listed above is
real, integrated code with passing tests, not stubs.

Deliberately **not** included, because building them for real (rather than
faking them) is each its own substantial project:

- **Image search** (download/OCR/alt-text/nearby-text pipeline) and **video
  search** (caption/subtitle indexing) — no image or video content pipeline
  exists here at all.
- **AI/embedding-based ranking** — the composite score is BM25 plus the
  listed signals; no embedding model or vector index is included.
- **True distributed search** (multiple crawler/index nodes behind a
  distributed queue) — the queue/scheduler split is architected so a
  multi-worker version could share it, but this build runs single-process.
- **Personal knowledge base connectors** (Notion/Obsidian/email exports) —
  only local files and the web are indexed; the `document`/`formats`
  module boundary is where such connectors would plug in.

On scheduling and the browser target specifically, since these were
previously listed as unimplemented and now partly are:
- **Crawl scheduling** is real but deliberately simple: `nexus crawl <url>
  --watch --interval N` loops the process itself. It is not a cron
  daemon/distributed scheduler — for unattended operation, run it under a
  process supervisor (systemd, a container restart policy, etc.).
- **`browser/` (WASM/IndexedDB/Web Worker)** compiles to a WASM target
  behind the `wasm` Cargo feature (off by default; doesn't affect normal
  native builds). It has not been exercised in this pass beyond compiling
  — no browser-based end-to-end test was run against it, unlike the
  native crawl/search/API pipeline, which was.
- **TLS certificate pinning** (`network::tls`) validates a *desired*
  policy shape only; it is not wired into an active enforcement path in
  the HTTP client. See `PRIVACY.md` for the current, accurate scope of
  every privacy/network component.

The module boundaries are designed so all of the above could be added
without a rewrite.
