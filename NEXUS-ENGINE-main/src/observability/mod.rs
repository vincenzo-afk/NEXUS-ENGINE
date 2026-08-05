//! Observability: request counters, latency histograms, and a
//! Prometheus-text-format `/metrics` exposition function, plus a small
//! RAII span timer for structured `log`-crate-based tracing of durations.
//!
//! This deliberately does not add `tracing`/`tracing-subscriber` or a
//! `prometheus` crate dependency: this codebase already uses the plain
//! `log` crate throughout (see e.g. `crate::clicks`'s `log::debug!`
//! calls), and the metric primitives actually needed here — counters,
//! and a fixed-bucket latency histogram — are small enough to implement
//! directly against `std::sync::atomic`, in the same "own the small
//! stuff" spirit as `crate::api::result_cache`. A production deployment
//! that wants real distributed tracing (spans exported to Jaeger/Tempo/
//! etc.) should layer the `tracing` ecosystem on top of this rather than
//! this module trying to be that; what's here is the minimum needed to
//! answer "is the API healthy and fast right now" from a scrape.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

/// A monotonically increasing named counter (request counts, error
/// counts, cache hits, etc).
#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Fixed-bucket latency histogram (milliseconds), reported Prometheus-
/// style as cumulative bucket counts. Bucket boundaries are chosen to
/// resolve typical search-API latencies (a few ms to a few seconds), not
/// tuned against any specific production traffic (there is none yet).
const LATENCY_BUCKETS_MS: &[f64] = &[5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0];

pub struct Histogram {
    bucket_counts: Vec<AtomicU64>,
    sum_ms: AtomicU64,
    count: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            bucket_counts: (0..LATENCY_BUCKETS_MS.len() + 1)
                .map(|_| AtomicU64::new(0))
                .collect(),
            sum_ms: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    pub fn observe_ms(&self, value_ms: f64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(value_ms.round() as u64, Ordering::Relaxed);
        for (i, boundary) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if value_ms <= *boundary {
                self.bucket_counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        // "+Inf" bucket always increments, per the Prometheus histogram
        // convention of cumulative buckets.
        self.bucket_counts[LATENCY_BUCKETS_MS.len()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn mean_ms(&self) -> f64 {
        let count = self.count();
        if count == 0 {
            0.0
        } else {
            self.sum_ms.load(Ordering::Relaxed) as f64 / count as f64
        }
    }
}

/// The process-wide metrics registry. Meant to be constructed once (as
/// an `Arc<MetricsRegistry>` alongside `crate::api::mod`'s other
/// `Arc`-shared state) and passed into every request handler.
#[derive(Default)]
pub struct MetricsRegistry {
    counters: RwLock<HashMap<&'static str, std::sync::Arc<Counter>>>,
    histograms: RwLock<HashMap<&'static str, std::sync::Arc<Histogram>>>,
    started_at: Mutex<Option<Instant>>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        let registry = MetricsRegistry::default();
        *registry.started_at.lock().expect("metrics mutex poisoned") = Some(Instant::now());
        registry
    }

    pub fn counter(&self, name: &'static str) -> std::sync::Arc<Counter> {
        if let Some(c) = self.counters.read().expect("metrics rwlock poisoned").get(name) {
            return std::sync::Arc::clone(c);
        }
        let mut counters = self.counters.write().expect("metrics rwlock poisoned");
        std::sync::Arc::clone(
            counters
                .entry(name)
                .or_insert_with(|| std::sync::Arc::new(Counter::default())),
        )
    }

    pub fn histogram(&self, name: &'static str) -> std::sync::Arc<Histogram> {
        if let Some(h) = self.histograms.read().expect("metrics rwlock poisoned").get(name) {
            return std::sync::Arc::clone(h);
        }
        let mut histograms = self.histograms.write().expect("metrics rwlock poisoned");
        std::sync::Arc::clone(
            histograms
                .entry(name)
                .or_insert_with(|| std::sync::Arc::new(Histogram::default())),
        )
    }

    pub fn uptime_seconds(&self) -> f64 {
        self.started_at
            .lock()
            .expect("metrics mutex poisoned")
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }

    /// Renders every registered metric in Prometheus text exposition
    /// format, suitable for a `GET /metrics` handler.
    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# HELP nexus_uptime_seconds Process uptime in seconds\n# TYPE nexus_uptime_seconds gauge\nnexus_uptime_seconds {:.3}\n",
            self.uptime_seconds()
        ));

