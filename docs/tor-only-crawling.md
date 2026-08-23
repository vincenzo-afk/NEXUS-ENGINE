# Bounded Tor-Only Crawling

NEXUS can index public `.onion` pages only through its actual Tor SOCKS proxy path. This is an **operator-controlled command**, not a public search-page action: selecting Tor search mode queries already-indexed onion documents; it does not let anonymous visitors trigger crawling.

## Preconditions

The operator must provide a lawful, curated onion seed list and configure a working SOCKS proxy. `nexus tor --enable --proxy HOST:PORT` records the intended proxy. `nexus tor --check` performs the explicit circuit test. `nexus crawl --tor-only` repeats the liveness requirement before it fetches an onion URL, so proxy configuration alone cannot be mistaken for an active circuit.

```bash
nexus tor --enable --proxy 127.0.0.1:9050
nexus tor --check
nexus crawl --tor-only --seed-file ./lawful-onion-seeds.txt \
  --max-pages 50 --max-depth 1 --domain example.onion
```

The `--tor-only` option accepts only HTTP(S) `.onion` seeds, keeps discovered clearnet URLs out of the crawl, preserves robots handling, and refuses `--ignore-robots`. Page, depth, per-domain, response-size, retry, and queue limits remain controlled by `web_crawl` configuration and command overrides.

> A successful configuration check is not a claim that every onion site is reachable or lawful to crawl. Do not add a Tor proxy, a seed corpus, scheduled crawling, or a public crawler trigger without operator approval, legal review appropriate to the deployment, and resource limits.
