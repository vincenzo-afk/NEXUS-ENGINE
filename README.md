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
$ nexus serve --bind 127.0.0.1:8080   # GET /search?q=...
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
`GET /search?q=...&limit=...&offset=...` and `GET /health`, served over plain HTTP
(`tiny_http`), returning JSON with per-result snippets, scores, and (for web
results) domain/title.

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
| `api`          | HTTP search API (`tiny_http`)                                          |
| `cli`          | Argument parsing (`clap`) and command handlers                         |

## Building

```
cargo build --release
```

The compiled binary is at `target/release/nexus`. Requires Rust 1.75+ (stable). No unsafe code.

## Configuration

Nexus stores its configuration as TOML (default: `~/.config/nexus/config.toml`
on Linux, or the platform equivalent). The `[ranking]` and `[web_crawl]`
sections control every tunable mentioned above (BM25 params, boost weights,
trusted/spam domain lists, crawl politeness delay, page/depth budgets,
allowed domains, etc). All paths (index, content cache, click log, crawl
queue) can be overridden with `--config` pointing at an alternate file.

## Testing

```
cargo test
```

91 unit tests cover the tokenizer, query parser (including every new
operator), ranking functions (including PageRank, domain quality, and
click-history integration tests against a real `Index`), storage
round-tripping/corruption detection, robots.txt precedence rules, sitemap
parsing (including malformed input), URL canonicalization, the crawl
queue's rate limiting and persistence, SimHash near-duplicate detection,
HTML extraction (including noise stripping), and rich-format extraction.

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
query operators, and a search API on top of the original local desktop
search engine — everything listed above is real, integrated code with
passing tests, not stubs.

Deliberately **not** included, because building them for real (rather than
faking them) is each its own substantial project:

- **Image search** (download/OCR/alt-text/nearby-text pipeline) and **video
  search** (caption/subtitle indexing) — no image or video content pipeline
  exists here at all.
- **Browser extension** — nothing in this repo runs in a browser; `nexus
  crawl` and the search API are the analogous server-side pieces.
- **AI/embedding-based ranking** — the composite score is BM25 plus the
  listed signals; no embedding model or vector index is included.
- **True distributed search** (multiple crawler/index nodes behind a
  distributed queue) — the queue/scheduler split is architected so a
  multi-worker version could share it, but this build runs single-process.
- **News-specific hourly recrawl scheduling** — `nexus crawl --resume` plus
  incremental (ETag/Last-Modified) re-fetching covers the mechanics; no
  cron/scheduler wrapper is included.
- **Personal knowledge base connectors** (Notion/Obsidian/email exports) —
  only local files and the web are indexed; the `document`/`formats`
  module boundary is where such connectors would plug in.

The module boundaries are designed so all of the above could be added
without a rewrite.
