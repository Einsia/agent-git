//! Prometheus text-exposition metrics for agit-hub — hand-rolled on plain atomics.
//!
//! **Why not the `prometheus` crate.** The rest of this project is deliberately lean and hermetic
//! (see Cargo.toml: rust-s3 over aws-sdk, tokio-rustls over system OpenSSL). The `prometheus` crate
//! drags in a protobuf stack and a global default registry we do not need — a handful of `AtomicU64`
//! plus a `render()` that prints the text format (v0.0.4) is a few dozen lines, has no transitive
//! deps, and lets us pin label cardinality by construction rather than by discipline.
//!
//! **Cardinality is bounded by construction.** Every label value here is drawn from a *fixed* set —
//! HTTP method folded to a 7-way enum, status folded to its class (`2xx`…), auth/push results to a
//! closed list. User-controlled strings (agent names, usernames, owners, paths) are **never** used as
//! a label value, so no request can grow the metric families. That is the whole reason the storage is
//! a fixed-size array of atomics rather than a map keyed on strings.
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// HTTP methods we count, folded to a closed set (anything else → `OTHER`). Order matches
/// [`method_idx`].
const METHODS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OTHER"];

fn method_idx(m: &str) -> usize {
    match m {
        "GET" => 0,
        "POST" => 1,
        "PUT" => 2,
        "DELETE" => 3,
        "PATCH" => 4,
        "HEAD" => 5,
        _ => 6,
    }
}

/// Status *class* — the label value for `http_requests_total`. Folding 3-digit codes to their class
/// keeps the family at 7×5 series max no matter what git http-backend returns.
const CLASSES: [&str; 5] = ["1xx", "2xx", "3xx", "4xx", "5xx"];

fn class_idx(status: u16) -> usize {
    match status / 100 {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        _ => 4, // 5xx and anything malformed
    }
}

/// Latency histogram bucket upper bounds (seconds) and their exposition strings. The classic
/// Prometheus default ladder.
const BUCKETS: [(f64, &str); 11] = [
    (0.005, "0.005"),
    (0.01, "0.01"),
    (0.025, "0.025"),
    (0.05, "0.05"),
    (0.1, "0.1"),
    (0.25, "0.25"),
    (0.5, "0.5"),
    (1.0, "1"),
    (2.5, "2.5"),
    (5.0, "5"),
    (10.0, "10"),
];

/// Auth outcomes — a closed set, so `result` can never be an unbounded label. Order matches
/// [`AuthResult::idx`].
const AUTH_RESULTS: [&str; 4] = ["login_ok", "login_fail", "token_ok", "token_denied"];

#[derive(Clone, Copy)]
pub enum AuthResult {
    LoginOk,
    LoginFail,
    TokenOk,
    TokenDenied,
}

impl AuthResult {
    fn idx(self) -> usize {
        match self {
            AuthResult::LoginOk => 0,
            AuthResult::LoginFail => 1,
            AuthResult::TokenOk => 2,
            AuthResult::TokenDenied => 3,
        }
    }
}

/// The whole metrics surface. One instance lives behind an `Arc` on `CtxInner`; every handler that
/// records shares it. All counters are `Relaxed` — order between metric writes never matters, only
/// that each add is atomic.
pub struct Metrics {
    start: Instant,
    /// `http_requests_total[method][class]`.
    requests: [[AtomicU64; 5]; 7],
    /// Per-bucket observation counts (NON-cumulative; `render` sums them into the cumulative `le`
    /// form). An observation lands in the first bucket whose bound it fits; one above 10s lands in
    /// none (only in `_count` / `+Inf`).
    duration_buckets: [AtomicU64; 11],
    duration_count: AtomicU64,
    /// Summed latency in microseconds (integer, so the running total is exact and lock-free);
    /// rendered as seconds.
    duration_sum_micros: AtomicU64,
    auth: [AtomicU64; 4],
    git_push_accepted: AtomicU64,
    git_push_rejected: AtomicU64,
    secret_scan_rejects: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Metrics {
        Metrics {
            start: Instant::now(),
            requests: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            duration_count: AtomicU64::new(0),
            duration_sum_micros: AtomicU64::new(0),
            auth: std::array::from_fn(|_| AtomicU64::new(0)),
            git_push_accepted: AtomicU64::new(0),
            git_push_rejected: AtomicU64::new(0),
            secret_scan_rejects: AtomicU64::new(0),
        }
    }

