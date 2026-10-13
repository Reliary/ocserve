//! Session revert / unrevert engine (SNAPSHOT.md §2, upstream
//! `session/revert.ts` + `session/summary.ts` @ v1.18.31).
//!
//! `revert` finds the target `{messageID, partID?}`, snapshots the worktree if
//! needed, restores the pre-turn files, reverts the tool `patch` parts,
//! computes the revert-range diff, persists per-message `summary.diffs` +
//! session `summary`, emits `session.diff`, and sets the session marker.
//! `unrevert` restores the snapshot and clears the marker. File work is
//! best-effort via the snapshot engine (non-git ⇒ no-op, upstream parity).

use serde_json::{Value, json};

use crate::snapshot::{Patch, Snapshots};

/// The revert marker stored on the session (`session.revert`).
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub message_id: String,
    pub part_id: Option<String>,
    pub snapshot: Option<String>,
    pub diff: Option<String>,
}

impl Marker {
    pub fn to_value(&self) -> Value {
        let mut m = json!({"messageID": self.message_id});
        if let Some(p) = &self.part_id {
            m["partID"] = Value::String(p.clone());
        }
        if let Some(s) = &self.snapshot {
            m["snapshot"] = Value::String(s.clone());
        }
        if let Some(d) = &self.diff {
            m["diff"] = Value::String(d.clone());
        }
        m
    }

    pub fn from_value(v: &Value) -> Option<Self> {
        let message_id = v.get("messageID").and_then(|x| x.as_str())?.to_string();
        Some(Self {
            message_id,
            part_id: v.get("partID").and_then(|x| x.as_str()).map(String::from),
            snapshot: v.get("snapshot").and_then(|x| x.as_str()).map(String::from),
            diff: v.get("diff").and_then(|x| x.as_str()).map(String::from),
        })
    }
}

/// Result of a successful `revert` — the values the HTTP handler needs to
/// persist/emit. The DB reads/writes stay in the store crate.
pub struct RevertOutcome {
    pub marker: Marker,
    /// per-user-message `summary.diffs` for the revert range (messageID → diffs)
    pub message_diffs: Vec<(String, Value)>,
    /// session summary `{additions,deletions,files}`
    pub summary: Value,
    /// the `session.diff` event payload (`FileDiff[]`)
    pub diff: Value,
}

/// Find the revert target `{messageID, partID?}` by walking the message graph
/// (upstream `revert.ts:43-67`): the first part matching `partID` (when given)
/// or the message matching `messageID` (when no partID). Returns the resolved
/// `(messageID, Option<partID>)`.
pub fn resolve_target(
    messages: &[(Value, Vec<Value>)],
    message_id: &str,
    part_id: Option<&str>,
) -> Option<(String, Option<String>)> {
    let mut last_user: Option<String> = None;
    for (info, parts) in messages {
        if info["role"] == "user" {
            last_user = info["id"].as_str().map(String::from);
        }
        match part_id {
            Some(pid) => {
                if parts.iter().any(|p| p["id"].as_str() == Some(pid)) {
                    // has a text/tool part among remaining? upstream keeps partID
                    // when a text|tool part precedes it, else reverts to the user
                    let keep_part = parts.iter().any(|p| {
                        let t = p["type"].as_str().unwrap_or("");
                        (t == "text" || t == "tool")
                            && p["id"].as_str().map(|id| id < pid).unwrap_or(false)
                    });
                    let mid = if keep_part {
                        info["id"].as_str().unwrap_or(message_id).to_string()
                    } else {
                        last_user.clone().unwrap_or_else(|| message_id.to_string())
                    };
                    return Some((mid, keep_part.then(|| pid.to_string())));
                }
            }
            None => {
                if info["id"].as_str() == Some(message_id) {
                    return Some((message_id.to_string(), None));
                }
            }
        }
    }
    None
}

/// Collect the `patch` parts (snapshot hash + files) that apply to the revert
/// range, deduped by file (upstream `revert.ts:44-52`).
pub fn collect_patches(messages: &[(Value, Vec<Value>)]) -> Vec<Patch> {
    let mut by_hash: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for (_info, parts) in messages {
        for p in parts {
            if p["type"] != "patch" {
                continue;
            }
            let hash = p["hash"].as_str().unwrap_or("");
            if hash.is_empty() {
                continue;
            }
            for f in p["files"].as_array().into_iter().flatten() {
                if let Some(fs) = f.as_str()
                    && seen.insert(fs.to_string())
                {
                    by_hash
                        .entry(hash.to_string())
                        .or_default()
                        .push(fs.to_string());
                }
            }
        }
    }
    by_hash
        .into_iter()
        .map(|(hash, files)| Patch { hash, files })
        .collect()
}

/// Compute the revert-range per-message diffs + session summary from the
/// messages from the target onward (upstream `revert.ts:72-83` +
/// `summary.computeDiff`): the first `step-start.snapshot` is `from`, the last
/// `step-finish.snapshot` is `to`; `diffFull(from,to)` gives the file diffs.
pub fn compute_range(
    snapshots: &Snapshots,
    messages: &[(Value, Vec<Value>)],
) -> (Vec<Value>, Value) {
    let mut from: Option<String> = None;
    let mut to: Option<String> = None;
    for (_info, parts) in messages {
        if from.is_none() {
            for p in parts {
                if p["type"] == "step-start"
                    && let Some(s) = p["snapshot"].as_str()
                {
                    from = Some(s.to_string());
                    break;
                }
            }
        }
        for p in parts {
            if p["type"] == "step-finish"
                && let Some(s) = p["snapshot"].as_str()
            {
                to = Some(s.to_string());
            }
        }
    }
    let diffs = match (from, to) {
        (Some(f), Some(t)) => snapshots.diff_full(&f, &t),
        _ => Vec::new(),
    };
    let additions: i64 = diffs.iter().filter_map(|d| d["additions"].as_i64()).sum();
    let deletions: i64 = diffs.iter().filter_map(|d| d["deletions"].as_i64()).sum();
    let files = diffs.len() as i64;
    (
        diffs,
        json!({"additions": additions, "deletions": deletions, "files": files}),
    )
}
