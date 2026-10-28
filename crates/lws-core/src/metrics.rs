//! Process metrics in the Prometheus text format. Plain atomics: cheap enough
//! to update on every request and every block.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

macro_rules! metrics {
    ($( $(#[doc = $doc:literal])* $kind:ident $name:ident ),* $(,)?) => {
        #[derive(Default)]
        pub struct Metrics {
            $( $(#[doc = $doc])* pub $name: AtomicU64, )*
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
                out
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
    }
}