        let counters = self.counters.read().expect("metrics rwlock poisoned");
        let mut names: Vec<&&str> = counters.keys().collect();
        names.sort();
        for name in names {
            out.push_str(&format!(
                "# TYPE {name} counter\n{name} {}\n",
                counters[name].get()
            ));
        }

        let histograms = self.histograms.read().expect("metrics rwlock poisoned");
        let mut hnames: Vec<&&str> = histograms.keys().collect();
        hnames.sort();
        for name in hnames {
            let h = &histograms[name];
            out.push_str(&format!("# TYPE {name} histogram\n"));
            let mut cumulative = 0u64;
            for (i, boundary) in LATENCY_BUCKETS_MS.iter().enumerate() {
                cumulative = h.bucket_counts[i].load(Ordering::Relaxed);
                out.push_str(&format!("{name}_bucket{{le=\"{boundary}\"}} {cumulative}\n"));
            }
            let inf_count = h.bucket_counts[LATENCY_BUCKETS_MS.len()].load(Ordering::Relaxed);
            out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {inf_count}\n"));
            let _ = cumulative;
            out.push_str(&format!("{name}_sum {:.0}\n", h.mean_ms() * h.count() as f64));
            out.push_str(&format!("{name}_count {}\n", h.count()));
        }

        out
    }
}

/// An RAII timer: create at the start of an operation, and its `Drop`
/// impl records the elapsed time (in milliseconds) into the given
/// histogram — so a handler can't forget to record timing on an early
/// return/error path, the same "guard does the bookkeeping" pattern as
/// `crate::api::request_queue::QueuePermit`.
pub struct Span {
    started_at: Instant,
    histogram: std::sync::Arc<Histogram>,
}

impl Span {
    pub fn start(histogram: std::sync::Arc<Histogram>) -> Self {
        Span {
            started_at: Instant::now(),
            histogram,
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let elapsed_ms = self.started_at.elapsed().as_secs_f64() * 1000.0;
        self.histogram.observe_ms(elapsed_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increments() {
        let registry = MetricsRegistry::new();
        let c = registry.counter("requests_total");
        c.inc();
        c.inc();
        c.add(3);
        assert_eq!(registry.counter("requests_total").get(), 5);
    }

    #[test]
    fn histogram_tracks_count_and_mean() {
        let registry = MetricsRegistry::new();
        let h = registry.histogram("search_latency_ms");
        h.observe_ms(10.0);
        h.observe_ms(30.0);
        assert_eq!(h.count(), 2);
        assert!((h.mean_ms() - 20.0).abs() < 1e-6);
    }

    #[test]
    fn span_records_elapsed_time_on_drop() {
        let registry = MetricsRegistry::new();
        let histogram = registry.histogram("op_latency_ms");
        {
            let _span = Span::start(std::sync::Arc::clone(&histogram));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(histogram.count(), 1);
        assert!(histogram.mean_ms() >= 4.0);
    }

    #[test]
    fn prometheus_output_contains_registered_metrics() {
        let registry = MetricsRegistry::new();
        registry.counter("cache_hits_total").inc();
        registry.histogram("search_latency_ms").observe_ms(15.0);
        let rendered = registry.render_prometheus();
        assert!(rendered.contains("cache_hits_total 1"));
        assert!(rendered.contains("search_latency_ms_bucket"));
        assert!(rendered.contains("nexus_uptime_seconds"));
    }
}
