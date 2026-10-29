//! Process metrics in the Prometheus text format, and as JSON for the
//! dashboard and instance heartbeats. Plain atomics: cheap enough to update on
//! every request and every block.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// A latency histogram with fixed bucket bounds, in seconds.
pub struct Histogram {
    bounds: &'static [f64],
    /// Observations at or below each bound (not cumulative), then +Inf.
    counts: Vec<AtomicU64>,
    sum_micros: AtomicU64,
}

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Histogram {
        Histogram {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_micros: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        let i = self.bounds.iter().position(|b| secs <= *b).unwrap_or(self.bounds.len());
        self.counts[i].fetch_add(1, Relaxed);
        self.sum_micros.fetch_add(elapsed.as_micros().min(u128::from(u64::MAX)) as u64, Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Relaxed)).sum()
    }

    /// Estimated `q` quantile (0..1) in seconds, interpolated within a
    /// bucket as Prometheus' histogram_quantile does. None when empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let counts: Vec<u64> = self.counts.iter().map(|c| c.load(Relaxed)).collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = q.clamp(0.0, 1.0) * total as f64;
        let mut seen = 0u64;
        for (i, &n) in counts.iter().enumerate() {
            if n > 0 && (seen + n) as f64 >= rank {
                let lower = if i == 0 { 0.0 } else { self.bounds[i - 1] };
                let Some(&upper) = self.bounds.get(i) else { return Some(lower) };
                return Some(lower + (upper - lower) * ((rank - seen as f64) / n as f64).clamp(0.0, 1.0));
            }
            seen += n;
        }
        self.bounds.last().copied()
    }

    fn render(&self, out: &mut String, name: &str, help: &str) {
        let _ = writeln!(out, "# HELP beldex_lws_{name} {help}");
        let _ = writeln!(out, "# TYPE beldex_lws_{name} histogram");
        let mut cumulative = 0;
        for (i, bound) in self.bounds.iter().enumerate() {
            cumulative += self.counts[i].load(Relaxed);
            let _ = writeln!(out, "beldex_lws_{name}_bucket{{le=\"{bound}\"}} {cumulative}");
        }
        cumulative += self.counts[self.bounds.len()].load(Relaxed);
        let _ = writeln!(out, "beldex_lws_{name}_bucket{{le=\"+Inf\"}} {cumulative}");
        let _ = writeln!(out, "beldex_lws_{name}_sum {}", self.sum_micros.load(Relaxed) as f64 / 1e6);
        let _ = writeln!(out, "beldex_lws_{name}_count {cumulative}");
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "count": self.count(),
            "sum_seconds": self.sum_micros.load(Relaxed) as f64 / 1e6,
            "p50": self.quantile(0.5),
            "p95": self.quantile(0.95),
            "p99": self.quantile(0.99),
            // Raw buckets, so a client can compute quantiles over a window.
            "bounds": self.bounds,
            "counts": self.counts.iter().map(|c| c.load(Relaxed)).collect::<Vec<_>>(),
        })
    }
}

const HTTP_BOUNDS: &[f64] = &[0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
const SCAN_BOUNDS: &[f64] = &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];