    /// Record one finished request: bump the `{method,class}` counter and the latency histogram.
    pub fn record_request(&self, method: &str, status: u16, dur_secs: f64) {
        self.requests[method_idx(method)][class_idx(status)].fetch_add(1, Ordering::Relaxed);
        self.duration_count.fetch_add(1, Ordering::Relaxed);
        let micros = (dur_secs * 1_000_000.0).max(0.0) as u64;
        self.duration_sum_micros.fetch_add(micros, Ordering::Relaxed);
        for (bound, _) in BUCKETS.iter() {
            if dur_secs <= *bound {
                let i = BUCKETS.iter().position(|(b, _)| b == bound).unwrap();
                self.duration_buckets[i].fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    pub fn record_auth(&self, r: AuthResult) {
        self.auth[r.idx()].fetch_add(1, Ordering::Relaxed);
    }

    /// A git push that passed (`accepted`) or failed (`!accepted`) the authorization gate.
    pub fn record_git_push(&self, accepted: bool) {
        let c = if accepted { &self.git_push_accepted } else { &self.git_push_rejected };
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// A push refused by the server-side secret scan. NOTE: the scan runs in the out-of-process
    /// `pre-receive` hook (a separate `agit-hub pre-receive` invocation), which cannot reach this
    /// in-memory counter; the authoritative record is the audit log's `git-push-rejected`. This
    /// counter is here for completeness and for any in-process caller, and is documented as such.
    pub fn record_secret_scan_reject(&self) {
        self.secret_scan_rejects.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the full registry in Prometheus text exposition format (v0.0.4).
    pub fn render(&self) -> String {
        let mut s = String::new();

        // ── build / uptime ──
        let _ = writeln!(s, "# HELP agit_hub_build_info Build metadata; the value is always 1.");
        let _ = writeln!(s, "# TYPE agit_hub_build_info gauge");
        let _ = writeln!(s, "agit_hub_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(s, "# HELP agit_hub_uptime_seconds Seconds since this server process started serving.");
        let _ = writeln!(s, "# TYPE agit_hub_uptime_seconds gauge");
        let _ = writeln!(s, "agit_hub_uptime_seconds {:.3}", self.start.elapsed().as_secs_f64());

        // ── http_requests_total ──
        let _ = writeln!(s, "# HELP http_requests_total Total HTTP requests by method and status class.");
        let _ = writeln!(s, "# TYPE http_requests_total counter");
        for (mi, m) in METHODS.iter().enumerate() {
            for (ci, c) in CLASSES.iter().enumerate() {
                let v = self.requests[mi][ci].load(Ordering::Relaxed);
                if v > 0 {
                    let _ = writeln!(s, "http_requests_total{{method=\"{m}\",status=\"{c}\"}} {v}");
                }
            }
        }

        // ── http_request_duration_seconds (histogram) ──
        let _ = writeln!(s, "# HELP http_request_duration_seconds HTTP request latency in seconds.");
        let _ = writeln!(s, "# TYPE http_request_duration_seconds histogram");
        let mut cumulative = 0u64;
        for (i, (_, le)) in BUCKETS.iter().enumerate() {
            cumulative += self.duration_buckets[i].load(Ordering::Relaxed);
            let _ = writeln!(s, "http_request_duration_seconds_bucket{{le=\"{le}\"}} {cumulative}");
        }
        let count = self.duration_count.load(Ordering::Relaxed);
        let _ = writeln!(s, "http_request_duration_seconds_bucket{{le=\"+Inf\"}} {count}");
        let sum_secs = self.duration_sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let _ = writeln!(s, "http_request_duration_seconds_sum {sum_secs:.6}");
        let _ = writeln!(s, "http_request_duration_seconds_count {count}");

        // ── auth_attempts_total ──
        let _ = writeln!(s, "# HELP auth_attempts_total Authentication attempts by outcome.");
        let _ = writeln!(s, "# TYPE auth_attempts_total counter");
        for (i, r) in AUTH_RESULTS.iter().enumerate() {
            let v = self.auth[i].load(Ordering::Relaxed);
            let _ = writeln!(s, "auth_attempts_total{{result=\"{r}\"}} {v}");
        }

        // ── git_push_total ──
        let _ = writeln!(s, "# HELP git_push_total git smart-http push attempts by authorization outcome.");
        let _ = writeln!(s, "# TYPE git_push_total counter");
        let _ = writeln!(s, "git_push_total{{result=\"accepted\"}} {}", self.git_push_accepted.load(Ordering::Relaxed));
        let _ = writeln!(s, "git_push_total{{result=\"rejected\"}} {}", self.git_push_rejected.load(Ordering::Relaxed));

        // ── secret_scan_rejects_total ──
        let _ = writeln!(s, "# HELP secret_scan_rejects_total Pushes refused in-process by the secret scan (see note: the pre-receive hook is out-of-process; the audit log is authoritative).");
        let _ = writeln!(s, "# TYPE secret_scan_rejects_total counter");
        let _ = writeln!(s, "secret_scan_rejects_total {}", self.secret_scan_rejects.load(Ordering::Relaxed));

        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_has_valid_exposition_shape() {
        let m = Metrics::new();
        m.record_request("GET", 200, 0.003);
        m.record_request("POST", 404, 1.5);
        let out = m.render();
        // Type declarations present for every family.
        assert!(out.contains("# TYPE http_requests_total counter"));
        assert!(out.contains("# TYPE http_request_duration_seconds histogram"));
        assert!(out.contains("# TYPE auth_attempts_total counter"));
        assert!(out.contains("# TYPE git_push_total counter"));
        assert!(out.contains("# TYPE secret_scan_rejects_total counter"));
        assert!(out.contains("# TYPE agit_hub_build_info gauge"));
        // Recorded series show up, folded to method + status class.
        assert!(out.contains("http_requests_total{method=\"GET\",status=\"2xx\"} 1"));
        assert!(out.contains("http_requests_total{method=\"POST\",status=\"4xx\"} 1"));
        // Histogram totals.
        assert!(out.contains("http_request_duration_seconds_bucket{le=\"+Inf\"} 2"));
        assert!(out.contains("http_request_duration_seconds_count 2"));
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let m = Metrics::new();
        m.record_request("GET", 200, 0.003); // <= 0.005
        m.record_request("GET", 200, 0.2); // <= 0.25
        let out = m.render();
        // The 0.003 obs is in every bucket from 0.005 up; the 0.2 obs joins from 0.25 up.
        assert!(out.contains("http_request_duration_seconds_bucket{le=\"0.005\"} 1"));
        assert!(out.contains("http_request_duration_seconds_bucket{le=\"0.1\"} 1"));
        assert!(out.contains("http_request_duration_seconds_bucket{le=\"0.25\"} 2"));
        assert!(out.contains("http_request_duration_seconds_bucket{le=\"+Inf\"} 2"));
    }

    #[test]
    fn unknown_method_folds_to_other_and_status_to_class() {
        let m = Metrics::new();
        m.record_request("BREW", 418, 0.01);
        let out = m.render();
        assert!(out.contains("http_requests_total{method=\"OTHER\",status=\"4xx\"} 1"));
    }
}
