# Curated Bootstrap Corpus

NEXUS can crawl real public pages and rank them using its own index. This repository ships a **curated bootstrap harness** rather than claiming to operate a general Internet-scale search engine. The current seed manifest is a small, topical collection of public technical sites: Rust documentation, web standards, Internet policy documents, a project repository, and reference material.

The harness applies NEXUS's existing robots parser, a descriptive `NexusBot` user agent, per-domain cooldowns, canonical URL handling, response-size limits, safety filtering, SimHash duplicate suppression, conditional revalidation, persistent crawl queue, and PageRank refresh. It intentionally disables speculative sitemap expansion and feed probing for the small bootstrap budget, because one third-party discovery tree can contain enough entries to consume a bounded run before content pages are fetched. Durable crawlers can enable either discovery channel after configuring separate budgets. Sites remain authoritative over their own crawl policy: a seed can be skipped by `robots.txt`, fail transiently, or return content unsuitable for indexing. Do not add a source unless it is public, relevant, and appropriate to fetch under those constraints.

## Build a Corpus

Build a release binary first, then run the saved harness. It defaults to an isolated `.bootstrap-corpus/` directory so it never modifies a developer's usual NEXUS index.

```bash
cargo build --release
scripts/build_bootstrap_corpus.sh
```

The default budget runs 18 resumed passes, up to 12 fetches per pass, at depth two. It is deliberately bounded. Tune the output directory or budget explicitly when assessing a larger corpus:

```bash
scripts/build_bootstrap_corpus.sh \
  --output-dir /srv/nexus/bootstrap-2026-08 \
  --passes 24 \
  --per-pass-pages 16 \
  --max-depth 2
```

The generated `bootstrap-report.txt` records the seed manifest hash, each crawl pass, PageRank calculation, final corpus statistics, and query smoke checks. Review it before packaging `index.nxi` and `web_content_cache/` into a service release.

## Quality Gate

`docs/bootstrap-relevance-queries.tsv` defines a compact smoke set. Each query declares the source domain or title term expected to appear in NEXUS's first page of Web results. The build harness runs `scripts/evaluate_bootstrap_corpus.sh` and fails if any expected source disappears. The set is a regression signal, not a scientific relevance benchmark; it guards against empty, off-topic, or unexpectedly degraded bootstrap results.

## Validation Record — 2026-08-23

The verified hosted bootstrap package contained **89 web documents** after eight bounded crawl passes from the curated manifest. All **8 of 8** relevance-smoke queries returned an expected source in the first result, including Rust learning material, crate documentation, HTTP documentation, the Robots Exclusion Protocol, a search-engine reference, the Linux kernel, reserved domains, and an HTML specification.

Against the already-warm hosted native engine, the response field reporting NEXUS search execution time ranged from **0.036 ms** to **0.142 ms**, with a mean of **0.068 ms** across those eight one-result Web queries. This is an in-process engine metric, not an end-to-end Internet latency, cold-start, freshness, recall, or general-web relevance measurement. The corpus remains a deliberately bounded technical bootstrap package.

> **Scope boundary:** A 1 vCPU / 512 MB service can maintain a small, curated index. It cannot provide Google-scale crawl coverage or freshness. General web search requires a separate durable crawl/ingestion pipeline, multi-GB storage, broad source discovery, quality evaluation, and operations capacity. Local mode also requires a user-owned NEXUS indexer to search that user's files. Tor mode requires a functional SOCKS proxy and an independently obtained, lawfully indexed onion corpus; it does not turn ordinary Web results into onion results.

## Operational Notes

Use a recurring service only after selecting the deployment model. A long-running crawl should preserve its index, content cache, and queue on durable storage, run one process at a time against each corpus, and record crawl reports. Do not run parallel jobs against the same index path. Do not disable robots checks for third-party sites.

For broader starting points, Common Crawl offers public archived web data and a CDXJ index, but its multi-billion-page WARC corpus is a batch-ingestion workload—not a live search API. See its [getting-started guide](https://commoncrawl.org/get-started), its [CDXJ index documentation](https://commoncrawl.org/cdxj-index), and the [Robots Exclusion Protocol](https://datatracker.ietf.org/doc/html/rfc9309).