macro_rules! metrics {
    (
        $( $(#[doc = $doc:literal])* $kind:ident $name:ident ),* ;
        $( $(#[doc = $hdoc:literal])* histogram $hname:ident ($bounds:expr) ),* $(,)?
    ) => {
        pub struct Metrics {
            $( $(#[doc = $doc])* pub $name: AtomicU64, )*
            $( $(#[doc = $hdoc])* pub $hname: Histogram, )*
            /// Process start, unix seconds.
            pub started_at: i64,
        }

        impl Default for Metrics {
            fn default() -> Metrics {
                Metrics {
                    $( $name: AtomicU64::new(0), )*
                    $( $hname: Histogram::new($bounds), )*
                    started_at: crate::now_secs(),
                }
            }
        }

        impl Metrics {
            pub fn render(&self) -> String {
                let mut out = String::new();
                $(
                    let help = concat!($($doc),*).trim();
                    let _ = writeln!(out, "# HELP beldex_lws_{} {}", stringify!($name), help);
                    let _ = writeln!(out, "# TYPE beldex_lws_{} {}", stringify!($name), stringify!($kind));
                    let _ = writeln!(out, "beldex_lws_{} {}", stringify!($name), self.$name.load(Relaxed));
                )*
                $( self.$hname.render(&mut out, stringify!($hname), concat!($($hdoc),*).trim()); )*
                let _ = writeln!(out, "# HELP beldex_lws_build_info Version of this server.");
                let _ = writeln!(out, "# TYPE beldex_lws_build_info gauge");
                let _ = writeln!(out, "beldex_lws_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));
                let _ = writeln!(out, "# HELP beldex_lws_process_start_time_seconds Start time of the process, unix seconds.");
                let _ = writeln!(out, "# TYPE beldex_lws_process_start_time_seconds gauge");
                let _ = writeln!(out, "beldex_lws_process_start_time_seconds {}", self.started_at);
                out
            }

            /// Every value as JSON: counters and gauges by name, histograms
            /// with count, sum and estimated p50/p95/p99.
            pub fn snapshot(&self) -> serde_json::Value {
                let mut map = serde_json::Map::new();
                $( map.insert(stringify!($name).into(), self.$name.load(Relaxed).into()); )*
                $( map.insert(stringify!($hname).into(), self.$hname.json()); )*
                map.insert("started_at".into(), self.started_at.into());
                map.insert("uptime_seconds".into(), (crate::now_secs() - self.started_at).max(0).into());
                map.insert("version".into(), env!("CARGO_PKG_VERSION").into());
                serde_json::Value::Object(map)
            }
        }
    };
}

metrics! {
    /// Blocks parsed and scanned.
    counter blocks_scanned_total,
    /// Owned outputs found.
    counter outputs_found_total,
    /// Candidate spends found.
    counter spends_found_total,
    /// Chain reorganisations handled.
    counter reorgs_total,
    /// Scan passes run (each checks every lane once).
    counter scan_passes_total,
    /// Scanner batches that failed and were retried.
    counter scan_errors_total,
    /// Account commits refused because the account changed underneath.
    counter commits_rejected_total,
    /// Time spent in kernel scanning, milliseconds.
    counter scan_cpu_ms_total,
    /// Blocks on the daemon's chain.
    gauge daemon_height,
    /// Lowest scan height among active accounts this scanner owns.
    gauge min_scan_height,
    /// Active accounts this scanner owns.
    gauge scanned_accounts,
    /// Scan lanes in the last pass.
    gauge scan_lanes,
    /// HTTP requests served.
    counter http_requests_total,
    /// HTTP responses with a 4xx status.
    counter http_client_errors_total,
    /// HTTP responses with a 5xx status.
    counter http_server_errors_total,
    /// Requests refused by the per-client rate limiter.
    counter http_rate_limited_total,
    /// Requests shed because the server was at its concurrency limit.
    counter http_shed_total,
    /// Requests in flight.
    gauge http_in_flight,
    /// Accounts created through /login.
    counter accounts_created_total,
    /// New-block announcements received over OxenMQ (--sub).
    counter block_notifications_total,
    /// 1 while at least one OxenMQ block subscription is live.
    gauge block_notifications_connected,
    /// Administrative changes made through the admin API.
    counter admin_actions_total;

    /// Wallet API request latency, seconds.
    histogram http_request_duration_seconds(HTTP_BOUNDS),
    /// Time to scan and commit one batch of blocks, seconds.
    histogram scan_batch_duration_seconds(SCAN_BOUNDS),
    /// Time to download one batch of blocks from beldexd, seconds.
    histogram daemon_fetch_duration_seconds(SCAN_BOUNDS),
}

impl Metrics {
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Relaxed);
    }
    pub fn set(gauge: &AtomicU64, v: u64) {
        gauge.store(v, Relaxed);
    }
    pub fn get(value: &AtomicU64) -> u64 {
        value.load(Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let m = Metrics::default();
        Metrics::add(&m.blocks_scanned_total, 3);
        let text = m.render();
        assert!(text.contains("# TYPE beldex_lws_blocks_scanned_total counter\nbeldex_lws_blocks_scanned_total 3\n"));
        assert!(text.contains("# HELP beldex_lws_daemon_height Blocks on the daemon's chain."));
        assert!(text.contains("beldex_lws_build_info{version=\""));
    }

    #[test]
    fn histograms_render_cumulatively_and_estimate_quantiles() {
        let m = Metrics::default();
        assert_eq!(m.http_request_duration_seconds.quantile(0.5), None);
        for _ in 0..90 {
            m.http_request_duration_seconds.observe(Duration::from_micros(800));
        }
        for _ in 0..10 {
            m.http_request_duration_seconds.observe(Duration::from_millis(400));
        }
        let text = m.render();
        assert!(text.contains("beldex_lws_http_request_duration_seconds_bucket{le=\"0.001\"} 90\n"));
        assert!(text.contains("beldex_lws_http_request_duration_seconds_bucket{le=\"0.5\"} 100\n"));
        assert!(text.contains("beldex_lws_http_request_duration_seconds_count 100\n"));
        let p50 = m.http_request_duration_seconds.quantile(0.5).unwrap();
        assert!(p50 > 0.0 && p50 <= 0.001, "{p50}");
        let p99 = m.http_request_duration_seconds.quantile(0.99).unwrap();
        assert!(p99 > 0.25 && p99 <= 0.5, "{p99}");
        let snap = m.snapshot();
        assert_eq!(snap["http_request_duration_seconds"]["count"], 100);
        assert_eq!(snap["version"], env!("CARGO_PKG_VERSION"));
    }
}
