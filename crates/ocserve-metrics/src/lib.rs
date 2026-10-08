//! ocserve-metrics: SRE.md §2 metric set (metric-per-KPI table).
//! Implementation note: hand-rolled Prometheus text on the main port's
//! `/metrics` + this module's shared registry (SRE.md named the `metrics`
//! crate as the vehicle; the wire format is identical and the dep-free
//! surface keeps the binary small — doc updated in the same change).
//!
//! Label cardinality is bounded by construction (AGENTS §2.3): route labels
//! are normalized (`/session/ses_x/…` → `/session/{id}/…`), event types come
//! from the frozen contract, hook names from the upstream table.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Process-wide metric registry (cheap atomics + bounded label maps).
#[derive(Default)]
pub struct Registry {
    counters: Mutex<BTreeMap<&'static str, AtomicU64>>,
    gauges: Mutex<BTreeMap<&'static str, AtomicI64>>,
    /// per-label counters: (name, label) → value
    labeled: Mutex<BTreeMap<(&'static str, String), AtomicU64>>,
    /// duration sums/counts/max in microseconds for _max gauges
    durations: Mutex<BTreeMap<(&'static str, String), Dur>>,
}

#[derive(Default)]
struct Dur {
    sum_us: u64,
    count: u64,
    max_us: u64,
}

pub static REG: Registry = Registry {
    counters: Mutex::new(BTreeMap::new()),
    gauges: Mutex::new(BTreeMap::new()),
    labeled: Mutex::new(BTreeMap::new()),
    durations: Mutex::new(BTreeMap::new()),
};

/// Monotonic RSS peak (never decreases; sampled by the serve sampler).
pub static RSS_PEAK: AtomicI64 = AtomicI64::new(0);

pub fn counter(name: &'static str, delta: u64) {
    let mut g = REG.counters.lock().expect("metrics lock");
    g.entry(name)
        .or_insert_with(|| AtomicU64::new(0))
        .fetch_add(delta, Ordering::Relaxed);
}

pub fn labeled_counter(name: &'static str, label: &str, delta: u64) {
    let mut g = REG.labeled.lock().expect("metrics lock");
    g.entry((name, label.to_string()))
        .or_insert_with(|| AtomicU64::new(0))
        .fetch_add(delta, Ordering::Relaxed);
}

pub fn gauge(name: &'static str, value: i64) {
    let mut g = REG.gauges.lock().expect("metrics lock");
    g.entry(name)
        .or_insert_with(|| AtomicI64::new(0))
        .store(value, Ordering::Relaxed);
}

/// Relative gauge adjustment (e.g., SSE client leaves).
pub fn gauge_delta(name: &'static str, delta: i64) {
    let mut g = REG.gauges.lock().expect("metrics lock");
    g.entry(name)
        .or_insert_with(|| AtomicI64::new(0))
        .fetch_add(delta, Ordering::Relaxed);
}

/// Record a duration observation (µs) for `{name}{label}` + update max.
pub fn observe(name: &'static str, label: &str, micros: u64) {
    let mut g = REG.durations.lock().expect("metrics lock");
    let d = g.entry((name, label.to_string())).or_default();
    d.sum_us += micros;
    d.count += 1;
    d.max_us = d.max_us.max(micros);
}

/// Normalize a request path to a bounded route label.
pub fn route_label(path: &str) -> String {
    // ids are prefixed tokens: ses_/msg_/prt_/evt_/call_ → {id}
    let mut out = String::with_capacity(path.len());
    for seg in path.split('/') {
        if let Some(usize_underscore) = seg.find('_') {
            let head = &seg[..usize_underscore];
            if matches!(head, "ses" | "msg" | "prt" | "evt" | "call" | "prm") {
                out.push_str("{id}");
                out.push('/');
                continue;
            }
        }
        out.push_str(seg);
        out.push('/');
    }
    while out.ends_with('/') && out.len() > 1 {
        out.pop();
    }
    out
}

/// Render the Prometheus text exposition.
pub fn render() -> String {
    let mut s = String::with_capacity(8 * 1024);
    {
        let g = REG.counters.lock().expect("metrics lock");
        // counters (name-keyed, one # TYPE per family)
        for (name, v) in g.iter() {
            s.push_str(&format!("# TYPE {name} counter\n"));
            s.push_str(&format!("{name} {}\n", v.load(Ordering::Relaxed)));
        }
    }
    {
        let g = REG.labeled.lock().expect("metrics lock");
        let mut by_name: BTreeMap<&'static str, Vec<(&String, u64)>> = BTreeMap::new();
        for ((name, label), v) in g.iter() {
            by_name
                .entry(name)
                .or_default()
                .push((label, v.load(Ordering::Relaxed)));
        }
        for (name, rows) in by_name {
            s.push_str(&format!("# TYPE {name} counter\n"));
            for (label, v) in rows {
                // label carried as `key="value"` inside the stored string
                s.push_str(&format!("{name}{{{label}}} {v}\n"));
            }
        }
    }
    {
        let g = REG.gauges.lock().expect("metrics lock");
        for (name, v) in g.iter() {
            s.push_str(&format!("# TYPE {name} gauge\n"));
            s.push_str(&format!("{name} {}\n", v.load(Ordering::Relaxed)));
        }
    }
    {
        let g = REG.durations.lock().expect("metrics lock");
        let mut by_name: BTreeMap<&'static str, Vec<(&String, &Dur)>> = BTreeMap::new();
        for ((name, label), d) in g.iter() {
            by_name.entry(name).or_default().push((label, d));
        }
        for (name, rows) in by_name {
            s.push_str(&format!("# TYPE {name} summary\n"));
            for (label, d) in rows {
                if d.count == 0 {
                    continue;
                }
                s.push_str(&format!(
                    "{name}_sum{{{label}}} {}\n",
                    d.sum_us as f64 / 1e6
                ));
                s.push_str(&format!("{name}_count{{{label}}} {}\n", d.count));
                s.push_str(&format!(
                    "{name}_max{{{label}}} {}\n",
                    d.max_us as f64 / 1e6
                ));
            }
        }
    }
    s
}

/// Current RSS in bytes (Linux VmRSS).
pub fn rss_bytes() -> Option<i64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<i64>().ok())
        .map(|kb| kb * 1024)
}

