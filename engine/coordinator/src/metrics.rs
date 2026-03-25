//! In-memory latency histogram, request counters, and shared snapshot types.

/// Serializable latency snapshot sent in metric reports.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct LatencySnapshot {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub min_ms: f64,
    pub mean_ms: f64,
}

/// Per-task metrics snapshot included in every reporting interval.
#[derive(Debug, serde::Serialize, Clone)]
pub struct TaskSnapshot {
    pub name: String,
    pub requests: u64,
    pub errors: u64,
    pub latency: LatencySnapshot,
}

/// In-memory latency histogram and request counters.
///
/// Uses a simple HDR-style bucket array (1ms buckets up to 10s) for
/// percentile calculations without pulling in a heavy dependency.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

const MAX_LATENCY_MS: usize = 10_000; // 10 seconds

pub struct LatencyHistogram {
    buckets: Vec<AtomicU64>,
    count: AtomicU64,
    sum_ms: AtomicU64,
    max_ms: AtomicU64,
    min_ms: AtomicU64,
}

impl LatencyHistogram {
    pub fn new() -> Self {
        let mut buckets = Vec::with_capacity(MAX_LATENCY_MS + 1);
        for _ in 0..=MAX_LATENCY_MS {
            buckets.push(AtomicU64::new(0));
        }
        Self {
            buckets,
            count: AtomicU64::new(0),
            sum_ms: AtomicU64::new(0),
            max_ms: AtomicU64::new(0),
            min_ms: AtomicU64::new(u64::MAX),
        }
    }

    pub fn record(&self, latency_ms: u64) {
        let bucket = (latency_ms as usize).min(MAX_LATENCY_MS);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(latency_ms, Ordering::Relaxed);

        // CAS loop for max
        let mut current = self.max_ms.load(Ordering::Relaxed);
        while latency_ms > current {
            match self.max_ms.compare_exchange_weak(
                current,
                latency_ms,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => current = v,
            }
        }

        // CAS loop for min
        let mut current = self.min_ms.load(Ordering::Relaxed);
        while latency_ms < current {
            match self.min_ms.compare_exchange_weak(
                current,
                latency_ms,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => current = v,
            }
        }
    }

    /// Returns the value at the given percentile (0.0–1.0).
    pub fn percentile(&self, p: f64) -> f64 {
        let total = self.count.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let target = (total as f64 * p).ceil() as u64;
        let mut cumulative: u64 = 0;
        for (i, bucket) in self.buckets.iter().enumerate() {
            cumulative += bucket.load(Ordering::Relaxed);
            if cumulative >= target {
                return i as f64;
            }
        }
        MAX_LATENCY_MS as f64
    }

    #[allow(dead_code)]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn mean_ms(&self) -> f64 {
        let c = self.count.load(Ordering::Relaxed);
        if c == 0 {
            return 0.0;
        }
        self.sum_ms.load(Ordering::Relaxed) as f64 / c as f64
    }

    pub fn max_ms(&self) -> f64 {
        let v = self.max_ms.load(Ordering::Relaxed);
        if v == 0 {
            0.0
        } else {
            v as f64
        }
    }

    pub fn min_ms(&self) -> f64 {
        let v = self.min_ms.load(Ordering::Relaxed);
        if v == u64::MAX {
            0.0
        } else {
            v as f64
        }
    }
}

// ── Per-task entry ────────────────────────────────────────────────────────────

struct TaskEntry {
    histogram: LatencyHistogram,
    requests: AtomicU64,
    errors: AtomicU64,
}

