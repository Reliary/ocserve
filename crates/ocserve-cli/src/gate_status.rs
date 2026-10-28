//! Gate health: did the verification programs actually run, recently, and pass?
//!
//! This exists because the opposite was proven on 2026-10-27. `nightly.sh` had
//! been exiting non-zero since 2026-10-17 (19 of 21 runs red), and nothing
//! anywhere surfaced it: the timer's `Result=exit-code` lived only in
//! `systemctl --user show`, the log lived in a gitignored directory, and
//! `doctor` — the instrument an operator actually runs — said nothing at all.
//! A gate nobody sees is not a gate.
//!
//! The contract is a receipt, not log parsing: each gate script writes
//! `<data_dir>/gate-status/<gate>.json` atomically (see
//! `scripts/lib/gate-status.sh`), and this module reads them. That keeps the
//! check independent of where the repo is checked out, of journald retention,
//! and of whether systemd is present at all.
//!
//! Two receipt kinds:
//!
//! * `run`     — a discrete program (nightly, drift watch). Judged by `result`
//!   and the age of `finished_at`.
//! * `sampler` — a long-running loop (soak). Judged by the freshness of the
//!   output file it declares, plus the health code on the last row.
//!
//! Absent receipts are INFORMATIONAL, never a failure: a bare machine with no
//! timers installed must still get `doctor` exit 0 (the boot-with-defaults
//! invariant). Once a unit is installed, or a receipt exists, staleness is a
//! failure — silence is the thing that must not be green.

use std::path::{Path, PathBuf};

/// Expected cadence and the unit that produces a gate, when one is installed.
struct Gate {
    /// Receipt file stem, and the label `doctor` prints.
    name: &'static str,
    /// systemd user unit that runs it (empty when there is no unit).
    unit: &'static str,
    /// How often it is expected to run.
    cadence_secs: i64,
}

const GATES: &[Gate] = &[
    Gate {
        name: "nightly",
        unit: "ocserve-nightly",
        cadence_secs: 24 * 3600,
    },
    Gate {
        name: "drift-watch",
        unit: "ocserve-drift-watch",
        cadence_secs: 24 * 3600,
    },
    Gate {
        name: "soak",
        unit: "ocserve-soak",
        cadence_secs: 24 * 3600,
    },
];

/// Grace over the cadence before a gate counts as stale. A daily timer checked
/// at any point in its day is never stale; two missed days always is.
const STALE_GRACE_NUM: i64 = 3;
const STALE_GRACE_DEN: i64 = 2;

/// A run that has said `running` for this long died without closing its receipt.
const RUNNING_ABANDONED_SECS: i64 = 2 * 3600;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Receipt {
    pub gate: String,
    /// `run` or `sampler`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// `running`, `ok`, or `failed`.
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: i64,
    /// Steps that failed, for the operator to jump straight to.
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default)]
    pub detail: String,
    /// Sampler only: the file it appends to, and its tick interval.
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub interval: i64,
}

fn default_kind() -> String {
    "run".to_string()
}

/// What `doctor` reports for one gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// Last run failed.
    Failed,
    /// Last run is older than the grace window, or never completed.
    Stale,
    /// A run started and never closed.
    Abandoned,
    /// Sampler output is not being written.
    Dead,
    /// Nothing has run and nothing is installed: informational.
    Unknown,
}

impl Verdict {
    fn is_failure(self) -> bool {
        matches!(
            self,
            Verdict::Failed | Verdict::Stale | Verdict::Abandoned | Verdict::Dead
        )
    }

    fn label(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Failed => "FAILED",
            Verdict::Stale => "STALE",
            Verdict::Abandoned => "ABANDONED",
            Verdict::Dead => "NOT SAMPLING",
            Verdict::Unknown => "not run",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone)]
pub struct GateRow {
    pub name: String,
    pub verdict: Verdict,
    pub age_secs: Option<i64>,
    pub detail: String,
}

impl GateRow {
    /// Human age, coarse on purpose: the question is "days or minutes".
    pub fn age_text(&self) -> String {
        match self.age_secs {
            None => "-".to_string(),
            Some(s) if s < 0 => "clock skew".to_string(),
            Some(s) if s < 90 => format!("{s}s ago"),
            Some(s) if s < 90 * 60 => format!("{}m ago", s / 60),
            Some(s) if s < 48 * 3600 => format!("{}h ago", s / 3600),
            Some(s) => format!("{}d ago", s / 86400),
        }
    }
}

