//! L1 recycle decision + memory-pressure probe (SRE §5 — the "middle
//! ground" resource stack: bounded-by-construction → graceful recycle →
//! partition → OOMPolicy → MemoryMax backstop).
//!
//! Pure functions live here so every guard has a testable seam; the sampler
//! (ocserve-cli main) supplies the observations.

/// `OCSERVE_SIDECAR_RECYCLE_MB` parse: `0` disables, absent → default 450
/// (warm-up peak measured 365 MB — the threshold must sit above it or every
/// fresh spawn would be recycled again after one embed).
pub fn parse_threshold_mb(raw: Option<&str>) -> u64 {
    match raw {
        None => 450,
        Some(s) => s.parse().unwrap_or(450),
    }
}

pub fn recycle_threshold_mb() -> u64 {
    parse_threshold_mb(std::env::var("OCSERVE_SIDECAR_RECYCLE_MB").ok().as_deref())
}

/// The L1 decision. All five conditions must hold (each one has a planted
/// control test — removing it turns a test red):
/// - threshold enabled (`0` = kill switch off)
/// - RSS at/above threshold
/// - ≥2 consecutive samples (debounce: one warm-up blip must not recycle)
/// - zero in-flight plugin RPCs (hooks are never interrupted)
/// - min uptime (a respawned sidecar warms up again — recycling it inside
///   the window would be a respawn→warm→recycle storm)
pub fn should_recycle(
    threshold_mb: u64,
    rss_mb: u64,
    consecutive_high: u32,
    in_flight: usize,
    uptime_s: u64,
) -> bool {
    threshold_mb > 0
        && rss_mb >= threshold_mb
        && consecutive_high >= 2
        && in_flight == 0
        && uptime_s >= 300
}

/// First line of `/proc/pressure/memory` is `some avg10=… avg60=… total=…`.
/// Returns avg10 × 100 as an integer (gauge is i64; 12.34% → 1234).
pub fn parse_pressure_avg10(text: &str) -> Option<i64> {
    let some = text.lines().find(|l| l.starts_with("some "))?;
    let tok = some.split_whitespace().find(|t| t.starts_with("avg10="))?;
    let v: f64 = tok.trim_start_matches("avg10=").parse().ok()?;
    Some((v * 100.0).round() as i64)
}

/// PSI for OUR cgroup (cgroup v2). Missing file / cgroup v1 → None (gauge
/// simply absent — never an error, this is instrumentation not control flow).
pub fn psi_avg10_centi() -> Option<i64> {
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cg.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let text = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.pressure")).ok()?;
    parse_pressure_avg10(&text)
}

/// VmRSS of one specific pid in bytes (the sampler's sidecar decision must
/// NOT use "first node/bun child of mine" — after any respawn the plugin
/// browser host can sort first and would be recycled by mistake).
pub fn rss_of_pid(pid: u32) -> Option<i64> {
    if pid == 0 {
        return None;
    }
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<i64>()
        .ok()
        .map(|kb| kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_defaults_and_kill_switch() {
        assert_eq!(parse_threshold_mb(None), 450);
        assert_eq!(parse_threshold_mb(Some("0")), 0);
        assert_eq!(parse_threshold_mb(Some("600")), 600);
        assert_eq!(parse_threshold_mb(Some("junk")), 450); // never panic on env
    }

    #[test]
    fn recycle_needs_every_condition() {
        // baseline: everything true → recycles
        assert!(should_recycle(450, 500, 2, 0, 400));
        // kill switch (threshold 0) → never (planted control target)
        assert!(!should_recycle(0, 9999, 9, 0, 9999));
        // below threshold → no
        assert!(!should_recycle(450, 449, 2, 0, 400));
        // debounce: single high sample (warm-up blip) → no
        assert!(!should_recycle(450, 500, 1, 0, 400));
        // in-flight hook → never interrupt
        assert!(!should_recycle(450, 500, 2, 1, 400));
        // min uptime: fresh spawn (warm-up again soon) → no storm
        assert!(!should_recycle(450, 500, 2, 0, 299));
        assert!(should_recycle(450, 500, 2, 0, 300));
    }

    #[test]
    fn pressure_line_parses() {
        let text = "some avg10=12.34 avg60=3.00 avg300=1.00 total=251798\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=250908\n";
        assert_eq!(parse_pressure_avg10(text), Some(1234));
        assert_eq!(parse_pressure_avg10("some avg10=0.00 total=0\n"), Some(0));
        assert_eq!(parse_pressure_avg10("full avg10=0.00 total=0\n"), None); // no `some` line
        assert_eq!(parse_pressure_avg10("garbage"), None);
    }

    #[test]
    fn rss_reads_own_process() {
        // this very test process: rss must be > 0
        let me = std::process::id();
        assert!(rss_of_pid(me).unwrap_or(0) > 0);
        assert_eq!(rss_of_pid(0), None);
        assert_eq!(rss_of_pid(4194305), None); // non-existent high pid
    }

    #[test]
    fn live_psi_is_readable_or_absent() {
        // cgroup v2 box: Some; either way must not panic
        let _ = psi_avg10_centi();
    }
}