/// Sample RSS + peak (serve-side task; SRE §2).
pub fn sample_rss() {
    if let Some(r) = rss_bytes() {
        gauge("ocserve_rss_bytes", r);
        RSS_PEAK.fetch_max(r, Ordering::Relaxed);
        gauge("ocserve_rss_peak_bytes", RSS_PEAK.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_label_bounds_cardinality() {
        assert_eq!(
            route_label("/session/ses_001a0fa51a310336fe84dc7cbf/message"),
            "/session/{id}/message"
        );
        assert_eq!(route_label("/global/health"), "/global/health");
        assert_eq!(
            route_label("/session/ses_abc/message/msg_001a0fa51a310336fe84dc7cbf"),
            "/session/{id}/message/{id}"
        );
        // non-id long segments untouched (bounded but honest labels)
        assert_eq!(
            route_label("/experimental/session"),
            "/experimental/session"
        );
    }

    #[test]
    fn render_contains_types_and_values() {
        counter("ocserve_probe_total", 2);
        labeled_counter("ocserve_events_emitted_total", "type=\"session.idle\"", 3);
        gauge("ocserve_writer_queue_depth", 1);
        observe(
            "ocserve_http_request_duration_seconds",
            "route=\"/x\",method=\"GET\"",
            1_500,
        );
        let out = render();
        assert!(out.contains("ocserve_probe_total 2"), "{out}");
        assert!(
            out.contains("ocserve_events_emitted_total{type=\"session.idle\"} 3"),
            "{out}"
        );
        assert!(out.contains("ocserve_writer_queue_depth 1"), "{out}");
        assert!(
            out.contains(
                "ocserve_http_request_duration_seconds_max{route=\"/x\",method=\"GET\"} 0.0015"
            ),
            "{out}"
        );
    }

    #[test]
    fn rss_peak_monotonic() {
        sample_rss();
        let a = RSS_PEAK.load(Ordering::Relaxed);
        assert!(a > 0, "peak set from /proc");
        sample_rss();
        assert!(RSS_PEAK.load(Ordering::Relaxed) >= a, "monotonic");
    }
}
