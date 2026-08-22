# NEXUS Release Performance and Memory Profile

## Scope

This profile measures the optimized native `nexus` binary after the graph co-occurrence memory repair. The benchmark was run on 2026-08-22 UTC using the release binary with SHA-256 `0aeac740f915fe8aaff2a1d788ff2cd149d0f08c1ed3177b7077b2bfc18f17c6`. The host exposed six CPUs, 3.8 GiB total memory, and approximately 2.0 GiB available memory at benchmark start.

The harness uses isolated XDG data/config/cache directories. It creates deterministic text corpora, runs the real release CLI and HTTP server, and records wall-clock elapsed time, user CPU time, system CPU time, peak resident set size (RSS), file I/O, and context switches through GNU `time`.

> The results are engineering measurements from this host and workload, not cross-machine capacity guarantees. The crawl measurement includes public-network latency and should not be compared directly with CPU-bound local operations.

## Workloads and Results

| Workload | Scale | Elapsed | User + system CPU | Peak RSS | Derived throughput |
|---|---:|---:|---:|---:|---:|
| Local indexing | 101 docs / 0.94 MiB | 0.17 s | 0.17 s | 29.4 MiB | 594.11 docs/s |
| Local indexing | 1,536 docs / 14.41 MiB | 2.60 s | 2.58 s | 188.6 MiB | 590.76 docs/s; 5.53 MiB/s |
| Persisted CLI local search | 100 queries, small index | 2.07 s | 2.03 s | 26.2 MiB | 48.30 queries/s |
| Persisted CLI local search | 100 queries, large index | 13.30 s | 13.24 s | 171.7 MiB | 7.51 queries/s |
| HTTP local search, sequential | 48 requests | 0.42 s | 0.34 s | 171.3 MiB | 114.28 requests/s |
| HTTP local search, four workers | 48 requests | 0.26 s | 0.32 s | 171.7 MiB | 184.61 requests/s |
| Persisted index statistics | large index | 0.12 s | 0.12 s | 171.4 MiB | — |
| Remove indexed folder | 101 docs | 0.03 s | 0.03 s | 25.9 MiB | — |
| Controlled public crawl | `https://example.org`, one page | 5.25 s | 0.00 s | 11.2 MiB | One fetched and indexed page |

The CLI search figures include process startup and reloading the persisted index for each invocation. The HTTP figures include server startup/teardown and were intentionally held below the default per-client rate limit, making them conservative end-to-end figures rather than a pure handler microbenchmark.

## Confirmed Memory Defect and Repair

The original large-corpus run indexed a 15.1 MB, 1,536-document corpus until the process reached **1.74 GB RSS**, after which the environment terminated it with `SIGTERM` before persistence/statistics completed. Profiling and source inspection identified the cause as quadratic entity co-occurrence construction: a document with many identifier-like entities generated one graph pair for every entity pair.

The repair preserves all extracted entity mentions, but deterministically limits the co-occurrence graph to the first **32 unique entities per source**. This bounds each source to at most **496 graph pairs** rather than allowing unconstrained quadratic growth. A focused regression verifies both the pair bound and that entity mentions outside the co-occurrence subset are still retained.

| Large-corpus indexing | Before repair | After repair | Change |
|---|---:|---:|---:|
| Completion | Terminated before completion | Completed, 1,536 documents | Restored successful completion |
| Peak RSS | 1.74 GB | 188.6 MiB | 89.15% reduction; 9.22× lower |
| Elapsed time | Not completed | 2.60 s | Measurable and repeatable |

## Interpretation

The repaired indexer now scales nearly linearly between the two measured corpus sizes: indexing throughput was 594.11 docs/s on the small corpus and 590.76 docs/s on the 1,536-document corpus. The large persisted index uses approximately 171 MiB RSS when loaded for CLI/API search, so low-memory deployments should budget at least several hundred MiB for a comfortably operating server and concurrent process headroom.

The primary remaining performance opportunity is persisted CLI search on larger indexes. Its 7.51 queries/s figure is dominated by the intentionally end-to-end workload, which starts a process and deserializes the index for every request. Long-lived HTTP serving avoids that repeated startup/load cost and reached 114.28 sequential requests/s and 184.61 requests/s with four request workers on the same corpus.

## Reproduction

Build and profile a fresh release binary from the repository root:

```bash
cargo build --release -j 1
scripts/run_profile.sh /tmp/nexus-profile
```

The script writes `metadata.txt`, raw GNU-time files under `metrics/`, workload output under `logs/`, isolated runtime state under `xdg-*`, and generated corpus data below the selected output directory. It requires standard Unix utilities including `bash`, `curl`, `timeout`, and GNU `/usr/bin/time`.

## Validation

The memory repair passed the focused graph regression group. The final source state also passed:

```text
cargo test --all-targets -j 1
396 passed, 0 failed

cargo build --lib --release --target wasm32-unknown-unknown --features wasm -j 1
passed
```
