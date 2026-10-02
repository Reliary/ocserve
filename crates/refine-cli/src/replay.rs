//! Differential replay: recorded corpus (testdata/golden) vs a live target.
//! TESTING §4 system level / PLAN §6 — the harness is the oracle for wire compat.
//!
//! Modes per manifest entry:
//! - "bytes": response body must match the recorded body byte-for-byte
//!   (status + bytes; volatile fields are excluded from the corpus at record time)
//! - "keys":  JSON key-path projection must be a subset-equal of recorded keys
//!   (used for volatile routes where values change: session list, project, …)
//!
//! Self-test: replay against upstream 1.18.31 must pass 100% (harness validity,
//! M0 exit criterion). Replay against refine must pass the implemented subset.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Entry {
    pub path: String,
    pub status: u16,
    pub mode: String,
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    #[serde(default)]
    #[allow(dead_code)] // recorded for corpus audit, not compared
    pub len: Option<usize>,
    /// Declared deferral milestone (never a silent skip — printed every run).
    #[serde(default)]
    pub defer: Option<String>,
}

pub type Manifest = std::collections::BTreeMap<String, Entry>;

pub fn load_manifest(corpus: &Path) -> Result<Manifest> {
    let raw =
        std::fs::read_to_string(corpus.join("manifest.json")).context("read manifest.json")?;
    serde_json::from_str(&raw).context("parse manifest")
}

pub struct RouteResult {
    #[allow(dead_code)] // kept for failure aggregation context
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Replay one GET route against `base`.
pub async fn replay_route(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    e: &Entry,
) -> RouteResult {
    let mut rr = RouteResult {
        name: name.to_string(),
        ok: true,
        detail: String::new(),
    };
    let resp = match client.get(format!("{base}{}", e.path)).send().await {
        Ok(r) => r,
        Err(err) => {
            rr.ok = false;
            rr.detail = format!("request failed: {err}");
            return rr;
        }
    };
    let status = resp.status().as_u16();
    if status != e.status {
        rr.ok = false;
        rr.detail = format!("status {status} != recorded {}", e.status);
        return rr;
    }
    match e.mode.as_str() {
        "bytes" => {
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(err) => {
                    rr.ok = false;
                    rr.detail = format!("body read: {err}");
                    return rr;
                }
            };
            let corpus_file = format!("{name}.body");
            let corpus_path = corpus_dir().join(&corpus_file);
            let recorded = match std::fs::read(&corpus_path) {
                Ok(b) => b,
                Err(err) => {
                    rr.ok = false;
                    rr.detail = format!("corpus read {corpus_file}: {err}");
                    return rr;
                }
            };
            if body.as_ref() != recorded.as_slice() {
                rr.ok = false;
                rr.detail = format!(
                    "bytes differ (got {}B, recorded {}B): got={:?}… recorded={:?}…",
                    body.len(),
                    recorded.len(),
                    String::from_utf8_lossy(&body[..body.len().min(80)]),
                    String::from_utf8_lossy(&recorded[..recorded.len().min(80)]),
                );
            }
        }
        "keys" => {
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(err) => {
                    rr.ok = false;
                    rr.detail = format!("body read: {err}");
                    return rr;
                }
            };
            let value: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(err) => {
                    rr.ok = false;
                    rr.detail = format!("invalid JSON: {err}");
                    return rr;
                }
            };
            let recorded: BTreeSet<String> =
                e.keys.clone().unwrap_or_default().into_iter().collect();
            let mut got = BTreeSet::new();
            refine_http::keypaths(&value, "$", &mut got);
            if got != recorded {
                let missing: Vec<_> = recorded.difference(&got).take(5).collect();
                let extra: Vec<_> = got.difference(&recorded).take(5).collect();
                rr.ok = false;
                rr.detail = format!("key paths differ; missing={missing:?} extra={extra:?}");
            }
        }
        "keys_subset" => {
            // Weaker, explicitly-labeled oracle for volatile/high-cardinality
            // routes (provider model lists change): every RECORDED key must be
            // present in the response; extra keys allowed. Recorded lists for
            // truncated entries are a minimum-requirements sample.
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(err) => {
                    rr.ok = false;
                    rr.detail = format!("body read: {err}");
                    return rr;
                }
            };
            let value: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(err) => {
                    rr.ok = false;
                    rr.detail = format!("invalid JSON: {err}");
                    return rr;
                }
            };
            let recorded: BTreeSet<String> =
                e.keys.clone().unwrap_or_default().into_iter().collect();
            let mut got = BTreeSet::new();
            refine_http::keypaths(&value, "$", &mut got);
            let missing: Vec<_> = recorded.difference(&got).take(5).collect();
            if !missing.is_empty() {
                rr.ok = false;
                rr.detail = format!("required keys missing: {missing:?}");
            }
        }
        other => {
            rr.ok = false;
            rr.detail = format!("unknown mode {other}");
        }
    }
    rr
}

fn corpus_dir() -> std::path::PathBuf {
    // harness runs from workspace root; env override for installed use
    std::env::var_os("REFINE_CORPUS")
        .map(Into::into)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden"))
}

/// Replay the whole manifest. `allow_missing` tolerates routes the target
/// doesn't implement yet (printed separately; M1 grows the implemented set).
pub async fn replay_all(base: &str, allow_missing: bool) -> Result<(usize, usize, Vec<String>)> {
    let manifest = load_manifest(&corpus_dir())?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut pass = 0;
    let mut fail = 0;
    let mut failures = Vec::new();
    for (name, entry) in &manifest {
        if let Some(ms) = &entry.defer {
            println!("DEFER {name} (declared {ms})");
            continue;
        }
        let r = replay_route(&client, base, name, entry).await;
        if r.ok {
            pass += 1;
            println!("PASS {name}");
        } else if allow_missing && r.detail.contains("status") && r.detail.contains("404") {
            println!("SKIP {name} (not implemented yet): {}", r.detail);
        } else {
            fail += 1;
            println!("FAIL {name}: {}", r.detail);
            failures.push(format!("{name}: {}", r.detail));
        }
    }
    Ok((pass, fail, failures))
}