fn receipt_path(data_dir: &Path, gate: &str) -> PathBuf {
    data_dir.join("gate-status").join(format!("{gate}.json"))
}

/// Read one receipt. A corrupt receipt is a failure, not an absence: it means
/// a write was torn, and a torn receipt must never read as "never ran".
fn read_receipt(path: &Path) -> Result<Option<Receipt>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("unreadable: {e}")),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("corrupt receipt: {e}"))
}

/// Newest modification time of a sampler output, in unix seconds.
fn mtime_secs(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// The health code recorded on the newest row of a soak CSV.
///
/// The column is `health` at index 9 (see `scripts/soak.sh`); `000` means the
/// probe could not connect and anything `5xx`/non-`2xx` means the server
/// answered badly. Both are failures even when the file is being written.
fn last_health(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let row = text.lines().rev().find(|l| !l.trim().is_empty())?;
    // Header only (no tick yet) must read as "no reading", not as code "health".
    if row.starts_with("ts,") {
        return None;
    }
    row.split(',').nth(9).map(|s| s.trim().to_string())
}

/// Judge one gate.
///
/// `unit_installed` is true when a systemd unit exists for the gate, which is
/// what turns "never ran" from information into a failure.
fn judge(gate: &Gate, data_dir: &Path, unit_installed: bool, now: i64) -> GateRow {
    let path = receipt_path(data_dir, gate.name);
    let receipt = match read_receipt(&path) {
        Ok(r) => r,
        Err(e) => {
            return GateRow {
                name: gate.name.to_string(),
                verdict: Verdict::Failed,
                age_secs: None,
                detail: e,
            };
        }
    };
    let Some(r) = receipt else {
        return GateRow {
            name: gate.name.to_string(),
            verdict: if unit_installed {
                Verdict::Stale
            } else {
                Verdict::Unknown
            },
            age_secs: None,
            detail: if unit_installed {
                "unit is installed but no receipt exists (never completed a run)".to_string()
            } else {
                "no receipt (not installed on this host)".to_string()
            },
        };
    };

    let stale_after = gate.cadence_secs * STALE_GRACE_NUM / STALE_GRACE_DEN;

    if r.kind == "sampler" {
        let out = Path::new(&r.path);
        let age = mtime_secs(out).map(|t| now - t);
        let interval = if r.interval > 0 { r.interval } else { 60 };
        let live = age.is_some_and(|a| a <= interval * 3);
        if !live {
            return GateRow {
                name: gate.name.to_string(),
                verdict: Verdict::Dead,
                age_secs: age,
                detail: format!(
                    "sampler output {} is not being written (interval {interval}s)",
                    if r.path.is_empty() {
                        "<unset>"
                    } else {
                        &r.path
                    }
                ),
            };
        }
        match last_health(out).as_deref() {
            Some("200") | Some("204") => GateRow {
                name: gate.name.to_string(),
                verdict: Verdict::Ok,
                age_secs: age,
                detail: "sampling; last health 2xx".to_string(),
            },
            Some(code) => GateRow {
                name: gate.name.to_string(),
                verdict: Verdict::Failed,
                age_secs: age,
                detail: format!("sampling, but last health code is {code}"),
            },
            None => GateRow {
                name: gate.name.to_string(),
                verdict: Verdict::Failed,
                age_secs: age,
                detail: "cannot read the health column from the sampler output".to_string(),
            },
        }
    } else {
        // Discrete run.
        if r.result == "running" {
            let age = now - r.started_at;
            return GateRow {
                name: gate.name.to_string(),
                verdict: if age > RUNNING_ABANDONED_SECS {
                    Verdict::Abandoned
                } else {
                    Verdict::Ok
                },
                age_secs: Some(age),
                detail: if age > RUNNING_ABANDONED_SECS {
                    format!("started {}s ago and never finished", age)
                } else {
                    "running".to_string()
                },
            };
        }
        let finished = if r.finished_at > 0 {
            r.finished_at
        } else {
            r.started_at
        };
        let age = now - finished;
        let detail = if r.result == "ok" {
            r.detail.clone()
        } else if r.failed.is_empty() {
            if r.detail.is_empty() {
                "failed".to_string()
            } else {
                r.detail.clone()
            }
        } else {
            format!("{} failed: {}", r.failed.len(), r.failed.join("; "))
        };
        GateRow {
            name: gate.name.to_string(),
            verdict: if r.result != "ok" {
                Verdict::Failed
            } else if age > stale_after {
                Verdict::Stale
            } else {
                Verdict::Ok
            },
            age_secs: Some(age),
            detail,
        }
    }
}

/// Is the systemd user unit for this gate installed on this host?
fn unit_installed(unit: &str) -> bool {
    if unit.is_empty() {
        return false;
    }
    let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) else {
        return false;
    };
    PathBuf::from(home)
        .join(".config/systemd/user")
        .join(format!("{unit}.service"))
        .exists()
}

