# Nexus Privacy Policy

Nexus is a local-first, privacy-first search engine. This document describes
what data it does and doesn't collect, and the privacy-related settings you
can control in `config.toml`.

## Data collection

- **No personal data is collected.** Nexus has no analytics backend, no
  telemetry endpoint, and no account system. There is nothing to opt out of
  because nothing is sent anywhere by default.
- **Search queries stay on your device.** Local filesystem search never
  leaves your machine. Web search (`nexus crawl`, `nexus search`) makes
  outbound HTTP requests only to fetch the pages you ask it to crawl —
  never to a Nexus-operated server, because there isn't one.
- **No tracking cookies.** The bundled frontend and search API don't set
  cookies or any other cross-session tracking identifier.
- **No third-party analytics.** No embedded analytics, ad, or tracking
  scripts of any kind.

## Configurable privacy controls

These map directly to the `[privacy]` section of `config.toml` and to what
`nexus privacy-policy` prints:

| Setting | What it does |
|---|---|
| `block_sponsored_results` | Search results are never paid placements; this exists to make that explicit and keep it easy to audit if the ranking code ever changes. |
| `filter_bubble_disabled` | Ranking doesn't personalize based on a stored user profile — the same query returns the same ranked results for everyone (click history affects *global* result popularity, not a per-user profile; see below). |
| `anonymize_queries` | When enabled, queries are not written to any persistent log with an associated client identifier. |
| `disable_telemetry` | Telemetry is disabled by default and there is currently no telemetry implementation to enable — this flag exists so a future telemetry feature can't ship silently on-by-default. |
| `auto_delete_history_days` | If set, locally stored search history (used for autocomplete suggestions) older than this many days is deleted automatically. |

## Click history

Nexus's ranking uses a *click-history* signal (`nexus click <query> <rank>`,
or a client hitting the equivalent API/WebSocket action): a running count of
how often a given document has been chosen across searches, used as a small
ranking boost. This is intentionally **not** a per-user profile — it's a
single aggregate counter per document, stored locally in `clicks.nxc`
alongside your index. It does not track *who* clicked, only *how often* a
document was clicked in total, and it never leaves your machine.

## The privacy/network modules

Nexus includes an optional privacy/networking layer for crawling privately:

- **Tor / SOCKS5 proxy support** (`nexus tor --enable`): routes crawl
  requests through a local Tor daemon you run yourself. Nexus does not
  bundle or manage a Tor process; you're responsible for having one
  running and reachable at the configured SOCKS5 address.
- **Session encryption** (`privacy::session`, `privacy::crypto`): X25519
  key agreement plus ChaCha20-Poly1305 authenticated encryption
  (via the audited `chacha20poly1305` crate), used for the WebSocket
  search session mechanism. This protects payload confidentiality/
  integrity at the application layer, in addition to — not instead of —
  running the server behind TLS.
- **TLS policy validation** (`network::tls`): validates a *desired* TLS
  policy shape (minimum version, pinning configuration). As of this
  writing it validates configuration only; it is not yet wired into an
  active certificate-pinning enforcement path in the HTTP client. Don't
  rely on it as an active security control until that's implemented.

## Reporting a concern

This is an open-source project; if you find a privacy or security issue,
please open an issue in the repository rather than assuming behavior from
this document alone — code is the source of truth, and this file should be
kept in sync with it, but always verify against the actual implementation
for anything security-critical.