impl TaskEntry {
    fn new() -> Self {
        Self {
            histogram: LatencyHistogram::new(),
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    fn snapshot(&self, name: String) -> TaskSnapshot {
        TaskSnapshot {
            name,
            requests: self.requests.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            latency: LatencySnapshot {
                p50_ms: self.histogram.percentile(0.50),
                p95_ms: self.histogram.percentile(0.95),
                p99_ms: self.histogram.percentile(0.99),
                max_ms: self.histogram.max_ms(),
                min_ms: self.histogram.min_ms(),
                mean_ms: self.histogram.mean_ms(),
            },
        }
    }
}

// ── Shared metrics ────────────────────────────────────────────────────────────

/// Shared metrics state accessed by all worker tasks and the reporter.
pub struct Metrics {
    pub histogram: Arc<LatencyHistogram>,
    pub requests_total: Arc<AtomicU64>,
    pub errors_total: Arc<AtomicU64>,
    /// Requests completed in the current 1-second window (for RPS calculation)
    pub window_requests: Arc<AtomicU64>,
    /// Errors in the current 1-second window
    pub window_errors: Arc<AtomicU64>,
    /// Per-task counters; lazily populated on first record for each task name.
    per_task: Arc<RwLock<HashMap<String, Arc<TaskEntry>>>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            histogram: Arc::new(LatencyHistogram::new()),
            requests_total: Arc::new(AtomicU64::new(0)),
            errors_total: Arc::new(AtomicU64::new(0)),
            window_requests: Arc::new(AtomicU64::new(0)),
            window_errors: Arc::new(AtomicU64::new(0)),
            per_task: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn record_success(&self, latency_ms: u64) {
        self.histogram.record(latency_ms);
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.window_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_error(&self, latency_ms: u64) {
        self.histogram.record(latency_ms);
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.errors_total.fetch_add(1, Ordering::Relaxed);
        self.window_requests.fetch_add(1, Ordering::Relaxed);
        self.window_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a successful request and attribute it to `task`.
    pub fn record_success_task(&self, task: &str, latency_ms: u64) {
        self.record_success(latency_ms);
        let entry = self.get_or_create_task(task);
        entry.histogram.record(latency_ms);
        entry.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failed request and attribute it to `task`.
    pub fn record_error_task(&self, task: &str, latency_ms: u64) {
        self.record_error(latency_ms);
        let entry = self.get_or_create_task(task);
        entry.histogram.record(latency_ms);
        entry.requests.fetch_add(1, Ordering::Relaxed);
        entry.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn get_or_create_task(&self, task: &str) -> Arc<TaskEntry> {
        // Fast path: entry already exists, read lock only.
        if let Ok(guard) = self.per_task.read() {
            if let Some(e) = guard.get(task) {
                return Arc::clone(e);
            }
        }
        // Slow path: first time seeing this task name.
        let mut guard = self.per_task.write().unwrap();
        Arc::clone(
            guard
                .entry(task.to_string())
                .or_insert_with(|| Arc::new(TaskEntry::new())),
        )
    }

    /// Snapshot per-task metrics, sorted by task name.
    pub fn task_snapshots(&self) -> Vec<TaskSnapshot> {
        let guard = self.per_task.read().unwrap();
        let mut snaps: Vec<TaskSnapshot> = guard
            .iter()
            .map(|(name, entry)| entry.snapshot(name.clone()))
            .collect();
        snaps.sort_by(|a, b| a.name.cmp(&b.name));
        snaps
    }

    pub fn drain_window(&self) -> (u64, u64) {
        let reqs = self.window_requests.swap(0, Ordering::Relaxed);
        let errs = self.window_errors.swap(0, Ordering::Relaxed);
        (reqs, errs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LatencyHistogram ──────────────────────────────────────────────────────

    #[test]
    fn histogram_empty_returns_zeros() {
        let h = LatencyHistogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.mean_ms(), 0.0);
        assert_eq!(h.max_ms(), 0.0);
        assert_eq!(h.min_ms(), 0.0);
        assert_eq!(h.percentile(0.50), 0.0);
    }

    #[test]
    fn histogram_single_record() {
        let h = LatencyHistogram::new();
        h.record(100);
        assert_eq!(h.count(), 1);
        assert_eq!(h.mean_ms(), 100.0);
        assert_eq!(h.max_ms(), 100.0);
        assert_eq!(h.min_ms(), 100.0);
    }

    #[test]
    fn histogram_percentile_p50() {
        let h = LatencyHistogram::new();
        for ms in [10, 20, 30, 40, 50] {
            h.record(ms);
        }
        // p50 of [10,20,30,40,50] → bucket 30
        assert_eq!(h.percentile(0.50), 30.0);
    }

    #[test]
    fn histogram_percentile_p99_uniform() {
        let h = LatencyHistogram::new();
        for ms in 1..=100 {
            h.record(ms);
        }
        // p99 of 100 values → bucket 99
        assert_eq!(h.percentile(0.99), 99.0);
    }

    #[test]
    fn histogram_max_min_tracked() {
        let h = LatencyHistogram::new();
        h.record(500);
        h.record(10);
        h.record(250);
        assert_eq!(h.max_ms(), 500.0);
        assert_eq!(h.min_ms(), 10.0);
    }

    #[test]
    fn histogram_clamps_to_max_bucket() {
        let h = LatencyHistogram::new();
        h.record(99_999); // beyond MAX_LATENCY_MS
        assert_eq!(h.count(), 1);
        assert_eq!(h.percentile(1.0), MAX_LATENCY_MS as f64);
    }

    #[test]
    fn histogram_mean_is_correct() {
        let h = LatencyHistogram::new();
        h.record(100);
        h.record(200);
        h.record(300);
        assert_eq!(h.mean_ms(), 200.0);
    }

    // ── Metrics ───────────────────────────────────────────────────────────────

    #[test]
    fn metrics_record_success_increments_total() {
        let m = Metrics::new();
        m.record_success(50);
        m.record_success(100);
        assert_eq!(m.requests_total.load(Ordering::Relaxed), 2);
        assert_eq!(m.errors_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn metrics_record_error_increments_both_totals() {
        let m = Metrics::new();
        m.record_error(0);
        assert_eq!(m.requests_total.load(Ordering::Relaxed), 1);
        assert_eq!(m.errors_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn metrics_drain_window_resets_counters() {
        let m = Metrics::new();
        m.record_success(10);
        m.record_success(20);
        m.record_error(5);
        let (reqs, errs) = m.drain_window();
        assert_eq!(reqs, 3);
        assert_eq!(errs, 1);
        // After drain the window is zeroed.
        let (reqs2, errs2) = m.drain_window();
        assert_eq!(reqs2, 0);
        assert_eq!(errs2, 0);
    }

    #[test]
    fn metrics_clone_shares_state() {
        let m = Metrics::new();
        let m2 = m.clone();
        m.record_success(42);
        assert_eq!(m2.requests_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn task_snapshot_tracks_per_task() {
        let m = Metrics::new();
        m.record_success_task("login", 50);
        m.record_success_task("login", 100);
        m.record_error_task("search", 200);

        assert_eq!(m.requests_total.load(Ordering::Relaxed), 3);

        let snaps = m.task_snapshots();
        assert_eq!(snaps.len(), 2);

        let login = snaps.iter().find(|s| s.name == "login").unwrap();
        assert_eq!(login.requests, 2);
        assert_eq!(login.errors, 0);
        assert!(login.latency.mean_ms > 0.0);

        let search = snaps.iter().find(|s| s.name == "search").unwrap();
        assert_eq!(search.requests, 1);
        assert_eq!(search.errors, 1);
    }

    #[test]
    fn task_snapshots_shared_across_clones() {
        let m = Metrics::new();
        let m2 = m.clone();
        m.record_success_task("t1", 10);
        // Clone shares the same per_task map.
        let snaps = m2.task_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].name, "t1");
    }
}

/// Sink for metric JSON lines. Writes to stdout in normal mode,
/// sends to a channel in serve mode.
pub type MetricSink = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// Returns a sink that writes metric lines to stdout (default behaviour).
pub fn stdout_sink() -> MetricSink {
    std::sync::Arc::new(|line| println!("{}", line))
}

impl Clone for Metrics {
    fn clone(&self) -> Self {
        Self {
            histogram: self.histogram.clone(),
            requests_total: self.requests_total.clone(),
            errors_total: self.errors_total.clone(),
            window_requests: self.window_requests.clone(),
            window_errors: self.window_errors.clone(),
            per_task: self.per_task.clone(),
        }
    }
}