/// Evaluate every gate. Pure apart from the filesystem, so tests can drive it
/// with a temporary data dir and a fixed clock.
fn rows_with(data_dir: &Path, now: i64, installed: impl Fn(&str) -> bool) -> Vec<GateRow> {
    GATES
        .iter()
        .map(|g| judge(g, data_dir, installed(g.unit), now))
        .collect()
}

/// Now, in unix seconds. One clock for every judgement so a receipt and a
/// sampler output are never compared across different time bases.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn rows_at(data_dir: &Path, now: i64) -> Vec<GateRow> {
    rows_with(data_dir, now, unit_installed)
}

pub fn rows(data_dir: &Path) -> Vec<GateRow> {
    rows_at(data_dir, now_secs())
}

/// Print the gate section and report whether every gate is healthy.
///
/// Returns `true` when there is nothing to complain about — including the bare
/// machine case, where no gate has ever run and no unit is installed.
pub fn report(data_dir: &Path) -> bool {
    let rows = rows(data_dir);
    let failures = rows.iter().filter(|r| r.verdict.is_failure()).count();
    println!(
        "  gates:           {} tracked, {failures} unhealthy",
        rows.len()
    );
    for r in &rows {
        let warn = if r.verdict.is_failure() {
            "  <-- INVESTIGATE"
        } else {
            ""
        };
        println!(
            "    {:<12} {:<12} {:>10}  {}{warn}",
            r.name,
            r.verdict.label(),
            r.age_text(),
            r.detail
        );
    }
    failures == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, gate: &str, body: &str) {
        let p = receipt_path(dir, gate);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn ok_run(gate: &str, finished: i64) -> String {
        format!(
            r#"{{"gate":"{gate}","kind":"run","result":"ok","started_at":{},"finished_at":{finished},"failed":[],"detail":"all steps ok"}}"#,
            finished - 300
        )
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn bare_machine_is_not_a_failure() {
        // No data dir at all: nothing ran, nothing installed, doctor stays green.
        // This is the boot-with-defaults invariant; the check must never turn a
        // fresh install into a red run.
        let d =
            std::env::temp_dir().join(format!("ocserve-gate-bare-{}-{}", std::process::id(), NOW));
        let _ = std::fs::remove_dir_all(&d);
        // installed=false is the point: this host HAS the units, and the test
        // must prove the BARE-machine contract, not this box's state.
        let rows = rows_with(&d, NOW, |_| false);
        assert!(rows.iter().all(|r| !r.verdict.is_failure()));
        assert!(rows.iter().any(|r| r.verdict == Verdict::Unknown));
        // And the mirror image: an installed unit with no receipt IS a failure.
        let installed = rows_with(&d, NOW, |_| true);
        assert!(installed.iter().all(|r| r.verdict == Verdict::Stale));
    }

    #[test]
    fn fresh_ok_receipt_is_green() {
        let d = tempdir();
        write(&d, "nightly", &ok_run("nightly", NOW - 3600));
        write(&d, "drift-watch", &ok_run("drift-watch", NOW - 3600));
        let rows = rows_at(&d, NOW);
        let n = rows.iter().find(|r| r.name == "nightly").unwrap();
        assert_eq!(n.verdict, Verdict::Ok);
    }

    #[test]
    fn failed_receipt_names_the_failed_steps() {
        let d = tempdir();
        write(
            &d,
            "nightly",
            &format!(
                r#"{{"gate":"nightly","kind":"run","result":"failed","started_at":{},"finished_at":{},"failed":["cargo audit","capdiff"],"detail":""}}"#,
                NOW - 600,
                NOW - 60
            ),
        );
        let rows = rows_at(&d, NOW);
        let n = rows.iter().find(|r| r.name == "nightly").unwrap();
        assert_eq!(n.verdict, Verdict::Failed);
        assert!(n.detail.contains("2 failed"), "detail was {:?}", n.detail);
        assert!(n.detail.contains("capdiff"));
    }

    #[test]
    fn old_ok_receipt_is_stale_after_the_grace_window() {
        let d = tempdir();
        // A daily gate is green for 36h and stale after it.
        write(&d, "nightly", &ok_run("nightly", NOW - 30 * 3600));
        assert_eq!(
            rows_at(&d, NOW)
                .iter()
                .find(|r| r.name == "nightly")
                .unwrap()
                .verdict,
            Verdict::Ok
        );
        write(&d, "nightly", &ok_run("nightly", NOW - 40 * 3600));
        assert_eq!(
            rows_at(&d, NOW)
                .iter()
                .find(|r| r.name == "nightly")
                .unwrap()
                .verdict,
            Verdict::Stale
        );
    }

    #[test]
    fn corrupt_receipt_is_a_failure_not_an_absence() {
        // A torn write must never read as "never ran" and thereby as green.
        let d = tempdir();
        write(&d, "nightly", "{\"gate\":\"nightl");
        let n = rows_at(&d, NOW)
            .into_iter()
            .find(|r| r.name == "nightly")
            .unwrap();
        assert_eq!(n.verdict, Verdict::Failed);
        assert!(n.detail.contains("corrupt"), "detail was {:?}", n.detail);
    }

    #[test]
    fn abandoned_running_receipt_is_not_green() {
        let d = tempdir();
        write(
            &d,
            "nightly",
            &format!(
                r#"{{"gate":"nightly","kind":"run","result":"running","started_at":{},"finished_at":0,"failed":[],"detail":""}}"#,
                NOW - 5 * 3600
            ),
        );
        let n = rows_at(&d, NOW)
            .into_iter()
            .find(|r| r.name == "nightly")
            .unwrap();
        assert_eq!(n.verdict, Verdict::Abandoned);
    }

    #[test]
    fn sampler_judged_by_output_freshness_and_health_code() {
        let d = tempdir();
        let out = d.join("soak.csv");
        std::fs::write(
            &out,
            "ts,rss,peak,sidecar,wal,sse,queue,locks,tasks,health\n1,2,3,4,5,6,7,8,9,200\n",
        )
        .unwrap();
        write(
            &d,
            "soak",
            &format!(
                r#"{{"gate":"soak","kind":"sampler","result":"running","started_at":{},"finished_at":0,"failed":[],"detail":"","path":"{}","interval":60}}"#,
                NOW - 3600,
                out.display()
            ),
        );
        // Fresh file with health 200 → ok (the mtime is "now" by construction,
        // so this also proves the check reads mtime rather than a receipt field).
        let now = now_secs();
        let r = rows_with(&d, now, |_| false)
            .into_iter()
            .find(|x| x.name == "soak")
            .unwrap();
        assert_eq!(r.verdict, Verdict::Ok, "detail {:?}", r.detail);

        // A stale output file is NOT SAMPLING even though the receipt is there.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let f = std::fs::File::options().write(true).open(&out).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let r = rows_with(&d, now, |_| false)
            .into_iter()
            .find(|x| x.name == "soak")
            .unwrap();
        assert_eq!(r.verdict, Verdict::Dead, "detail {:?}", r.detail);
    }

    #[test]
    fn sampler_with_a_dead_probe_is_a_failure() {
        let d = tempdir();
        let out = d.join("soak.csv");
        std::fs::write(
            &out,
            "ts,rss,peak,sidecar,wal,sse,queue,locks,tasks,health\n1,2,3,4,5,6,7,8,9,000\n",
        )
        .unwrap();
        write(
            &d,
            "soak",
            &format!(
                r#"{{"gate":"soak","kind":"sampler","result":"running","started_at":{},"finished_at":0,"failed":[],"detail":"","path":"{}","interval":60}}"#,
                now_secs() - 120,
                out.display()
            ),
        );
        let r = rows_with(&d, now_secs(), |_| false)
            .into_iter()
            .find(|x| x.name == "soak")
            .unwrap();
        assert_eq!(r.verdict, Verdict::Failed);
        assert!(r.detail.contains("000"));
    }

    /// Unique temp dir per call: parallel tests in one binary share a pid.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "ocserve-gate-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
